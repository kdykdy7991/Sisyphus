// Vision extraction: turn a screenshot into 0..N structured knowledge drafts.
//
// This module owns the Vision system prompt (kept centrally in the backend,
// not scattered in React) and the structured-output contract. The model is asked
// for a stable JSON shape and every response is runtime-validated here before
// any draft is allowed to reach the Review screen — malformed / empty results
// never become drafts.

use crate::db::KnowledgePayload;
use crate::llm::{OpenAiCompatClient, parse_data_url};

/// Backend-owned system prompt. The model is a personal-interview knowledge
/// organizer, not an OCR dump: it must preserve meaning, may polish phrasing,
/// must not invent facts, must split independent questions, and should produce
/// reasonable metadata + follow-ups.
pub const VISION_SYSTEM_PROMPT: &str = "\
你是一个「个人面试知识整理」助手。用户会给你一张或多张面试相关的截图（可能是题目、
笔记、面经、讲稿等）。你的任务是：\
\
1. 读取截图中的文字并理解其上下文，而不是简单 OCR 逐字抄录。\
2. 识别其中包含的每一道独立面试题，并为每一道题整理出：问题、答案、领域、主题、标签、可能的追问。\
3. 若截图中包含多道互相独立的题目，必须拆分成多条（items 数组里多条），不要强行合并成一条。\
4. 问题与答案必须一一对应：答案只回答对应的问题。\
5. 保留原始知识含义；可以整理表达方式与排版，但不要凭空补充截图里不存在的大量细节。\
6. 无法从截图确认的内容不要编造。若答案在截图中不明确，请基于该题自然的相关知识做适度整理，但不要添加与截图无关的大段杜撰内容。\
7. 为每条推荐合理的 domain（领域）、topic（主题）、tags（标签，1~4 个）。\
8. 提取截图中已出现或自然隐含的追问（followUpQuestions）；没有则返回空数组。\
9. 严格遵守结构化输出：只输出 JSON，不要输出任何解释、前后缀或 Markdown 代码块。";

/// The user-facing instruction accompanying the image.
const VISION_USER_INSTRUCTION: &str = "\
请从这张图片中识别并整理其中所有独立的面试题，严格输出如下 JSON 结构（不要输出无关内容）：\
{\"items\":[{\"question\":\"问题\",\"answer\":\"整理后的答案\",\"domain\":\"领域\",\"topic\":\"主题\",\"tags\":[\"标签1\",\"标签2\"],\"followUpQuestions\":[\"追问1\"]}]} \
如果图片中不包含任何面试题，则输出 {\"items\":[]}。";

/// Build the OpenAI messages array for a single image. Each image is extracted
/// independently; image order/adjacency is never used to merge content.
pub fn vision_messages(image_data_url: &str) -> Vec<serde_json::Value> {
    let content = vec![
        serde_json::json!({"type": "text", "text": VISION_USER_INSTRUCTION}),
        serde_json::json!({"type": "image_url", "image_url": {"url": image_data_url}}),
    ];
    vec![
        serde_json::json!({"role": "system", "content": VISION_SYSTEM_PROMPT}),
        serde_json::json!({"role": "user", "content": content}),
    ]
}

/// Structured contract for one extracted draft. Field names are camelCase to
/// match the model's JSON and the frontend `Knowledge` type.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisionDraft {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub follow_up_questions: Vec<String>,
}

/// Parse and strictly validate the model's content string into drafts.
/// Returns an error (never silently bad data) for invalid JSON, missing/changed
/// types, or any item with a blank question/answer. An empty `items: []` is a
/// valid result meaning "no questions found".
pub fn parse_drafts(content: &str) -> Result<Vec<VisionDraft>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("模型返回的不是合法 JSON：{e}"))?;

    let items = parsed
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or_else(|| "模型返回缺少 items 数组".to_string())?;

    let mut drafts = Vec::with_capacity(items.len());
    for item in items {
        let draft: VisionDraft = serde_json::from_value(item.clone())
            .map_err(|e| format!("items 中的条目字段类型错误：{e}"))?;
        if draft.question.trim().is_empty() {
            return Err("存在一条草稿缺少 question（问题）".to_string());
        }
        if draft.answer.trim().is_empty() {
            return Err(format!("草稿「{}」缺少 answer（答案）", draft.question.trim()));
        }
        drafts.push(draft);
    }
    Ok(drafts)
}

/// Convert validated drafts into frontend `Knowledge`-compatible payloads,
/// assigning fresh draft ids, timestamps and a source label. On confirm these
/// are inserted into SQLite (numeric id replaces the draft id).
pub fn to_knowledge_payloads(drafts: &[VisionDraft], source: &str) -> Vec<KnowledgePayload> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    drafts
        .iter()
        .map(|d| KnowledgePayload {
            id: format!("draft-{}", uuid()),
            question: d.question.trim().to_string(),
            answer: d.answer.trim().to_string(),
            domain: if d.domain.trim().is_empty() {
                "未分类".to_string()
            } else {
                d.domain.trim().to_string()
            },
            topic: d.topic.trim().to_string(),
            tags: d.tags.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            follow_ups: d.follow_up_questions.clone(),
            related_ids: Vec::new(),
            source: format!("截图导入 · {source}"),
            created_at: now.clone(),
            updated_at: now.clone(),
            favorite: Some(false),
            last_read_at: None,
        })
        .collect()
}

/// Random hex id for a draft (not persisted).
fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}-{:06x}", randish() % 0x1000000)
}

/// Small unseeded pseudo-random helper so we don't pull in a rand dependency.
fn randish() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() ^ d.subsec_nanos() as u64) as u64)
        .unwrap_or(7)
}

/// Shared tail after a successful LLM call: validate the structured output and
/// map drafts to frontend payloads.
async fn run_extraction(
    client: &OpenAiCompatClient,
    messages: Vec<serde_json::Value>,
    source: &str,
    max_tokens: u32,
) -> Result<Vec<KnowledgePayload>, crate::llm::LlmError> {
    let content = client.chat_json(messages, max_tokens).await?;
    let drafts = parse_drafts(&content.to_string()).map_err(crate::llm::LlmError::BadResponse)?;
    Ok(to_knowledge_payloads(&drafts, source))
}

/// Run the full extraction for one image (independent mode): validate the data
/// URL, call the LLM, validate the structured output, return drafts.
pub async fn extract_from_image(
    client: &OpenAiCompatClient,
    image_data_url: &str,
    source: &str,
) -> Result<Vec<KnowledgePayload>, crate::llm::LlmError> {
    parse_data_url(image_data_url)?; // fail fast on unsupported/corrupt image
    run_extraction(client, vision_messages(image_data_url), source, 3000).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_items() {
        let content = r#"{"items":[
            {"question":"Q1","answer":"A1","domain":"后端","topic":"Redis","tags":["Redis"],"followUpQuestions":["追问1"]},
            {"question":"Q2","answer":"A2","domain":"数据库","topic":"MySQL","tags":[],"followUpQuestions":[]}
        ]}"#;
        let drafts = parse_drafts(content).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].question, "Q1");
        assert_eq!(drafts[0].follow_up_questions, vec!["追问1".to_string()]);
        assert_eq!(drafts[1].topic, "MySQL");
    }

    #[test]
    fn empty_items_is_valid_zero_drafts() {
        let drafts = parse_drafts(r#"{"items":[]}"#).unwrap();
        assert!(drafts.is_empty());
    }

    #[test]
    fn blank_question_is_rejected() {
        let content = r#"{"items":[{"question":"  ","answer":"A"}]}"#;
        let err = parse_drafts(content).unwrap_err();
        assert!(err.contains("question"), "error mentions question: {err}");
    }

    #[test]
    fn blank_answer_is_rejected() {
        let content = r#"{"items":[{"question":"Q","answer":""}]}"#;
        let err = parse_drafts(content).unwrap_err();
        assert!(err.contains("answer"), "error mentions answer: {err}");
    }

    #[test]
    fn invalid_json_is_rejected() {
        assert!(parse_drafts("not json at all").is_err());
        assert!(parse_drafts(r#"{"items":"oops"}"#).is_err());
        assert!(parse_drafts(r#"{"foo":1}"#).is_err());
    }

    #[test]
    fn single_messages_use_independent_prompt() {
        let msgs = vision_messages("data:image/png;base64,QQ==");
        let system = msgs[0]["content"].as_str().unwrap();
        assert_eq!(system, VISION_SYSTEM_PROMPT, "independent keeps its own prompt");
        assert!(!system.contains("连续内容"));
    }

    #[test]
    fn to_payload_maps_fields_and_defaults() {
        let drafts = vec![VisionDraft {
            question: " Q ".to_string(),
            answer: " A ".to_string(),
            domain: " ".to_string(), // blank -> 未分类
            topic: "T".to_string(),
            tags: vec![" a ".to_string(), "".to_string()],
            follow_up_questions: vec!["F".to_string()],
        }];
        let items = to_knowledge_payloads(&drafts, "shot.png");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].question, "Q");
        assert_eq!(items[0].domain, "未分类");
        assert_eq!(items[0].tags, vec!["a".to_string()]);
        assert!(items[0].id.starts_with("draft-"));
        assert!(items[0].source.contains("shot.png"));
    }
}