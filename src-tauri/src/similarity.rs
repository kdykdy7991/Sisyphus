// Similarity judgment for a new knowledge draft against existing knowledge.
//
// For each draft (before the user confirms), we pull a small set of FTS5
// candidates from the existing knowledge base and ask the LLM to classify the
// relationship as SAME / RELATED / NONE. This reuses the shared OpenAI-compatible
// client (llm.rs) — no second HTTP layer, no embeddings, no vector DB.
//
// The judge answers:
//   SAME    -> essentially the same core interview question (paraphrase / angle)
//   RELATED -> same topic, but genuinely a different question
//   NONE    -> no meaningful relationship

use serde_json::Value;

use crate::db::KnowledgePayload;
use crate::llm::{LlmError, OpenAiCompatClient};

/// Backend-owned prompt for the relation judge. Stresses semantic "same core
/// question" over surface text overlap, with the canonical examples.
pub const SIMILARITY_SYSTEM_PROMPT: &str = "\
你是一个「个人面试知识整理」应用中的知识关系判断助手。\
给定一个新提取的面试草稿和几篇已有知识候选，请判断它们的关系。\
\
关键判断标准是「是不是同一个核心面试问题」，而不是单纯文字相似度：\
\
- SAME：核心问题本质相同，只是措辞不同、答案角度不同，或内容可合并。\
- RELATED：属于同一个主题或有明显知识关系，但本质是不同的问题。\
- NONE：没有足够强的关系。\
\
示例：\
「Redis 为什么快？」与「Redis 为什么性能高？」 -> SAME\
「Redis 为什么快？」与「Redis 单线程模型是什么？」 -> RELATED\
「Redis 为什么快？」与「TCP 为什么三次握手？」 -> NONE\
\
只选出最相关的一条候选（若存在）。严格遵守结构化输出：只输出 JSON，不要输出任何解释或 Markdown 代码块。";

/// Relationship categories.
#[derive(Debug, Clone, PartialEq)]
pub enum Relation {
    Same,
    Related,
    None,
}

impl Relation {
    pub fn code(&self) -> &'static str {
        match self {
            Relation::Same => "SAME",
            Relation::Related => "RELATED",
            Relation::None => "NONE",
        }
    }
}

/// A validated judgment.
#[derive(Debug)]
pub struct SimilarityVerdict {
    pub relation: Relation,
    pub knowledge_id: Option<String>,
    pub reason: String,
}

/// Build the user message: the draft + up-to-5 candidates, with answers capped
/// to keep the payload small (never whole-base; only FTS candidates).
fn build_user_message(draft: &KnowledgePayload, candidates: &[KnowledgePayload]) -> Value {
    let mut draft_answer = draft.answer.clone();
    draft_answer.truncate(1200);
    serde_json::json!({
        "task": "下面是一个新整理的面试草稿和若干已有知识候选。请按系统提示的关系定义，判断草稿与候选的关系。只选最相关的一条（若存在则填其 id），否则 relation 为 NONE 且 knowledgeId 为 null。",
        "draft": {
            "question": draft.question,
            "answer": draft_answer,
            "domain": draft.domain,
            "topic": draft.topic,
            "tags": draft.tags,
        },
        "candidates": candidates.iter().map(|c| {
            let mut answer = c.answer.clone();
            answer.truncate(500);
            serde_json::json!({
                "id": c.id,
                "question": c.question,
                "answer": answer,
                "domain": c.domain,
                "topic": c.topic,
                "tags": c.tags,
            })
        }).collect::<Vec<_>>(),
        "output_format": "{\"relation\":\"SAME|RELATED|NONE\",\"knowledgeId\":\"所选候选的 id，NONE 时为 null\",\"reason\":\"简短说明（NONE 时长可留空）\"}",
    })
}

/// Ask the LLM to judge the relationship. Returns a validated verdict.
pub async fn judge(
    client: &OpenAiCompatClient,
    draft: &KnowledgePayload,
    candidates: &[KnowledgePayload],
) -> Result<SimilarityVerdict, LlmError> {
    let messages = vec![
        serde_json::json!({"role": "system", "content": SIMILARITY_SYSTEM_PROMPT}),
        serde_json::json!({"role": "user", "content": build_user_message(draft, candidates)}),
    ];
    let content = client.chat_json(messages, 800).await?;
    let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    parse_verdict(&content, &candidate_ids).map_err(|e| LlmError::BadResponse {
        message: e,
        body: Some(content.to_string()),
    })
}

/// Parse and strictly validate the model's structured output. Rejects unknown
/// relations, and rejects SAME/RELATED without a knowledgeId that is one of the
/// candidate ids. NONE requires nothing except the relation.
pub fn parse_verdict(content: &Value, candidate_ids: &[String]) -> Result<SimilarityVerdict, String> {
    let relation_raw = content
        .get("relation")
        .and_then(Value::as_str)
        .ok_or_else(|| "模型判断缺少 relation 字段".to_string())?
        .trim()
        .to_uppercase();
    let relation = match relation_raw.as_str() {
        "SAME" => Relation::Same,
        "RELATED" => Relation::Related,
        "NONE" => Relation::None,
        other => return Err(format!("未知的关系类型：{other:?}")),
    };
    let reason = content.get("reason").and_then(Value::as_str).unwrap_or("").trim().to_string();

    match relation {
        Relation::None => Ok(SimilarityVerdict { relation, knowledge_id: None, reason }),
        _ => {
            let kid = content
                .get("knowledgeId")
                .and_then(Value::as_str)
                .map(String::from)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "该关系需要有效的 knowledgeId".to_string())?;
            if !candidate_ids.iter().any(|id| id == &kid) {
                return Err("knowledgeId 不在候选集中".to_string());
            }
            Ok(SimilarityVerdict { relation, knowledge_id: Some(kid), reason })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn same_with_valid_candidate() {
        let v = parse_verdict(&json(r#"{"relation":"SAME","knowledgeId":"123","reason":"核心问题相同"}"#), &["123".into()]).unwrap();
        assert_eq!(v.relation, Relation::Same);
        assert_eq!(v.knowledge_id.as_deref(), Some("123"));
    }

    #[test]
    fn related_is_accepted() {
        let v = parse_verdict(&json(r#"{"relation":"RELATED","knowledgeId":"7","reason":"同主题不同题"}"#), &["7".into(), "8".into()]).unwrap();
        assert_eq!(v.relation, Relation::Related);
        assert_eq!(v.knowledge_id.as_deref(), Some("7"));
    }

    #[test]
    fn none_needs_no_id() {
        let v = parse_verdict(&json(r#"{"relation":"NONE","knowledgeId":null,"reason":""}"#), &["123".into()]).unwrap();
        assert_eq!(v.relation, Relation::None);
        assert_eq!(v.knowledge_id, None);
    }

    #[test]
    fn case_insensitive_relation() {
        let v = parse_verdict(&json(r#"{"relation":"same","knowledgeId":"1","reason":""}"#), &["1".into()]).unwrap();
        assert_eq!(v.relation, Relation::Same);
    }

    #[test]
    fn unknown_relation_rejected() {
        assert!(parse_verdict(&json(r#"{"relation":"MAYBE","knowledgeId":null}"#), &[]).is_err());
    }

    #[test]
    fn same_without_valid_candidate_rejected() {
        assert!(parse_verdict(&json(r#"{"relation":"SAME","knowledgeId":"999","reason":""}"#), &["1".into()]).is_err());
        assert!(parse_verdict(&json(r#"{"relation":"SAME","knowledgeId":null,"reason":""}"#), &["1".into()]).is_err());
    }

    #[test]
    fn missing_relation_rejected() {
        assert!(parse_verdict(&json(r#"{"foo":1}"#), &[]).is_err());
    }
}