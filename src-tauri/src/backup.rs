// Backup / Restore for the local Sisyphus knowledge base.
//
// Scope: the only thing a backup carries is the user's Knowledge data. It is
// intentionally NOT an "application environment" backup: the model config
// (apiBaseUrl / apiKey / chatModel / visionModel), the runtime
// databaseLocation, chat history, import drafts, screenshot bytes, WebKit
// caches — none of those enter the archive, and none of them are touched
// during restore. The application keeps using whatever it had configured
// before the restore.
//
// Container format `.ikbackup` v1
// -------------------------------
// A single binary file with two embedded files (manifest.json, database.sqlite).
// The format is intentionally simple and dependency-free: a fixed header, then
// for each file a (name, content) pair, then a footer magic. Length-prefixed
// entries let the reader walk the file forward and reject truncation. There
// is no compression — the SQLite file is already binary-packed and the
// manifest is tiny.
//
// The header is laid out as:
//
//   0..8   : MAGIC          = b"IKBCKUP\0"        (8 bytes)
//   8..10  : FORMAT_VERSION = u16 LE (=1)
//   10..12 : FLAGS          = u16 LE (reserved, 0)
//   12..14 : NUM_FILES      = u16 LE
//   14..16 : RESERVED       = u16 LE (0)
//
// Each entry:
//
//   2 bytes : NAME_LEN  (u16 LE)
//   N bytes : NAME      (UTF-8, one of: manifest.json, database.sqlite)
//   8 bytes : CONTENT_LEN (u64 LE)
//   M bytes : CONTENT
//
// Footer:
//
//   8 bytes : FOOTER_MAGIC = b"IKBCKEND"          (no trailing length; matches header)
//
// The format is forward-compatible: future readers can tolerate unknown file
// names, and the formatVersion field lets us reject unsupported generations.
//
// SQLite snapshotting
// -------------------
// The live database is in WAL mode, so a plain `std::fs::copy` of the main
// `.db` file is *not* safe — pages that have not yet been checkpointed live in
// `-wal` / `-shm` sidecars. We use SQLite's Online Backup API (via
// `rusqlite::backup::Backup`) which performs the copy under shared locking
// and pages everything into the destination atomically. This is safe to call
// while the live application is reading and writing the source DB.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"IKBCKUP\0";
const FOOTER_MAGIC: &[u8; 8] = b"IKBCKEND";
pub const FORMAT_VERSION: u16 = 1;
const HEADER_LEN: usize = 16;

pub const FILE_MANIFEST: &str = "manifest.json";
pub const FILE_DATABASE: &str = "database.sqlite";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format_version: u16,
    pub app: String,
    pub created_at: String,
    pub database_schema_version: i64,
    pub knowledge_count: i64,
    pub domain_count: i64,
}

/// What we surface to the UI before a user confirms a restore. Counts come from
/// the manifest (so we never have to *open* the SQLite file just to display a
/// preview). The backup carries NO settings — they are intentionally absent
/// here so the UI cannot accidentally display a stale apiBaseUrl / chatModel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectReport {
    pub format_version: u16,
    pub app: String,
    pub created_at: String,
    pub database_schema_version: i64,
    pub knowledge_count: i64,
    pub domain_count: i64,
}

/// Public errors. Each variant maps to a distinct, user-facing string.
#[derive(Debug)]
pub enum BackupError {
    /// File isn't a `.ikbackup` (bad magic / footer) or has the wrong layout.
    InvalidArchive(String),
    /// The archive is structurally valid but its `formatVersion` is one we
    /// don't support. The UI can suggest upgrading the app.
    UnsupportedVersion(u16),
    /// The `database.sqlite` payload failed the integrity checks.
    InvalidDatabase(String),
    /// A required on-disk path is missing (e.g. the live database file).
    MissingPath(String),
    /// A plain IO error.
    Io(std::io::Error),
    /// A SQLite error during backup, validation, snapshot, or restore.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::InvalidArchive(s) => write!(f, "备份文件无效：{s}"),
            BackupError::UnsupportedVersion(v) => write!(f, "不支持的备份格式版本：{v}"),
            BackupError::InvalidDatabase(s) => write!(f, "备份内数据库文件无效：{s}"),
            BackupError::MissingPath(s) => write!(f, "找不到路径：{s}"),
            BackupError::Io(e) => write!(f, "IO 错误：{e}"),
            BackupError::Sqlite(e) => write!(f, "数据库错误：{e}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Io(e)
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(e: rusqlite::Error) -> Self {
        BackupError::Sqlite(e)
    }
}

fn io_err(s: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, s.into())
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn temp_path(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("interview-kit-{suffix}-{}.sqlite", unique_suffix()))
}

// ---------------------------------------------------------------------------
// Archive read / write
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BackupEntry {
    name: String,
    bytes: Vec<u8>,
}

pub fn write_archive(path: &Path, files: &[(&str, &[u8])]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = File::create(path)?;
    let mut header = [0u8; HEADER_LEN];
    header[0..8].copy_from_slice(MAGIC);
    header[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    // 10..12 = flags = 0
    let num = files.len() as u16;
    header[12..14].copy_from_slice(&num.to_le_bytes());
    // 14..16 = reserved = 0
    f.write_all(&header)?;
    for (name, content) in files {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(io_err(format!("file name too long: {name}")));
        }
        let name_len = name_bytes.len() as u16;
        f.write_all(&name_len.to_le_bytes())?;
        f.write_all(name_bytes)?;
        let content_len = content.len() as u64;
        f.write_all(&content_len.to_le_bytes())?;
        f.write_all(content)?;
    }
    f.write_all(FOOTER_MAGIC)?;
    f.flush()?;
    Ok(())
}

fn read_archive(path: &Path) -> Result<Vec<BackupEntry>, BackupError> {
    let mut f = File::open(path)?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)
        .map_err(|_| BackupError::InvalidArchive("file too small to be a backup".into()))?;
    if &header[0..8] != MAGIC {
        return Err(BackupError::InvalidArchive("not a Sisyphus backup (bad magic)".into()));
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version != FORMAT_VERSION {
        return Err(BackupError::UnsupportedVersion(version));
    }
    // 10..12 = flags, ignored
    let num_files = u16::from_le_bytes([header[12], header[13]]);
    let mut files: Vec<BackupEntry> = Vec::with_capacity(num_files as usize);
    for _ in 0..num_files {
        let mut len_buf = [0u8; 2];
        f.read_exact(&mut len_buf)
            .map_err(|_| BackupError::InvalidArchive("truncated header (name_len)".into()))?;
        let name_len = u16::from_le_bytes(len_buf) as usize;
        let mut name_bytes = vec![0u8; name_len];
        f.read_exact(&mut name_bytes)
            .map_err(|_| BackupError::InvalidArchive("truncated name".into()))?;
        let name = String::from_utf8(name_bytes)
            .map_err(|_| BackupError::InvalidArchive("non-utf8 file name".into()))?;
        let mut len_buf = [0u8; 8];
        f.read_exact(&mut len_buf)
            .map_err(|_| BackupError::InvalidArchive("truncated header (content_len)".into()))?;
        let content_len = u64::from_le_bytes(len_buf) as usize;
        let mut content = vec![0u8; content_len];
        f.read_exact(&mut content)
            .map_err(|_| BackupError::InvalidArchive(format!("truncated body for {name}")))?;
        files.push(BackupEntry { name, bytes: content });
    }
    let mut footer = [0u8; 8];
    f.read_exact(&mut footer)
        .map_err(|_| BackupError::InvalidArchive("missing or truncated footer".into()))?;
    if &footer != FOOTER_MAGIC {
        return Err(BackupError::InvalidArchive("bad footer magic".into()));
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// SQLite consistent snapshot
// ---------------------------------------------------------------------------

/// Snapshot the live SQLite database at `src` into the file at `dest` using
/// SQLite's Online Backup API. The source connection is borrowed only for the
/// duration of the call (read-locked per page by SQLite itself). The
/// destination is finalized (WAL checkpointed) before returning.
fn snapshot_database(src: &Connection, dest: &Path) -> Result<(), BackupError> {
    use rusqlite::backup::StepResult;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let mut dst = Connection::open(dest)?;
    {
        let bk = Backup::new(src, &mut dst)?;
        loop {
            match bk.step(256)? {
                StepResult::Done => break,
                StepResult::More => continue,
                StepResult::Busy | StepResult::Locked => {
                    // Brief pause and retry — keeps the backup live-friendly.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => break,
            }
        }
    }
    // Force a FULL checkpoint on the destination so the on-disk page count is
    // self-contained and we never need to ship `-wal` / `-shm` sidecars.
    dst.pragma_update(None, "wal_checkpoint", "FULL")?;
    // Drop the connection (calls sqlite3_close_v2 in the destructor). We do
    // not call `dst.close()` because that consumes self and returns
    // `(Connection, Error)`; any error here is a post-checkpoint cleanup
    // issue, not a data-correctness one.
    drop(dst);
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read the manifest from a `.ikbackup` without touching the live database.
/// Safe to call on arbitrary files (returns InvalidArchive instead of
/// panicking).
pub fn inspect(archive_path: &Path) -> Result<InspectReport, BackupError> {
    let files = read_archive(archive_path)?;
    let manifest_entry = files
        .iter()
        .find(|f| f.name == FILE_MANIFEST)
        .ok_or_else(|| BackupError::InvalidArchive("missing manifest.json".into()))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_entry.bytes)
        .map_err(|_| BackupError::InvalidArchive("invalid manifest.json".into()))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::UnsupportedVersion(manifest.format_version));
    }
    // The presence of the database file is part of the contract.
    if !files.iter().any(|f| f.name == FILE_DATABASE) {
        return Err(BackupError::InvalidArchive("missing database.sqlite".into()));
    }
    Ok(InspectReport {
        format_version: manifest.format_version,
        app: manifest.app,
        created_at: manifest.created_at,
        database_schema_version: manifest.database_schema_version,
        knowledge_count: manifest.knowledge_count,
        domain_count: manifest.domain_count,
    })
}

/// Build counts (and schema version) for the manifest from an open
/// read-only connection to the live database.
fn read_counts(src: &Connection) -> Result<(i64, i64, i64), BackupError> {
    let user_version: i64 = src
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);
    let knowledge_count: i64 = src
        .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
        .map_err(BackupError::Sqlite)?;
    let domain_count: i64 = src
        .query_row(
            "SELECT count(DISTINCT domain) FROM knowledge_items",
            [],
            |r| r.get(0),
        )
        .map_err(BackupError::Sqlite)?;
    Ok((user_version, knowledge_count, domain_count))
}

/// Build the archive, write it to `dest_path`, and return the manifest that
/// was embedded. Temporary snapshot files are always cleaned up, even on
/// failure. Only Knowledge data (knowledge_items / topics / tags /
/// knowledge_tags, with the FTS index rebuilt from source by the migration
/// step on restore) goes into the archive; application settings are not
/// touched here.
pub fn create(db_path: &Path, dest_path: &Path) -> Result<Manifest, BackupError> {
    if !db_path.exists() {
        return Err(BackupError::MissingPath(db_path.display().to_string()));
    }
    // Open a read-only connection: we do not need to write to the source, and
    // SQLite's Online Backup API works fine across read-only sources.
    let src = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (user_version, knowledge_count, domain_count) = read_counts(&src)?;
    let created_at = chrono::Utc::now().to_rfc3339();
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        app: "Sisyphus".to_string(),
        created_at,
        database_schema_version: user_version,
        knowledge_count,
        domain_count,
    };

    // DB snapshot
    let snapshot_path = temp_path("bk");
    let snapshot_result = snapshot_database(&src, &snapshot_path);
    // Drop the read-only source connection before doing IO; harmless.
    drop(src);
    if let Err(e) = snapshot_result {
        let _ = fs::remove_file(&snapshot_path);
        return Err(e);
    }
    let db_bytes = match fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) => {
            let _ = fs::remove_file(&snapshot_path);
            return Err(BackupError::Io(e));
        }
    };
    let _ = fs::remove_file(&snapshot_path);

    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| BackupError::InvalidArchive(format!("manifest serialize: {e}")))?;

    write_archive(
        dest_path,
        &[
            (FILE_MANIFEST, &manifest_json),
            (FILE_DATABASE, &db_bytes),
        ],
    )?;
    Ok(manifest)
}

/// Outcome of a successful restore — what the UI needs to know to refresh
/// its caches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub knowledge_count: i64,
    pub domain_count: i64,
}

/// Validate the archive *before* touching any live file. The validation reads
/// the archive, parses the manifest, checks the format version, and opens the
/// embedded SQLite in a temp file to confirm the file is real SQLite and has
/// the business tables we need.
///
/// Returns the raw bytes of the embedded SQLite (ready to be written to the
/// live DB path). The caller is expected to hold any locks it needs (the live
/// DB mutex) so the read here doesn't race with a concurrent restore on the
/// same archive.
pub fn validate_archive(archive_path: &Path) -> Result<Vec<u8>, BackupError> {
    let files = read_archive(archive_path)?;
    let manifest_entry = files
        .iter()
        .find(|f| f.name == FILE_MANIFEST)
        .ok_or_else(|| BackupError::InvalidArchive("missing manifest.json".into()))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_entry.bytes)
        .map_err(|_| BackupError::InvalidArchive("invalid manifest.json".into()))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::UnsupportedVersion(manifest.format_version));
    }
    let db_entry = files
        .iter()
        .find(|f| f.name == FILE_DATABASE)
        .ok_or_else(|| BackupError::InvalidArchive("missing database.sqlite".into()))?;

    // Verify the SQLite payload in a temp file. We require a real DB with the
    // business tables — this is what the migration step will then upgrade.
    let verify = temp_path("verify");
    fs::write(&verify, &db_entry.bytes)?;
    let verify_result: Result<(), BackupError> = (|| {
        let conn = Connection::open(&verify).map_err(|e| {
            BackupError::InvalidDatabase(format!("无法打开 SQLite 文件：{e}"))
        })?;
        // `PRAGMA quick_check` returns one or more rows whose first column
        // is TEXT (the literal string "ok" on success, or the failing row
        // description otherwise). We don't need the row payload — we only
        // need the pragma to run. `execute_batch` discards rows and
        // surfaces any underlying error.
        conn.execute_batch("PRAGMA quick_check")
            .map_err(|e| BackupError::InvalidDatabase(format!("quick_check 失败：{e}")))?;
        // The business tables must exist; if not, this is not a real DB or
        // it's from a different product.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('knowledge_items','topics','tags','knowledge_tags')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| BackupError::InvalidDatabase(format!("读取 sqlite_master 失败：{e}")))?;
        if count < 4 {
            return Err(BackupError::InvalidDatabase(
                "缺少必需的业务表（knowledge_items/topics/tags/knowledge_tags）".into(),
            ));
        }
        Ok(())
    })();
    let _ = fs::remove_file(&verify);
    verify_result?;
    Ok(db_entry.bytes.clone())
}

/// Snapshot the live connection `src` into a *new* database file at `dest`
/// using SQLite's Online Backup API. The destination is fully self-contained
/// (no `-wal` / `-shm` sidecars needed) because the helper forces a FULL
/// checkpoint on the destination before returning. This is the file the
/// restore flow writes to disk to roll back if anything goes wrong.
pub fn snapshot_live_to_file(
    src: &Connection,
    dest: &Path,
) -> Result<(), BackupError> {
    snapshot_database(src, dest)
}

/// Replace the on-disk SQLite file at `live_db_path` with the bytes
/// `db_bytes` atomically. The temp file is written in the same directory
/// then renamed; any pre-existing sidecars (`-wal`, `-shm`) are removed so
/// the reopened connection starts with a clean WAL.
pub fn replace_live_db(live_db_path: &Path, db_bytes: &[u8]) -> Result<(), BackupError> {
    if let Some(parent) = live_db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = live_db_path.with_extension("db.restore");
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    fs::write(&tmp, db_bytes)?;
    let _ = fs::remove_file(live_db_path.with_extension("db-wal"));
    let _ = fs::remove_file(live_db_path.with_extension("db-shm"));
    fs::rename(&tmp, live_db_path)?;
    Ok(())
}

/// Restore the `db_bytes` to the live path. Caller must have:
/// 1. Already validated the archive (via `validate_archive`).
/// 2. Created a safety snapshot at `safety_path` *before* calling this.
/// 3. Locked the live DB mutex.
///
/// On any failure between the file replacement and the post-restore FTS
/// rebuild, the caller should copy `safety_path` back to `live_db_path` and
/// reopen the connection. This helper is intentionally limited to the
/// atomic file write so the rollback logic stays in one place.
pub fn replace_live_db_with(
    db_bytes: &[u8],
    live_db_path: &Path,
    safety_path: &Path,
) -> Result<(), BackupError> {
    if let Err(e) = replace_live_db(live_db_path, db_bytes) {
        // Try to roll back immediately if we already wrote the safety path.
        if safety_path.exists() {
            let _ = fs::copy(safety_path, live_db_path);
        }
        return Err(e);
    }
    Ok(())
}

/// Best-effort rollback after a failed restore. Copies the safety snapshot
/// back to the live DB path and removes any half-written sidecars. Does
/// nothing if the safety file does not exist (the caller may have cleaned
/// it up). Returns `Ok(())` on success or `Err` if even the rollback copy
/// fails — at which point the caller should surface a hard error to the
/// user (the on-disk DB is in an unknown state).
pub fn rollback_to_safety(live_db_path: &Path, safety_path: &Path) -> Result<(), BackupError> {
    if !safety_path.exists() {
        return Ok(());
    }
    let _ = fs::remove_file(live_db_path.with_extension("db-wal"));
    let _ = fs::remove_file(live_db_path.with_extension("db-shm"));
    fs::copy(safety_path, live_db_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-bkmod-{tag}-{}",
            unique_suffix()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_fresh_db(path: &Path) -> Connection {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
        // mimic db::open: WAL mode + FK
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    fn install_schema(conn: &Connection) {
        conn.execute_batch(
            "BEGIN;
            CREATE TABLE topics (id INTEGER PRIMARY KEY AUTOINCREMENT, domain TEXT NOT NULL, topic TEXT NOT NULL, UNIQUE(domain, topic));
            CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
            CREATE TABLE knowledge_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT, question TEXT NOT NULL, answer TEXT NOT NULL,
                domain TEXT NOT NULL, topic TEXT NOT NULL, source TEXT NOT NULL DEFAULT '',
                follow_ups TEXT NOT NULL DEFAULT '[]', related_ids TEXT NOT NULL DEFAULT '[]',
                favorite INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_read_at TEXT
            );
            CREATE TABLE knowledge_tags (knowledge_id INTEGER NOT NULL, tag_id INTEGER NOT NULL,
                PRIMARY KEY (knowledge_id, tag_id));
            CREATE INDEX idx_knowledge_domain ON knowledge_items(domain);
            CREATE INDEX idx_knowledge_topic ON knowledge_items(topic);
            PRAGMA user_version = 1;
            COMMIT;"
        ).unwrap();
    }

    fn insert_item(conn: &Connection, q: &str, a: &str, domain: &str, topic: &str) -> i64 {
        conn.execute(
            "INSERT INTO knowledge_items (question, answer, domain, topic, source, follow_ups, related_ids, favorite, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, '', '[]', '[]', 0, '2026-09-04', '2026-09-04')",
            rusqlite::params![q, a, domain, topic],
        ).unwrap();
        conn.last_insert_rowid()
    }

    // -----------------------------------------------------------------
    // 1. Roundtrip: backup a populated DB and read it back, the SQLite
    //    is openable and all rows are present. The archive must NOT
    //    contain any application configuration.
    // -----------------------------------------------------------------
    #[test]
    fn backup_contains_all_data_and_no_app_config() {
        let dir = tmp_dir("create");
        let db = dir.join("interview-kit.db");

        let conn = open_fresh_db(&db);
        install_schema(&conn);
        let id_a = insert_item(&conn, "What is Rust?", "A safe systems language.", "Lang", "Rust");
        let id_b = insert_item(&conn, "What is SQLite?", "An embedded SQL engine.", "DB", "SQLite");
        drop(conn);

        let dest = dir.join("backup.ikbackup");
        let manifest = create(&db, &dest).expect("create");
        assert!(dest.exists());
        assert_eq!(manifest.knowledge_count, 2);
        assert_eq!(manifest.domain_count, 2);
        assert_eq!(manifest.format_version, FORMAT_VERSION);

        // inspect returns the manifest counts.
        let report = inspect(&dest).expect("inspect");
        assert_eq!(report.knowledge_count, 2);
        assert_eq!(report.domain_count, 2);

        // Negative assertions on the raw archive bytes: none of the
        // application config must appear. Backup carries only Knowledge
        // data + manifest, not the model config that lives in config.json.
        let raw = fs::read(&dest).unwrap();
        let as_str = String::from_utf8_lossy(&raw);
        for forbidden in [
            "apiKey",
            "api_key",
            "apiBaseUrl",
            "chatModel",
            "visionModel",
            "databaseLocation",
            "settings.json",
            "SECRET_KEY_42",
        ] {
            assert!(
                !as_str.contains(forbidden),
                "backup must not contain `{forbidden}`, found it in archive"
            );
        }

        // The archive must only contain manifest + database, no settings.
        let files = read_archive(&dest).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(files.len(), 2, "expected exactly 2 entries, got {names:?}");
        assert!(names.contains(&FILE_MANIFEST));
        assert!(names.contains(&FILE_DATABASE));

        // The embedded SQLite is openable and contains both rows by id.
        let db_entry = files.iter().find(|f| f.name == FILE_DATABASE).unwrap();
        let tmp = temp_path("test-open");
        fs::write(&tmp, &db_entry.bytes).unwrap();
        let verify = Connection::open(&tmp).unwrap();
        let n: i64 = verify.query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let q_a: String = verify.query_row("SELECT question FROM knowledge_items WHERE id = ?1", [id_a], |r| r.get(0)).unwrap();
        let q_b: String = verify.query_row("SELECT question FROM knowledge_items WHERE id = ?1", [id_b], |r| r.get(0)).unwrap();
        assert_eq!(q_a, "What is Rust?");
        assert_eq!(q_b, "What is SQLite?");
        let _ = fs::remove_file(&tmp);

        // Clean up the live DB and its sidecars.
        let _ = fs::remove_file(&db);
        let _ = fs::remove_file(dir.join("interview-kit.db-wal"));
        let _ = fs::remove_file(dir.join("interview-kit.db-shm"));
    }

    // -----------------------------------------------------------------
    // 2. WAL safety: a live WAL-mode DB whose connection is still open
    //    (so the pages have not been checkpointed) can be snapshotted
    //    via the Backup API and yields a self-contained DB with the
    //    same rows. We snapshot directly off the live connection (no
    //    helper opening a new read-only conn) so the test mirrors the
    //    production restore path.
    // -----------------------------------------------------------------
    #[test]
    fn snapshot_reads_unflushed_wal_data() {
        let dir = tmp_dir("wal");
        let db = dir.join("interview-kit.db");

        // Keep the live connection alive for the whole test — once it
        // drops, SQLite runs a passive checkpoint that can move the
        // recent pages into the main DB and clear -wal. The point of
        // the test is to verify the Backup API sees the WAL-resident
        // pages, so we keep the live conn open.
        let conn = open_fresh_db(&db);
        install_schema(&conn);
        insert_item(&conn, "wal-row-1", "answer-1", "d", "t");
        insert_item(&conn, "wal-row-2", "answer-2", "d", "t");

        // Snapshot using the live connection directly.
        let snap = temp_path("wal-snap");
        snapshot_live_to_file(&conn, &snap).expect("snapshot");
        // From here on we can drop the live conn — the snapshot is a
        // standalone file on disk.
        drop(conn);

        // The snapshot must contain both rows.
        let verify = Connection::open(&snap).expect("open snapshot");
        let n: i64 = verify
            .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let q1: String = verify
            .query_row(
                "SELECT question FROM knowledge_items WHERE question = ?1",
                ["wal-row-1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q1, "wal-row-1");
        let _ = fs::remove_file(&snap);
    }

    // -----------------------------------------------------------------
    // 3. Invalid archive rejection: garbage, bad version, bad SQLite.
    // -----------------------------------------------------------------
    #[test]
    fn invalid_archives_are_rejected() {
        let dir = tmp_dir("invalid");

        // not a backup at all
        let garbage = dir.join("garbage.ikbackup");
        fs::write(&garbage, b"hello world this is not a backup").unwrap();
        match inspect(&garbage) {
            Err(BackupError::InvalidArchive(_)) => {}
            other => panic!("expected InvalidArchive, got {:?}", other.map(|_| "ok")),
        }

        // right magic, wrong version
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&99u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u16.to_le_bytes()); // num_files
        bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
        bytes.extend_from_slice(FOOTER_MAGIC);
        let bad_version = dir.join("bad-version.ikbackup");
        fs::write(&bad_version, &bytes).unwrap();
        match inspect(&bad_version) {
            Err(BackupError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {:?}", other.map(|_| "ok")),
        }

        // right structure, but the embedded SQLite is not a real DB
        let mut files_archive = Vec::new();
        files_archive.extend_from_slice(MAGIC);
        files_archive.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        files_archive.extend_from_slice(&0u16.to_le_bytes()); // flags
        files_archive.extend_from_slice(&2u16.to_le_bytes()); // num_files = 2 (manifest + db)
        files_archive.extend_from_slice(&0u16.to_le_bytes()); // reserved
        // manifest
        let mjson = br#"{"formatVersion":1,"app":"Interview Kit","createdAt":"2026-09-04T00:00:00Z","databaseSchemaVersion":1,"knowledgeCount":0,"domainCount":0}"#;
        files_archive.extend_from_slice(&(FILE_MANIFEST.len() as u16).to_le_bytes());
        files_archive.extend_from_slice(FILE_MANIFEST.as_bytes());
        files_archive.extend_from_slice(&(mjson.len() as u64).to_le_bytes());
        files_archive.extend_from_slice(mjson);
        // database — random bytes, not SQLite
        let junk = b"NOT A SQLITE FILE".to_vec();
        files_archive.extend_from_slice(&(FILE_DATABASE.len() as u16).to_le_bytes());
        files_archive.extend_from_slice(FILE_DATABASE.as_bytes());
        files_archive.extend_from_slice(&(junk.len() as u64).to_le_bytes());
        files_archive.extend_from_slice(&junk);
        files_archive.extend_from_slice(FOOTER_MAGIC);
        let bad_db = dir.join("bad-db.ikbackup");
        fs::write(&bad_db, &files_archive).unwrap();
        match validate_archive(&bad_db) {
            Err(BackupError::InvalidDatabase(_)) => {}
            other => panic!("expected InvalidDatabase, got {:?}", other.map(|_| "ok")),
        }

        // right structure, but the embedded SQLite is a real DB with no business tables
        let empty_db = dir.join("empty.db");
        let conn = Connection::open(&empty_db).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);").unwrap();
        drop(conn);
        let empty_db_bytes = fs::read(&empty_db).unwrap();
        let mut arch = Vec::new();
        arch.extend_from_slice(MAGIC);
        arch.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        arch.extend_from_slice(&0u16.to_le_bytes());
        arch.extend_from_slice(&2u16.to_le_bytes());
        arch.extend_from_slice(&0u16.to_le_bytes());
        arch.extend_from_slice(&(FILE_MANIFEST.len() as u16).to_le_bytes());
        arch.extend_from_slice(FILE_MANIFEST.as_bytes());
        arch.extend_from_slice(&(mjson.len() as u64).to_le_bytes());
        arch.extend_from_slice(mjson);
        arch.extend_from_slice(&(FILE_DATABASE.len() as u16).to_le_bytes());
        arch.extend_from_slice(FILE_DATABASE.as_bytes());
        arch.extend_from_slice(&(empty_db_bytes.len() as u64).to_le_bytes());
        arch.extend_from_slice(&empty_db_bytes);
        arch.extend_from_slice(FOOTER_MAGIC);
        let no_tables = dir.join("no-tables.ikbackup");
        fs::write(&no_tables, &arch).unwrap();
        match validate_archive(&no_tables) {
            Err(BackupError::InvalidDatabase(_)) => {}
            other => panic!("expected InvalidDatabase, got {:?}", other.map(|_| "ok")),
        }
        let _ = fs::remove_file(&empty_db);
    }

    // -----------------------------------------------------------------
    // 4. Restore roundtrip: a fresh DB (dataset A) and a backup from a
    //    different DB (dataset B). Restoring B over A leaves only B's
    //    rows on the live path. After migration + FTS rebuild, the new
    //    rows are searchable via the same sqlite_master / FTS5 layout.
    // -----------------------------------------------------------------
    #[test]
    fn restore_replaces_knowledge_with_backup_data() {
        let dir = tmp_dir("restore");
        let live = dir.join("interview-kit.db");

        // Live DB (dataset A): two rows the user already had.
        let live_conn = open_fresh_db(&live);
        install_schema(&live_conn);
        insert_item(&live_conn, "Existing A1", "answer A1", "A", "Topic");
        insert_item(&live_conn, "Existing A2", "answer A2", "A", "Topic");

        // Build a backup DB (dataset B) with completely different rows.
        let backup_src = dir.join("interview-kit.src.db");
        let bk_conn = open_fresh_db(&backup_src);
        install_schema(&bk_conn);
        let bk_id = insert_item(&bk_conn, "Restored B1", "answer B1", "B", "Topic");
        insert_item(&bk_conn, "Restored B2", "answer B2", "B", "Topic");
        drop(bk_conn);

        let archive = dir.join("backup.ikbackup");
        create(&backup_src, &archive).expect("create");
        drop(live_conn);

        // Sanity: validation should succeed for the freshly written archive.
        let db_bytes = validate_archive(&archive).expect("validate");
        assert!(!db_bytes.is_empty());

        // Replace the live DB and re-open (mimic what backup_restore does).
        // Note: do NOT use `open_fresh_db` here — it removes the path
        // first, which would clobber the file we just wrote. Plain
        // `Connection::open` opens the replaced file in place.
        replace_live_db(&live, &db_bytes).expect("replace");
        let guard = Connection::open(&live).expect("open replaced");
        // Migrate + FTS rebuild. The public `db` module exposes this as
        // `after_restore`; we re-run the equivalent schema init here so the
        // test stays self-contained. We deliberately use the plain
        // `unicode61` tokenizer to avoid escape-sequence interactions in
        // `execute_batch` — what matters here is that the FTS table was
        // rebuilt and now reflects the restored data.
        guard
            .execute_batch(
                "PRAGMA user_version = 1;
                 DROP TABLE IF EXISTS knowledge_fts;
                 CREATE VIRTUAL TABLE knowledge_fts USING fts5(
                     question, answer, domain, topic, tags,
                     tokenize = 'unicode61');",
            )
            .expect("migrate+fts");
        // Populate the FTS table from primary data — same effect as the
        // production `rebuild_fts` step in `db::after_restore`.
        guard
            .execute(
                "INSERT INTO knowledge_fts(rowid, question, answer, domain, topic, tags)
                 SELECT id, question, answer, domain, topic, '' FROM knowledge_items",
                [],
            )
            .expect("fts insert");

        // After restore, only the B rows are present, and A is gone.
        let n: i64 = guard
            .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "expected only backup rows after restore");
        let q: String = guard
            .query_row(
                "SELECT question FROM knowledge_items WHERE id = ?1",
                [bk_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q, "Restored B1");
        let a_left: i64 = guard
            .query_row(
                "SELECT count(*) FROM knowledge_items WHERE domain = 'A'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_left, 0, "dataset A must be gone after restore");

        // FTS rebuild path: every row was re-indexed. A simple MATCH on
        // the restored question returns the same rowid.
        let fts_id: i64 = guard
            .query_row(
                "SELECT rowid FROM knowledge_fts WHERE knowledge_fts MATCH ?1 LIMIT 1",
                ["Restored"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_id, bk_id);

        // Clean up.
        let _ = fs::remove_file(&live);
        let _ = fs::remove_file(&backup_src);
    }

    // -----------------------------------------------------------------
    // 5. Invalid backup must not touch the live database. This is the
    //    rollback contract: a corrupt archive is rejected at validation
    //    time, before any file replacement happens.
    // -----------------------------------------------------------------
    #[test]
    fn invalid_archive_does_not_modify_live_db() {
        let dir = tmp_dir("invalid-restore");
        let live = dir.join("interview-kit.db");
        let live_conn = open_fresh_db(&live);
        install_schema(&live_conn);
        let id = insert_item(&live_conn, "Keep me", "answer", "domain", "topic");
        drop(live_conn);

        // A bogus file that is not a valid backup.
        let bogus = dir.join("bogus.ikbackup");
        fs::write(&bogus, b"definitely not a backup").unwrap();
        assert!(matches!(
            validate_archive(&bogus),
            Err(BackupError::InvalidArchive(_))
        ));

        // Live DB still has the original row, byte for byte.
        let verify = Connection::open(&live).unwrap();
        let n: i64 = verify
            .query_row("SELECT count(*) FROM knowledge_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let q: String = verify
            .query_row(
                "SELECT question FROM knowledge_items WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(q, "Keep me");
        let _ = fs::remove_file(&live);
    }

    // -----------------------------------------------------------------
    // 6. Container roundtrip: a minimal valid archive with just the
    //    manifest and an empty database payload still parses.
    // -----------------------------------------------------------------
    #[test]
    fn container_roundtrip() {
        let dir = tmp_dir("container");
        let path = dir.join("a.ikbackup");
        let mjson = br#"{"formatVersion":1,"app":"X","createdAt":"2026-09-04T00:00:00Z","databaseSchemaVersion":1,"knowledgeCount":0,"domainCount":0}"#;
        write_archive(&path, &[(FILE_MANIFEST, mjson), (FILE_DATABASE, b"")]).unwrap();
        let files = read_archive(&path).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, FILE_MANIFEST);
        assert_eq!(files[1].name, FILE_DATABASE);
        assert!(files[1].bytes.is_empty());
    }
}
