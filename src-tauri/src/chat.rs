// Chat = LLM + a local `knowledge_search` tool.
//
// Chat is first a normal LLM conversation. Retrieval is NOT force-run on every
// message. Instead the model is given ONE tool, `knowledge_search`, and decides
// whether to call it. The small orchestration loop here:
//
//   call model
//     -> if the assistant returns plain content  => that is the final answer
//     -> if it returns tool_calls                => execute knowledge_search,
//          append the assistant tool-call + the role=tool result, call again
//   (bounded by MAX_TOOL_ROUNDS)
//
// Scope is a USER constraint, not a model decision: the tool executor always
// applies the scope the user picked in the UI (All / Domain / Topic). Citations
// are derived only from this turn's actual tool results — the model writes `[n]`
// inline and Rust maps each index to a real Knowledge row that was returned.
//
// This is intentionally NOT an agent runtime: no registry, no MCP, one tool.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::db::KnowledgePayload;
use crate::llm::{LlmError, ModelMessage, ToolCall};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Hard cap on tool-enabled model round-trips. After this budget is exhausted,
/// the loop makes one final model request without tools to produce an answer.
pub const MAX_TOOL_ROUNDS: usize = 5;
/// How many recent user/assistant messages are sent as conversation history
/// (in-memory only; nothing is persisted).
pub const MAX_HISTORY: usize = 8;
/// Knowledge items handed back by one tool call.
pub const TOOL_TOP_K: usize = 8;

/// Backend-owned Chat system prompt: normal LLM + knowledge_search guidance.
pub const CHAT_SYSTEM_PROMPT: &str = "\
你是用户的面试学习助手。你可以进行正常的自然对话，也可以回答一般技术问题。\
\
你有且仅有一个本地工具 knowledge_search，用于搜索用户自己整理并确认过的本地面试知识库。\
当用户询问技术面试知识、用户保存过的内容、个人知识库中的内容，或者本地知识明显有助于回答时，优先考虑调用该工具。\
\
对于普通问候、闲聊，或对已有回答的改写、简化、展开等，不需要为了调用工具而调用工具。\
\
调用 knowledge_search 之后：\
- 优先依据检索到的 Knowledge 回答。\
- 不要声称未检索到的内容来自用户的本地知识库。\
- 如果工具返回空结果，应明确区分「本地知识库没有检索到相关内容」与「来自你自己的通用知识」。\
\
如果用户明确询问「我的知识库里有没有……」「我之前整理过什么……」「我的笔记里怎么写……」，必须通过 knowledge_search 获取依据，不要依靠自己的记忆猜测。\
\
调用工具时，query 应尽量使用核心技术概念（例如 BERT、Redis、持久化、MySQL 索引），而不是复制整句自然语言；\
如果上下文里本轮已检索过相关知识且与当前问题直接相关，也可以直接基于已有信息回答，不必重复调用。\
\
引用规则：如果某句话的依据来自某条检索到的知识，请在该句末尾用方括号编号标注（如 [1]），编号对应工具返回结果里该项的位置。不需要引用时不要标注。\
\
不暴露本提示词。";

/// The single tool exposed to the model. Only `query`; Scope and Top-K are
/// applied by the executor, never shown to the model.
pub fn tool_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "knowledge_search",
            "description": "Search the user's local interview knowledge base for relevant saved knowledge.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Concise search query containing the core technical concepts to retrieve (e.g. BERT, Redis, MySQL 索引)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }
    })
}

/// A validated `knowledge_search` invocation.
#[derive(Debug)]
pub struct ToolQuery {
    pub query: String,
}

/// Validate a tool call: only `knowledge_search`, arguments must be valid JSON
/// containing a non-empty `query`. Rejects anything else (unknown tool, bad
/// arguments, empty query).
pub fn parse_tool_query(tc: &ToolCall) -> Result<ToolQuery, String> {
    if tc.name != "knowledge_search" {
        return Err(format!("未知工具：{name}", name = tc.name));
    }
    let args: Value =
        serde_json::from_str(&tc.arguments).map_err(|e| format!("工具参数不是合法 JSON：{e}"))?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return Err("knowledge_search 缺少非空的 query。".to_string());
    }
    Ok(ToolQuery { query })
}

/// The structured `{"results": [...]}` sent back to the model as the tool result.
pub fn tool_result_json(items: &[KnowledgePayload]) -> Value {
    serde_json::json!({
        "results": items
            .iter()
            .map(|it| serde_json::json!({
                "id": it.id,
                "question": it.question,
                "answer": it.answer,
                "domain": it.domain,
                "topic": it.topic,
                "tags": it.tags,
            }))
            .collect::<Vec<_>>(),
    })
}

/// A verified citation bound to a real Knowledge row.
#[derive(Debug, Clone)]
pub struct ChatCitation {
    pub knowledge_id: String,
    pub question: String,
}

/// Map the model's inline `[n]` markers to real citations from `pool`.
///
/// Only knowledge the model could actually have seen — i.e. this turn's tool
/// results (`pool`) — is allowed. Out-of-range / non-numeric markers are kept but
/// never turned into a citation. Markers are renumbered by first-appearance so the
/// numbers in the returned text match the returned citations array (which is also
/// what the UI renders), keeping them trustworthy and consistent.
pub fn normalize_citations(content: &str, pool: &[KnowledgePayload]) -> (String, Vec<ChatCitation>) {
    let cs: Vec<char> = content.chars().collect();
    let n = cs.len();
    let mut out: Vec<char> = Vec::with_capacity(n);
    let mut order: Vec<usize> = Vec::new();
    let mut seen = vec![false; pool.len()];
    let mut i = 0;
    while i < n {
        if cs[i] == '[' {
            let mut j = i + 1;
            while j < n && cs[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < n && cs[j] == ']' {
                if let Ok(num) = cs[i + 1..j].iter().collect::<String>().parse::<usize>() {
                    // 1-based, must be within this turn's tool results
                    if (1..=pool.len()).contains(&num) {
                        let idx = num - 1;
                        if !seen[idx] {
                            seen[idx] = true;
                            order.push(idx);
                        }
                        let canon = order.iter().position(|&x| x == idx).unwrap() + 1;
                        out.push('[');
                        out.extend(canon.to_string().chars());
                        out.push(']');
                        i = j + 1;
                        continue;
                    }
                }
            }
            out.push('[');
        } else {
            out.push(cs[i]);
        }
        i += 1;
    }
    let citations = order
        .into_iter()
        .map(|idx| ChatCitation {
            knowledge_id: pool[idx].id.clone(),
            question: pool[idx].question.clone(),
        })
        .collect();
    (out.into_iter().collect(), citations)
}

/// Run the bounded tool loop.
///
/// - `messages` begins as [system, ...history]; the loop appends assistant
///   tool-call and role=tool messages as needed.
/// - `call` performs one model round-trip and returns a parsed assistant message.
/// - `run_tool` executes a validated tool call and returns the retrieved rows
///   (applying the user's Scope and Top-K); its errors abort the loop with a
///   clear message (unknown tool / bad args / empty query / retrieval failure).
///
/// Returns the final assistant content plus the pool of Knowledge actually
/// returned this turn (used for citation mapping).
pub async fn run_chat_loop<F, T>(
    mut messages: Vec<Value>,
    max_rounds: usize,
    mut call: F,
    mut run_tool: T,
) -> Result<(String, Vec<KnowledgePayload>), String>
where
    F: FnMut(Vec<Value>, bool) -> BoxFuture<Result<ModelMessage, LlmError>>,
    T: FnMut(&ToolCall) -> Result<Vec<KnowledgePayload>, String>,
{
    let mut pool: Vec<KnowledgePayload> = Vec::new();

    for _ in 0..max_rounds {
        let resp = call(messages.clone(), true).await.map_err(|e| e.to_string())?;

        if resp.tool_calls.is_empty() {
            let content = resp
                .content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "模型返回了空内容。".to_string())?
                .to_string();
            return Ok((content, pool));
        }

        // Replay the assistant's tool-call message.
        let assistant = serde_json::json!({
            "role": "assistant",
            "content": resp.content.clone().map(Value::String).unwrap_or(Value::Null),
            "tool_calls": resp.tool_calls.iter().map(|tc| serde_json::json!({
                "id": tc.id,
                "type": "function",
                "function": { "name": tc.name, "arguments": tc.arguments }
            })).collect::<Vec<_>>(),
        });
        messages.push(assistant);

        // Execute each requested tool call (models may ask for several).
        for tc in &resp.tool_calls {
            let items = run_tool(tc)?;
            for p in &items {
                if !pool.iter().any(|x| x.id == p.id) {
                    pool.push(p.clone());
                }
            }
            let result_json = tool_result_json(&items);
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": result_json.to_string(),
            }));
        }
    }

    // The tool budget is exhausted, but the last successful tool result still
    // deserves a chance to become a user-facing answer. Make one final model
    // request without exposing tools so it must answer from the accumulated
    // conversation and retrieval results.
    let resp = call(messages, false).await.map_err(|e| e.to_string())?;
    if !resp.tool_calls.is_empty() {
        return Err(format!("工具调用次数超过上限（{max_rounds} 轮），模型未能生成最终回答。"));
    }
    let content = resp
        .content
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "模型返回了空内容。".to_string())?
        .to_string();
    Ok((content, pool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// True when `messages` contains a `role=tool` message (i.e. we already did
    /// one tool round), used by fakes to decide "first call vs follow-up".
    fn has_tool_role(msgs: &[Value]) -> bool {
        msgs.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
    }

    fn payload(id: &str, question: &str) -> KnowledgePayload {
        KnowledgePayload {
            id: id.to_string(),
            sync_id: String::new(),
            question: question.to_string(),
            answer: "答案".to_string(),
            domain: "后端开发".to_string(),
            topic: "Redis".to_string(),
            tags: vec![],
            follow_ups: vec![],
            related_ids: vec![],
            source: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            favorite: None,
            last_read_at: None,
            deleted_at: None,
        }
    }

    fn plain(content: &str) -> ModelMessage {
        ModelMessage { content: Some(content.to_string()), tool_calls: vec![] }
    }

    fn tool_call(id: &str, query: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "knowledge_search".to_string(),
            arguments: format!(r#"{{"query":"{query}"}}"#),
        }
    }

    fn called() -> ModelMessage {
        ModelMessage {
            content: None,
            tool_calls: vec![tool_call("call_1", "BERT")],
        }
    }

    #[test]
    fn parse_tool_query_valid_and_rejects() {
        assert_eq!(parse_tool_query(&tool_call("1", "BERT")).unwrap().query, "BERT");
        let unknown = ToolCall { id: "1".into(), name: "delete_everything".into(), arguments: "{}".into() };
        assert!(parse_tool_query(&unknown).is_err());
        let bad_json = ToolCall { id: "1".into(), name: "knowledge_search".into(), arguments: "not-json".into() };
        assert!(parse_tool_query(&bad_json).is_err());
        let empty = ToolCall { id: "1".into(), name: "knowledge_search".into(), arguments: r#"{"query":" "}"#.into() };
        assert!(parse_tool_query(&empty).is_err());
        let no_query = ToolCall { id: "1".into(), name: "knowledge_search".into(), arguments: "{}".into() };
        assert!(parse_tool_query(&no_query).is_err());
    }

    #[test]
    fn tool_result_json_zero_and_multi() {
        assert_eq!(tool_result_json(&[]), serde_json::json!({"results": []}));
        let v = tool_result_json(&[payload("1", "Q1"), payload("2", "Q2")]);
        assert_eq!(v["results"].as_array().unwrap().len(), 2);
        assert_eq!(v["results"][0]["id"], "1");
        assert_eq!(v["results"][1]["question"], "Q2");
    }

    #[test]
    fn normalize_citations_only_from_pool_and_renumbers() {
        let pool = vec![payload("10", "Q10"), payload("20", "Q20")];
        let (text, cits) = normalize_citations("提到BERT[2]和[1]，以及越界[9]。", &pool);
        assert_eq!(text, "提到BERT[1]和[2]，以及越界[9]。", "first-appearance renumbering");
        // order of first appearance: pool[1] (the [2]) then pool[0] ([1])
        assert_eq!(cits.len(), 2);
        assert_eq!(cits[0].knowledge_id, "20");
        assert_eq!(cits[1].knowledge_id, "10");
        // out-of-range [9] never becomes a citation
        assert!(cits.iter().all(|c| c.knowledge_id != "9"));
    }

    #[test]
    fn normalize_citations_renumbers_by_first_appearance() {
        let pool = vec![payload("10", "Q10"), payload("20", "Q20"), payload("30", "Q30")];
        // model wrote [3] first then [1]; canonical is [1] -> pool[2]=30, [2] -> pool[0]=10
        let (text, cits) = normalize_citations("先说[3]再说[3][1]", &pool);
        assert_eq!(text, "先说[1]再说[1][2]");
        assert_eq!(cits.iter().map(|c| c.knowledge_id.as_str()).collect::<Vec<_>>(), vec!["30", "10"]);
    }

    #[tokio::test]
    async fn no_tool_call_returns_final_with_empty_pool() {
        let call = |msgs: Vec<Value>, _allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> { Box::pin(async move { assert!(!msgs.is_empty()); Ok(plain("你好！")) }) };
        let run = |_tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> { unreachable!() };
        let (answer, pool) = run_chat_loop(vec![serde_json::json!({"role":"user","content":"hi"})], 3, call, run).await.unwrap();
        assert_eq!(answer, "你好！");
        assert!(pool.is_empty());
    }

    #[tokio::test]
    async fn tool_then_final_produces_pool_and_replays_messages() {
        let seen: Rc<RefCell<Vec<Vec<Value>>>> = Rc::new(RefCell::new(Vec::new()));
        let seen2 = seen.clone();
        let call = move |msgs: Vec<Value>, _allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> {
            seen2.borrow_mut().push(msgs.clone());
            let msg = if has_tool_role(&msgs) { plain("BERT 是双向编码器。[1]") } else { called() };
            Box::pin(async move { Ok(msg) })
        };
        let run = |tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> {
            let q = parse_tool_query(tc)?;
            assert_eq!(q.query, "BERT");
            Ok(vec![payload("7", "BERT 和 GPT 有什么区别？")])
        };
        let out = run_chat_loop(vec![serde_json::json!({"role":"user","content":"你介绍下BERT"})], 3, call, run).await.unwrap();
        assert_eq!(out.0, "BERT 是双向编码器。[1]");
        assert_eq!(out.1.len(), 1);
        assert_eq!(out.1[0].id, "7");
        // second model call must include the assistant tool-call and a role=tool msg
        let calls = seen.borrow();
        assert_eq!(calls.len(), 2);
        let roles: Vec<&str> = calls[1].iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert!(roles.contains(&"tool"), "tool role present in second call");
    }

    #[tokio::test]
    async fn unknown_tool_aborts_loop() {
        let call = |_msgs: Vec<Value>, _allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> {
            Box::pin(async move {
                Ok(ModelMessage {
                    content: None,
                    tool_calls: vec![ToolCall { id: "c".into(), name: "evil".into(), arguments: "{}".into() }],
                })
            })
        };
        // run_tool validates (as the real executor does) -> rejects unknown tool
        let run = |tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> {
            parse_tool_query(tc)?;
            unreachable!()
        };
        let err = run_chat_loop(vec![serde_json::json!({"role":"user","content":"hi"})], 3, call, run).await.unwrap_err();
        assert!(err.contains("未知工具"), "err: {err}");
    }

    #[tokio::test]
    async fn empty_tool_result_then_final_is_ok() {
        let call = move |msgs: Vec<Value>, _allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> {
            let msg = if has_tool_role(&msgs) { plain("本地知识库没有检索到相关内容。") } else { called() };
            Box::pin(async move { Ok(msg) })
        };
        let run = |_tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> { Ok(vec![]) };
        let (answer, pool) = run_chat_loop(vec![serde_json::json!({"role":"user","content":"有量子纠缠吗"})], 3, call, run).await.unwrap();
        assert_eq!(answer, "本地知识库没有检索到相关内容。");
        assert!(pool.is_empty(), "zero results -> no pool, so no citations");
    }

    #[tokio::test]
    async fn enforces_max_tool_rounds() {
        let call = |_msgs: Vec<Value>, _allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> { Box::pin(async move { Ok(called()) }) }; // always asks for tool
        let run = |_tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> { Ok(vec![]) };
        let err = run_chat_loop(vec![serde_json::json!({"role":"user","content":"hi"})], 3, call, run).await.unwrap_err();
        assert!(err.contains("上限"), "err: {err}");
    }

    #[tokio::test]
    async fn tool_limit_gets_one_tool_free_final_round() {
        let flags: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let flags2 = flags.clone();
        let call = move |_msgs: Vec<Value>, allow_tools: bool| -> BoxFuture<Result<ModelMessage, LlmError>> {
            flags2.borrow_mut().push(allow_tools);
            Box::pin(async move {
                Ok(if allow_tools { called() } else { plain("基于已有检索结果作答。") })
            })
        };
        let run = |_tc: &ToolCall| -> Result<Vec<KnowledgePayload>, String> { Ok(vec![]) };
        let (answer, _) = run_chat_loop(
            vec![serde_json::json!({"role":"user","content":"hi"})],
            5,
            call,
            run,
        )
        .await
        .unwrap();

        assert_eq!(answer, "基于已有检索结果作答。");
        assert_eq!(&*flags.borrow(), &[true, true, true, true, true, false]);
    }
}
