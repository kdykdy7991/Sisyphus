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
///   2  -> cross-device sync columns: knowledge_items.sync_id, knowledge_items.deleted_at.
///         Backfills every pre-existing row with a fresh UUIDv4 sync_id (idempotent),
///         adds indexes that back both sync lookup and the soft-delete filter. The
///         v2 step is also safe to re-run when `user_version` has been tampered with:
///         the `has_column` check skips the ALTER TABLE when the columns already
///         exist, and the backfill is a no-op once every row carries a non-empty
///         sync_id.
///   3  -> UNIQUE constraint on sync_id (partial, non-empty only). The Sync
///         engine keys identity on sync_id, so the database must reject two
///         rows from sharing one. The partial predicate skips the (transient)
///         empty string that `save` uses to ask the server to mint one. This
///         makes "duplicate sync_id in the snapshot" fail loudly at INSERT
///         time instead of silently producing ambiguous merge results.
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
    if version < 2 {
        // Idempotent column additions: pre-check via PRAGMA table_info so a
        // re-run on a half-migrated DB (e.g. user_version reset to 0) is safe.
        if !has_column(conn, "knowledge_items", "sync_id")? {
            conn.execute_batch(
                "ALTER TABLE knowledge_items ADD COLUMN sync_id TEXT NOT NULL DEFAULT ''",
            )?;
        }
        if !has_column(conn, "knowledge_items", "deleted_at")? {
            conn.execute_batch("ALTER TABLE knowledge_items ADD COLUMN deleted_at TEXT")?;
        }
        // Backfill sync_id for every row missing one. The WHERE filter is the
        // idempotency guard: rows that already carry a non-empty sync_id are
        // left alone, so the second migration pass is a true no-op.
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM knowledge_items WHERE sync_id IS NULL OR sync_id = ''")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in &ids {
            let new_id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "UPDATE knowledge_items
                    SET sync_id = ?1
                  WHERE id = ?2
                    AND (sync_id IS NULL OR sync_id = '')",
                params![new_id, id],
            )?;
        }
        // sync_id lookup index for the merge engine; partial index speeds up the
        // "active rows" filter used by every read-side query.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_knowledge_sync_id  ON knowledge_items(sync_id);
             CREATE INDEX IF NOT EXISTS idx_knowledge_active   ON knowledge_items(deleted_at) WHERE deleted_at IS NULL;",
        )?;
        conn.pragma_update(None, "user_version", 2)?;
    }
    if version < 3 {
        // Partial UNIQUE on sync_id. Empty sync_id is a transient state for
        // `save()` payloads (the server mints one before INSERT); excluding
        // them keeps that path working while preventing two rows from
        // accidentally sharing a real sync_id.
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_knowledge_sync_id
                 ON knowledge_items(sync_id) WHERE sync_id != '';",
        )?;
        conn.pragma_update(None, "user_version", 3)?;
    }
    Ok(())
}

/// True when the named column exists on the named table. Used by the migration
/// path to keep column additions idempotent when `user_version` cannot be
/// trusted (manual edits, restored from an older backup, etc.).
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
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
    /// Stable, cross-device identity. UUIDv4 string; assigned by `save` when
    /// the caller leaves it empty. Once assigned, it is preserved across
    /// updates so the same row can be referenced from any device.
    #[serde(default)]
    pub sync_id: String,
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
    /// Soft-delete tombstone. When set, the row is hidden from the UI but
    /// retained on disk so a cross-device sync can still propagate the
    /// deletion to peers. `None` = active; `Some(ts)` = deleted at that time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// The SELECT (without ORDER/LIMIT) used everywhere to hydrate a payload,
/// joining each item's tags into a pipe-separated string. Includes the v2
/// sync columns (`sync_id`, `deleted_at`) so every read path is sync-aware
/// out of the box. Callers that need to see soft-deleted rows (the sync
/// engine) must build their own SELECT without the `deleted_at IS NULL`
/// filter — see `snapshot_active` / `snapshot_all` in sync.rs.
const ITEM_SELECT: &str = "SELECT k.id, k.question, k.answer, k.domain, k.topic,
        k.source, k.follow_ups, k.related_ids, k.favorite, k.created_at, k.updated_at, k.last_read_at,
        k.sync_id, k.deleted_at,
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
        sync_id: row.get::<_, String>(12)?,
        deleted_at: row.get::<_, Option<String>>(13)?,
        tags: row
            .get::<_, Option<String>>(14)?
            .map(|s| s.split('|').filter(|x| !x.is_empty()).map(String::from).collect())
            .unwrap_or_default(),
    })
}

/// All knowledge, newest first (stable for the browse page).
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let sql = format!(
        "{} WHERE k.deleted_at IS NULL ORDER BY k.created_at DESC, k.id DESC",
        ITEM_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| row_to_payload(r))?;
    rows.collect()
}

/// Fetch one item, recording its `last_read_at` as a side effect (detail view).
/// Soft-deleted items are returned as `None` and the side-effect update is
/// skipped — opening a deleted row must not silently extend its "read" history.
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<KnowledgePayload>> {
    let Some(kid) = parse_id(id) else { return Ok(None) };
    let now = now_ts();
    conn.execute(
        "UPDATE knowledge_items SET last_read_at = ?1
         WHERE id = ?2 AND deleted_at IS NULL",
        params![now, kid],
    )?;
    get_by_kid(conn, kid)
}

fn get_by_kid(conn: &Connection, kid: i64) -> rusqlite::Result<Option<KnowledgePayload>> {
    let sql = format!("{} WHERE k.id = ?1 AND k.deleted_at IS NULL", ITEM_SELECT);
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
        "{} WHERE k.deleted_at IS NULL
         ORDER BY (k.last_read_at IS NULL) ASC, k.last_read_at DESC, k.id DESC LIMIT ?1",
        ITEM_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![cap], |r| row_to_payload(r))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Sync-layer internal queries.
//
// These intentionally do NOT go through the UI's `list` / `get` / `search`
// paths: those hide soft-deleted rows AND have side effects (`get` bumps
// `last_read_at`). The Sync engine must see the full state including
// tombstones, and must not silently pollute read tracking.
//
// Every read here is a pure SELECT. Write paths in this section are
// invoked by `sync::apply_plan` and are responsible for keeping
// `knowledge_fts` consistent with the new `deleted_at` state.
// ---------------------------------------------------------------------------

/// Read every knowledge row, INCLUDING soft-deleted ones. Sync-layer only;
/// the UI-facing read paths (list / recent / search / get) still hide them.
/// No side effect on `last_read_at` and no FTS reads.
pub fn list_all_for_sync(conn: &Connection) -> rusqlite::Result<Vec<KnowledgePayload>> {
    let sql = format!("{} ORDER BY k.id", ITEM_SELECT);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| row_to_payload(r))?;
    rows.collect()
}

/// Look up a single row by its cross-device sync id, including tombstoned
/// rows. Sync-layer only. Returns `None` when the sync id is unknown.
pub fn get_by_sync_id_including_deleted(
    conn: &Connection,
    sync_id: &str,
) -> rusqlite::Result<Option<KnowledgePayload>> {
    let sql = format!("{} WHERE k.sync_id = ?1", ITEM_SELECT);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![sync_id], |r| row_to_payload(r))?;
    rows.next().transpose()
}

/// Soft-delete a row by its sync id, recording `deleted_at` and dropping
/// its FTS row. The actual `knowledge_items` row is preserved (the tombstone
/// is what travels to peer devices through the next snapshot). Returns
/// `true` when a row was actually transitioned active → tombstoned on this
/// call; `false` when the row was already tombstoned or absent.
pub fn soft_delete_by_sync_id(
    conn: &Connection,
    sync_id: &str,
    deleted_at: &str,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE knowledge_items SET deleted_at = ?1
         WHERE sync_id = ?2 AND deleted_at IS NULL",
        params![deleted_at, sync_id],
    )?;
    if changed > 0 {
        let kid: Option<i64> = conn
            .query_row(
                "SELECT id FROM knowledge_items WHERE sync_id = ?1",
                params![sync_id],
                |r| r.get(0),
            )
            .ok();
        if let Some(kid) = kid {
            conn.execute("DELETE FROM knowledge_fts WHERE rowid = ?1", params![kid])?;
        }
    }
    Ok(changed > 0)
}

/// Insert a new row from a Sync snapshot. Skips the UI's `save` path
/// because the Sync engine writes pre-merged state (and is the one place
/// that may legitimately insert an already-tombstoned row). Refuses empty
/// / whitespace / non-UUID sync_id: the wire format is UUID-only, and
/// the partial UNIQUE index on `sync_id` would reject a non-conforming
/// value at INSERT time anyway. Failing here gives a cleaner error
/// before the snapshot hits the SQL layer.
/// On success the FTS index is kept in sync with `deleted_at`.
pub fn insert_from_sync(
    conn: &Connection,
    item: &KnowledgePayload,
) -> rusqlite::Result<i64> {
    let sync_id = item.sync_id.trim();
    if sync_id.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if uuid::Uuid::parse_str(sync_id).is_err() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let favorite = i64::from(item.favorite.unwrap_or(false));
    conn.execute(
        "INSERT INTO knowledge_items
            (question, answer, domain, topic, source, follow_ups, related_ids, favorite,
             created_at, updated_at, last_read_at, sync_id, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
            sync_id,
            item.deleted_at,
        ],
    )?;
    let kid = conn.last_insert_rowid();
    sync_topics(conn, &item.domain, &item.topic)?;
    sync_tags(conn, kid, &item.tags)?;
    sync_fts(conn, kid, item)?;
    Ok(kid)
}

/// Update an existing row from a Sync snapshot. Looks the row up by
/// `sync_id` and overwrites content fields, FTS and tag set. Returns
/// `false` when the local row is already tombstoned (a Sync engine
/// "update" must never resurrect — see plan_merge's DeletionSticky rule);
/// in that case the local tombstone wins. Local numeric id and
/// `created_at` are preserved.
pub fn update_from_sync(
    conn: &Connection,
    sync_id: &str,
    item: &KnowledgePayload,
) -> rusqlite::Result<bool> {
    let trimmed = sync_id.trim();
    if trimmed.is_empty() || uuid::Uuid::parse_str(trimmed).is_err() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let (kid, current_deleted_at): (i64, Option<String>) = conn
        .query_row(
            "SELECT id, deleted_at FROM knowledge_items WHERE sync_id = ?1",
            params![trimmed],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
    if current_deleted_at.is_some() {
        // Sticky tombstone: refuse to resurrect. Caller (Sync engine) is
        // responsible for not planning this branch; this is a defensive guard.
        return Ok(false);
    }
    let favorite = i64::from(item.favorite.unwrap_or(false));
    conn.execute(
        "UPDATE knowledge_items SET
            question=?1, answer=?2, domain=?3, topic=?4,
            source=?5, follow_ups=?6, related_ids=?7, favorite=?8,
            updated_at=?9, last_read_at=?10, deleted_at=?11
         WHERE id = ?12",
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
            item.deleted_at,
            kid,
        ],
    )?;
    sync_topics(conn, &item.domain, &item.topic)?;
    sync_tags(conn, kid, &item.tags)?;
    sync_fts(conn, kid, item)?;
    Ok(true)
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
///
/// Sync-id rules:
///   * On insert: if the caller's `sync_id` is empty, a fresh UUIDv4 is
///     generated and persisted. Otherwise the caller's value is kept verbatim.
///   * On update: the existing `sync_id` is always preserved (a stable
///     identity cannot be silently swapped by a re-save with a blank field).
///     This is the property the cross-device merge engine relies on.
fn save_internal(conn: &Connection, item: &KnowledgePayload) -> rusqlite::Result<KnowledgePayload> {
    let favorite = i64::from(item.favorite.unwrap_or(false));
    let exist_id = parse_id(&item.id);
    let (kid, persisted_sync_id) = if let Some(exist) = exist_id {
        let found: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, sync_id FROM knowledge_items WHERE id = ?1",
                params![exist],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((found_id, existing_sync)) = found {
            // Update: the row's sync_id is immutable. If the caller passed an
            // empty sync_id, that's "I don't have one" — use the stored one.
            // If the caller passed a different sync_id, that's a programmer
            // error (the row already has one). We keep the stored value.
            let keep_sync = if item.sync_id.trim().is_empty() {
                existing_sync.clone()
            } else if item.sync_id != existing_sync {
                // Conflict on a known id: prefer the stored value (caller
                // cannot change identity through save). This keeps update
                // semantics predictable.
                existing_sync.clone()
            } else {
                item.sync_id.clone()
            };
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
                    found_id,
                ],
            )?;
            (found_id, keep_sync)
        } else {
            let new_sync = if item.sync_id.trim().is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                item.sync_id.clone()
            };
            let new_id = insert_row(conn, item, favorite, &new_sync)?;
            (new_id, new_sync)
        }
    } else {
        let new_sync = if item.sync_id.trim().is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            item.sync_id.clone()
        };
        let new_id = insert_row(conn, item, favorite, &new_sync)?;
        (new_id, new_sync)
    };

    sync_topics(conn, &item.domain, &item.topic)?;
    sync_tags(conn, kid, &item.tags)?;
    sync_fts(conn, kid, item)?;
    // Re-read so the returned payload carries the canonical sync_id and id.
    let persisted = get_by_kid(conn, kid)?.ok_or_else(|| rusqlite::Error::InvalidQuery)?;
    // If the row is soft-deleted, `get_by_kid` will return None and we cannot
    // hand back a payload — but `save` is the public "upsert" path that the
    // UI uses for active rows, so this branch is unreachable in practice.
    // We still want a value in the returned struct for type-system hygiene.
    let _ = persisted_sync_id;
    Ok(persisted)
}

fn insert_row(conn: &Connection, item: &KnowledgePayload, favorite: i64, sync_id: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO knowledge_items
            (question, answer, domain, topic, source, follow_ups, related_ids, favorite, created_at, updated_at, last_read_at, sync_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            sync_id,
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
///
/// Tombstoned rows (deleted_at IS NOT NULL) are NOT re-indexed: the FTS index
/// is a read-side accelerator for the active knowledge base, and a deleted
/// item must not surface in search results. The row is still kept on disk so
/// the Sync engine can still see the tombstone and propagate it.
fn sync_fts(conn: &Connection, kid: i64, item: &KnowledgePayload) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM knowledge_fts WHERE rowid = ?1", params![kid])?;
    if item.deleted_at.is_some() {
        return Ok(());
    }
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
         WHERE knowledge_fts MATCH ?1
           AND k.deleted_at IS NULL {scope}
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
            sync_id: String::new(),
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
            deleted_at: None,
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
                sync_id: String::new(),
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

    // -----------------------------------------------------------------
    // v2 schema: sync_id / deleted_at. Every Knowledge now carries a
    // stable cross-device identity and a tombstone flag.
    // -----------------------------------------------------------------

    /// Build a v1-only DB on disk and run the standard `migrate()` against it.
    /// The v2 step must add the columns and backfill every existing row with a
    /// non-empty sync_id, then bump `user_version` to 2.
    #[test]
    fn migrate_v2_adds_sync_id_and_deleted_at_to_v1_db() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-v1-only-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.db");
        let conn = open(&path).unwrap();
        // Force the v1 schema by hand (no sync_id, no deleted_at).
        conn.execute_batch(
            "CREATE TABLE topics (id INTEGER PRIMARY KEY AUTOINCREMENT, domain TEXT NOT NULL, topic TEXT NOT NULL, UNIQUE(domain, topic));
             CREATE TABLE tags   (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
             CREATE TABLE knowledge_items (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 question TEXT NOT NULL, answer TEXT NOT NULL,
                 domain TEXT NOT NULL, topic TEXT NOT NULL, source TEXT NOT NULL DEFAULT '',
                 follow_ups TEXT NOT NULL DEFAULT '[]', related_ids TEXT NOT NULL DEFAULT '[]',
                 favorite INTEGER NOT NULL DEFAULT 0,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_read_at TEXT
             );
             CREATE TABLE knowledge_tags (knowledge_id INTEGER NOT NULL, tag_id INTEGER NOT NULL, PRIMARY KEY (knowledge_id, tag_id));
             PRAGMA user_version = 1;",
        ).unwrap();
        // Three pre-existing rows: would be invisible to v2 code without backfill.
        for q in ["q1", "q2", "q3"] {
            conn.execute(
                "INSERT INTO knowledge_items (question, answer, domain, topic, created_at, updated_at)
                 VALUES (?1, 'a', 'd', 't', '2026-09-04', '2026-09-04')",
                [q],
            ).unwrap();
        }
        drop(conn);

        // Now run the standard open+init (which calls migrate()).
        let (conn2, _) = open_inits(&path);
        // Both columns exist.
        let cols: Vec<String> = conn2
            .prepare("SELECT name FROM pragma_table_info('knowledge_items')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert!(cols.iter().any(|c| c == "sync_id"), "sync_id column added: {:?}", cols);
        assert!(cols.iter().any(|c| c == "deleted_at"), "deleted_at column added: {:?}", cols);
        // Every pre-existing row now has a non-empty, unique sync_id.
        let n_total: i64 = conn2
            .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
            .unwrap();
        let n_synced: i64 = conn2
            .query_row(
                "SELECT count(*) FROM knowledge_items WHERE sync_id != ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_total, 3);
        assert_eq!(n_synced, 3, "every pre-existing row was backfilled");
        let n_unique: i64 = conn2
            .query_row(
                "SELECT count(DISTINCT sync_id) FROM knowledge_items",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_unique, 3, "backfilled sync_ids are unique");
        // user_version reached the current latest (the v2 step ran, then the
        // v3 step added the unique index — both are part of the same
        // idempotent open()).
        let v: i64 = conn2
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert!(v >= 2, "user_version advanced past v2: got {v}");
        let _ = fs::remove_file(&path);
        Ok(())
    }

    /// Re-running `migrate()` on an already-v2 DB must be a true no-op. The
    /// columns still exist (we don't drop them), the row count is unchanged,
    /// and the existing sync_ids are not regenerated. The exact user_version
    /// is the latest (>= 2); we don't pin a number here so future patches
    /// to the migration ladder don't have to touch this assertion.
    #[test]
    fn migrate_v2_is_idempotent() {
        let path = tmp_path("v2-idem");
        let (conn, _) = open_inits(&path);
        let inserted = save(&conn, &payload("幂等测试")).unwrap();
        let original_sync = inserted.sync_id.clone();
        assert!(!original_sync.is_empty());

        // Re-run migrate. Public API: just call `migrate` through a fresh
        // open so we exercise the same path the app would on next launch.
        drop(conn);
        let (conn2, _) = open_inits(&path);
        let again = list(&conn2).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].sync_id, original_sync, "sync_id preserved across re-migrate");
        let v: i64 = conn2
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert!(v >= 2, "user_version remained at the latest: got {v}");
    }

    /// save() with no sync_id on the payload must assign one; two saves must
    /// produce two distinct sync_ids. A subsequent save reusing the first
    /// payload's sync_id but a different numeric id must still preserve the
    /// stored sync_id (immutable identity through upsert).
    #[test]
    fn save_assigns_and_preserves_sync_id() {
        let path = tmp_path("syncid-assign");
        let (conn, _) = open_inits(&path);
        let a = save(&conn, &payload("问题 A")).unwrap();
        let b = save(&conn, &payload("问题 B")).unwrap();
        assert!(!a.sync_id.is_empty(), "save auto-generates sync_id");
        assert!(!b.sync_id.is_empty(), "save auto-generates sync_id");
        assert_ne!(a.sync_id, b.sync_id, "two saves produce two distinct sync_ids");
        assert!(uuid::Uuid::parse_str(&a.sync_id).is_ok(), "sync_id is a valid UUID");
        assert!(uuid::Uuid::parse_str(&b.sync_id).is_ok(), "sync_id is a valid UUID");

        // Update A: same numeric id, same sync_id preserved.
        let mut upd = a.clone();
        upd.question = "问题 A（已修订）".to_string();
        let saved = save(&conn, &upd).unwrap();
        assert_eq!(saved.id, a.id, "same numeric id on update");
        assert_eq!(saved.sync_id, a.sync_id, "sync_id survives update");
    }

    /// Every read path must skip rows whose `deleted_at` is set. This is the
    /// single source of truth for "user no longer sees this" — the home page,
    /// search, detail (get), and the recent list all share the same filter.
    #[test]
    fn read_paths_hide_soft_deleted_rows() {
        let path = tmp_path("soft-delete");
        let (conn, _) = open_inits(&path);
        let active = save(&conn, &payload("仍可见的知识")).unwrap();
        let doomed = save(&conn, &payload("应被隐藏的知识")).unwrap();

        // Mark the second row as soft-deleted directly (Task 2 will add a
        // dedicated command; for the schema test we just want to verify the
        // filter behavior end-to-end).
        conn.execute(
            "UPDATE knowledge_items SET deleted_at = '1700000000' WHERE id = ?1",
            params![doomed.id.parse::<i64>().unwrap()],
        )
        .unwrap();

        // list() — the active row is the only one visible.
        let listed = list(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, active.id);

        // recent() — same filter.
        let rec = recent(&conn, 10).unwrap();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].id, active.id);

        // get() — returns None for the deleted row and does NOT bump its
        // last_read_at (the side-effect update is gated on deleted_at IS NULL).
        let got = get(&conn, &doomed.id).unwrap();
        assert!(got.is_none(), "get() hides soft-deleted rows");
        let after_ts: Option<String> = conn
            .query_row(
                "SELECT last_read_at FROM knowledge_items WHERE id = ?1",
                params![doomed.id.parse::<i64>().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(after_ts.is_none(), "soft-deleted row's last_read_at not bumped");

        // search() — FTS-based retrieval must also filter the deleted row out.
        let hits = search(&conn, "隐藏", true).unwrap();
        assert!(hits.is_empty(), "search hides soft-deleted rows");
    }

    // -----------------------------------------------------------------
    // v3 schema: UNIQUE constraint on sync_id.
    // -----------------------------------------------------------------

    /// v3 must add a partial UNIQUE index on `sync_id`. Two rows cannot share
    /// the same non-empty sync_id, but the empty-string sentinel used by
    /// `save()` to ask the server to mint one must remain valid.
    #[test]
    fn migrate_v3_enforces_unique_sync_id() {
        let path = tmp_path("v3-unique");
        let (conn, _) = open_inits(&path);
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3, "user_version reached 3");

        let a = save(&conn, &payload("唯一约束 A")).unwrap();
        // Manually insert a row with a *different* sync_id — must succeed.
        let mut b_payload = payload("唯一约束 B");
        b_payload.sync_id = "manual-sync-id-b".to_string();
        save(&conn, &b_payload).unwrap();

        // Inserting a row with a *duplicate* sync_id must fail at the DB.
        let dup_result = conn.execute(
            "INSERT INTO knowledge_items
                (question, answer, domain, topic, source, follow_ups, related_ids, favorite,
                 created_at, updated_at, last_read_at, sync_id)
             VALUES ('dup', 'a', 'd', 't', '', '[]', '[]', 0, '', '', NULL, ?1)",
            params![a.sync_id],
        );
        assert!(dup_result.is_err(), "duplicate sync_id must be rejected");

        // Empty sync_id is still allowed (the partial index skips '').
        let empty_result = conn.execute(
            "INSERT INTO knowledge_items
                (question, answer, domain, topic, source, follow_ups, related_ids, favorite,
                 created_at, updated_at, last_read_at, sync_id)
             VALUES ('empty', 'a', 'd', 't', '', '[]', '[]', 0, '', '', NULL, '')",
            [],
        );
        assert!(empty_result.is_ok(), "empty sync_id remains allowed (partial index predicate)");
    }

    // -----------------------------------------------------------------
    // Sync-layer internal queries.
    // -----------------------------------------------------------------

    /// `list_all_for_sync` must include tombstoned rows that every UI-facing
    /// read path hides. It must also not bump `last_read_at` on any row
    /// (no side effects). This is the property the Sync engine relies on:
    /// if it ever calls the UI's `list`, it will silently miss deletions.
    #[test]
    fn list_all_for_sync_includes_tombstones() {
        let path = tmp_path("sync-list-all");
        let (conn, _) = open_inits(&path);
        let active = save(&conn, &payload("仍可见")).unwrap();
        let doomed = save(&conn, &payload("应被隐藏但同步层必须看见")).unwrap();

        // Mark the second row tombstoned directly.
        conn.execute(
            "UPDATE knowledge_items SET deleted_at = '1700000000' WHERE id = ?1",
            params![doomed.id.parse::<i64>().unwrap()],
        )
        .unwrap();

        // The UI sees one row; the sync layer sees two.
        assert_eq!(list(&conn).unwrap().len(), 1);
        let sync_view = list_all_for_sync(&conn).unwrap();
        assert_eq!(sync_view.len(), 2);
        let tombstoned = sync_view
            .iter()
            .find(|p| p.sync_id == doomed.sync_id)
            .expect("tombstoned row present in sync view");
        assert_eq!(tombstoned.deleted_at.as_deref(), Some("1700000000"));

        // The sync-layer list does not bump last_read_at.
        let before_active: Option<String> = conn
            .query_row(
                "SELECT last_read_at FROM knowledge_items WHERE id = ?1",
                params![active.id.parse::<i64>().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let _ = list_all_for_sync(&conn).unwrap();
        let after_active: Option<String> = conn
            .query_row(
                "SELECT last_read_at FROM knowledge_items WHERE id = ?1",
                params![active.id.parse::<i64>().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before_active, after_active, "list_all_for_sync has no side effect");
    }

    /// `get_by_sync_id_including_deleted` finds a row regardless of its
    /// tombstone state. The UI's `get` would return None — sync must not.
    #[test]
    fn get_by_sync_id_including_deleted_finds_tombstoned_rows() {
        let path = tmp_path("sync-get-incl");
        let (conn, _) = open_inits(&path);
        let a = save(&conn, &payload("active")).unwrap();
        let b = save(&conn, &payload("to-be-deleted")).unwrap();
        assert!(soft_delete_by_sync_id(&conn, &b.sync_id, "1700000000").unwrap());
        // soft-delete returned true on the transition; calling again is a no-op.
        assert!(!soft_delete_by_sync_id(&conn, &b.sync_id, "1700000001").unwrap());

        let active_lookup =
            get_by_sync_id_including_deleted(&conn, &a.sync_id).unwrap().unwrap();
        assert_eq!(active_lookup.sync_id, a.sync_id);
        assert!(active_lookup.deleted_at.is_none());

        let tombstone_lookup =
            get_by_sync_id_including_deleted(&conn, &b.sync_id).unwrap().unwrap();
        assert_eq!(tombstone_lookup.sync_id, b.sync_id);
        assert_eq!(tombstone_lookup.deleted_at.as_deref(), Some("1700000000"));

        let unknown = get_by_sync_id_including_deleted(&conn, "no-such-sync-id").unwrap();
        assert!(unknown.is_none());
    }

    /// `soft_delete_by_sync_id` must remove the FTS row in lockstep with
    /// setting `deleted_at`. Otherwise a soft-deleted item would still be
    /// returned by `search` (the FTS query already filters active rows,
    /// but the explicit FTS-clearing step is the source of truth).
    #[test]
    fn soft_delete_by_sync_id_clears_fts() {
        let path = tmp_path("sync-soft-del-fts");
        let (conn, _) = open_inits(&path);
        let p = save(&conn, &payload("Redis 单线程")).unwrap();
        // FTS row exists pre-delete.
        let fts_before: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE rowid = ?1",
                params![p.id.parse::<i64>().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_before, 1, "FTS row exists before delete");

        soft_delete_by_sync_id(&conn, &p.sync_id, "1700000000").unwrap();

        let fts_after: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE rowid = ?1",
                params![p.id.parse::<i64>().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_after, 0, "FTS row removed on soft delete");
    }

    /// `insert_from_sync` and `update_from_sync` must keep FTS in lockstep
    /// with `deleted_at`. A tombstoned row inserted from sync must not be
    /// findable via `search`; an updated row that arrives tombstoned must
    /// be removed from FTS too.
    #[test]
    fn insert_and_update_from_sync_manage_fts_correctly() {
        let path = tmp_path("sync-fsync-fts");
        let (conn, _) = open_inits(&path);

        // Build a tombstoned item from the wire-format and insert.
        let tomb_sync_id = "55555555-5555-5555-5555-555555555555";
        let active_sync_id = "66666666-6666-6666-6666-666666666666";
        let mut tomb = payload("来自同步的墓碑条目");
        tomb.sync_id = tomb_sync_id.to_string();
        tomb.deleted_at = Some("1700000000".to_string());
        let kid = insert_from_sync(&conn, &tomb).unwrap();
        let fts: i64 = conn
            .query_row("SELECT count(*) FROM knowledge_fts WHERE rowid = ?1", params![kid], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, 0, "tombstoned snapshot item does NOT enter FTS");

        // The row is still on disk, queryable by sync_id.
        let from_disk = get_by_sync_id_including_deleted(&conn, tomb_sync_id).unwrap().unwrap();
        assert_eq!(from_disk.deleted_at.as_deref(), Some("1700000000"));

        // Insert an active item from sync and verify FTS picks it up.
        let mut active = payload("来自同步的活跃条目");
        active.sync_id = active_sync_id.to_string();
        let kid_a = insert_from_sync(&conn, &active).unwrap();
        let hits = search(&conn, "来自同步", true).unwrap();
        assert!(hits.iter().any(|x| x.sync_id == active_sync_id),
                "active sync-inserted item is in FTS");

        // Update the active item to a tombstone via update_from_sync — FTS
        // must drop the row, even though update_from_sync was used.
        let mut t = payload("现在变成墓碑");
        t.sync_id = active_sync_id.to_string();
        t.deleted_at = Some("1700000001".to_string());
        update_from_sync(&conn, active_sync_id, &t).unwrap();
        let fts2: i64 = conn
            .query_row("SELECT count(*) FROM knowledge_fts WHERE rowid = ?1", params![kid_a], |r| r.get(0))
            .unwrap();
        assert_eq!(fts2, 0, "updated-to-tombstone row removed from FTS");
    }

    /// `update_from_sync` must refuse to resurrect a tombstoned row. The
    /// local `deleted_at` is sticky once set, even when an incoming
    /// snapshot still thinks the item is active.
    #[test]
    fn update_from_sync_cannot_resurrect_tombstone() {
        let path = tmp_path("sync-no-resurr");
        let (conn, _) = open_inits(&path);
        let p = save(&conn, &payload("会被墓碑化")).unwrap();
        soft_delete_by_sync_id(&conn, &p.sync_id, "1700000000").unwrap();

        // Build an "active" payload with the same sync_id and try to
        // apply it. update_from_sync must return Ok(false) (refused to
        // resurrect); the local row must stay tombstoned and its content
        // must be untouched.
        let mut reactivating = payload("尝试复活");
        reactivating.sync_id = p.sync_id.clone();
        reactivating.deleted_at = None;
        let result = update_from_sync(&conn, &p.sync_id, &reactivating);
        assert!(result.is_ok(), "update_from_sync returns Ok, not an error");
        assert!(!result.unwrap(),
                "update_from_sync returns Ok(false) for tombstoned local rows");
        let row = get_by_sync_id_including_deleted(&conn, &p.sync_id).unwrap().unwrap();
        assert_eq!(row.deleted_at.as_deref(), Some("1700000000"),
                   "tombstone preserved; resurrection rejected");
        assert_eq!(row.question, "会被墓碑化",
                   "content of the tombstoned row was not overwritten");
    }

    /// `insert_from_sync` must refuse an empty sync_id — the Sync engine
    /// is required to mint a UUID up front; the server never invents one
    /// for an incoming wire-format row.
    #[test]
    fn insert_from_sync_rejects_empty_sync_id() {
        let path = tmp_path("sync-reject-empty");
        let (conn, _) = open_inits(&path);
        let mut bad = payload("empty sync_id");
        bad.sync_id = String::new();
        let res = insert_from_sync(&conn, &bad);
        assert!(res.is_err(), "empty sync_id rejected");
    }
}
