use rusqlite::{params, Connection};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tokenizer;

/// Managed app-wide database handle. Interior mutability so Tauri can share it.
/// `trigram` is retained for historical command-compatibility but is no longer
/// consulted: retrieval always runs through the Jieba/unicode61 FTS index.
pub struct Db {
    pub conn: Mutex<Connection>,
    pub trigram: bool,
}

/// Resolve the on-disk SQLite file inside the platform app-data directory.
/// e.g. Linux: ~/.local/share/com.interviewkit.desktop/interview-kit.db
pub fn resolve_db_path(data_dir: PathBuf) -> std::io::Result<PathBuf> {
    fs::create_dir_all(&data_dir)?;
    Ok(data_dir.join("interview-kit.db"))
}

/// Open (or create) the connection at `path`. Does not run migrations —
/// caller invokes `init` afterwards.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Idempotent bootstrap: run migrations, create/rebuild the FTS index. The
/// database starts empty — the user builds their knowledge base by importing
/// screenshots / saving items themselves. Returns true when the FTS index is
/// ready (always).
pub fn init(conn: &Connection) -> rusqlite::Result<bool> {
    migrate(conn)?;
    let ready = init_fts(conn)?;
    Ok(ready)
}

/// Versioned schema setup (PRAGMA user_version).
///   1  -> relational tables (topics, knowledge_items, tags, knowledge_tags).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
            CREATE TABLE IF NOT EXISTS topics (
                id     INTEGER PRIMARY KEY AUTOINCREMENT,
                domain TEXT NOT NULL,
                topic  TEXT NOT NULL,
                UNIQUE(domain, topic)
            );
            CREATE TABLE IF NOT EXISTS tags (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS knowledge_items (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                question     TEXT NOT NULL,
                answer       TEXT NOT NULL,
                domain       TEXT NOT NULL,
                topic        TEXT NOT NULL,
                source       TEXT NOT NULL DEFAULT '',
                follow_ups   TEXT NOT NULL DEFAULT '[]',
                related_ids  TEXT NOT NULL DEFAULT '[]',
                favorite     INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL,
                last_read_at TEXT
            );
            CREATE TABLE IF NOT EXISTS knowledge_tags (
                knowledge_id INTEGER NOT NULL,
                tag_id       INTEGER NOT NULL,
                PRIMARY KEY (knowledge_id, tag_id),
                FOREIGN KEY (knowledge_id) REFERENCES knowledge_items(id) ON DELETE CASCADE,
                FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_knowledge_domain   ON knowledge_items(domain);
            CREATE INDEX IF NOT EXISTS idx_knowledge_topic    ON knowledge_items(topic);
            CREATE INDEX IF NOT EXISTS idx_knowledge_lastread ON knowledge_items(last_read_at);
            CREATE INDEX IF NOT EXISTS idx_knowledge_tags_tag ON knowledge_tags(tag_id);
            COMMIT;
            PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

/// FTS5 tokenizer for the derived index, as a directive string (SQL-wrapped in
/// double quotes, tokenchars value single-quoted — the quoting FTS5 accepts).
///
/// `unicode61` is built-in FTS5 (no custom C tokenizer / no loadable extension);
/// document text is pre-tokenized to space-joined word tokens (tokenizer::doc_space),
/// so unicode61 matches whole words rather than the old `trigram` substrings.
/// `tokenchars '+#'` keeps `+` and `#` as part of tokens, so `C++` / `C#` stay
/// distinct single tokens instead of collapsing to a lone `c` (jieba already
/// splits `B+Tree` / `Node.js` / `TCP/IP` into their constituents on its own).
const FTS_TOKENIZER: &str = "unicode61 tokenchars '+#'";

/// Does this tokenizer keep extra token characters (so matching against the
/// stored DDL must require them too)?
fn tokenizer_uses_tokenchars() -> bool {
    FTS_TOKENIZER.contains("tokenchars")
}

fn fts_ddl() -> String {
    format!(
        "CREATE VIRTUAL TABLE knowledge_fts USING fts5(
            question, answer, domain, topic, tags,
            tokenize = \"{}\"
         );",
        FTS_TOKENIZER
    )
}

/// True when knowledge_fts already uses the Jieba/unicode61 scheme, including
/// (if configured) the same `tokenchars` option. A plain `unicode61` table from
/// an earlier build is treated as stale and rebuilt to the current scheme.
fn fts_is_jieba(conn: &Connection) -> rusqlite::Result<bool> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='knowledge_fts'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(match sql {
        Some(s) => {
            s.contains("unicode61") && (!tokenizer_uses_tokenchars() || s.contains("tokenchars"))
        }
        None => false,
    })
}

/// Drop and rebuild the derived FTS index from `knowledge_items`. The primary
/// table is never touched — data, IDs, tags and topics are preserved; only the
/// tokenized read-model is rebuilt (rowid = knowledge id stays stable).
pub fn rebuild_fts(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("DROP TABLE IF EXISTS knowledge_fts; {}", fts_ddl()))?;
    for p in list(conn)? {
        let Some(kid) = p.id.parse::<i64>().ok() else { continue };
        sync_fts(conn, kid, &p)?;
    }
    Ok(())
}

/// Run pending migrations then drop+rebuild the derived FTS index. Used by
/// Restore after the database file is replaced: never trust the FTS table from
/// the backup, the source of truth is `knowledge_items`.
pub fn after_restore(conn: &Connection) -> rusqlite::Result<()> {
    migrate(conn)?;
    rebuild_fts(conn)?;
    Ok(())
}

/// Create (or, if the schema/tokenizer differs — e.g. legacy `trigram` table —
/// rebuild) the FTS index. Idempotent: no-op on an already-Jieba index; runs
/// automatically at app startup. Never requires deleting the database.
fn init_fts(conn: &Connection) -> rusqlite::Result<bool> {
    if !fts_is_jieba(conn)? {
        rebuild_fts(conn)?;
    }
    Ok(true)
}

/// Minimal DB row mirror of the frontend `Knowledge`; camelCase matches TS.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgePayload {
    pub id: String,
    pub question: String,
    pub answer: String,
    pub domain: String,
    pub topic: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub follow_ups: Vec<String>,
    #[serde(default)]
    pub related_ids: Vec<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// The SELECT (without ORDER/LIMIT) used everywhere to hydrate a payload,
/// joining each item's tags into a pipe-separated string.
const ITEM_SELECT: &str = "SELECT k.id, k.question, k.answer, k.domain, k.topic,
        k.source, k.follow_ups, k.related_ids, k.favorite, k.created_at, k.updated_at, k.last_read_at,
        (SELECT group_concat(t.name, '|') FROM knowledge_tags kt JOIN tags t ON t.id = kt.tag_id
          WHERE kt.knowledge_id = k.id) AS tags
      FROM knowledge_items k ";

fn row_to_payload(row: &rusqlite::Row) -> rusqlite::Result<KnowledgePayload> {
    Ok(KnowledgePayload {
        id: row.get::<_, i64>(0)?.to_string(),
        question: row.get(1)?,
        answer: row.get(2)?,
        domain: row.get(3)?,
        topic: row.get(4)?,
        source: row.get(5)?,
        follow_ups: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        related_ids: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
        favorite: Some(row.get::<_, i64>(8)? != 0),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        last_read_at: row.get(11)?,
        tags: row
            .get::<_, Option<String>>(12)?
            .map(|s| s.split('|').filter(|x| !x.is_empty()).map(String::from).collect())
            .unwrap_or_default(),
    })
}

/// All knowledge, newest first (stable for the browse page).
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let sql = format!("{} ORDER BY k.created_at DESC, k.id DESC", ITEM_SELECT);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| row_to_payload(r))?;
    rows.collect()
}

/// Fetch one item, recording its `last_read_at` as a side effect (detail view).
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<KnowledgePayload>> {
    let Some(kid) = parse_id(id) else { return Ok(None) };
    let now = now_ts();
    conn.execute(
        "UPDATE knowledge_items SET last_read_at = ?1 WHERE id = ?2",
        params![now, kid],
    )?;
    get_by_kid(conn, kid)
}

fn get_by_kid(conn: &Connection, kid: i64) -> rusqlite::Result<Option<KnowledgePayload>> {
    let sql = format!("{} WHERE k.id = ?1", ITEM_SELECT);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![kid], |r| row_to_payload(r))?;
    rows.next().transpose()
}

fn parse_id(id: &str) -> Option<i64> {
    id.parse::<i64>().ok()
}

/// Items ordered by last read time (most recent first, unread last).
pub fn recent(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let cap = limit.clamp(1, 500) as i64;
    let sql = format!(
        "{} ORDER BY (k.last_read_at IS NULL) ASC, k.last_read_at DESC, k.id DESC LIMIT ?1",
        ITEM_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![cap], |r| row_to_payload(r))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Write path (insert-or-update) + normalized tags/topics + FTS sync
// ---------------------------------------------------------------------------

pub fn save(conn: &Connection, item: &KnowledgePayload) -> rusqlite::Result<KnowledgePayload> {
    begin(conn)?;
    let result = save_internal(conn, item);
    match result {
        Ok(r) => {
            conn.execute_batch("COMMIT")?;
            Ok(r)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn begin(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN")
}

/// Delete every knowledge row plus the derived tag / topic / FTS state.
/// The schema (tables, indexes, `user_version`) is left intact and the
/// connection stays open, so a running app keeps working without a restart.
/// Returns the number of knowledge items that were removed.
///
/// `knowledge_tags` would cascade from `knowledge_items` (foreign_keys=ON), but
/// it is deleted explicitly as well: an emptied base must never depend on the
/// pragma being set, and must not leave orphaned tag rows behind.
pub fn clear(conn: &Connection) -> rusqlite::Result<usize> {
    begin(conn)?;
    let result = (|| -> rusqlite::Result<usize> {
        let count: i64 = conn.query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))?;
        conn.execute_batch(
            "DELETE FROM knowledge_tags;
             DELETE FROM knowledge_items;
             DELETE FROM tags;
             DELETE FROM topics;
             DELETE FROM knowledge_fts;",
        )?;
        Ok(count as usize)
    })();
    match result {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Insert a new row or update an existing one (matched by numeric id string).
/// Returns the persisted payload with the real DB id. Also keeps topics, tags
/// and the FTS index in sync.
fn save_internal(conn: &Connection, item: &KnowledgePayload) -> rusqlite::Result<KnowledgePayload> {
    let favorite = i64::from(item.favorite.unwrap_or(false));
    let exist_id = parse_id(&item.id);
    let kid: i64 = if let Some(exist) = exist_id {
        let found: Option<i64> = conn
            .query_row(
                "SELECT id FROM knowledge_items WHERE id = ?1",
                params![exist],
                |r| r.get(0),
            )
            .ok();
        if let Some(found) = found {
            conn.execute(
                "UPDATE knowledge_items SET question=?1, answer=?2, domain=?3, topic=?4,
                        source=?5, follow_ups=?6, related_ids=?7, favorite=?8, updated_at=?9, last_read_at=?10
                 WHERE id = ?11",
                params![
                    item.question,
                    item.answer,
                    item.domain,
                    item.topic,
                    item.source,
                    serde_json::to_string(&item.follow_ups).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&item.related_ids).unwrap_or_else(|_| "[]".into()),
                    favorite,
                    item.updated_at,
                    item.last_read_at,
                    found,
                ],
            )?;
            found
        } else {
            insert_row(conn, item, favorite)?
        }
    } else {
        insert_row(conn, item, favorite)?
    };

    sync_topics(conn, &item.domain, &item.topic)?;
    sync_tags(conn, kid, &item.tags)?;
    sync_fts(conn, kid, item)?;
    get_by_kid(conn, kid)?.ok_or_else(|| rusqlite::Error::InvalidQuery)
}

fn insert_row(conn: &Connection, item: &KnowledgePayload, favorite: i64) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO knowledge_items
            (question, answer, domain, topic, source, follow_ups, related_ids, favorite, created_at, updated_at, last_read_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            item.question,
            item.answer,
            item.domain,
            item.topic,
            item.source,
            serde_json::to_string(&item.follow_ups).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&item.related_ids).unwrap_or_else(|_| "[]".into()),
            favorite,
            item.created_at,
            item.updated_at,
            item.last_read_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Normalize the (domain, topic) pair into `topics`.
fn sync_topics(conn: &Connection, domain: &str, topic: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO topics(domain, topic) VALUES (?1, ?2)
         ON CONFLICT(domain, topic) DO NOTHING",
        params![domain, topic],
    )?;
    Ok(())
}

/// Rewrite the tag set for a knowledge row (delete stale, add new).
fn sync_tags(conn: &Connection, kid: i64, tags: &[String]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM knowledge_tags WHERE knowledge_id = ?1", params![kid])?;
    for tag in tags {
        let name = tag.trim();
        if name.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        let tid: i64 = conn.query_row("SELECT id FROM tags WHERE name = ?1", params![name], |r| r.get(0))?;
        conn.execute(
            "INSERT OR IGNORE INTO knowledge_tags(knowledge_id, tag_id) VALUES (?1, ?2)",
            params![kid, tid],
        )?;
    }
    Ok(())
}

/// Keep the FTS5 row in sync with the knowledge row. Only the derived index is a
/// word-token read-model: the raw text lives forever in `knowledge_items` and is
/// what every reader (UI / Detail / Chat context / LLM) consumes. `knowledge_fts`
/// columns here hold jieba space-joined tokens, never the original.
fn sync_fts(conn: &Connection, kid: i64, item: &KnowledgePayload) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM knowledge_fts WHERE rowid = ?1", params![kid])?;
    conn.execute(
        "INSERT INTO knowledge_fts(rowid, question, answer, domain, topic, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            kid,
            tokenizer::doc_space(&item.question),
            tokenizer::doc_space(&item.answer),
            tokenizer::doc_space(&item.domain),
            tokenizer::doc_space(&item.topic),
            tokenizer::doc_space(&item.tags.join(" ")),
        ],
    )?;
    Ok(())
}

/// Text search (Knowledge page + Similarity). Tokenizes the query with jieba
/// into CORE terms, then runs a two-tier FTS plan: AND of the core terms first
/// (precise), falling back to OR (recall-first) when nothing matches. No LIKE
/// fallback — the index is tokenized, so a whole-substring LIKE is meaningless.
/// `_trigram_available` is ignored (kept only for call-site compatibility).
pub fn search(conn: &Connection, query: &str, _trigram_available: bool) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }
    if tokenizer::core_tokens(q).is_empty() {
        return Ok(vec![]);
    }
    let first = run_fts(conn, &tokenizer::and_query(q), None, None, 50)?;
    if !first.is_empty() {
        return Ok(first);
    }
    run_fts(conn, &tokenizer::or_query(q), None, None, 50)
}

/// Execute a single FTS5 MATCH (plus optional domain/topic scope), ranked by
/// BM25 with field weights so `question` outranks `topic`/`tags`, which outrank
/// `answer`. FTS5-only: the derived index holds jieba word-tokens, so no LIKE
/// fallback is needed or meaningful.
fn run_fts(
    conn: &Connection,
    match_query: &str,
    domain: Option<&str>,
    topic: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<KnowledgePayload>> {
    if match_query.trim().is_empty() {
        return Ok(vec![]);
    }
    let cap = limit.clamp(1, 200) as i64;

    // Scope conditions appended to the WHERE, params start after ?1.
    fn scope_conditions(
        domain: Option<&str>,
        topic: Option<&str>,
        params: &mut Vec<rusqlite::types::Value>,
    ) -> String {
        let mut sql = String::new();
        if let Some(v) = domain {
            if !v.trim().is_empty() {
                let idx = params.len() + 1;
                params.push(rusqlite::types::Value::Text(v.trim().to_string()));
                sql.push_str(&format!(" AND k.domain = ?{idx} "));
            }
        }
        if let Some(v) = topic {
            if !v.trim().is_empty() {
                let idx = params.len() + 1;
                params.push(rusqlite::types::Value::Text(v.trim().to_string()));
                sql.push_str(&format!(" AND k.topic = ?{idx} "));
            }
        }
        sql
    }

    let mut params = vec![rusqlite::types::Value::Text(match_query.to_string())];
    let scope = scope_conditions(domain, topic, &mut params);
    params.push(rusqlite::types::Value::Integer(cap));
    let sql = format!(
        "{} JOIN knowledge_fts ON knowledge_fts.rowid = k.id
         WHERE knowledge_fts MATCH ?1 {scope}
         ORDER BY bm25(knowledge_fts, 8.0, 0.5, 3.0, 3.0, 2.0) LIMIT ?{}",
        ITEM_SELECT,
        params.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| row_to_payload(r))?;
    rows.collect()
}

/// Scope-restricted search used by Chat retrieval. Same two-tier jieba plan as
/// `search`, but the domain/topic filter and Top-K limit are pushed into the SQL
/// so only in-scope knowledge is returned. `_trigram_available` is ignored.
pub fn search_scoped(
    conn: &Connection,
    query: &str,
    domain: Option<&str>,
    topic: Option<&str>,
    _trigram_available: bool,
    limit: usize,
) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }
    if tokenizer::core_tokens(q).is_empty() {
        return Ok(vec![]);
    }
    let first = run_fts(conn, &tokenizer::and_query(q), domain, topic, limit)?;
    if !first.is_empty() {
        return Ok(first);
    }
    run_fts(conn, &tokenizer::or_query(q), domain, topic, limit)
}

/// Sortable timestamp (UNIX seconds as text) used for `last_read_at`.
fn now_ts() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

// ---------------------------------------------------------------------------
// (No seed data. A fresh database starts empty — the user builds their
// knowledge base by importing screenshots / saving items themselves.)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-09-04 10:20";

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("test.db")
    }

    fn open_inits(path: &std::path::Path) -> (Connection, bool) {
        let conn = open(path).unwrap();
        let trigram = init(&conn).unwrap();
        (conn, trigram)
    }

    fn payload(question: &str) -> KnowledgePayload {
        KnowledgePayload {
            id: String::new(),
            question: question.to_string(),
            answer: "答案正文".to_string(),
            domain: "后端开发".to_string(),
            topic: "Redis".to_string(),
            tags: vec!["Redis".to_string(), "测试".to_string()],
            follow_ups: vec![],
            related_ids: vec![],
            source: "unit-test".to_string(),
            created_at: NOW.to_string(),
            updated_at: NOW.to_string(),
            favorite: Some(false),
            last_read_at: None,
        }
    }

    /// Insert a purpose-built row (question/answer/domain/topic) and return it.
    fn insert(
        conn: &Connection,
        question: &str,
        answer: &str,
        domain: &str,
        topic: &str,
    ) -> KnowledgePayload {
        save(
            conn,
            &KnowledgePayload {
                id: String::new(),
                question: question.to_string(),
                answer: answer.to_string(),
                domain: domain.to_string(),
                topic: topic.to_string(),
                tags: vec![],
                ..payload("_")
            },
        )
        .unwrap()
    }

    /// The knowledge base used by the retrieval test cases.
    fn jieba_kb(conn: &Connection) {
        insert(conn, "BERT 和 GPT 有什么区别？", "BERT 是双向编码器，GPT 是单向自回归解码器。", "自然语言处理", "BERT 与 GPT 模型对比");
        insert(conn, "Redis 的 RDB 和 AOF 有什么区别？", "RDB 是按时生成的持久化快照，AOF 记录每个写操作。", "后端开发", "Redis");
        insert(conn, "TCP 为什么需要三次握手？", "三次握手用于确认双方收发能力与初始序号。", "计算机网络", "TCP");
        insert(conn, "MySQL 索引为什么使用 B+Tree？", "B+Tree 树高低、叶子有序链接，适合范围扫描。", "数据库", "MySQL");
        insert(conn, "C++ 的智能指针有哪些？", "shared_ptr / unique_ptr / weak_ptr。", "后端开发", "C++");
        insert(conn, "Node.js 的异步 IO 模型是什么？", "基于事件循环和非阻塞 IO。", "后端开发", "Node.js");
        insert(conn, "TCP/IP 的四层网络模型是什么？", "链路层、网络层、传输层、应用层。", "计算机网络", "TCP/IP");
    }

    #[test]
    fn init_creates_empty_schema() {
        let path = tmp_path("empty");
        let (conn, trigram) = open_inits(&path);
        // Fresh DB starts empty: no seed data, no tags, no topics, no FTS rows.
        let items = list(&conn).unwrap();
        assert!(items.is_empty(), "fresh DB must start with zero knowledge");
        assert!(trigram, "bundled SQLite should support trigram tokenizer");
        let tags: i64 = conn.query_row("SELECT count(*) FROM tags", [], |r| r.get(0)).unwrap();
        let topics: i64 = conn.query_row("SELECT count(*) FROM topics", [], |r| r.get(0)).unwrap();
        let fts: i64 = conn.query_row("SELECT count(*) FROM knowledge_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(tags, 0);
        assert_eq!(topics, 0);
        assert_eq!(fts, 0);
    }

    #[test]
    fn save_inserts_and_updates() {
        let path = tmp_path("crud");
        let (conn, _) = open_inits(&path);
        let inserted = save(&conn, &payload("Redis 集群怎么扩容？")).unwrap();
        assert!(inserted.id.parse::<i64>().unwrap() > 0, "got db id");
        assert_eq!(inserted.question, "Redis 集群怎么扩容？");

        // update in place
        let mut updated = inserted.clone();
        updated.question = "Redis 集群横向扩容怎么做？".to_string();
        let saved = save(&conn, &updated).unwrap();
        assert_eq!(saved.id, inserted.id, "same row on update");
        assert_eq!(saved.question, "Redis 集群横向扩容怎么做？");
        // Fresh DB + 1 insert = exactly 1 row.
        assert_eq!(list(&conn).unwrap().len(), 1);
    }

    #[test]
    fn clear_removes_knowledge_and_all_derived_rows() {
        let path = tmp_path("clear");
        let (conn, _) = open_inits(&path);
        // Build payloads that carry tags — the bare `insert()` helper passes an
        // empty tag list, which would leave the derived `tags` table empty and
        // make the "derived state is gone" assertions below vacuous.
        let mk = |q: &str, d: &str, t: &str| KnowledgePayload {
            id: String::new(),
            question: q.to_string(),
            domain: d.to_string(),
            topic: t.to_string(),
            ..payload("_")
        };
        save(&conn, &mk("Redis 为什么快？", "后端开发", "Redis")).unwrap();
        save(&conn, &mk("MySQL 索引为什么用 B+Tree？", "数据库", "MySQL")).unwrap();
        assert_eq!(list(&conn).unwrap().len(), 2);
        // The inserts above populate every derived table.
        assert!(count(&conn, "tags") > 0, "tags populated before clear");
        assert!(count(&conn, "topics") > 0, "topics populated before clear");
        assert!(count(&conn, "knowledge_tags") > 0, "knowledge_tags populated before clear");
        assert!(count(&conn, "knowledge_fts") > 0, "FTS populated before clear");

        let removed = clear(&conn).unwrap();
        assert_eq!(removed, 2, "clear reports how many items it deleted");

        // Knowledge and every derived table are empty; the schema survives
        // (list() still runs) so no reopen / remigrate is needed.
        assert!(list(&conn).unwrap().is_empty());
        for t in ["tags", "topics", "knowledge_tags", "knowledge_fts"] {
            assert_eq!(count(&conn, t), 0, "{t} must be empty after clear");
        }

        // The emptied base stays usable: saving again re-derives tags + FTS.
        let again = save(&conn, &payload("清空后再存一条")).unwrap();
        assert!(!again.id.is_empty());
        assert_eq!(list(&conn).unwrap().len(), 1);
        assert!(count(&conn, "knowledge_fts") > 0, "FTS rebuilt on next save");
    }

    /// Row count of a table, used by the clear test.
    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn get_records_last_read_at() {
        let path = tmp_path("lastread");
        let (conn, _) = open_inits(&path);
        // Insert a single item; verify `get` populates `last_read_at`.
        let inserted = insert(&conn, "测试题", "答案", "测试", "Topic");
        assert!(inserted.last_read_at.is_none(), "fresh insert is unread");

        let got = get(&conn, &inserted.id).unwrap().expect("item found");
        assert!(got.last_read_at.is_some(), "last_read_at set after read");
    }

    #[test]
    fn recent_orders_by_last_read_at() {
        let path = tmp_path("recent");
        let (conn, _) = open_inits(&path);
        // Insert four items and pin distinct `last_read_at` values directly,
        // because `now_ts()` is seconds-resolution and several `get` calls in
        // the same second would tie. Pinning is fine for the SQL ordering
        // test — the only thing under test is the ORDER BY clause.
        let a = insert(&conn, "A 题", "a", "d", "t");
        let b = insert(&conn, "B 题", "b", "d", "t");
        let c = insert(&conn, "C 题", "c", "d", "t");
        let d = insert(&conn, "D 题", "d", "d", "t");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for (id, secs_ago) in [
            (a.id.parse::<i64>().unwrap(), 30i64),
            (b.id.parse::<i64>().unwrap(), 10i64), // newest
            (c.id.parse::<i64>().unwrap(), 20i64),
            (d.id.parse::<i64>().unwrap(), 40i64), // oldest
        ] {
            conn.execute(
                "UPDATE knowledge_items SET last_read_at = ?1 WHERE id = ?2",
                rusqlite::params![(now - secs_ago as u64).to_string(), id],
            )
            .unwrap();
        }

        let recent = recent(&conn, 4).unwrap();
        assert_eq!(recent.len(), 4);
        assert_eq!(recent[0].id, b.id, "most recent read first");
        assert_eq!(recent[1].id, c.id);
        assert_eq!(recent[2].id, a.id);
        assert_eq!(recent[3].id, d.id, "earliest read last");
    }

    #[test]
    fn jieba_search_case1_10() {
        let path = tmp_path("jieba1");
        let (conn, _) = open_inits(&path);
        jieba_kb(&conn);
        let has = |rows: &[KnowledgePayload], pat: &str| rows.iter().any(|x| x.question.contains(pat));
        // Case 1..5: BERT surfaces for keyword, with/without spaces and scaffolding
        for q in ["BERT", "BERT 是什么", "BERT是什么", "你介绍下BERT", "介绍一下 BERT"] {
            let rows = search(&conn, q, true).unwrap();
            assert!(!rows.is_empty(), "{q:?} returns something");
            assert!(has(&rows, "BERT 和 GPT"), "{q:?} recalls the BERT knowledge");
        }
        // Case 6
        assert!(has(&search(&conn, "讲讲Redis", true).unwrap(), "Redis"));
        // Case 7: Redis 持久化 -> RDB/AOF
        assert!(has(&search(&conn, "Redis 持久化", true).unwrap(), "RDB 和 AOF"));
        // Case 8
        assert!(has(&search(&conn, "MySQL索引", true).unwrap(), "B+Tree"));
        // Case 9
        assert!(has(&search(&conn, "TCP三次握手", true).unwrap(), "三次握手"));
        // Case 10: unrelated -> no noise from over-wide OR
        assert!(search(&conn, "量子纠缠实验", true).unwrap().is_empty());
    }

    #[test]
    fn jieba_tech_tokens_recall() {
        let path = tmp_path("jieba_tech");
        let (conn, _) = open_inits(&path);
        jieba_kb(&conn);
        let has = |rows: &[KnowledgePayload], pat: &str| rows.iter().any(|x| x.question.contains(pat));
        assert!(has(&search(&conn, "B+Tree", true).unwrap(), "B+Tree"), "B+Tree natural input");
        assert!(has(&search(&conn, "C++", true).unwrap(), "智能指针"), "C++ survives tokenchars");
        assert!(has(&search(&conn, "Node.js", true).unwrap(), "异步 IO"), "Node.js split consistently");
        assert!(has(&search(&conn, "TCP/IP", true).unwrap(), "四层网络模型"), "TCP/IP split consistently");
    }

    #[test]
    fn jieba_search_scoped_case11() {
        let path = tmp_path("jieba2");
        let (conn, _) = open_inits(&path);
        jieba_kb(&conn);
        // topic scope: only Redis topic rows
        let topic_scope = search_scoped(&conn, "Redis", None, Some("Redis"), true, 8).unwrap();
        assert!(!topic_scope.is_empty());
        assert!(topic_scope.iter().all(|x| x.topic == "Redis"), "topic scope enforced");
        // domain scope: only 数据库 rows
        let domain_scope = search_scoped(&conn, "MySQL", Some("数据库"), None, true, 8).unwrap();
        assert!(!domain_scope.is_empty());
        assert!(domain_scope.iter().all(|x| x.domain == "数据库"), "domain scope enforced");
        // scope must strictly exclude: BERT row is in 自然语言处理, not 后端开发
        let foreign = search_scoped(&conn, "BERT", Some("后端开发"), None, true, 8).unwrap();
        assert!(foreign.is_empty(), "BERT must not be recalled inside 后端开发 scope");
        let own = search_scoped(&conn, "BERT", Some("自然语言处理"), None, true, 8).unwrap();
        assert!(own.iter().any(|x| x.question.contains("BERT")), "in-scope BERT recalled");
    }

    #[test]
    fn migration_rebuilds_legacy_trigram_fts() {
        let path = tmp_path("migrate");
        let (conn, _) = open_inits(&path);
        // Insert a row whose content matches the search query below so the
        // rebuilt FTS can find it. (No demo seed anymore — the test builds
        // its own.)
        insert(&conn, "Redis 为什么单线程？", "Redis 核心命令使用单线程执行。", "后端开发", "Redis");
        let before: i64 = conn.query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0)).unwrap();
        assert!(before > 0);

        // Simulate a legacy install: a trigram-indexed FTS with RAW text.
        conn.execute_batch(
            "DROP TABLE IF EXISTS knowledge_fts;
             CREATE VIRTUAL TABLE knowledge_fts USING fts5(
                question, answer, domain, topic, tags, tokenize='trigram');",
        )
        .unwrap();
        let items = list(&conn).unwrap();
        for p in items {
            let kid: i64 = p.id.parse().unwrap();
            conn.execute(
                "INSERT INTO knowledge_fts(rowid,question,answer,domain,topic,tags) VALUES (?1,?2,?3,?4,?5,?6)",
                params![kid, p.question, p.answer, p.domain, p.topic, p.tags.join(" ")],
            )
            .unwrap();
        }

        // Re-running init must detect the legacy tokenizer and rebuild.
        assert!(init(&conn).unwrap());
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name='knowledge_fts'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("unicode61"), "FTS rebuilt onto unicode61: {ddl}");

        // Primary data + IDs untouched by the index rebuild.
        let after: i64 = conn.query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0)).unwrap();
        assert_eq!(after, before, "knowledge_items unchanged by migration");
        // Tokenized index actually works after rebuild.
        assert!(!search(&conn, "Redis", true).unwrap().is_empty());
    }

    #[test]
    fn fts_index_keeps_tokenchars() {
        let path = tmp_path("tokchars");
        let (conn, _) = open_inits(&path);
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name='knowledge_fts'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("tokenchars"), "FTS index keeps +/# tokens: {ddl}");
        // A single row containing both C++ and C# must match each distinctly.
        insert(&conn, "C++ 和 C# 的区别是什么？", "C++ 面向底层，C# 由微软推出。", "后端开发", "C++");
        let cpp = search(&conn, "C++", true).unwrap();
        assert!(cpp.iter().any(|x| x.question.contains("C++")));
        let cs = search(&conn, "C#", true).unwrap();
        assert!(cs.iter().any(|x| x.question.contains("C#")));
    }

    #[test]
    fn data_persists_across_reopen() {
        let path = tmp_path("persist");
        // first session
        let (conn, _) = open_inits(&path);
        let inserted = save(&conn, &payload("持久化测试问题")).unwrap();
        drop(conn);
        // second session reopens the same file
        let (conn2, _) = open_inits(&path);
        let items = list(&conn2).unwrap();
        assert!(items.iter().any(|x| x.question == "持久化测试问题"), "restarted app still sees inserted row");
        let _ = inserted;
    }
}
