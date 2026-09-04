use tauri::State;

use crate::chat;
use crate::config::{self, ApiConfig, ApiConfigState};
use crate::db::{self, Db, KnowledgePayload};
use crate::llm::{LlmError, ModelMessage, OpenAiCompatClient};
use crate::similarity;
use crate::vision;

// Thin Tauri command layer: parse args, call db, convert errors to strings.
// Keeps db.rs free of any Tauri dependency.

/// Read the stored API configuration. The API key is intentionally *not*
/// included in the payload sent to the UI: the UI only needs to know whether a
/// key is configured, never its value (prevents any accidental echo while still
/// letting the Settings page show a masked placeholder).
#[tauri::command]
pub fn settings_get(state: State<'_, ApiConfigState>) -> ApiConfig {
    let mut cfg = state.inner.lock().unwrap().clone();
    // Never echo the API key back to the UI. The field stays empty (showing the
    // masked placeholder); an empty key on save is treated as "keep existing".
    cfg.api_key = String::new();
    cfg
}

/// Persist the API configuration. An incoming `api_key` is ignored when it is
/// empty so the UI can save the rest of the config without re-echoing the key.
#[tauri::command]
pub fn settings_save(state: State<'_, ApiConfigState>, config: ApiConfig) -> Result<(), String> {
    let data_dir = state.data_dir.clone();
    let mut guard = state.inner.lock().unwrap();
    // Always floor the real DB location from the runtime, not user input.
    let db_location = guard.database_location.clone();
    guard.api_base_url = config.api_base_url;
    if !config.api_key.is_empty() {
        guard.api_key = config.api_key;
    }
    guard.chat_model = config.chat_model;
    guard.vision_model = config.vision_model;
    guard.database_location = db_location;
    let snapshot = guard.clone();
    drop(guard);
    config::save(&data_dir, &snapshot).map_err(|e| e.to_string())
}

/// Result of a connection test, shown to the user. Never contains a key.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionResult {
    ok: bool,
    message: String,
}

/// Validate the configured Base URL / Key by calling `GET {base}/models`.
/// Lightweight enough to be a manual "Test Connection" without heavy cost.
#[tauri::command]
pub async fn settings_test_connection(
    config_state: State<'_, ApiConfigState>,
) -> Result<ConnectionResult, String> {
    let cfg = config_state.inner.lock().unwrap().clone();
    if cfg.api_key.trim().is_empty() {
        return Ok(ConnectionResult { ok: false, message: "尚未配置 API Key。".to_string() });
    }
    if cfg.api_base_url.trim().is_empty() {
        return Ok(ConnectionResult { ok: false, message: "尚未配置 API Base URL。".to_string() });
    }
    let client = OpenAiCompatClient::new(cfg.api_base_url, cfg.api_key, cfg.vision_model);
    let result = match client.list_models().await {
        Ok(models) => {
            let message = if models.is_empty() {
                "连接成功（未返回模型列表）。".to_string()
            } else {
                let sample = models.iter().take(3).cloned().collect::<Vec<_>>().join("、");
                format!("连接成功，可访问 {} 个模型（如：{}）。", models.len(), sample)
            };
            ConnectionResult { ok: true, message }
        }
        Err(e) => ConnectionResult { ok: false, message: e.to_string() },
    };
    Ok(result)
}

/// Extract 0..N knowledge drafts from a screenshot via the Vision model.
/// `image_data_url` is a `data:image/...;base64,...` URL built in the frontend
/// from the in-memory object URL — it never touches disk. Returns structured,
/// validated drafts (never raw model text).
#[tauri::command]
pub async fn vision_extract(
    config_state: State<'_, ApiConfigState>,
    image_data_url: String,
    source: String,
) -> Result<Vec<KnowledgePayload>, String> {
    if config_state.inner.lock().unwrap().api_key.trim().is_empty() {
        return Err(LlmError::MissingConfig("尚未配置 API Key，请先在「设置」中填写。".into()).to_string());
    }
    let cfg = config_state.inner.lock().unwrap().clone();
    let client = OpenAiCompatClient::new(cfg.api_base_url, cfg.api_key, cfg.vision_model);
    vision::extract_from_image(&client, &image_data_url, &source)
        .await
        .map_err(|e| e.to_string())
}

/// A knowledge-organization suggestion surfaced in Review before Confirm.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilaritySuggestion {
    relation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_id: Option<String>,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_topic: Option<String>,
}

/// Analyze one draft against existing knowledge: FTS5 Top-K candidates -> LLM
/// relation judge. Returns None when there are no candidates (so no LLM call)
/// or when the judgment fails — Similarity is auxiliary and must never block
/// the main Import flow. No embeddings, no vector DB, no full-base scan.
#[tauri::command]
pub async fn analyze_similarity(
    db: State<'_, Db>,
    config_state: State<'_, ApiConfigState>,
    item: KnowledgePayload,
) -> Result<Option<SimilaritySuggestion>, String> {
    let cfg = config_state.inner.lock().unwrap().clone();
    if cfg.api_key.trim().is_empty() {
        return Ok(None); // no LLM configured -> no suggestion
    }

    // Candidate query from the draft's topic + tags + question (reuse search).
    let mut terms: Vec<String> = Vec::new();
    if !item.topic.trim().is_empty() {
        terms.push(item.topic.trim().to_string());
    }
    terms.extend(item.tags.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
    if !item.question.trim().is_empty() {
        terms.push(item.question.trim().to_string());
    }
    let query = terms.join(" ").trim().to_string();
    if query.is_empty() {
        return Ok(None);
    }

    let trigram = db.trigram;
    let mut candidates = {
        let conn = db.conn.lock().unwrap();
        db::search(&conn, &query, trigram).unwrap_or_default()
    };
    candidates.truncate(5);
    if candidates.is_empty() {
        return Ok(None); // no obvious candidate -> NONE, skip the LLM call
    }

    let model = if cfg.chat_model.trim().is_empty() {
        cfg.vision_model.clone()
    } else {
        cfg.chat_model.clone()
    };
    let client = OpenAiCompatClient::new(cfg.api_base_url, cfg.api_key, model);

    match similarity::judge(&client, &item, &candidates).await {
        Ok(verdict) => {
            let matched = verdict
                .knowledge_id
                .as_ref()
                .and_then(|id| candidates.iter().find(|c| &c.id == id));
            Ok(Some(SimilaritySuggestion {
                relation: verdict.relation.code().to_string(),
                knowledge_id: verdict.knowledge_id.clone(),
                reason: verdict.reason.clone(),
                matched_question: matched.map(|c| c.question.clone()),
                matched_domain: matched.map(|c| c.domain.clone()),
                matched_topic: matched.map(|c| c.topic.clone()),
            }))
        }
        Err(_) => Ok(None), // degrade to "no suggestion" on any LLM/candidate error
    }
}

/// Chat response payload: the answer plus verified local-knowledge citations.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatResult {
    answer: String,
    citations: Vec<ChatCitationOut>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatCitationOut {
    knowledge_id: String,
    question: String,
}

/// One prior conversational turn received from the frontend.
#[derive(serde::Deserialize)]
pub struct ChatHistoryEntry {
    role: String,
    content: String,
}

/// Knowledge Chat.
///   question + scope -> scoped FTS5 Top K -> Chat model with context -> verified citations.
/// Reuses the shared OpenAI-compatible client with `chatModel`.
#[tauri::command]
pub async fn knowledge_chat(
    db: State<'_, Db>,
    config_state: State<'_, ApiConfigState>,
    question: String,
    scope: String,
    history: Vec<ChatHistoryEntry>,
) -> Result<ChatResult, String> {
    let q = question.trim();
    if q.is_empty() {
        return Err("问题不能为空。".to_string());
    }
    let cfg = config_state.inner.lock().unwrap().clone();
    if cfg.api_key.trim().is_empty() {
        return Err("尚未配置 API Key，请先在「设置」中填写。".to_string());
    }
    if cfg.chat_model.trim().is_empty() {
        return Err("尚未配置 Chat Model，请先在「设置」中填写。".to_string());
    }

    // Scope is a USER constraint, never decided by the model.
    let (domain, topic) = parse_scope(&scope);
    let trigram = db.trigram;

    // In-memory conversation history (capped); Chat is a normal LLM conversation.
    let mut base: Vec<serde_json::Value> =
        vec![serde_json::json!({"role": "system", "content": chat::CHAT_SYSTEM_PROMPT})];
    let start = history.len().saturating_sub(chat::MAX_HISTORY);
    for h in history.into_iter().skip(start) {
        let role = if h.role == "user" { "user" } else { "assistant" };
        base.push(serde_json::json!({"role": role, "content": h.content}));
    }

    let tools: serde_json::Value = serde_json::json!([chat::tool_schema()]);
    let client = OpenAiCompatClient::new(cfg.api_base_url, cfg.api_key, cfg.chat_model);
    let model = client.clone();
    let tools_for_call = tools.clone();

    // knowledge_search executor — reuses db::search_scoped (jieba + FTS5 + BM25)
    // and always applies the user's current Scope + Top-K.
    let mut run_tool = |tc: &crate::llm::ToolCall| -> Result<Vec<KnowledgePayload>, String> {
        let tq = chat::parse_tool_query(tc)?;
        let conn = db.conn.lock().unwrap();
        db::search_scoped(&conn, &tq.query, domain.as_deref(), topic.as_deref(), trigram, chat::TOOL_TOP_K)
            .map_err(|e| e.to_string())
    };
    // One model round-trip with the tools array attached.
    let mut call = move |msgs: Vec<serde_json::Value>| -> chat::BoxFuture<Result<ModelMessage, LlmError>> {
        let c = model.clone();
        let t = tools_for_call.clone();
        Box::pin(async move { c.chat_with_tools(&msgs, Some(&t)).await })
    };

    let (answer, pool) = chat::run_chat_loop(base, chat::MAX_TOOL_ROUNDS, &mut call, &mut run_tool)
        .await
        .map_err(|e| e.to_string())?;
    let (answer, citations) = chat::normalize_citations(&answer, &pool);

    Ok(ChatResult {
        answer,
        citations: citations
            .into_iter()
            .map(|c| ChatCitationOut { knowledge_id: c.knowledge_id, question: c.question })
            .collect(),
    })
}

/// Parse `""` -> all, `"domain:XXX"` -> domain, `"topic:YYY"` -> topic.
fn parse_scope(scope: &str) -> (Option<String>, Option<String>) {
    if let Some(rest) = scope.strip_prefix("domain:") {
        return (Some(rest.to_string()), None);
    }
    if let Some(rest) = scope.strip_prefix("topic:") {
        return (None, Some(rest.to_string()));
    }
    (None, None)
}

#[tauri::command]
pub fn knowledge_list(state: State<'_, Db>) -> Result<Vec<KnowledgePayload>, String> {
    let conn = state.conn.lock().unwrap();
    db::list(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn knowledge_get(state: State<'_, Db>, id: String) -> Result<Option<KnowledgePayload>, String> {
    let conn = state.conn.lock().unwrap();
    db::get(&conn, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn knowledge_search(state: State<'_, Db>, query: String) -> Result<Vec<KnowledgePayload>, String> {
    let trigram = state.trigram;
    let conn = state.conn.lock().unwrap();
    db::search(&conn, &query, trigram).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn knowledge_save(state: State<'_, Db>, item: KnowledgePayload) -> Result<KnowledgePayload, String> {
    let conn = state.conn.lock().unwrap();
    db::save(&conn, &item).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn knowledge_recent(state: State<'_, Db>, limit: Option<usize>) -> Result<Vec<KnowledgePayload>, String> {
    let conn = state.conn.lock().unwrap();
    db::recent(&conn, limit.unwrap_or(10)).map_err(|e| e.to_string())
}