use tauri::State;

use crate::backup;
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

/// Delete every knowledge item (plus derived tags / topics / FTS rows).
/// Returns how many items were removed so the UI can report it. Irreversible —
/// the Settings page guards this behind an explicit confirm.
#[tauri::command]
pub fn knowledge_clear(state: State<'_, Db>) -> Result<usize, String> {
    let conn = state.conn.lock().unwrap();
    db::clear(&conn).map_err(|e| e.to_string())
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