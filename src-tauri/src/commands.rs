use tauri::State;
use base64::Engine;

use crate::backup;
use crate::chat;
use crate::config::{ApiConfig, ApiConfigState};
use crate::config::{self, WebDavConfig, WebDavConfigState};
use crate::db::{self, Db, KnowledgePayload};
use crate::llm::{LlmError, ModelMessage, OpenAiCompatClient};
use crate::log::{self, LogState};
use crate::similarity;
use crate::sync;
use crate::vision;
use crate::webdav;

// Thin Tauri command layer: parse args, call db, convert errors to strings.
// Keeps db.rs free of any Tauri dependency.

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedImage {
    name: String,
    url: String,
}

/// Convert paths received from Tauri's native drag/drop event into the same
/// in-memory data URLs used by picker and clipboard imports.
#[tauri::command]
pub fn import_read_dropped_images(paths: Vec<String>) -> Result<Vec<DroppedImage>, String> {
    let mut images = Vec::new();
    for raw in paths {
        let path = std::path::Path::new(&raw);
        let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
        let mime = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            _ => continue,
        };
        let bytes = std::fs::read(path).map_err(|error| format!("无法读取图片 {}：{error}", path.display()))?;
        if bytes.is_empty() { continue; }
        let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("拖入的图片").to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        images.push(DroppedImage { name, url: format!("data:{mime};base64,{encoded}") });
    }
    Ok(images)
}

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
    let context_tokens = config.vision_context_tokens.max(1);
    let max_tokens = config.vision_max_tokens.max(1);
    if max_tokens > context_tokens {
        return Err("Vision 最大输出 Token 不能超过模型上下文大小。".into());
    }
    if !matches!(config.vision_reasoning_effort.as_str(), "low" | "medium" | "high") {
        return Err("思考强度必须是 low、medium 或 high。".into());
    }
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
    guard.vision_context_tokens = context_tokens;
    guard.vision_max_tokens = max_tokens;
    guard.vision_thinking_enabled = config.vision_thinking_enabled;
    guard.vision_reasoning_effort = config.vision_reasoning_effort;
    guard.database_location = db_location;
    let snapshot = guard.clone();
    drop(guard);
    let mut store = state.profiles.lock().unwrap();
    let active = store.active_id.clone();
    if let Some(profile) = store.profiles.iter_mut().find(|p| p.id == active) { profile.config = snapshot; }
    config::save_profiles(&data_dir, &store).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_profiles(state: State<'_, ApiConfigState>) -> config::ApiConfigProfiles {
    let mut store = state.profiles.lock().unwrap().clone();
    for p in &mut store.profiles { p.config.api_key.clear(); }
    store
}

#[tauri::command]
pub fn settings_profile_create(state: State<'_, ApiConfigState>, name: String) -> Result<String, String> {
    let name = name.trim(); if name.is_empty() { return Err("配置名称不能为空。".into()); }
    let id = format!("profile-{}", chrono::Utc::now().timestamp_micros());
    let mut config = ApiConfig::default(); config.database_location = state.db_path.display().to_string();
    let mut store = state.profiles.lock().unwrap();
    store.profiles.push(config::ApiConfigProfile { id: id.clone(), name: name.into(), config });
    config::save_profiles(&state.data_dir, &store).map_err(|e| e.to_string())?; Ok(id)
}

#[tauri::command]
pub fn settings_profile_rename(state: State<'_, ApiConfigState>, id: String, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() { return Err("配置名称不能为空。".into()); }
    let mut store = state.profiles.lock().unwrap();
    let profile = store.profiles.iter_mut().find(|p| p.id == id).ok_or("配置不存在。")?;
    profile.name = name.into();
    config::save_profiles(&state.data_dir, &store).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_profile_switch(state: State<'_, ApiConfigState>, id: String) -> Result<ApiConfig, String> {
    let mut store = state.profiles.lock().unwrap();
    let cfg = store.profiles.iter().find(|p| p.id == id).map(|p| p.config.clone()).ok_or("配置不存在。")?;
    store.active_id = id; config::save_profiles(&state.data_dir, &store).map_err(|e| e.to_string())?;
    *state.inner.lock().unwrap() = cfg.clone(); let mut public = cfg; public.api_key.clear(); Ok(public)
}

#[tauri::command]
pub fn settings_profile_delete(state: State<'_, ApiConfigState>, id: String) -> Result<(), String> {
    let mut store = state.profiles.lock().unwrap();
    if store.profiles.len() <= 1 { return Err("至少需要保留一份配置。".into()); }
    let before = store.profiles.len(); store.profiles.retain(|p| p.id != id);
    if store.profiles.len() == before { return Err("配置不存在。".into()); }
    if id == store.active_id {
        store.active_id = store.profiles[0].id.clone();
        *state.inner.lock().unwrap() = store.profiles[0].config.clone();
    }
    config::save_profiles(&state.data_dir, &store).map_err(|e| e.to_string())
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
    log_state: State<'_, LogState>,
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
        Err(e) => {
            // The "test connection" path is the user's first stop when
            // uploads mysteriously fail. Always log here so the diagnostic
            // is on disk even if the user dismisses the UI error.
            log::append_failure(&log_state, "settings_test_connection", &e);
            ConnectionResult { ok: false, message: e.to_string() }
        }
    };
    Ok(result)
}

/// Extract 0..N knowledge drafts from a screenshot via the Vision model.
/// `image_data_url` is a `data:image/...;base64,...` URL built in the frontend
/// from the in-memory object URL — it never touches disk. Returns structured,
/// validated drafts (never raw model text).
///
/// Failures are mirrored to the diagnostic log (`logs/app-YYYY-MM-DD.log`)
/// so the user can open the log folder from Settings and see the raw
/// upstream response body, not just the user-facing summary.
#[tauri::command]
pub async fn vision_extract(
    db: State<'_, Db>,
    config_state: State<'_, ApiConfigState>,
    log_state: State<'_, LogState>,
    image_data_url: String,
    source: String,
) -> Result<Vec<KnowledgePayload>, String> {
    if config_state.inner.lock().unwrap().api_key.trim().is_empty() {
        return Err(LlmError::MissingConfig("尚未配置 API Key，请先在「设置」中填写。".into()).to_string());
    }
    let cfg = config_state.inner.lock().unwrap().clone();
    let client = OpenAiCompatClient::new(cfg.api_base_url, cfg.api_key, cfg.vision_model)
        .with_reasoning(cfg.vision_thinking_enabled, cfg.vision_reasoning_effort);
    let existing_categories = {
        let conn = db.conn.lock().unwrap();
        let items = db::list(&conn).map_err(|e| e.to_string())?;
        let mut categories = items
            .into_iter()
            .filter(|item| !item.topic.trim().is_empty() && item.topic.chars().count() <= 24)
            .map(|item| (item.domain, item.topic))
            .collect::<std::collections::BTreeSet<_>>();
        for topic in db::list_topics(&conn).map_err(|e| e.to_string())? {
            if !categories.iter().any(|(_, existing)| existing == &topic) {
                categories.insert(("未分类".to_string(), topic));
            }
        }
        categories.into_iter().collect::<Vec<_>>()
    };
    vision::extract_from_image(
        &client,
        &image_data_url,
        &source,
        cfg.vision_max_tokens,
        &existing_categories,
    )
        .await
        .map_err(|e| {
            log::append_failure(&log_state, "vision_extract", &e);
            e.to_string()
        })
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
    log_state: State<'_, LogState>,
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
    let mut call = move |msgs: Vec<serde_json::Value>, allow_tools: bool| -> chat::BoxFuture<Result<ModelMessage, LlmError>> {
        let c = model.clone();
        let t = tools_for_call.clone();
        Box::pin(async move {
            c.chat_with_tools(&msgs, allow_tools.then_some(&t)).await
        })
    };

    let (answer, pool) = chat::run_chat_loop(base, chat::MAX_TOOL_ROUNDS, &mut call, &mut run_tool)
        .await
        .map_err(|e: String| {
            // run_chat_loop returns String errors (LlmError is converted inside
            // the loop), so we don't have access to the response body here.
            // Log what we have — the upstream body is also captured by
            // `log::append_failure` if the loop re-throws the LlmError.
            log::append(&log_state, "knowledge_chat", &e);
            e
        })?;
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeExportSummary {
    path: String,
    item_count: usize,
}

fn knowledge_markdown(items: &[KnowledgePayload]) -> String {
    let mut out = format!("# Interview Kit 知识库\n\n共 {} 条知识。\n", items.len());
    for (index, item) in items.iter().enumerate() {
        let question = item.question.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!("\n---\n\n## {}. {}\n\n", index + 1, question));
        out.push_str(&format!("**主题：** {}\n\n", if item.topic.is_empty() { "无主题" } else { &item.topic }));
        if !item.tags.is_empty() {
            out.push_str(&format!("**标签：** {}\n\n", item.tags.join("、")));
        }
        out.push_str("### 答案\n\n");
        out.push_str(item.answer.trim());
        out.push('\n');
        if !item.follow_ups.is_empty() {
            out.push_str("\n### 延伸问题\n\n");
            for follow_up in &item.follow_ups {
                out.push_str(&format!("- {}\n", follow_up.trim()));
            }
        }
    }
    out
}

#[tauri::command]
pub fn knowledge_export_markdown(
    state: State<'_, Db>,
    destination_path: String,
    ids: Option<Vec<String>>,
) -> Result<KnowledgeExportSummary, String> {
    let mut items = {
        let conn = state.conn.lock().unwrap();
        db::list(&conn).map_err(|e| e.to_string())?
    };
    if let Some(ids) = ids {
        let selected: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        items.retain(|item| selected.contains(item.id.as_str()));
        if items.is_empty() {
            return Err("没有可导出的已选知识。".to_string());
        }
    }
    let document = knowledge_markdown(&items);
    std::fs::write(&destination_path, document.as_bytes())
        .map_err(|e| format!("写入导出文档失败：{e}"))?;
    Ok(KnowledgeExportSummary {
        path: destination_path,
        item_count: items.len(),
    })
}

#[tauri::command]
pub fn knowledge_get(state: State<'_, Db>, id: String) -> Result<Option<KnowledgePayload>, String> {
    let conn = state.conn.lock().unwrap();
    db::get(&conn, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn topic_list(state: State<'_, Db>) -> Result<Vec<String>, String> {
    let conn = state.conn.lock().unwrap();
    db::list_topics(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn topic_create(state: State<'_, Db>, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("主题名称不能为空。".to_string());
    }
    if name.chars().count() > 24 || name.contains(['\n', '\r']) {
        return Err("主题名称不能超过 24 个字符或包含换行。".to_string());
    }
    let conn = state.conn.lock().unwrap();
    db::create_topic(&conn, name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn topic_delete(state: State<'_, Db>, name: String) -> Result<bool, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("主题名称不能为空。".to_string());
    }
    let conn = state.conn.lock().unwrap();
    db::delete_empty_topic(&conn, name).map_err(|e| e.to_string())
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

/// Delete every knowledge item (plus derived tags / topics / FTS rows).
/// Returns how many items were removed so the UI can report it. Irreversible —
/// the Settings page guards this behind an explicit confirm.
#[tauri::command]
pub fn knowledge_clear(state: State<'_, Db>) -> Result<usize, String> {
    let conn = state.conn.lock().unwrap();
    db::clear(&conn).map_err(|e| e.to_string())
}

/// Soft-delete a single knowledge item by its local `id`. Stamps `deleted_at`
/// (so list/search/get stop returning it) and drops its FTS row. The
/// underlying row stays in place so future backups/snapshots still carry
/// the tombstone. Returns `true` if a row was actually transitioned active →
/// tombstoned; `false` if the id is unknown or already tombstoned. The UI
/// guards this with an explicit confirm.
#[tauri::command]
pub fn knowledge_delete(state: State<'_, Db>, id: String) -> Result<bool, String> {
    let kid: i64 = id
        .parse()
        .map_err(|_| format!("knowledge id 必须是数字：{id}"))?;
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    let conn = state.conn.lock().unwrap();
    db::soft_delete_by_id(&conn, kid, &now).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Sync (local file transport)
// ---------------------------------------------------------------------------

/// Build a `.iksync` snapshot of the current local knowledge state and
/// write it to `destination_path`. The write is atomic (stage at
/// `<path>.tmp` then rename) so an interrupted export can never leave a
/// half-written file at the canonical path.
#[tauri::command]
pub fn sync_export_local(
    db: State<'_, Db>,
    destination_path: String,
) -> Result<sync::SyncExportSummary, String> {
    let conn = db.conn.lock().unwrap();
    let dest = std::path::PathBuf::from(&destination_path);
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    sync::sync_export_local(&conn, &dest).map_err(|e| e.to_string())
}

/// Inspect a `.iksync` file without touching the database. Used by the
/// UI to display a preview before the user confirms an import.
#[tauri::command]
pub fn sync_inspect(source_path: String) -> Result<sync::SyncInspectReport, String> {
    let path = std::path::PathBuf::from(&source_path);
    sync::sync_inspect(&path).map_err(|e| e.to_string())
}

/// Read a `.iksync` file, validate it, and merge it into the local
/// database. The merge is all-or-nothing (see `sync::sync_import_local`).
#[tauri::command]
pub fn sync_import_local(
    db: State<'_, Db>,
    source_path: String,
) -> Result<sync::SyncImportSummary, String> {
    let mut guard = db.conn.lock().unwrap();
    let path = std::path::PathBuf::from(&source_path);
    sync::sync_import_local(&mut guard, &path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Sync (WebDAV transport)
//
// WebDAV credentials are DEVICE configuration: they live in `webdav.json`
// next to `config.json` and are never part of a snapshot, a backup or the
// knowledge database. `webdav_config_get` therefore blanks the password —
// the UI can only ever send it, never read it back.
// ---------------------------------------------------------------------------

/// Read the WebDAV configuration. The password is intentionally *not*
/// included: the UI only needs to know a URL / username are configured, and
/// an empty password on save means "keep the existing one".
#[tauri::command]
pub fn webdav_config_get(state: State<'_, WebDavConfigState>) -> WebDavConfig {
    let mut cfg = state.inner.lock().unwrap().clone();
    cfg.password = String::new();
    cfg
}

/// Persist the WebDAV configuration. An empty `password` keeps the stored
/// one (the UI never receives it back, so it can only submit an empty field).
#[tauri::command]
pub fn webdav_config_save(
    state: State<'_, WebDavConfigState>,
    config: WebDavConfig,
) -> Result<(), String> {
    let data_dir = state.data_dir.clone();
    let mut guard = state.inner.lock().unwrap();
    let merged = config::merge_webdav(&guard, config);
    *guard = merged.clone();
    drop(guard);
    config::save_webdav(&data_dir, &merged).map_err(|e| e.to_string())
}

/// Verify the WebDAV endpoint without touching `latest.iksync`.
///
/// A plain `GET /` returning 200 is not enough, so this probes with OPTIONS
/// and PROPFIND (Depth: 0) on the configured root and makes sure the
/// `sync/` collection is usable, creating it when missing. The only remote
/// write it can perform is that MKCOL — the snapshot file is never read or
/// written here.
#[tauri::command]
pub async fn webdav_test_connection(
    state: State<'_, WebDavConfigState>,
) -> Result<webdav::WebDavTestResult, String> {
    let cfg = state.inner.lock().unwrap().clone();
    let client = webdav::ReqwestWebDavClient::new(&cfg).map_err(|e| e.to_string())?;
    Ok(webdav::test_connection(&client).await)
}

/// Full WebDAV sync round-trip.
///
/// Downloads the remote snapshot, merges it with the local one using the
/// shared Sync Engine, and uploads the merged result — so a Mac and a Pad
/// that both added items converge instead of overwriting each other.
/// A missing remote snapshot is a normal first sync, not an error. The
/// database is never held across a network round-trip.
#[tauri::command]
pub async fn sync_webdav(
    db: State<'_, Db>,
    webdav_state: State<'_, WebDavConfigState>,
) -> Result<sync::WebDavSyncSummary, String> {
    let cfg = webdav_state.inner.lock().unwrap().clone();
    let client = webdav::ReqwestWebDavClient::new(&cfg).map_err(|e| e.to_string())?;
    sync::sync_via_webdav(&db.conn, &client)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Backup / Restore
// ---------------------------------------------------------------------------

/// Result of a successful backup, returned to the UI so it can show a
/// success summary (counts + on-disk path).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub path: String,
    pub format_version: u16,
    pub created_at: String,
    pub knowledge_count: i64,
    pub domain_count: i64,
}

/// Build a `.ikbackup` at the user-chosen `destination_path`. The file format
/// is documented in `backup.rs`. The archive carries only Knowledge data
/// (`manifest.json` + `database.sqlite`); application configuration is never
/// read or written here. Safe to call while the application is open: the
/// snapshot is taken via SQLite's Online Backup API and respects WAL.
#[tauri::command]
pub fn backup_create(
    config_state: State<'_, ApiConfigState>,
    destination_path: String,
) -> Result<BackupSummary, String> {
    let dest = std::path::PathBuf::from(&destination_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let manifest = backup::create(&config_state.db_path, &dest)
        .map_err(|e| e.to_string())?;
    Ok(BackupSummary {
        path: dest.display().to_string(),
        format_version: manifest.format_version,
        created_at: manifest.created_at,
        knowledge_count: manifest.knowledge_count,
        domain_count: manifest.domain_count,
    })
}

/// Read a `.ikbackup` and return a preview (counts + manifest fields) without
/// mutating any state. Used by the Restore flow to confirm with the user
/// before overwriting the live database.
#[tauri::command]
pub fn backup_inspect(source_path: String) -> Result<backup::InspectReport, String> {
    let path = std::path::PathBuf::from(&source_path);
    backup::inspect(&path).map_err(|e| e.to_string())
}

/// Restore from a `.ikbackup`. The flow:
///
/// 1. Validate the archive (read manifest, open the embedded SQLite in a temp
///    file). If any check fails, no live state is touched.
/// 2. Lock the live DB mutex. Create a safety snapshot of the live
///    database via `snapshot_live_to_file`. From now on we have a rollback
///    target.
/// 3. Replace the live DB file with the embedded SQLite (atomic rename in
///    the same directory). WAL sidecars are dropped so the new connection
///    starts with a clean WAL.
/// 4. Replace the connection inside the Mutex with a freshly opened
///    connection at the new file, then run migrations + FTS rebuild.
///
/// Application configuration (apiBaseUrl, apiKey, chatModel, visionModel,
/// databaseLocation) is intentionally NOT touched by restore — the target
/// machine keeps whatever it had before. Settings persistence is left
/// entirely to the regular Settings save flow.
///
/// Any failure between step 2 and step 4 rolls back: copy the safety
/// snapshot back to the live path and reopen the live connection. The
/// pre-restore database is recovered.
#[tauri::command]
pub fn backup_restore(
    db: State<'_, Db>,
    config_state: State<'_, ApiConfigState>,
    source_path: String,
) -> Result<backup::RestoreOutcome, String> {
    let archive = std::path::PathBuf::from(&source_path);

    // 1. Validate (no live state touched).
    let db_bytes = backup::validate_archive(&archive).map_err(|e| e.to_string())?;

    // 2. Safety snapshot.
    let safety_path = std::env::temp_dir().join(format!(
        "interview-kit-safety-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    {
        let conn = db.conn.lock().unwrap();
        if let Err(e) = backup::snapshot_live_to_file(&conn, &safety_path) {
            return Err(e.to_string());
        }
    }

    // From here on, any error must roll back.
    let outcome: Result<backup::RestoreOutcome, String> = (|| {
        // 3. Replace the live DB file.
        backup::replace_live_db_with(&db_bytes, &config_state.db_path, &safety_path)
            .map_err(|e| e.to_string())?;

        // 4. Reopen the live connection at the new file, migrate, rebuild FTS.
        let mut guard = db.conn.lock().unwrap();
        let new_conn = rusqlite::Connection::open(&config_state.db_path)
            .map_err(|e| e.to_string())?;
        // Run the migrations on the *new* connection. They are idempotent
        // and will bring older schemas up to current; the FTS index is
        // always rebuilt from primary data so we never trust the backup's
        // derived `knowledge_fts` table.
        db::after_restore(&new_conn).map_err(|e| e.to_string())?;
        *guard = new_conn;
        drop(guard);

        // Read final counts for the UI summary.
        let conn = db.conn.lock().unwrap();
        let knowledge_count: i64 = conn
            .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let domain_count: i64 = conn
            .query_row(
                "SELECT count(DISTINCT domain) FROM knowledge_items",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(backup::RestoreOutcome {
            knowledge_count,
            domain_count,
        })
    })();

    // Best-effort safety cleanup on either path.
    if outcome.is_ok() {
        let _ = std::fs::remove_file(&safety_path);
    } else {
        // Roll back: copy the safety snapshot back over the live file and
        // reopen the connection. We do not surface a rollback error — the
        // original error is what the user needs to see. The rollback can
        // still fail; in that pathological case the on-disk DB may be in
        // an unknown state, but the original `outcome` already carries a
        // user-facing message.
        if let Err(rb_err) =
            backup::rollback_to_safety(&config_state.db_path, &safety_path)
        {
            eprintln!(
                "[interview-kit] restore rollback failed: {}",
                rb_err
            );
        } else {
            // Reopen the live connection against the rolled-back file.
            if let Ok(mut guard) = db.conn.lock() {
                if let Ok(new_conn) = rusqlite::Connection::open(&config_state.db_path) {
                    *guard = new_conn;
                }
            }
        }
        let _ = std::fs::remove_file(&safety_path);
    }
    outcome
}

// ---------------------------------------------------------------------------
// Diagnostic log access (Settings → "打开日志目录" affordance).
// ---------------------------------------------------------------------------

/// Open the diagnostic log directory in the OS file manager. The directory
/// is created at app startup; if for some reason it is missing we recreate
/// it before revealing, so the user always lands on a real folder.
#[tauri::command]
pub fn log_open_dir(log_state: State<'_, LogState>) -> Result<(), String> {
    let path = log_state.log_dir().to_path_buf();
    if !path.is_dir() {
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("无法创建日志目录：{e}"))?;
    }
    log::reveal_in_file_manager(&path).map_err(|e| format!("无法打开日志目录：{e}"))?;
    log::append(
        &log_state,
        "session",
        &format!("open log dir {}", path.display()),
    );
    Ok(())
}

/// Return the on-disk path of the log directory so the UI can show it as
/// text (helps users on platforms where the file manager didn't pop up).
#[tauri::command]
pub fn log_get_dir(log_state: State<'_, LogState>) -> Result<String, String> {
    Ok(log_state.log_dir().display().to_string())
}
