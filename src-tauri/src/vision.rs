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
/// must not invent facts, must split independent questions, and — most
/// importantly for this revision — must pick a layout that matches the
/// answer's *content* (parallel points / steps / comparisons / short answer)
/// instead of collapsing everything into one long paragraph.
///
/// The prompt is written as a Rust raw string literal so its newlines,
/// numbered list markers, indented sub-explanations, and code-fence examples
/// are preserved byte-for-byte when serialised into the request. A previous
/// version used line-continuation `"\"` which is correct, but is harder to
/// read and easy to drift away from the desired layout during future edits.
pub const VISION_SYSTEM_PROMPT: &str = r###"
你正在从面试相关截图中提取并整理个人知识库内容。

目标不是逐字 OCR，而是理解截图内容，并整理成清晰、适合后续复习的问答知识。

## 1. 独立问题拆分
- 每个独立面试问题生成一个 item。
- 如果截图包含多个问题，必须拆成多个 item。
- 不要把多个不同问题合并成一个问答。

## 2. question
- 保留原问题核心含义。
- 可以轻微整理，使表达完整、自然。
- 不改变问题本身的考察重点。

## 3. answer 内容原则
- 保留截图中的有效信息和原意。
- 可以整理表达、去除重复、修正明显口语化和碎片化内容。
- 不要凭空补充截图中没有的信息。
- 不要为了显得完整而扩写大量通用知识。
- 优先保留截图原本已经存在的结构、层级和顺序。

## 4. answer 排版原则
根据内容本身的结构决定排版，不要机械套模板。

- 如果答案很短、只有一个核心结论：
  直接使用 1~3 句简洁文字，不强行分点。
- 如果存在两个及以上并列知识点：
  必须分点。
  每一点只表达一个主要结论。
- 如果存在步骤、流程或顺序：
  使用有序列表（1. 2. 3.）。
- 如果存在分类、特点、原因、区别、优缺点、条件：
  使用小标题（## ）、编号或无序列表（- ）进行组织。
- 如果一个要点需要展开：
  先写核心结论，再在下一行用 2~4 个空格的缩进补充解释。
- 如果适合先给总括：
  可以先用 1~2 句给出核心结论，再展开具体内容。

## 5. 禁止以下写法
- 把多个独立知识点压缩成一个长段落。
- 使用连续大量逗号或分号堆成一个超长句。
- 为了凑格式，把一个本来很短的答案机械拆成很多点。
- 为了"看起来专业"而加入截图中没有的知识。

## 6. answer 使用换行保留层级
answer 是一个 string，请用换行与列表体现层级。模型看到的 answer 里，
真实换行会原样保存到知识库；不要在 answer 里写 \\n 字面量。

可以参考以下形态：

核心结论。

1. 第一个关键点
   进一步解释。

2. 第二个关键点
   进一步解释。

或：

## 核心区别

- A：……
- B：……

## 7. domain / topic / tags
- 根据问题内容推荐合理分类。
- 标签保持简洁、可搜索（1~4 个）。
- 不生成大量泛化标签。

## 8. followUpQuestions
- 只生成与当前问题紧密相关、面试中可能继续追问的问题。
- 不为了数量强行生成。
- 没有则返回空数组。

## 9. 如果截图内容不足以形成有效知识
- 可以返回空 items。
- 不猜测或编造缺失内容。

只返回合法 JSON，不输出 JSON 之外的文字，也不要使用 Markdown 代码块包裹。
"###;

/// The user-facing instruction accompanying the image. It stays short and
/// just points the model at the per-image task + JSON shape; the formatting
/// rules live in the system prompt so a single edit covers every call.
const VISION_USER_INSTRUCTION: &str = r###"
请从这张图片中识别并整理所有独立的面试题，严格遵循 system 中的整理与排版规则，
并按下面的 JSON 结构输出（不要输出任何解释、前后缀或 Markdown 代码块）：

{
  "items": [
    {
      "question": "整理后的问题",
      "answer": "整理后的答案，使用换行和列表保留层级（真实换行，不是 \\n）",
      "domain": "领域",
      "topic": "主题",
      "tags": ["标签1", "标签2"],
      "followUpQuestions": ["相关追问"]
    }
  ]
}

如果图片中不包含任何面试题，则输出 {"items":[]}。
"###;

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
            // Empty sync_id -> save() will mint a fresh UUIDv4 on insert.
            sync_id: String::new(),
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
            deleted_at: None,
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
        // A leftover phrase from the old multi-image mode should not leak in.
        assert!(!system.contains("连续内容"));
    }

    /// The new prompt must spell out the layout rules so the model knows when
    /// to break a long paragraph into a list, when to keep a short answer as
    /// a one-liner, and when an indented sub-explanation is appropriate. If
    /// someone later strips these requirements by accident, this test fires.
    #[test]
    fn system_prompt_specifies_layout_rules() {
        assert!(
            VISION_SYSTEM_PROMPT.contains("独立问题拆分"),
            "prompt must require splitting independent questions"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("排版原则"),
            "prompt must spell out layout principles"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("禁止以下写法"),
            "prompt must forbid long-paragraph collapse"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("不要机械套模板"),
            "prompt must warn against templating every answer the same way"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("1~3 句简洁文字"),
            "prompt must allow short answers to stay short"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("两个及以上并列知识点"),
            "prompt must require bullet/numbered list for parallel points"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("步骤、流程或顺序"),
            "prompt must require ordered list for steps"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("区别、优缺点、条件"),
            "prompt must cover comparison-style answers"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("## "),
            "prompt must allow ## subheadings"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("凭空补充"),
            "prompt must forbid fabricating content not in the screenshot"
        );
    }

    /// The new prompt's user instruction must remind the model that answer is
    /// a real string with real newlines, not a JSON-escaped `\\n` literal.
    #[test]
    fn user_instruction_keeps_json_shape() {
        assert!(VISION_USER_INSTRUCTION.contains("\"items\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"question\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"answer\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"domain\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"topic\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"tags\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"followUpQuestions\""));
        assert!(VISION_USER_INSTRUCTION.contains("{\"items\":[]}"));
    }

    /// The parser must keep the embedded newlines in `answer` intact so the
    /// layout the model produced (headings, lists, indented sub-lines) reaches
    /// SQLite byte-for-byte. Only outer whitespace is trimmed.
    #[test]
    fn parser_preserves_newlines_in_answer() {
        let content = r#"{"items":[{
            "question":"Redis 为什么快？",
            "answer":"核心结论。\n\n1. 基于内存\n   读写绕开磁盘。\n\n2. 单线程避免上下文切换\n   主流程不抢锁。\n\n## 参考\n\n- 内存访问\n- 非阻塞 IO",
            "domain":"后端",
            "topic":"Redis",
            "tags":["Redis"],
            "followUpQuestions":[]
        }]}"#;
        let drafts = parse_drafts(content).unwrap();
        assert_eq!(drafts.len(), 1);
        let a = &drafts[0].answer;
        assert!(a.contains("\n\n1. 基于内存"), "numbered list line preserved");
        assert!(a.contains("   读写绕开磁盘。"), "indented sub-line preserved");
        assert!(a.contains("\n\n## 参考\n"), "heading + blank lines preserved");
        assert!(a.contains("\n- 内存访问"), "unordered list preserved");
        // Sanity: the answer is not collapsed to one line.
        assert!(a.lines().count() >= 8);
    }

    /// `to_knowledge_payloads` must NOT eat internal newlines either; it
    /// trims the outer whitespace only.
    #[test]
    fn payload_preserves_newlines_in_answer() {
        let drafts = vec![VisionDraft {
            question: "Q".to_string(),
            answer: " 首行\n\n1. A\n   解释\n\n2. B\n".to_string(),
            domain: "后端".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_up_questions: vec![],
        }];
        let items = to_knowledge_payloads(&drafts, "shot.png");
        let ans = &items[0].answer;
        assert!(ans.starts_with("首行"), "leading whitespace trimmed");
        assert!(!ans.ends_with('\n'), "trailing whitespace trimmed");
        assert!(ans.contains("\n\n1. A\n   解释\n\n2. B"));
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
