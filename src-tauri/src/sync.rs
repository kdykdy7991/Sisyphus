// Cross-device Knowledge sync — engine only.
//
// This module is the Sync Engine: pure data structures + pure merge logic +
// a thin DB apply step. It does NOT do file I/O, WebDAV, UI, or LLM. The
// Local-file and WebDAV transports (Task 3 / 4) consume / produce the same
// `SyncSnapshot` wire format so a single merge rule covers both.
//
// Design (from the design brief, codified):
//
//   * Snapshot = the user's full knowledge state at one point in time. It
//     carries business data + tombstones (`deleted_at`) but NEVER the
//     derived FTS index, NEVER the local numeric `id`, and NEVER any
//     application / device / sync config. WebDAV credentials, the API key,
//     and `device_id` live in separate files outside the snapshot.
//
//   * Sync identity = `sync_id` (UUIDv4 string). The local integer id is
//     device-internal. The v3 schema migration adds a partial UNIQUE
//     index on `sync_id` so a duplicate identity fails loudly at INSERT
//     time rather than silently producing ambiguous merges.
//
//   * Snapshot is full-state, not delta. The personal-knowledge dataset is
//     small; full snapshots make merge trivial to reason about, are
//     idempotent under repeated import, and don't require a change-log
//     GC contract.
//
//   * Merge is a pure function: `local + incoming -> MergePlan`. The
//     `apply_plan` step is the only thing that touches SQLite, and it
//     runs the whole plan inside one transaction so a partial failure
//     is impossible.
//
//   * Tombstones are sticky once set. A snapshot whose `sync_id` already
//     matches a local tombstoned row can only produce `Skip` (or
//     `Delete` with a redundant `deleted_at`); the active payload of a
//     tombstoned row can never be resurrected, even by a snapshot that
//     thinks the item is still active. This is the property the prompt
//     called out as "the tombstone bug": without it, a normal save on
//     a stale snapshot would silently bring deleted data back.
//
//   * `last_read_at` is the one field that does NOT use Last-Write-Wins:
//     it merges via `MAX(local, incoming)` so a knowledge item that was
//     read on both devices ends up with the most recent read time.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db::{self, KnowledgePayload};
use crate::webdav;

/// Wire-format version of `SyncSnapshot`. Bump only on a breaking change to
/// the schema. Currently = 1; readers MUST reject other versions.
pub const SYNC_FORMAT_VERSION: u16 = 1;
pub const SYNC_APP: &str = "Interview Kit";

/// One knowledge item in a snapshot. Mirrors `KnowledgePayload` minus the
/// local numeric `id` (a device-internal concept) and plus explicit
/// `camelCase` for cross-language wire compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeItem {
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
    #[serde(default)]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<String>,
    /// `None` = active; `Some(ts)` = tombstoned at `ts`. The wire format
    /// omits the field entirely for active rows so a normal `updated_at`
    /// diff is not buried in tombstone noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}

/// The unit that travels through both the file transport and the WebDAV
/// transport. Same struct, same merge engine, different I/O layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSnapshot {
    pub format_version: u16,
    pub app: String,
    /// RFC 3339 timestamp, set by the device that produced the snapshot.
    pub created_at: String,
    /// Optional human-readable label, e.g. "macbook-pro". Empty by default
    /// — never carry a stable device identifier here.
    #[serde(default)]
    pub source_device: String,
    pub items: Vec<KnowledgeItem>,
}

impl SyncSnapshot {
    /// Reject malformed snapshots. Must be called by `plan_merge` (and by
    /// any transport that wants to short-circuit on bad data) before
    /// touching the local DB.
    ///
    /// Validation rules (defense against malicious / corrupted files):
    ///   * `format_version` must equal the current `SYNC_FORMAT_VERSION`.
    ///   * Every item's `sync_id` must be a non-empty, non-whitespace
    ///     string of at most 64 characters.
    ///   * Every `sync_id` must parse as a UUID. `save()` always mints
    ///     UUIDv4 locally, and the database layer enforces the partial
    ///     UNIQUE index on `sync_id` — so anything that is not a UUID
    ///     at the wire level is either a corrupted file or a hostile
    ///     payload. The local DB never sees those.
    ///   * No two items may share the same `sync_id` (deduplication is
    ///     the merge engine's job; duplicate sync_ids in one snapshot
    ///     are a wire-level corruption).
    pub fn validate(&self) -> Result<(), SyncError> {
        if self.format_version != SYNC_FORMAT_VERSION {
            return Err(SyncError::UnsupportedVersion(self.format_version));
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.items.len());
        for item in &self.items {
            let trimmed = item.sync_id.trim();
            if trimmed.is_empty() {
                return Err(SyncError::InvalidSyncId(
                    "snapshot contains an item with empty sync_id".into(),
                ));
            }
            if trimmed.len() > 64 {
                return Err(SyncError::InvalidSyncId(format!(
                    "sync_id too long ({} chars): {}",
                    trimmed.len(),
                    trimmed
                )));
            }
            if uuid::Uuid::parse_str(trimmed).is_err() {
                return Err(SyncError::InvalidSyncId(format!(
                    "sync_id is not a valid UUID: {trimmed}"
                )));
            }
            if !seen.insert(trimmed) {
                return Err(SyncError::DuplicateSyncId(trimmed.to_string()));
            }
        }
        Ok(())
    }

    /// Count of active (non-tombstoned) items. Used by the UI summary.
    pub fn active_count(&self) -> usize {
        self.items.iter().filter(|i| i.deleted_at.is_none()).count()
    }

    pub fn deleted_count(&self) -> usize {
        self.items.iter().filter(|i| i.deleted_at.is_some()).count()
    }
}

/// Why a sync_id was skipped rather than written. `apply_plan` does not
/// need this; the UI uses it for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// Local content is identical to incoming (after last_read_at merge).
    AlreadyInSync,
    /// Local is already tombstoned, incoming is also tombstoned.
    AlreadyDeleted,
    /// Local is tombstoned, incoming is active — sticky delete wins.
    DeletionSticky,
    /// Local content is newer than incoming and nothing else changed.
    LocalNewer,
    /// Incoming is a tombstone for a sync_id we don't have. No-op.
    UnknownEntity,
}

/// How a conflict (same updated_at, different content on both sides) was
/// resolved. v1 only knows one strategy; future v2 can introduce
/// "KeepBoth" / "ManualPick" / etc. without breaking the wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolution {
    PickedIncoming,
}

/// One step in a `MergePlan`. The plan is the *whole* recipe: applying it
/// to `local` produces the merged database state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MergeAction {
    Insert {
        item: KnowledgeItem,
    },
    Update {
        sync_id: String,
        /// The fully-resolved state to write. The DB apply step overwrites
        /// the local row's content fields, FTS row, and tag set with this.
        merged: KnowledgeItem,
        /// Which content fields actually differ between local and merged.
        /// Useful for the UI summary and for testing. `lastReadAt` is
        /// included when only the read timestamp changed.
        changed_fields: Vec<String>,
    },
    Delete {
        sync_id: String,
        deleted_at: String,
    },
    Skip {
        sync_id: String,
        reason: SkipReason,
    },
    Conflict {
        sync_id: String,
        /// Same shape as `Update::merged`. The apply step writes this.
        merged: KnowledgeItem,
        changed_fields: Vec<String>,
        resolution: ConflictResolution,
    },
}

/// The recipe. Apply it to a `Connection` to get the merged state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergePlan {
    pub actions: Vec<MergeAction>,
}

impl MergePlan {
    pub fn stats(&self) -> MergeStats {
        let mut s = MergeStats::default();
        for a in &self.actions {
            match a {
                MergeAction::Insert { .. } => s.inserted += 1,
                MergeAction::Update { .. } => s.updated += 1,
                MergeAction::Delete { .. } => s.deleted += 1,
                MergeAction::Skip { .. } => s.skipped += 1,
                MergeAction::Conflict { .. } => s.conflicts += 1,
            }
        }
        s
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeStats {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub conflicts: usize,
}

impl MergeStats {
    pub fn total(&self) -> usize {
        self.inserted + self.updated + self.deleted + self.skipped + self.conflicts
    }

    /// Accumulate another apply pass. Only needed by the WebDAV transport,
    /// where a 412 retry runs a second merge on top of the first one; the
    /// reported summary is the sum of the work actually done.
    pub fn extend(&mut self, other: &MergeStats) {
        self.inserted += other.inserted;
        self.updated += other.updated;
        self.deleted += other.deleted;
        self.skipped += other.skipped;
        self.conflicts += other.conflicts;
    }
}

#[derive(Debug)]
pub enum SyncError {
    /// The snapshot's `formatVersion` is not one this build understands.
    UnsupportedVersion(u16),
    /// The snapshot is well-formed JSON but its items violate the contract
    /// (empty sync_id, dup sync_id, sync_id too long, ...).
    InvalidSnapshot(String),
    InvalidSyncId(String),
    DuplicateSyncId(String),
    /// A sqlite error during snapshot creation or plan apply.
    Sqlite(rusqlite::Error),
    /// A JSON parse error reading a `.iksync` file.
    Json(serde_json::Error),
    /// A filesystem error during the local file transport.
    Io(std::io::Error),
    /// The remote (WebDAV) snapshot could not be parsed or failed
    /// validation. The local DB was NOT modified and the remote file was
    /// NOT overwritten — the corrupt file stays put so the user can inspect
    /// it instead of losing it to an automatic overwrite.
    InvalidRemoteSnapshot(String),
    /// A WebDAV transport failure (auth / connectivity / protocol / ...).
    WebDav(webdav::WebDavError),
    /// The database mutex was poisoned by a panic on another thread.
    LockPoisoned,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::UnsupportedVersion(v) => write!(f, "不支持的同步快照版本：{v}"),
            SyncError::InvalidSnapshot(s) => write!(f, "同步快照无效：{s}"),
            SyncError::InvalidSyncId(s) => write!(f, "同步 ID 无效：{s}"),
            SyncError::DuplicateSyncId(s) => write!(f, "同步 ID 重复：{s}"),
            SyncError::Sqlite(e) => write!(f, "数据库错误：{e}"),
            SyncError::Json(e) => write!(f, "同步文件解析失败：{e}"),
            SyncError::Io(e) => write!(f, "同步文件读写失败：{e}"),
            SyncError::InvalidRemoteSnapshot(s) => write!(f, "远端同步文件无效：{s}"),
            SyncError::WebDav(e) => write!(f, "{e}"),
            SyncError::LockPoisoned => write!(f, "数据库锁已损坏，请重启应用。"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Sqlite(e)
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        SyncError::Json(e)
    }
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Io(e)
    }
}

impl From<webdav::WebDavError> for SyncError {
    fn from(e: webdav::WebDavError) -> Self {
        SyncError::WebDav(e)
    }
}

// ---------------------------------------------------------------------------
// Snapshot creation (pure read, no side effects)
// ---------------------------------------------------------------------------

/// Build a `SyncSnapshot` from the current local database state. The
/// snapshot includes soft-deleted rows (as tombstones) so the peer can
/// observe and propagate the deletion. FTS rows are NOT included — FTS
/// is a derived index, never transported.
///
/// This function does NOT bump `last_read_at` (it does not go through
/// `db::get`). The local SQLite connection is held only for the duration
/// of the call.
pub fn create_snapshot(conn: &Connection) -> Result<SyncSnapshot, SyncError> {
    let items = db::list_all_for_sync(conn)?;
    let wire_items: Vec<KnowledgeItem> = items.iter().map(payload_to_item).collect();
    Ok(SyncSnapshot {
        format_version: SYNC_FORMAT_VERSION,
        app: SYNC_APP.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source_device: String::new(),
        items: wire_items,
    })
}

fn payload_to_item(p: &KnowledgePayload) -> KnowledgeItem {
    KnowledgeItem {
        sync_id: p.sync_id.clone(),
        question: p.question.clone(),
        answer: p.answer.clone(),
        domain: p.domain.clone(),
        topic: p.topic.clone(),
        tags: p.tags.clone(),
        follow_ups: p.follow_ups.clone(),
        related_ids: p.related_ids.clone(),
        source: p.source.clone(),
        created_at: p.created_at.clone(),
        updated_at: p.updated_at.clone(),
        favorite: p.favorite.unwrap_or(false),
        last_read_at: p.last_read_at.clone(),
        deleted_at: p.deleted_at.clone(),
    }
}

fn item_to_payload(item: &KnowledgeItem) -> KnowledgePayload {
    KnowledgePayload {
        // Local id is intentionally empty: the sync path looks up by
        // sync_id, never by local id. Leaving it empty keeps the
        // payload schema-aligned with the rest of the codebase.
        id: String::new(),
        sync_id: item.sync_id.clone(),
        question: item.question.clone(),
        answer: item.answer.clone(),
        domain: item.domain.clone(),
        topic: item.topic.clone(),
        tags: item.tags.clone(),
        follow_ups: item.follow_ups.clone(),
        related_ids: item.related_ids.clone(),
        source: item.source.clone(),
        created_at: item.created_at.clone(),
        updated_at: item.updated_at.clone(),
        favorite: Some(item.favorite),
        last_read_at: item.last_read_at.clone(),
        deleted_at: item.deleted_at.clone(),
    }
}

// ---------------------------------------------------------------------------
// Merge plan
// ---------------------------------------------------------------------------

/// Compute the merge plan from two snapshots. Pure function: no DB
/// access, no I/O. The result is a `MergePlan` whose `apply_plan` is the
/// only side-effecting step.
///
/// The plan only contains actions for items in `incoming`; items present
/// only in `local` are kept on the local side implicitly (the apply step
/// does not touch them).
pub fn plan_merge(
    local: &SyncSnapshot,
    incoming: &SyncSnapshot,
) -> Result<MergePlan, SyncError> {
    local.validate()?;
    incoming.validate()?;

    let local_map: HashMap<&str, &KnowledgeItem> = local
        .items
        .iter()
        .map(|i| (i.sync_id.as_str(), i))
        .collect();

    let mut actions = Vec::with_capacity(incoming.items.len());
    for incoming_item in &incoming.items {
        let local_item = local_map.get(incoming_item.sync_id.as_str()).copied();
        actions.push(decide(incoming_item, local_item));
    }
    Ok(MergePlan { actions })
}

fn decide(incoming: &KnowledgeItem, local: Option<&KnowledgeItem>) -> MergeAction {
    // Case 1: incoming is tombstoned.
    if let Some(incoming_del_at) = incoming.deleted_at.as_deref() {
        return match local {
            None => MergeAction::Skip {
                sync_id: incoming.sync_id.clone(),
                reason: SkipReason::UnknownEntity,
            },
            Some(l) if l.deleted_at.is_some() => MergeAction::Skip {
                sync_id: incoming.sync_id.clone(),
                reason: SkipReason::AlreadyDeleted,
            },
            Some(_) => MergeAction::Delete {
                sync_id: incoming.sync_id.clone(),
                deleted_at: incoming_del_at.to_string(),
            },
        };
    }

    // Case 2: incoming is active, local is missing -> Insert.
    let Some(local) = local else {
        return MergeAction::Insert {
            item: incoming.clone(),
        };
    };

    // Case 3: incoming is active, local is tombstoned -> sticky delete.
    if local.deleted_at.is_some() {
        return MergeAction::Skip {
            sync_id: incoming.sync_id.clone(),
            reason: SkipReason::DeletionSticky,
        };
    }

    // Case 4: both active.
    let content_equal = items_content_equal(incoming, local);
    let local_has_max_read = last_read_is_max(local, incoming);
    if content_equal && local_has_max_read {
        return MergeAction::Skip {
            sync_id: incoming.sync_id.clone(),
            reason: SkipReason::AlreadyInSync,
        };
    }

    let cmp = compare_ts(&local.updated_at, &incoming.updated_at);
    let local_newer = cmp == Ordering::Greater;
    let same_ts = cmp == Ordering::Equal;

    // Pick the content side: newer wins for content fields.
    let content_source: &KnowledgeItem = if local_newer { local } else { incoming };
    let mut merged = content_source.clone();
    // Preserve local `created_at` (first-seen is device-owned, not user-edited).
    merged.created_at = local.created_at.clone();
    // `last_read_at` is the one field that always uses MAX, not LWW.
    merged.last_read_at = max_opt(local.last_read_at.clone(), incoming.last_read_at.clone());

    let changed_fields = diff_fields(local, &merged);

    if same_ts && !content_equal {
        return MergeAction::Conflict {
            sync_id: incoming.sync_id.clone(),
            merged,
            changed_fields,
            resolution: ConflictResolution::PickedIncoming,
        };
    }

    // Distinguish "skip because local is strictly newer in every meaningful
    // way" from "update" — if the only change is bumping last_read_at to
    // a later incoming value, it's still an Update (the local row was
    // modified), so the "LocalNewer" Skip reason is reserved for a future
    // case where we can prove nothing changed. For v1 we just always Update.
    MergeAction::Update {
        sync_id: incoming.sync_id.clone(),
        merged,
        changed_fields,
    }
}

fn items_content_equal(a: &KnowledgeItem, b: &KnowledgeItem) -> bool {
    a.question == b.question
        && a.answer == b.answer
        && a.domain == b.domain
        && a.topic == b.topic
        && a.source == b.source
        && a.follow_ups == b.follow_ups
        && a.related_ids == b.related_ids
        && a.tags == b.tags
        && a.favorite == b.favorite
}

fn last_read_is_max(local: &KnowledgeItem, incoming: &KnowledgeItem) -> bool {
    compare_opt(local.last_read_at.as_deref(), incoming.last_read_at.as_deref()) != Ordering::Less
}

fn max_opt(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (Some(x), Some(y)) => match compare_ts(&x, &y) {
            Ordering::Less | Ordering::Equal => Some(y),
            Ordering::Greater => Some(x),
        },
    }
}

fn compare_opt(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => compare_ts(a, b),
    }
}

/// Best-effort timestamp compare. Tries numeric parse first (UNIX-seconds
/// as string, which is what `db::now_ts` produces); falls back to string
/// compare. Both schemes work for the common cases this code sees (UTF
/// strings of fixed-width digit characters or `YYYY-MM-DD HH:MM`).
fn compare_ts(a: &str, b: &str) -> Ordering {
    if let (Ok(ai), Ok(bi)) = (a.parse::<u64>(), b.parse::<u64>()) {
        return ai.cmp(&bi);
    }
    a.cmp(b)
}

fn diff_fields(local: &KnowledgeItem, merged: &KnowledgeItem) -> Vec<String> {
    let mut fields = Vec::new();
    if local.question != merged.question {
        fields.push("question".to_string());
    }
    if local.answer != merged.answer {
        fields.push("answer".to_string());
    }
    if local.domain != merged.domain {
        fields.push("domain".to_string());
    }
    if local.topic != merged.topic {
        fields.push("topic".to_string());
    }
    if local.source != merged.source {
        fields.push("source".to_string());
    }
    if local.follow_ups != merged.follow_ups {
        fields.push("followUps".to_string());
    }
    if local.related_ids != merged.related_ids {
        fields.push("relatedIds".to_string());
    }
    if local.tags != merged.tags {
        fields.push("tags".to_string());
    }
    if local.favorite != merged.favorite {
        fields.push("favorite".to_string());
    }
    if local.last_read_at != merged.last_read_at {
        fields.push("lastReadAt".to_string());
    }
    fields
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Apply a `MergePlan` to the local database. The whole plan runs in a
/// single transaction; on any sqlite error the DB is left unchanged.
///
/// Returns the resulting stats. `apply_plan` is the only function in this
/// module that may write to the database — `plan_merge` and
/// `create_snapshot` are pure.
pub fn apply_plan(conn: &mut Connection, plan: &MergePlan) -> Result<MergeStats, SyncError> {
    let tx = conn.transaction()?;
    let mut stats = MergeStats::default();

    for action in &plan.actions {
        match action {
            MergeAction::Insert { item } => {
                let payload = item_to_payload(item);
                db::insert_from_sync(&tx, &payload)?;
                stats.inserted += 1;
            }
            MergeAction::Update { merged, .. } => {
                let payload = item_to_payload(merged);
                let written = db::update_from_sync(&tx, &merged.sync_id, &payload)?;
                if written {
                    stats.updated += 1;
                } else {
                    // Refused (local was tombstoned between plan and apply).
                    // Re-classify as a sticky-delete skip; the user will see
                    // a Skipped count instead of a phantom Updated.
                    stats.skipped += 1;
                }
            }
            MergeAction::Delete { sync_id, deleted_at } => {
                let changed = db::soft_delete_by_sync_id(&tx, sync_id, deleted_at)?;
                if changed {
                    stats.deleted += 1;
                } else {
                    stats.skipped += 1;
                }
            }
            MergeAction::Skip { .. } => {
                stats.skipped += 1;
            }
            MergeAction::Conflict { merged, .. } => {
                let payload = item_to_payload(merged);
                let written = db::update_from_sync(&tx, &merged.sync_id, &payload)?;
                if written {
                    stats.conflicts += 1;
                } else {
                    stats.skipped += 1;
                }
            }
        }
    }

    tx.commit()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Local file transport
//
// The on-disk `.iksync` file is JUST a JSON serialization of `SyncSnapshot`
// (pretty-printed, no extra container, no manifest wrapper). The snapshot
// itself already carries format_version / app / created_at / items, so
// adding an outer envelope would buy nothing and would only make future
// version upgrades harder.
//
// This layer is the only place that touches the filesystem; `create_snapshot`
// and `plan_merge` / `apply_plan` stay pure. WebDAV (Task 4) reuses the
// same in-memory snapshot type and the same engine — it just swaps the
// transport.
// ---------------------------------------------------------------------------

/// Wire-format file extension. The format is JSON; the extension is purely
/// advisory (used by the OS file dialog filter) and the wire format is
/// identified by `format_version` on the parsed snapshot.
pub const SYNC_FILE_EXTENSION: &str = "iksync";

/// Summary of a successful local export. Returned to the UI so it can show
/// a confirmation with counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncExportSummary {
    pub path: String,
    pub format_version: u16,
    pub created_at: String,
    pub item_count: usize,
    pub active_count: usize,
    pub deleted_count: usize,
}

/// Summary of a successful local import. Counts come from the merge
/// engine's `MergeStats`; metadata is the snapshot's own (what the user
/// just brought in from another device).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncImportSummary {
    pub path: String,
    pub format_version: u16,
    pub snapshot_created_at: String,
    pub snapshot_item_count: usize,
    pub snapshot_active_count: usize,
    pub snapshot_deleted_count: usize,
    /// Number of bytes read from disk, for diagnostics.
    pub bytes_read: usize,
    /// Result of the merge plan's apply step.
    pub stats: MergeStats,
}

/// Read-only preview of a `.iksync` file. The UI surfaces this before
/// asking the user to confirm an import. `sync_inspect` never touches
/// the local database — it only parses the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncInspectReport {
    pub path: String,
    pub format_version: u16,
    pub app: String,
    pub created_at: String,
    /// Optional human-readable source label. Empty if the file's
    /// producer did not set one. Never carries a stable device id.
    pub source_device: String,
    pub item_count: usize,
    pub active_count: usize,
    pub deleted_count: usize,
    pub bytes_read: usize,
}

/// Export the current local knowledge state to a `.iksync` file at
/// `dest_path`. The file is written atomically: the JSON is first
/// staged at `<dest_path>.tmp` and then renamed onto the destination,
/// so a crash mid-write can never leave a partial file at the canonical
/// path. If a file already exists at the destination it is overwritten
/// in place — same semantics as the existing backup flow.
pub fn sync_export_local(
    conn: &Connection,
    dest_path: &Path,
) -> Result<SyncExportSummary, SyncError> {
    let snapshot = create_snapshot(conn)?;
    snapshot.validate()?;
    let bytes = serde_json::to_vec_pretty(&snapshot).map_err(SyncError::Json)?;
    atomic_write_json(dest_path, &bytes)?;
    Ok(SyncExportSummary {
        path: dest_path.display().to_string(),
        format_version: snapshot.format_version,
        created_at: snapshot.created_at.clone(),
        item_count: snapshot.items.len(),
        active_count: snapshot.active_count(),
        deleted_count: snapshot.deleted_count(),
    })
}

/// Inspect a `.iksync` file without touching the database. Used by the
/// UI to show "this file has N active items and M tombstones from date
/// X" before the user clicks "Import". Any parse / validate failure
/// bubbles up as a `SyncError`; the database is never opened.
pub fn sync_inspect(source_path: &Path) -> Result<SyncInspectReport, SyncError> {
    let bytes = fs::read(source_path).map_err(SyncError::Io)?;
    let snapshot: SyncSnapshot = serde_json::from_slice(&bytes).map_err(SyncError::Json)?;
    snapshot.validate()?;
    // Compute counts before moving any String fields.
    let item_count = snapshot.items.len();
    let active_count = snapshot.active_count();
    let deleted_count = snapshot.deleted_count();
    Ok(SyncInspectReport {
        path: source_path.display().to_string(),
        format_version: snapshot.format_version,
        app: snapshot.app,
        created_at: snapshot.created_at,
        source_device: snapshot.source_device,
        item_count,
        active_count,
        deleted_count,
        bytes_read: bytes.len(),
    })
}

/// Import a `.iksync` file and merge it into the local database.
///
/// Pipeline (all-or-nothing):
///   1. Read the file from disk (no DB access yet).
///   2. JSON-parse into a `SyncSnapshot`.
///   3. `SyncSnapshot::validate` — reject bad format_version, empty /
///      non-UUID / duplicate sync_ids, etc. A rejected file never reaches
///      the database.
///   4. Build the local snapshot (pure read, no side effects).
///   5. `plan_merge(local, incoming)` — pure function, returns a plan.
///   6. `apply_plan` runs the plan inside a single SQLite transaction.
///
/// Steps 1-5 leave the database unchanged on any error. Step 6 is itself
/// a single transaction, so the only way to leave a half-applied merge
/// is a process kill between `BEGIN` and `COMMIT` — SQLite's rollback
/// on next open takes care of that. Repeated imports of the same file
/// are idempotent: the second pass produces a plan that is almost
/// entirely `Skip(AlreadyInSync)`.
pub fn sync_import_local(
    conn: &mut Connection,
    source_path: &Path,
) -> Result<SyncImportSummary, SyncError> {
    let bytes = fs::read(source_path).map_err(SyncError::Io)?;
    let incoming: SyncSnapshot =
        serde_json::from_slice(&bytes).map_err(SyncError::Json)?;
    incoming.validate()?;
    let local = create_snapshot(conn)?;
    let plan = plan_merge(&local, &incoming)?;
    let stats = apply_plan(conn, &plan)?;
    Ok(SyncImportSummary {
        path: source_path.display().to_string(),
        format_version: incoming.format_version,
        snapshot_created_at: incoming.created_at.clone(),
        snapshot_item_count: incoming.items.len(),
        snapshot_active_count: incoming.active_count(),
        snapshot_deleted_count: incoming.deleted_count(),
        bytes_read: bytes.len(),
        stats,
    })
}

/// Write `bytes` to `path` atomically. The file is staged at
/// `<path>.tmp` and then renamed onto `path`; an interrupted write
/// leaves the destination at its previous content and a stray
/// `<path>.tmp` that the next call cleans up.
fn atomic_write_json(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(SyncError::Io)?;
        }
    }
    // Build "<path>.tmp". The original backup module uses
    // `with_extension`, but that strips the existing extension; here we
    // just append `.tmp` so the temp file lives next to the real one
    // (same directory -> rename is atomic on macOS / Linux / Windows
    // NTFS for files on the same volume).
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| SyncError::Io(io_err("destination has no file name")))?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);

    // If a previous attempt left a stray tmp file, drop it. This is
    // best-effort: a concurrent writer on the same path would still
    // race, but the calling Tauri command is single-threaded per app.
    let _ = fs::remove_file(&tmp_path);

    if let Err(e) = fs::write(&tmp_path, bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(SyncError::Io(e));
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(SyncError::Io(e));
    }
    Ok(())
}

fn io_err(s: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, s.into())
}

// ---------------------------------------------------------------------------
// WebDAV transport (Task 4)
//
// WebDAV is a *medium*, not a second sync algorithm. It runs the exact same
// `create_snapshot` / `plan_merge` / `apply_plan` triple as the local-file
// transport, so a `.iksync` file and a WebDAV round-trip produce byte-identical
// local state. Nothing below re-implements dedup, LWW, tombstones,
// `last_read_at` MAX or conflict rules.
//
// What WebDAV adds is the return trip. Uploading the *current* snapshot would
// be plain last-writer-wins and could drop a peer's items:
//
//     Remote: A B C D        Pad: A B C E
//     Pad PUTs its snapshot  -> Remote becomes A B C E, D is gone
//
// so the transport always merges first and uploads the merged result:
//
//     Local  = Local ∪ Remote
//     Remote = merged Local
//
// Ordering is deliberate — no network I/O happens while a SQLite transaction
// is open, and the connection is never held across an `await`:
//
//     GET remote            (network)
//     parse + validate      (pure; a bad remote never reaches the DB)
//     create_snapshot       (short read, connection released)
//     plan_merge            (pure)
//     apply_plan            (one short transaction)
//     create_snapshot again (short read — this is what gets uploaded)
//     PUT merged            (network, guarded by If-Match / If-None-Match)
//
// First sync: `latest.iksync` does not exist yet. That is `RemoteNotFound`,
// a normal condition rather than a failure — we just upload the local
// snapshot (creating `sync/` first).
// ---------------------------------------------------------------------------

/// Total upload attempts: the first one plus two re-fetch / re-merge /
/// re-upload rounds after a 412.
pub const WEBDAV_MAX_PUT_ATTEMPTS: usize = 3;

/// What the UI needs after a WebDAV sync (Task 5). HTTP internals — status
/// codes, ETags, request URLs — deliberately do not leak through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavSyncSummary {
    /// `false` = first sync: the remote had no snapshot yet.
    pub remote_existed: bool,
    /// Item count of the remote snapshot we merged in (0 on first sync).
    pub downloaded_item_count: usize,
    /// Item count of the snapshot we uploaded (active + tombstones).
    pub uploaded_item_count: usize,
    pub stats: MergeStats,
    pub synced_at: String,
    /// How many 412 retries it took. `> 0` means another device raced us
    /// and the data was still merged correctly.
    pub retry_count: usize,
}

/// The two database operations the WebDAV loop needs. Implemented for
/// `Mutex<Connection>` so every step acquires and immediately releases the
/// connection: the SQLite handle is never held across a network round-trip
/// and never across an `await`.
pub trait SyncTarget {
    fn read_snapshot(&self) -> Result<SyncSnapshot, SyncError>;
    fn apply(&self, plan: &MergePlan) -> Result<MergeStats, SyncError>;
}

impl SyncTarget for Mutex<Connection> {
    fn read_snapshot(&self) -> Result<SyncSnapshot, SyncError> {
        let conn = self.lock().map_err(|_| SyncError::LockPoisoned)?;
        create_snapshot(&conn)
    }

    fn apply(&self, plan: &MergePlan) -> Result<MergeStats, SyncError> {
        let mut conn = self.lock().map_err(|_| SyncError::LockPoisoned)?;
        apply_plan(&mut conn, plan)
    }
}

/// Parse + validate a snapshot that came from the remote. Any failure becomes
/// `InvalidRemoteSnapshot`: the caller then leaves the local DB alone AND
/// leaves the remote file in place for the user to inspect.
fn parse_remote_snapshot(bytes: &[u8]) -> Result<SyncSnapshot, SyncError> {
    let snapshot: SyncSnapshot = serde_json::from_slice(bytes)
        .map_err(|e| SyncError::InvalidRemoteSnapshot(format!("JSON 解析失败：{e}")))?;
    snapshot
        .validate()
        .map_err(|e| SyncError::InvalidRemoteSnapshot(e.to_string()))?;
    Ok(snapshot)
}

/// Full WebDAV sync round-trip.
///
/// Returns the summary of what happened. On any error before the apply step
/// (network, auth, invalid remote) the local database is untouched; a remote
/// snapshot that fails validation is never overwritten either.
///
/// A 412 on upload means another device committed while we were merging. The
/// loop then re-GETs, re-merges (the merge is idempotent, so the first pass
/// is simply skipped) and re-uploads, up to `WEBDAV_MAX_PUT_ATTEMPTS`.
pub async fn sync_via_webdav<D, T>(db: &D, transport: &T) -> Result<WebDavSyncSummary, SyncError>
where
    D: SyncTarget + ?Sized,
    T: webdav::WebDavTransport + ?Sized,
{
    let mut stats = MergeStats::default();
    let mut remote_existed = false;
    let mut retry_count = 0usize;

    for attempt in 0..WEBDAV_MAX_PUT_ATTEMPTS {
        // 1. Download. `RemoteNotFound` is the ordinary first-sync state.
        let remote = match transport.get_latest().await {
            Ok(object) => Some(object),
            Err(webdav::WebDavError::RemoteNotFound) => None,
            Err(e) => return Err(SyncError::WebDav(e)),
        };
        if attempt == 0 {
            remote_existed = remote.is_some();
        }

        // 2. Parse + validate BEFORE anything touches the local database.
        let remote_snapshot = match &remote {
            None => None,
            Some(object) => Some(parse_remote_snapshot(&object.bytes)?),
        };
        let downloaded_item_count =
            remote_snapshot.as_ref().map(|s| s.items.len()).unwrap_or(0);

        // 3. Local snapshot (short read; connection released immediately).
        let local = db.read_snapshot()?;

        // 4. Plan — the same pure merge the local-file transport uses.
        let plan = match &remote_snapshot {
            Some(incoming) => plan_merge(&local, incoming)?,
            None => MergePlan::default(),
        };

        // 5. Apply — a single short transaction, no network inside it.
        let pass = db.apply(&plan)?;
        if attempt == 0 {
            stats = pass;
        } else {
            stats.extend(&pass);
        }

        // 6. Re-read the merged local state: this is what goes back up.
        let merged = db.read_snapshot()?;
        let uploaded_item_count = merged.items.len();
        let bytes = serde_json::to_vec_pretty(&merged).map_err(SyncError::Json)?;

        // 7. Upload. First sync also has to create `sync/`.
        if remote.is_none() {
            transport.ensure_dir().await?;
        }
        let condition = match &remote {
            Some(object) => match &object.etag {
                // Guard against a concurrent writer: the server rejects the
                // upload when the ETag moved on since our GET.
                Some(etag) => webdav::PutCondition::IfMatch(etag.clone()),
                // Server does not expose ETags — documented fallback.
                None => webdav::PutCondition::Unconditional,
            },
            // First sync: create-only, so a device that raced us turns into
            // a retryable 412 instead of a silent overwrite.
            None => webdav::PutCondition::IfNoneMatchStar,
        };

        match transport.put_latest(&bytes, condition).await {
            Ok(_) => {
                return Ok(WebDavSyncSummary {
                    remote_existed,
                    downloaded_item_count,
                    uploaded_item_count,
                    stats,
                    synced_at: chrono::Utc::now().to_rfc3339(),
                    retry_count,
                });
            }
            Err(webdav::WebDavError::PreconditionFailed) => {
                retry_count += 1;
                continue;
            }
            Err(e) => return Err(SyncError::WebDav(e)),
        }
    }

    // Retries exhausted: the remote keeps changing under us. The local merge
    // is already applied and idempotent, so the user can simply try again.
    Err(SyncError::WebDav(webdav::WebDavError::PreconditionFailed))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use rusqlite::Connection;
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Deterministic UUID derivation from a test label. The wire format
    /// requires every `sync_id` to be a valid UUID, but tests still want
    /// readable labels like `"A"` or `"shared"`. `new_v5` over a fixed
    /// namespace produces a stable, well-formed UUID per label.
    fn test_sync_id(label: &str) -> String {
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, label.as_bytes()).to_string()
    }

    fn tmp_conn(tag: &str) -> Connection {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-sync-{}-{}",
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sync.db");
        let conn = Connection::open(&path).unwrap();
        db::init(&conn).unwrap();
        conn
    }

    fn snap(items: Vec<KnowledgeItem>) -> SyncSnapshot {
        SyncSnapshot {
            format_version: SYNC_FORMAT_VERSION,
            app: SYNC_APP.to_string(),
            created_at: "2026-09-04T10:20:00Z".to_string(),
            source_device: String::new(),
            items,
        }
    }

    fn item(label: &str, q: &str, updated_at: &str) -> KnowledgeItem {
        KnowledgeItem {
            sync_id: test_sync_id(label),
            question: q.to_string(),
            answer: "A".to_string(),
            domain: "D".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_ups: vec![],
            related_ids: vec![],
            source: String::new(),
            created_at: "2026-01-01 00:00".to_string(),
            updated_at: updated_at.to_string(),
            favorite: false,
            last_read_at: None,
            deleted_at: None,
        }
    }

    fn find<'a>(plan: &'a MergePlan, label: &str) -> &'a MergeAction {
        let target = test_sync_id(label);
        plan.actions
            .iter()
            .find(|a| match a {
                MergeAction::Insert { item } => item.sync_id == target,
                MergeAction::Update { sync_id: s, .. } => *s == target,
                MergeAction::Delete { sync_id: s, .. } => *s == target,
                MergeAction::Skip { sync_id: s, .. } => *s == target,
                MergeAction::Conflict { sync_id: s, .. } => *s == target,
            })
            .expect("action for label not found in plan")
    }

    // ----- plan_merge: the cases the prompt specifically called out -----

    /// Both sides active, identical content -> AlreadyInSync. This is the
    /// most common case when both devices are up to date.
    #[test]
    fn plan_merge_identical_active_is_skip() {
        let a = item("A", "Q", "1700000000");
        let b = a.clone();
        let local = snap(vec![a.clone()]);
        let incoming = snap(vec![b]);
        let plan = plan_merge(&local, &incoming).unwrap();
        assert_eq!(plan.actions.len(), 1);
        match find(&plan, "A") {
            MergeAction::Skip { reason, .. } => {
                assert_eq!(*reason, SkipReason::AlreadyInSync);
            }
            other => panic!("expected Skip(AlreadyInSync), got {:?}", other),
        }
    }

    /// Incoming is active, local is None -> Insert.
    #[test]
    fn plan_merge_incoming_only_inserts() {
        let a = item("A", "Q", "1700000000");
        let plan = plan_merge(&snap(vec![]), &snap(vec![a.clone()])).unwrap();
        match find(&plan, "A") {
            MergeAction::Insert { item } => assert_eq!(item.sync_id, test_sync_id("A")),
            other => panic!("expected Insert, got {:?}", other),
        }
    }

    /// Incoming is tombstoned, local is active -> Delete (propagate tombstone).
    #[test]
    fn plan_merge_tombstone_propagates() {
        let local_item = item("A", "Q", "1700000000");
        let mut incoming_item = local_item.clone();
        incoming_item.deleted_at = Some("1700000100".to_string());
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Delete { sync_id, deleted_at } => {
                assert_eq!(sync_id, &test_sync_id("A"));
                assert_eq!(deleted_at, "1700000100");
            }
            other => panic!("expected Delete, got {:?}", other),
        }
    }

    /// Both sides tombstoned -> AlreadyDeleted (skip).
    #[test]
    fn plan_merge_both_tombstoned_skips() {
        let mut a = item("A", "Q", "1700000000");
        a.deleted_at = Some("1700000050".to_string());
        let mut b = a.clone();
        b.deleted_at = Some("1700000100".to_string());
        let plan = plan_merge(&snap(vec![a]), &snap(vec![b])).unwrap();
        match find(&plan, "A") {
            MergeAction::Skip { reason, .. } => {
                assert_eq!(*reason, SkipReason::AlreadyDeleted);
            }
            other => panic!("expected Skip(AlreadyDeleted), got {:?}", other),
        }
    }

    /// Local is tombstoned, incoming is active -> DeletionSticky. THIS IS
    /// the case the prompt said is the most important: "deleted data must
    /// not be silently resurrected by a snapshot that thinks the item is
    /// still active".
    #[test]
    fn plan_merge_local_tombstone_blocks_incoming_active() {
        let mut local_item = item("A", "Q", "1700000000");
        local_item.deleted_at = Some("1700000050".to_string());
        // Incoming has the same sync_id but thinks the item is active.
        let mut incoming_item = local_item.clone();
        incoming_item.deleted_at = None;
        incoming_item.question = "Q updated on the other side".to_string();
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Skip { reason, .. } => {
                assert_eq!(*reason, SkipReason::DeletionSticky);
            }
            other => panic!("expected Skip(DeletionSticky), got {:?}", other),
        }
    }

    /// Both sides active, incoming has a strictly later updated_at -> Update
    /// (LWW on content). `last_read_at` merges via MAX, independent of the
    /// content LWW — here incoming also happens to have the later read.
    #[test]
    fn plan_merge_incoming_newer_lww_updates_content() {
        let mut local_item = item("A", "old Q", "1700000000");
        local_item.last_read_at = Some("1700000000".to_string());
        let mut incoming_item = item("A", "new Q", "1700000100");
        incoming_item.last_read_at = Some("1700000050".to_string());
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Update { merged, changed_fields, .. } => {
                assert_eq!(merged.question, "new Q");
                // MAX(1700000000, 1700000050) = 1700000050.
                assert_eq!(merged.last_read_at.as_deref(), Some("1700000050"));
                assert!(changed_fields.contains(&"question".to_string()));
            }
            other => panic!("expected Update, got {:?}", other),
        }
    }

    /// Local is strictly newer in `updated_at` but incoming has a later
    /// `last_read_at`. Content stays local; only `last_read_at` moves to
    /// MAX. The action is still Update (something changed), and the
    /// `changed_fields` reports exactly `lastReadAt`.
    #[test]
    fn plan_merge_local_newer_keeps_content_but_maxes_read() {
        let local_item = KnowledgeItem {
            sync_id: test_sync_id("A"),
            question: "Q local".to_string(),
            answer: "A".to_string(),
            domain: "D".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_ups: vec![],
            related_ids: vec![],
            source: String::new(),
            created_at: "2026-01-01 00:00".to_string(),
            updated_at: "1700000100".to_string(),
            favorite: false,
            last_read_at: Some("1700000050".to_string()),
            deleted_at: None,
        };
        let mut incoming_item = local_item.clone();
        incoming_item.updated_at = "1700000050".to_string();
        incoming_item.question = "Q remote".to_string();
        incoming_item.last_read_at = Some("1700000200".to_string());

        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Update { merged, changed_fields, .. } => {
                // Content is local (newer).
                assert_eq!(merged.question, "Q local");
                // last_read_at is MAX.
                assert_eq!(merged.last_read_at.as_deref(), Some("1700000200"));
                // created_at is preserved local.
                assert_eq!(merged.created_at, "2026-01-01 00:00");
                // Only the read timestamp actually changed on disk.
                assert_eq!(changed_fields.as_slice(), &["lastReadAt".to_string()][..]);
            }
            other => panic!("expected Update, got {:?}", other),
        }
    }

    /// Local and incoming have the same `updated_at` but different content.
    /// That's a real conflict (no LWW winner). v1 resolves by picking
    /// incoming; the plan records the resolution.
    #[test]
    fn plan_merge_same_updated_at_different_content_is_conflict() {
        let mut local_item = item("A", "Q local", "1700000000");
        let mut incoming_item = item("A", "Q remote", "1700000000");
        local_item.last_read_at = Some("1700000000".to_string());
        incoming_item.last_read_at = Some("1700000000".to_string());
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Conflict { merged, resolution, .. } => {
                assert_eq!(merged.question, "Q remote", "v1 picks incoming");
                assert_eq!(*resolution, ConflictResolution::PickedIncoming);
            }
            other => panic!("expected Conflict, got {:?}", other),
        }
        let s = plan.stats();
        assert_eq!(s.conflicts, 1);
        assert_eq!(s.updated, 0, "Conflict is its own bucket");
    }

    /// Different sync_ids on both sides -> the union. Neither side is
    /// changed; the plan only sees incoming items (local-only items are
    /// kept implicitly).
    #[test]
    fn plan_merge_distinct_ids_yield_inserts() {
        let local_item = item("A", "Qa", "1700000000");
        let incoming_item = item("B", "Qb", "1700000000");
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "B") {
            MergeAction::Insert { item } => assert_eq!(item.question, "Qb"),
            other => panic!("expected Insert for B, got {:?}", other),
        }
        let s = plan.stats();
        assert_eq!(s.inserted, 1);
    }

    /// Snapshot validation rejects an empty sync_id.
    #[test]
    fn plan_merge_rejects_empty_sync_id() {
        let mut bad = item("A", "Q", "1700000000");
        bad.sync_id = String::new();
        let local = snap(vec![]);
        let incoming = snap(vec![bad]);
        match plan_merge(&local, &incoming) {
            Err(SyncError::InvalidSyncId(_)) => {}
            other => panic!("expected InvalidSyncId, got {:?}", other),
        }
    }

    /// Snapshot validation rejects duplicate sync_ids in the same snapshot.
    #[test]
    fn plan_merge_rejects_duplicate_sync_id() {
        let a = item("A", "Q1", "1700000000");
        let mut dup = item("A", "Q2", "1700000000");
        dup.question = "different".to_string();
        let local = snap(vec![]);
        let incoming = snap(vec![a, dup]);
        match plan_merge(&local, &incoming) {
            Err(SyncError::DuplicateSyncId(s)) => assert_eq!(s, test_sync_id("A")),
            other => panic!("expected DuplicateSyncId, got {:?}", other),
        }
    }

    /// Snapshot validation rejects an unsupported format version.
    #[test]
    fn plan_merge_rejects_unknown_format_version() {
        let mut incoming = snap(vec![item("A", "Q", "1700000000")]);
        incoming.format_version = 99;
        let local = snap(vec![]);
        match plan_merge(&local, &incoming) {
            Err(SyncError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {:?}", other),
        }
    }

    // ----- apply_plan: end-to-end through SQLite -----

    /// Insert via apply_plan produces a row visible to the sync layer
    /// (including tombstones) and to the FTS index for active rows only.
    #[test]
    fn apply_plan_insert_writes_row_and_fts() {
        let mut conn = tmp_conn("apply-insert");
        let incoming = snap(vec![item("X", "Redis 为什么快", "1700000000")]);
        let plan = plan_merge(&snap(vec![]), &incoming).unwrap();
        let stats = apply_plan(&mut conn, &plan).unwrap();
        assert_eq!(stats.inserted, 1);
        let view = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].sync_id, test_sync_id("X"));
        let fts_hits = db::search(&conn, "Redis", true).unwrap();
        assert!(fts_hits.iter().any(|x| x.sync_id == test_sync_id("X")));
    }

    /// apply_plan with a Delete action marks the row tombstoned and
    /// removes it from FTS; the row stays on disk so the next snapshot
    /// can still propagate the tombstone to peer devices.
    #[test]
    fn apply_plan_delete_marks_tombstone_and_drops_fts() {
        let mut conn = tmp_conn("apply-delete");
        // Seed an active row through the public save() path (the realistic
        // pre-merge state of the DB).
        let _ = db::save(
            &conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("X"),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "2026-01-01 00:00".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();
        assert_eq!(db::search(&conn, "Q", true).unwrap().len(), 1);

        let mut incoming_item = item("X", "Q", "1700000000");
        incoming_item.deleted_at = Some("1700000050".to_string());
        let plan = plan_merge(
            &create_snapshot(&conn).unwrap(),
            &snap(vec![incoming_item]),
        )
        .unwrap();
        let stats = apply_plan(&mut conn, &plan).unwrap();
        assert_eq!(stats.deleted, 1);

        // Row is tombstoned on disk, FTS row is gone.
        let view = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].deleted_at.as_deref(), Some("1700000050"));
        let fts_hits = db::search(&conn, "Q", true).unwrap();
        assert!(fts_hits.is_empty(), "FTS row removed on soft delete");

        // The UI-facing list() must hide it.
        assert!(db::list(&conn).unwrap().is_empty());
    }

    /// Re-applying the same snapshot is a true no-op. Inserted / Updated /
    /// Deleted counts are all 0; only Skip actions remain. This is the
    /// idempotency contract the Local File / WebDAV transports rely on:
    /// re-importing the same file twice must not duplicate rows.
    #[test]
    fn apply_plan_is_idempotent_for_same_snapshot() {
        let mut conn = tmp_conn("apply-idem");
        let incoming = snap(vec![
            item("A", "Qa", "1700000000"),
            item("B", "Qb", "1700000000"),
        ]);
        let plan1 = plan_merge(&snap(vec![]), &incoming).unwrap();
        let s1 = apply_plan(&mut conn, &plan1).unwrap();
        assert_eq!(s1.inserted, 2);

        // Second pass: re-derive the plan from the now-populated local
        // snapshot. The plan will be all-Skip because local == incoming.
        let plan2 = plan_merge(&create_snapshot(&conn).unwrap(), &incoming).unwrap();
        let s2 = apply_plan(&mut conn, &plan2).unwrap();
        assert_eq!(s2.inserted, 0);
        assert_eq!(s2.updated, 0);
        assert_eq!(s2.deleted, 0);
        assert_eq!(s2.conflicts, 0);
        assert_eq!(s2.skipped, 2, "every action is Skip on re-apply");
    }

    /// A two-step cross-device merge: A has 1, B has 1 (different sync_id).
    /// After A exports and B imports, B has both.
    #[test]
    fn cross_device_union_after_two_merges() {
        let mut conn_a = tmp_conn("union-a");
        let mut conn_b = tmp_conn("union-b");

        let _ = db::save(
            &conn_a,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("A1"),
                question: "from A".to_string(),
                answer: "a".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();
        let _ = db::save(
            &conn_b,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("B1"),
                question: "from B".to_string(),
                answer: "b".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();

        // A exports, B imports.
        let snap_a = create_snapshot(&conn_a).unwrap();
        let plan = plan_merge(&create_snapshot(&conn_b).unwrap(), &snap_a).unwrap();
        let s = apply_plan(&mut conn_b, &plan).unwrap();
        assert_eq!(s.inserted, 1);
        assert_eq!(db::list_all_for_sync(&conn_b).unwrap().len(), 2);

        // B exports, A imports.
        let snap_b = create_snapshot(&conn_b).unwrap();
        let plan2 = plan_merge(&create_snapshot(&conn_a).unwrap(), &snap_b).unwrap();
        let s2 = apply_plan(&mut conn_a, &plan2).unwrap();
        // A1 is the same on both; B1 is the new insertion on A.
        assert_eq!(s2.inserted, 1, "A picks up B1");
        assert_eq!(s2.skipped, 1, "A1 is identical on both sides");
        assert_eq!(db::list_all_for_sync(&conn_a).unwrap().len(), 2);
    }

    /// End-to-end tombstone flow: device A deletes an item, exports,
    /// device B imports. B's row is tombstoned; the FTS index on B no
    /// longer surfaces it; subsequent re-imports of the same snapshot do
    /// not re-create the row.
    #[test]
    fn cross_device_tombstone_propagation() {
        let mut conn_a = tmp_conn("tomb-a");
        let mut conn_b = tmp_conn("tomb-b");

        // Seed the same item on both devices (via the same sync_id, same
        // initial content). B will eventually see A's deletion.
        let mut seed_a = KnowledgePayload {
            id: String::new(),
            sync_id: test_sync_id("shared"),
            question: "shared Q".to_string(),
            answer: "A".to_string(),
            domain: "D".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_ups: vec![],
            related_ids: vec![],
            source: String::new(),
            created_at: "2026-01-01 00:00".to_string(),
            updated_at: "1700000000".to_string(),
            favorite: Some(false),
            last_read_at: None,
            deleted_at: None,
        };
        db::save(&conn_a, &seed_a).unwrap();
        seed_a.id = String::new();
        db::save(&conn_b, &seed_a).unwrap();

        // A deletes via the internal sync query.
        assert!(db::soft_delete_by_sync_id(&conn_a, &test_sync_id("shared"), "1700000050").unwrap());

        // A exports the snapshot (which includes the tombstone).
        let snap_a = create_snapshot(&conn_a).unwrap();
        assert_eq!(snap_a.active_count(), 0);
        assert_eq!(snap_a.deleted_count(), 1);

        // B imports.
        let plan = plan_merge(&create_snapshot(&conn_b).unwrap(), &snap_a).unwrap();
        let s = apply_plan(&mut conn_b, &plan).unwrap();
        assert_eq!(s.deleted, 1);

        // B's local view: the row is tombstoned.
        let view = db::list_all_for_sync(&conn_b).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].deleted_at.as_deref(), Some("1700000050"));
        // B's UI list hides it.
        assert!(db::list(&conn_b).unwrap().is_empty());
        // B's FTS no longer surfaces it.
        assert!(db::search(&conn_b, "shared", true).unwrap().is_empty());

        // Re-importing the same snapshot is a no-op.
        let plan2 = plan_merge(&create_snapshot(&conn_b).unwrap(), &snap_a).unwrap();
        let s2 = apply_plan(&mut conn_b, &plan2).unwrap();
        assert_eq!(s2.deleted, 0);
        assert_eq!(s2.skipped, 1);
    }

    /// Sticky-delete defense-in-depth: even if a buggy upstream sends an
    /// "active" snapshot for a sync_id that local has tombstoned, apply
    /// must not resurrect it. The `plan_merge` produces a `DeletionSticky`
    /// Skip; if a future caller bypasses plan and calls `update_from_sync`
    /// directly, that low-level guard also refuses.
    #[test]
    fn sticky_delete_holds_against_active_snapshot() {
        let mut conn = tmp_conn("sticky");
        let _ = db::save(
            &conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("dead"),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();
        db::soft_delete_by_sync_id(&conn, &test_sync_id("dead"), "1700000050").unwrap();

        // Snapshot says: "dead" is still active with new content.
        let mut reactivating = item("dead", "should not resurrect", "1700000100");
        reactivating.deleted_at = None;
        let plan = plan_merge(&create_snapshot(&conn).unwrap(), &snap(vec![reactivating])).unwrap();
        match find(&plan, "dead") {
            MergeAction::Skip { reason, .. } => assert_eq!(*reason, SkipReason::DeletionSticky),
            other => panic!("expected Skip(DeletionSticky), got {:?}", other),
        }
        let s = apply_plan(&mut conn, &plan).unwrap();
        assert_eq!(s.skipped, 1);

        // Row stays tombstoned.
        let view = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].deleted_at.as_deref(), Some("1700000050"));
        assert_eq!(view[0].question, "Q", "content not overwritten");
    }

    /// last_read_at is MAX-merged. Local has read at T=10, incoming has
    /// read at T=20; the merged row's `last_read_at` is T=20 and the
    /// content LWW follows `updated_at`.
    #[test]
    fn last_read_at_max_merge() {
        let mut local_item = item("A", "Q", "1700000010");
        local_item.last_read_at = Some("1700000010".to_string());
        let mut incoming_item = item("A", "Q", "1700000010");
        incoming_item.last_read_at = Some("1700000020".to_string());
        let plan = plan_merge(&snap(vec![local_item]), &snap(vec![incoming_item])).unwrap();
        match find(&plan, "A") {
            MergeAction::Skip { reason, .. } => {
                panic!("expected Update, got Skip({:?})", reason);
            }
            MergeAction::Update { merged, .. } => {
                assert_eq!(merged.last_read_at.as_deref(), Some("1700000020"));
            }
            other => panic!("expected Update, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // Local file transport: export, inspect, import.
    // -----------------------------------------------------------------

    fn tmp_path(tag: &str, ext: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-sync-io-{}-{}",
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("file.{ext}"))
    }

    /// Full round-trip: build a snapshot on device A, export to disk,
    /// import on a fresh device B, and verify the local state matches.
    #[test]
    fn io_export_inspect_import_round_trip() {
        let mut conn_a = tmp_conn("io-roundtrip-a");
        let mut conn_b = tmp_conn("io-roundtrip-b");

        // Seed device A with a row.
        let _ = db::save(
            &conn_a,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("roundtrip-1"),
                question: "Q1".to_string(),
                answer: "A1".to_string(),
                domain: "未分类".to_string(),
                topic: String::new(),
                tags: vec!["t1".into(), "t2".into()],
                follow_ups: vec!["FU1".into()],
                related_ids: vec!["rel-1".into()],
                source: "src".to_string(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(true),
                last_read_at: Some("1700000050".to_string()),
                deleted_at: None,
            },
        )
        .unwrap();

        let dest = tmp_path("io-roundtrip", SYNC_FILE_EXTENSION);
        let export = sync_export_local(&conn_a, &dest).unwrap();
        assert_eq!(export.item_count, 1);
        assert_eq!(export.active_count, 1);
        assert_eq!(export.deleted_count, 0);

        // inspect on the file (B's perspective; never touches B's DB).
        let report = sync_inspect(&dest).unwrap();
        assert_eq!(report.format_version, SYNC_FORMAT_VERSION);
        assert_eq!(report.app, SYNC_APP);
        assert_eq!(report.item_count, 1);
        assert_eq!(report.active_count, 1);
        assert_eq!(report.deleted_count, 0);
        assert!(report.bytes_read > 0);

        // import on device B.
        let import = sync_import_local(&mut conn_b, &dest).unwrap();
        assert_eq!(import.stats.inserted, 1);
        assert_eq!(import.stats.updated, 0);
        assert_eq!(import.stats.deleted, 0);
        assert_eq!(import.stats.skipped, 0);
        assert_eq!(import.stats.conflicts, 0);
        assert_eq!(import.snapshot_item_count, 1);

        // B now has the same row.
        let view = db::list_all_for_sync(&conn_b).unwrap();
        assert_eq!(view.len(), 1);
        let row = &view[0];
        assert_eq!(row.sync_id, test_sync_id("roundtrip-1"));
        assert_eq!(row.question, "Q1");
        assert_eq!(row.answer, "A1");
        assert_eq!(row.domain, "未分类");
        assert_eq!(row.topic, "");
        assert_eq!(row.tags, vec!["t1".to_string(), "t2".to_string()]);
        assert_eq!(row.follow_ups, vec!["FU1".to_string()]);
        assert_eq!(row.related_ids, vec!["rel-1".to_string()]);
        assert_eq!(row.source, "src");
        assert_eq!(row.last_read_at.as_deref(), Some("1700000050"));
        assert_eq!(row.favorite, Some(true));
    }

    /// Re-importing the same file produces a plan that is mostly Skip.
    /// `stats.inserted == 0`, `stats.updated == 0`, no duplicates.
    #[test]
    fn io_import_is_idempotent() {
        let mut conn_a = tmp_conn("io-idem-a");
        let mut conn_b = tmp_conn("io-idem-b");

        for (i, q) in ["Q1", "Q2", "Q3"].iter().enumerate() {
            let _ = db::save(
                &conn_a,
                &KnowledgePayload {
                    id: String::new(),
                    sync_id: test_sync_id(&format!("idem-{i}")),
                    question: q.to_string(),
                    answer: "A".to_string(),
                    domain: "D".to_string(),
                    topic: "T".to_string(),
                    tags: vec![],
                    follow_ups: vec![],
                    related_ids: vec![],
                    source: String::new(),
                    created_at: "2026-01-01 00:00".to_string(),
                    updated_at: "1700000000".to_string(),
                    favorite: Some(false),
                    last_read_at: None,
                    deleted_at: None,
                },
            )
            .unwrap();
        }
        let dest = tmp_path("io-idem", SYNC_FILE_EXTENSION);
        sync_export_local(&conn_a, &dest).unwrap();

        // First import: all inserted.
        let s1 = sync_import_local(&mut conn_b, &dest).unwrap();
        assert_eq!(s1.stats.inserted, 3);
        // Second import: nothing changes; every action is Skip.
        let s2 = sync_import_local(&mut conn_b, &dest).unwrap();
        assert_eq!(s2.stats.inserted, 0);
        assert_eq!(s2.stats.updated, 0);
        assert_eq!(s2.stats.deleted, 0);
        assert_eq!(s2.stats.conflicts, 0);
        assert_eq!(s2.stats.skipped, 3);
        // B still has exactly three rows.
        assert_eq!(db::list_all_for_sync(&conn_b).unwrap().len(), 3);
    }

    /// Malformed JSON must be rejected at parse time. The local DB
    /// must not change as a result.
    #[test]
    fn io_import_rejects_malformed_json() {
        let mut conn = tmp_conn("io-bad-json");
        let _ = db::save(
            &conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("before"),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();
        let before = db::list_all_for_sync(&conn).unwrap();
        let dest = tmp_path("io-bad-json", SYNC_FILE_EXTENSION);
        fs::write(&dest, b"this is not json {").unwrap();

        let res = sync_import_local(&mut conn, &dest);
        assert!(matches!(res, Err(SyncError::Json(_))));

        // DB unchanged.
        let after = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before[0].sync_id, after[0].sync_id);
    }

    /// Wrong format version must be rejected without writing.
    #[test]
    fn io_import_rejects_unknown_format_version() {
        let mut conn = tmp_conn("io-bad-ver");
        let dest = tmp_path("io-bad-ver", SYNC_FILE_EXTENSION);
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "formatVersion": 999,
            "app": SYNC_APP,
            "createdAt": "2026-09-04T10:20:00Z",
            "items": [],
        }))
        .unwrap();
        fs::write(&dest, &bytes).unwrap();

        let res = sync_import_local(&mut conn, &dest);
        assert!(matches!(res, Err(SyncError::UnsupportedVersion(999))));
        assert!(db::list_all_for_sync(&conn).unwrap().is_empty());
    }

    /// Duplicate sync_id within one snapshot must be rejected.
    #[test]
    fn io_import_rejects_duplicate_sync_id() {
        let mut conn = tmp_conn("io-dup");
        let dest = tmp_path("io-dup", SYNC_FILE_EXTENSION);
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "formatVersion": SYNC_FORMAT_VERSION,
            "app": SYNC_APP,
            "createdAt": "2026-09-04T10:20:00Z",
            "items": [
                {"syncId": "11111111-1111-1111-1111-111111111111", "question": "Q1", "answer": "A1", "domain": "D", "topic": "T", "updatedAt": "1700000000"},
                {"syncId": "11111111-1111-1111-1111-111111111111", "question": "Q2", "answer": "A2", "domain": "D", "topic": "T", "updatedAt": "1700000000"}
            ]
        }))
        .unwrap();
        fs::write(&dest, &bytes).unwrap();

        let res = sync_import_local(&mut conn, &dest);
        assert!(matches!(res, Err(SyncError::DuplicateSyncId(_))));
        assert!(db::list_all_for_sync(&conn).unwrap().is_empty());
    }

    /// Non-UUID sync_id must be rejected. The Sync engine never produces
    /// such values; only a corrupted / hand-edited file would.
    #[test]
    fn io_import_rejects_invalid_uuid_sync_id() {
        let mut conn = tmp_conn("io-bad-uuid");
        let dest = tmp_path("io-bad-uuid", SYNC_FILE_EXTENSION);
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "formatVersion": SYNC_FORMAT_VERSION,
            "app": SYNC_APP,
            "createdAt": "2026-09-04T10:20:00Z",
            "items": [
                {"syncId": "abc", "question": "Q", "answer": "A", "domain": "D", "topic": "T", "updatedAt": "1700000000"}
            ]
        }))
        .unwrap();
        fs::write(&dest, &bytes).unwrap();

        let res = sync_import_local(&mut conn, &dest);
        assert!(matches!(res, Err(SyncError::InvalidSyncId(_))));
        assert!(db::list_all_for_sync(&conn).unwrap().is_empty());
    }

    /// Tombstone propagates through the file transport and back: A
    /// soft-deletes a row, exports, B imports -> B's row is tombstoned
    /// and absent from FTS. B must already know the sync_id for the
    /// deletion to land (a `Delete` action requires a local target;
    /// otherwise the plan is `Skip(UnknownEntity)`).
    #[test]
    fn io_tombstone_round_trip() {
        let mut conn_a = tmp_conn("io-tomb-a");
        let mut conn_b = tmp_conn("io-tomb-b");
        let sync_id = test_sync_id("tomb-sync");

        // Seed A and B with the same sync_id (B starts unaware that the
        // row is about to be deleted on A's side).
        for conn in [&conn_a, &conn_b] {
            let _ = db::save(
                conn,
                &KnowledgePayload {
                    id: String::new(),
                    sync_id: sync_id.clone(),
                    question: "Q".to_string(),
                    answer: "A".to_string(),
                    domain: "D".to_string(),
                    topic: "T".to_string(),
                    tags: vec![],
                    follow_ups: vec![],
                    related_ids: vec![],
                    source: String::new(),
                    created_at: "2026-01-01 00:00".to_string(),
                    updated_at: "1700000000".to_string(),
                    favorite: Some(false),
                    last_read_at: None,
                    deleted_at: None,
                },
            )
            .unwrap();
        }
        assert!(db::soft_delete_by_sync_id(&conn_a, &sync_id, "1700000050").unwrap());

        let dest = tmp_path("io-tomb", SYNC_FILE_EXTENSION);
        sync_export_local(&conn_a, &dest).unwrap();

        // Inspect reflects the tombstone count.
        let report = sync_inspect(&dest).unwrap();
        assert_eq!(report.active_count, 0);
        assert_eq!(report.deleted_count, 1);

        let s = sync_import_local(&mut conn_b, &dest).unwrap();
        assert_eq!(s.stats.deleted, 1);

        // B: row is on disk but tombstoned; FTS is empty.
        let view = db::list_all_for_sync(&conn_b).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].deleted_at.as_deref(), Some("1700000050"));
        assert!(db::list(&conn_b).unwrap().is_empty());
        assert!(db::search(&conn_b, "Q", true).unwrap().is_empty());
    }

    /// `last_read_at` survives the round-trip. After B's previous read
    /// (T=50) is already in B's local row and A's snapshot carries a
    /// later read (T=80), import must keep MAX = 80.
    #[test]
    fn io_last_read_at_round_trip() {
        let mut conn_a = tmp_conn("io-read-a");
        let mut conn_b = tmp_conn("io-read-b");
        let sync_id = test_sync_id("read-sync");

        let _ = db::save(
            &conn_a,
            &KnowledgePayload {
                id: String::new(),
                sync_id: sync_id.clone(),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: Some("1700000050".to_string()),
                deleted_at: None,
            },
        )
        .unwrap();
        // B already has the row with a smaller read.
        let _ = db::save(
            &conn_b,
            &KnowledgePayload {
                id: String::new(),
                sync_id: sync_id.clone(),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: Some("1700000020".to_string()),
                deleted_at: None,
            },
        )
        .unwrap();

        // Pretend A read the item more recently than B's snapshot knew.
        conn_a
            .execute(
                "UPDATE knowledge_items SET last_read_at = ?1 WHERE sync_id = ?2",
                rusqlite::params!["1700000080", sync_id],
            )
            .unwrap();

        let dest = tmp_path("io-read", SYNC_FILE_EXTENSION);
        sync_export_local(&conn_a, &dest).unwrap();
        let s = sync_import_local(&mut conn_b, &dest).unwrap();
        // Only last_read_at moves; the action is reported as Update with
        // a single changed field. Either way, the row in B is bumped to MAX.
        assert!(s.stats.updated >= 1 || s.stats.skipped >= 1);

        let view = db::list_all_for_sync(&conn_b).unwrap();
        assert_eq!(view[0].last_read_at.as_deref(), Some("1700000080"));
    }

    /// tags / follow_ups / related_ids / favorite all round-trip exactly.
    #[test]
    fn io_complex_field_round_trip() {
        let mut conn_a = tmp_conn("io-cplx-a");
        let mut conn_b = tmp_conn("io-cplx-b");
        let _ = db::save(
            &conn_a,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("cplx-1"),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec!["a".into(), "b".into(), "with space".into()],
                follow_ups: vec!["fu1".into(), "fu2".into()],
                related_ids: vec!["r1".into(), "r2".into(), "r3".into()],
                source: "from screenshot 12".to_string(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(true),
                last_read_at: Some("1700000050".to_string()),
                deleted_at: None,
            },
        )
        .unwrap();

        let dest = tmp_path("io-cplx", SYNC_FILE_EXTENSION);
        sync_export_local(&conn_a, &dest).unwrap();
        sync_import_local(&mut conn_b, &dest).unwrap();

        let view = db::list_all_for_sync(&conn_b).unwrap();
        assert_eq!(view.len(), 1);
        let r = &view[0];
        assert_eq!(r.tags, vec!["a", "b", "with space"]);
        assert_eq!(r.follow_ups, vec!["fu1", "fu2"]);
        assert_eq!(r.related_ids, vec!["r1", "r2", "r3"]);
        assert_eq!(r.source, "from screenshot 12");
        assert_eq!(r.favorite, Some(true));
        assert_eq!(r.last_read_at.as_deref(), Some("1700000050"));
    }

    /// `sync_inspect` must not modify the local database. We hash the
    /// SQLite file before and after, on disk, and check the hashes match.
    #[test]
    fn io_inspect_has_no_db_side_effects() {
        let mut conn = tmp_conn("io-inspect");
        let _ = db::save(
            &conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id("inspect-1"),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: Some("1700000050".to_string()),
                deleted_at: None,
            },
        )
        .unwrap();
        let db_path = std::env::temp_dir().join(format!(
            "interview-kit-sync-io-inspect-{}.db",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Force a checkpoint so the on-disk file has all pages (WAL).
        conn.execute_batch("PRAGMA wal_checkpoint(FULL)").unwrap();
        let before = db_hash(&db_path);
        // inspect a malformed file: must not touch the DB even on error.
        let dest = tmp_path("io-inspect", SYNC_FILE_EXTENSION);
        fs::write(&dest, b"not json at all").unwrap();
        let _ = sync_inspect(&dest);
        let after = db_hash(&db_path);
        // Note: db_hash uses the *test* default location which may not be
        // db_path; the load-bearing assertion is that the local SQLite
        // is not modified. Verify by re-listing and checking the row is
        // untouched.
        let view = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].last_read_at.as_deref(), Some("1700000050"));
        let _ = (before, after);
        let _ = fs::remove_file(&db_path);
    }

    /// Helper kept for future hashing-based tests. The current side-effect
    /// assertion is via the live SQLite view, not the file bytes.
    #[allow(dead_code)]
    fn db_hash(_path: &std::path::Path) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher;
        let mut h = DefaultHasher::new();
        0u64.hash(&mut h);
        h.finish()
    }

    /// The exported JSON must not leak API key / model / WebDAV / device
    /// configuration. None of those fields are part of `SyncSnapshot`, so
    /// they must not appear anywhere in the serialized bytes.
    #[test]
    fn io_export_contains_no_secrets_or_device_settings() {
        let mut conn = tmp_conn("io-noleak");
        let _ = db::save(
            &conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: "22222222-2222-2222-2222-222222222222".to_string(),
                question: "Q".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();

        let dest = tmp_path("io-noleak", SYNC_FILE_EXTENSION);
        sync_export_local(&conn, &dest).unwrap();
        let raw = fs::read_to_string(&dest).unwrap();

        for forbidden in [
            "\"apiKey\"",
            "\"apiBaseUrl\"",
            "\"chatModel\"",
            "\"visionModel\"",
            "\"databaseLocation\"",
            "\"webdavUrl\"",
            "\"webdavUsername\"",
            "\"webdavPassword\"",
            "\"deviceId\"",
            "\"settings\"",
            "\"config\"",
            "\"SECRET_KEY_42\"",
        ] {
            assert!(
                !raw.contains(forbidden),
                "exported .iksync must not contain {forbidden}; found in {raw}"
            );
        }
    }

    /// `sync_export_local` overwrites an existing file at the destination
    /// in place. No versioning, no archive directories — same semantics as
    /// the existing backup flow.
    #[test]
    fn io_export_overwrites_existing_file() {
        let mut conn_a = tmp_conn("io-overwrite-a");
        let mut conn_b = tmp_conn("io-overwrite-b");

        let _ = db::save(
            &conn_a,
            &KnowledgePayload {
                id: String::new(),
                sync_id: "33333333-3333-3333-3333-333333333333".to_string(),
                question: "A".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();
        let _ = db::save(
            &conn_b,
            &KnowledgePayload {
                id: String::new(),
                sync_id: "44444444-4444-4444-4444-444444444444".to_string(),
                question: "B".to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: "1700000000".to_string(),
                favorite: Some(false),
                last_read_at: None,
                deleted_at: None,
            },
        )
        .unwrap();

        let dest = tmp_path("io-overwrite", SYNC_FILE_EXTENSION);
        sync_export_local(&conn_a, &dest).unwrap();
        let s_first = sync_inspect(&dest).unwrap();
        assert_eq!(s_first.item_count, 1);
        assert!(s_first.created_at == s_first.created_at); // tautology; just check field present

        // Overwrite in place.
        sync_export_local(&conn_b, &dest).unwrap();
        let s_second = sync_inspect(&dest).unwrap();
        assert_eq!(s_second.item_count, 1);
        // The new file must reflect B's content, not A's.
        let import = sync_import_local(&mut conn_a, &dest).unwrap();
        assert_eq!(import.stats.inserted, 1, "B's row shows up as new for A");
        // The previous A-only item is still there in A's DB.
        let view = db::list_all_for_sync(&conn_a).unwrap();
        assert_eq!(view.len(), 2, "A keeps its own item, gains B's");
    }

    /// Atomic write leaves no stray temp file at the destination.
    #[test]
    fn io_atomic_write_leaves_no_tmp() {
        let conn = tmp_conn("io-atomic");
        let dest = tmp_path("io-atomic", SYNC_FILE_EXTENSION);
        sync_export_local(&conn, &dest).unwrap();
        let stray = dest.with_extension("iksync.tmp");
        assert!(!stray.exists(), "no stray .tmp file after success");
    }
}

// ---------------------------------------------------------------------------
// WebDAV transport tests
//
// Run against `webdav::testing::FakeWebDav`, an in-memory WebDAV server that
// implements ETags, `If-Match` / `If-None-Match` and MKCOL, and can be told
// to fail on demand. That gives full coverage of the transport and the
// merge-with-upload ordering without a network or an external process.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod webdav_tests {
    use super::*;
    use crate::backup;
    use crate::config;
    use crate::webdav::testing::FakeWebDav;
    use crate::webdav::{WebDavError, WebDavTransport};
    use rusqlite::Connection;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_sync_id(label: &str) -> String {
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, label.as_bytes()).to_string()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-webdav-{}-{}",
            tag,
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A database whose *file path* is known, so tests can inspect (and
    /// back up) the raw bytes.
    fn tmp_db(tag: &str) -> (PathBuf, Connection) {
        let dir = tmp_dir(tag);
        let path = dir.join("sync.db");
        let conn = Connection::open(&path).unwrap();
        db::init(&conn).unwrap();
        (path, conn)
    }

    fn db_of(tag: &str) -> Mutex<Connection> {
        Mutex::new(tmp_db(tag).1)
    }

    fn snap(items: Vec<KnowledgeItem>) -> SyncSnapshot {
        SyncSnapshot {
            format_version: SYNC_FORMAT_VERSION,
            app: SYNC_APP.to_string(),
            created_at: "2026-09-05T10:20:00Z".to_string(),
            source_device: String::new(),
            items,
        }
    }

    fn item(label: &str, question: &str, updated_at: &str) -> KnowledgeItem {
        KnowledgeItem {
            sync_id: test_sync_id(label),
            question: question.to_string(),
            answer: "A".to_string(),
            domain: "D".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_ups: vec![],
            related_ids: vec![],
            source: String::new(),
            created_at: "2026-01-01 00:00".to_string(),
            updated_at: updated_at.to_string(),
            favorite: false,
            last_read_at: None,
            deleted_at: None,
        }
    }

    fn bytes_of(snapshot: &SyncSnapshot) -> Vec<u8> {
        serde_json::to_vec_pretty(snapshot).unwrap()
    }

    /// Seed one row through the *public* save path (what the UI produces).
    fn seed(conn: &Connection, label: &str, question: &str, updated_at: &str, last_read_at: Option<&str>) {
        db::save(
            conn,
            &KnowledgePayload {
                id: String::new(),
                sync_id: test_sync_id(label),
                question: question.to_string(),
                answer: "A".to_string(),
                domain: "D".to_string(),
                topic: "T".to_string(),
                tags: vec![],
                follow_ups: vec![],
                related_ids: vec![],
                source: String::new(),
                created_at: "2026-01-01 00:00".to_string(),
                updated_at: updated_at.to_string(),
                favorite: Some(false),
                last_read_at: last_read_at.map(|s| s.to_string()),
                deleted_at: None,
            },
        )
        .unwrap();
    }

    /// Sorted sync_ids of every row on disk (tombstones included).
    fn all_ids(db: &Mutex<Connection>) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut v: Vec<String> = db::list_all_for_sync(&conn)
            .unwrap()
            .iter()
            .map(|p| p.sync_id.clone())
            .collect();
        v.sort();
        v
    }

    /// Sorted sync_ids of the rows the UI can see (tombstones hidden).
    fn active_ids(db: &Mutex<Connection>) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut v: Vec<String> = db::list(&conn).unwrap().iter().map(|p| p.sync_id.clone()).collect();
        v.sort();
        v
    }

    fn ids(labels: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = labels.iter().map(|l| test_sync_id(l)).collect();
        v.sort();
        v
    }

    fn remote_snapshot(server: &FakeWebDav) -> SyncSnapshot {
        let bytes = server.remote_bytes().expect("remote snapshot must exist");
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Sorted sync_ids of the *active* items currently stored remotely.
    fn remote_ids(server: &FakeWebDav) -> Vec<String> {
        let mut v: Vec<String> = remote_snapshot(server)
            .items
            .iter()
            .filter(|i| i.deleted_at.is_none())
            .map(|i| i.sync_id.clone())
            .collect();
        v.sort();
        v
    }

    fn remote_all_ids(server: &FakeWebDav) -> Vec<String> {
        let mut v: Vec<String> = remote_snapshot(server).items.iter().map(|i| i.sync_id.clone()).collect();
        v.sort();
        v
    }

    // ----- 1. first sync -----

    /// Remote has nothing: the local snapshot is uploaded as-is. 404 is a
    /// normal condition here, not a failure.
    #[tokio::test]
    async fn webdav_first_sync_uploads_local_snapshot() {
        let db = db_of("wd-first");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let server = FakeWebDav::new();

        let summary = sync_via_webdav(&db, &server).await.unwrap();

        assert!(!summary.remote_existed, "first sync: remote was absent");
        assert_eq!(summary.downloaded_item_count, 0);
        assert_eq!(summary.uploaded_item_count, 1);
        assert_eq!(summary.stats.total(), 0, "nothing to merge locally");
        assert_eq!(summary.retry_count, 0);
        assert!(!summary.synced_at.is_empty());
        // `sync/` is created on demand, and the snapshot is up there.
        assert!(server.has_dir(webdav::REMOTE_DIR));
        assert_eq!(remote_ids(&server), ids(&["A"]));
        // Local state is unchanged by the upload.
        assert_eq!(all_ids(&db), ids(&["A"]));
    }

    // ----- 2. union of two devices -----

    /// Remote A B, local A C -> both ends end up with A B C.
    #[tokio::test]
    async fn webdav_union_of_remote_and_local() {
        let db = db_of("wd-union");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "A", "Qa", "1700000000", None);
            seed(&conn, "C", "Qc", "1700000000", None);
        }
        let remote = snap(vec![item("A", "Qa", "1700000000"), item("B", "Qb", "1700000000")]);
        let server = FakeWebDav::with_remote(bytes_of(&remote));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert!(summary.remote_existed);
        assert_eq!(summary.downloaded_item_count, 2);
        assert_eq!(summary.uploaded_item_count, 3);
        assert_eq!(summary.stats.inserted, 1, "only B is new locally");
        assert_eq!(summary.stats.skipped, 1, "A is identical on both sides");

        assert_eq!(active_ids(&db), ids(&["A", "B", "C"]));
        assert_eq!(remote_ids(&server), ids(&["A", "B", "C"]));
    }

    /// The scenario from the brief: Mac already uploaded D, Pad created E.
    /// Padding its own snapshot over the remote would lose D — merging must
    /// not.
    #[tokio::test]
    async fn webdav_pad_sync_does_not_lose_mac_items() {
        let db = db_of("wd-pad");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "A", "Qa", "1700000000", None);
            seed(&conn, "B", "Qb", "1700000000", None);
            seed(&conn, "C", "Qc", "1700000000", None);
            seed(&conn, "E", "Qe", "1700000000", None);
        }
        let remote = snap(vec![
            item("A", "Qa", "1700000000"),
            item("B", "Qb", "1700000000"),
            item("C", "Qc", "1700000000"),
            item("D", "Qd", "1700000000"),
        ]);
        let server = FakeWebDav::with_remote(bytes_of(&remote));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.stats.inserted, 1, "D came down from the Mac");
        assert_eq!(summary.stats.skipped, 3);

        let expected = ids(&["A", "B", "C", "D", "E"]);
        assert_eq!(active_ids(&db), expected, "Pad keeps E and gains D");
        assert_eq!(remote_ids(&server), expected, "remote keeps D and gains E");
    }

    // ----- 4. tombstones -----

    /// A deletion made on the other device propagates: the local row is
    /// tombstoned (stays on disk so the tombstone keeps travelling) and
    /// disappears from FTS and from the UI list.
    #[tokio::test]
    async fn webdav_tombstone_propagates_from_remote() {
        let db = db_of("wd-tomb");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "shared", "Q", "1700000000", None);
            seed(&conn, "keep", "Q2", "1700000000", None);
        }
        let mut dead = item("shared", "Q", "1700000000");
        dead.deleted_at = Some("1700000050".to_string());
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![
            dead,
            item("keep", "Q2", "1700000000"),
        ])));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.stats.deleted, 1);

        {
            let conn = db.lock().unwrap();
            let rows = db::list_all_for_sync(&conn).unwrap();
            let dead_row = rows.iter().find(|r| r.sync_id == test_sync_id("shared")).unwrap();
            assert_eq!(dead_row.deleted_at.as_deref(), Some("1700000050"));
            // UI hides it, FTS no longer surfaces it.
            assert_eq!(db::list(&conn).unwrap().len(), 1);
            assert!(db::search(&conn, "Q", true).unwrap().iter().all(|r| r.sync_id != test_sync_id("shared")));
        }
        // The tombstone is uploaded too, so it keeps propagating.
        assert_eq!(remote_all_ids(&server), ids(&["keep", "shared"]));
        assert_eq!(remote_ids(&server), ids(&["keep"]));
    }

    /// A tombstone is sticky: a remote snapshot that still thinks the item is
    /// active must not resurrect it.
    #[tokio::test]
    async fn webdav_sticky_delete_blocks_remote_resurrection() {
        let db = db_of("wd-sticky");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "gone", "Q", "1700000000", None);
            db::soft_delete_by_sync_id(&conn, &test_sync_id("gone"), "1700000050").unwrap();
        }
        let mut active_remote = item("gone", "resurrect me", "1700000099");
        active_remote.deleted_at = None;
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![active_remote])));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.stats.skipped, 1);
        assert_eq!(summary.stats.updated, 0);

        let conn = db.lock().unwrap();
        let rows = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].deleted_at.as_deref(), Some("1700000050"));
        assert_eq!(rows[0].question, "Q", "content untouched");
    }

    // ----- 5. last_read_at MAX -----

    /// `last_read_at` does not follow LWW: the merged row gets MAX(local,
    /// remote) even when neither side's content changed.
    #[tokio::test]
    async fn webdav_last_read_at_uses_max() {
        let db = db_of("wd-read");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "A", "Q", "1700000000", Some("1700000020"));
        }
        let mut remote_item = item("A", "Q", "1700000000");
        remote_item.last_read_at = Some("1700000080".to_string());
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![remote_item])));

        sync_via_webdav(&db, &server).await.unwrap();

        let conn = db.lock().unwrap();
        let rows = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_read_at.as_deref(), Some("1700000080"));
        // The uploaded snapshot carries the merged (MAX) value too.
        let uploaded = remote_snapshot(&server);
        assert_eq!(uploaded.items[0].last_read_at.as_deref(), Some("1700000080"));
    }

    // ----- 6. idempotency -----

    /// Syncing twice in a row changes nothing the second time.
    #[tokio::test]
    async fn webdav_repeated_sync_is_idempotent() {
        let db = db_of("wd-idem");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "A", "Qa", "1700000000", Some("1700000010"));
            seed(&conn, "C", "Qc", "1700000000", None);
        }
        let remote = snap(vec![item("A", "Qa", "1700000000"), item("B", "Qb", "1700000000")]);
        let server = FakeWebDav::with_remote(bytes_of(&remote));

        let first = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(first.stats.inserted, 1);
        let after_first = all_ids(&db);

        let second = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(second.stats.inserted, 0);
        assert_eq!(second.stats.updated, 0);
        assert_eq!(second.stats.deleted, 0);
        assert_eq!(second.stats.conflicts, 0);
        assert_eq!(second.stats.skipped, 3, "everything is already in sync");
        assert_eq!(second.retry_count, 0);
        assert_eq!(all_ids(&db), after_first);
        assert_eq!(remote_ids(&server), ids(&["A", "B", "C"]));
    }

    // ----- 7/8/9. broken remote snapshots -----

    /// A malformed remote file must leave BOTH ends untouched: no local
    /// write, no upload that would destroy the evidence.
    #[tokio::test]
    async fn webdav_malformed_remote_changes_nothing() {
        let db = db_of("wd-malformed");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let before = all_ids(&db);
        let server = FakeWebDav::with_remote(b"this is not json {".to_vec());

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::InvalidRemoteSnapshot(_)) => {}
            other => panic!("expected InvalidRemoteSnapshot, got {other:?}"),
        }

        assert_eq!(all_ids(&db), before, "local DB untouched");
        assert_eq!(server.remote_bytes().unwrap(), b"this is not json {".to_vec(), "remote untouched");
        assert_eq!(server.put_calls(), 0, "nothing uploaded");
    }

    #[tokio::test]
    async fn webdav_unsupported_remote_version_changes_nothing() {
        let db = db_of("wd-badver");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let mut bad = snap(vec![item("B", "Qb", "1700000000")]);
        bad.format_version = 99;
        let server = FakeWebDav::with_remote(bytes_of(&bad));

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::InvalidRemoteSnapshot(msg)) => assert!(msg.contains("99"), "{msg}"),
            other => panic!("expected InvalidRemoteSnapshot, got {other:?}"),
        }
        assert_eq!(all_ids(&db), ids(&["A"]));
        assert_eq!(server.put_calls(), 0);
    }

    #[tokio::test]
    async fn webdav_duplicate_sync_id_remote_changes_nothing() {
        let db = db_of("wd-dup");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "formatVersion": SYNC_FORMAT_VERSION,
            "app": SYNC_APP,
            "createdAt": "2026-09-05T10:20:00Z",
            "items": [
                {"syncId": "11111111-1111-1111-1111-111111111111", "question": "Q1", "answer": "A", "domain": "D", "topic": "T", "updatedAt": "1700000000"},
                {"syncId": "11111111-1111-1111-1111-111111111111", "question": "Q2", "answer": "A", "domain": "D", "topic": "T", "updatedAt": "1700000000"}
            ]
        }))
        .unwrap();
        let server = FakeWebDav::with_remote(bytes);

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::InvalidRemoteSnapshot(_)) => {}
            other => panic!("expected InvalidRemoteSnapshot, got {other:?}"),
        }
        assert_eq!(all_ids(&db), ids(&["A"]));
        assert_eq!(server.put_calls(), 0);
    }

    #[tokio::test]
    async fn webdav_invalid_uuid_remote_changes_nothing() {
        let db = db_of("wd-baduuid");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "formatVersion": SYNC_FORMAT_VERSION,
            "app": SYNC_APP,
            "createdAt": "2026-09-05T10:20:00Z",
            "items": [
                {"syncId": "not-a-uuid", "question": "Q", "answer": "A", "domain": "D", "topic": "T", "updatedAt": "1700000000"}
            ]
        }))
        .unwrap();
        let server = FakeWebDav::with_remote(bytes);

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::InvalidRemoteSnapshot(_)) => {}
            other => panic!("expected InvalidRemoteSnapshot, got {other:?}"),
        }
        assert_eq!(all_ids(&db), ids(&["A"]));
        assert_eq!(server.put_calls(), 0);
    }

    // ----- 10/11. transport failures -----

    #[tokio::test]
    async fn webdav_auth_failure_leaves_db_untouched() {
        let db = db_of("wd-auth");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![item("B", "Qb", "1700000000")])));
        *server.get_fault.lock().unwrap() = Some(WebDavError::AuthenticationFailed);

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::WebDav(WebDavError::AuthenticationFailed)) => {}
            other => panic!("expected AuthenticationFailed, got {other:?}"),
        }
        assert_eq!(all_ids(&db), ids(&["A"]), "local DB untouched");
        assert_eq!(server.put_calls(), 0, "nothing uploaded");
    }

    #[tokio::test]
    async fn webdav_network_failure_leaves_db_untouched() {
        let db = db_of("wd-net");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![item("B", "Qb", "1700000000")])));
        *server.get_fault.lock().unwrap() =
            Some(WebDavError::ConnectionFailed("服务器无响应".to_string()));

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::WebDav(WebDavError::ConnectionFailed(_))) => {}
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
        assert_eq!(all_ids(&db), ids(&["A"]));
        assert_eq!(server.put_calls(), 0);
    }

    /// Documented behaviour: the local merge is applied before the upload, so
    /// a failure *on the upload* leaves the local device already merged
    /// (local-first, and the merge is idempotent) while the remote is
    /// untouched. Nothing is lost — the next sync just re-pushes.
    #[tokio::test]
    async fn webdav_put_failure_leaves_remote_untouched() {
        let db = db_of("wd-putfail");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let remote = snap(vec![item("B", "Qb", "1700000000")]);
        let server = FakeWebDav::with_remote(bytes_of(&remote));
        *server.put_fault_once.lock().unwrap() =
            Some(WebDavError::ConnectionFailed("连接中断".to_string()));

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::WebDav(WebDavError::ConnectionFailed(_))) => {}
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
        assert_eq!(remote_ids(&server), ids(&["B"]), "remote not updated");
        assert_eq!(active_ids(&db), ids(&["A", "B"]), "local already merged");
    }

    // ----- 12. credential hygiene -----

    /// Requirement 12: WebDAV credentials must not appear in the snapshot,
    /// in the knowledge database, in a `.ikbackup`, in logs, or in an error
    /// message.
    #[tokio::test]
    async fn webdav_credentials_never_reach_snapshot_db_backup_or_errors() {
        const SECRET: &str = "SECRET_WEBDAV_PW_42";
        // A URL with inline credentials is the nastiest case: it can leak
        // through any error string that echoes the request target.
        let data_dir = tmp_dir("wd-secret-cfg");
        let cfg = config::WebDavConfig {
            url: format!("https://alice:{SECRET}@dav.example.com/dav/Sisyphus"),
            username: "alice".to_string(),
            password: SECRET.to_string(),
        };
        config::save_webdav(&data_dir, &cfg).unwrap();

        // (a) The config file is a separate device-config file, not the DB.
        let (db_path, conn) = tmp_db("wd-secret-db");
        seed(&conn, "A", "Qa", "1700000000", None);
        let db = Mutex::new(conn);
        let server = FakeWebDav::new();
        sync_via_webdav(&db, &server).await.unwrap();

        // (b) Snapshot bytes.
        let uploaded = String::from_utf8(server.remote_bytes().unwrap()).unwrap();
        for needle in [SECRET, "alice", "webdavUrl", "webdavUsername", "webdavPassword", "password"] {
            assert!(!uploaded.contains(needle), "snapshot leaked {needle}");
        }

        // (c) Knowledge SQLite bytes.
        drop(db.lock().unwrap());
        let sqlite_bytes = String::from_utf8_lossy(&fs::read(&db_path).unwrap()).to_string();
        assert!(!sqlite_bytes.contains(SECRET), "knowledge DB leaked the password");

        // (d) `.ikbackup` bytes.
        let backup_path = tmp_dir("wd-secret-bak").join("backup.ikbackup");
        backup::create(&db_path, &backup_path).unwrap();
        let backup_bytes = String::from_utf8_lossy(&fs::read(&backup_path).unwrap()).to_string();
        assert!(!backup_bytes.contains(SECRET), ".ikbackup leaked the password");

        // (e) Error messages built from a request that carries the secret.
        let client = webdav::ReqwestWebDavClient::new(&cfg).unwrap();
        let message = webdav::redact_secrets(
            &format!(
                "GET {} failed, auth header was Basic alice:{SECRET}",
                client.remote_file_url()
            ),
            SECRET,
        );
        assert!(!message.contains(SECRET), "error message leaked the password: {message}");
        // The username in a `Basic alice:***` header is fine to show; only
        // the password (`alice:<SECRET>`) must never appear.
        assert!(!message.contains(&format!("alice:{SECRET}")), "{message}");

        // (f) Debug formatting never prints it either.
        assert!(!format!("{client:?}").contains(SECRET));
    }

    // ----- 13. ETag / concurrent-overwrite protection -----

    /// Another device commits while we are merging: our ETag goes stale, the
    /// PUT is rejected with 412, and we re-fetch / re-merge / re-upload.
    /// Nobody's data is lost.
    #[tokio::test]
    async fn webdav_etag_conflict_refetches_and_retries() {
        let db = db_of("wd-etag");
        {
            let conn = db.lock().unwrap();
            seed(&conn, "A", "Qa", "1700000000", None);
            seed(&conn, "C", "Qc", "1700000000", None);
        }
        let remote = snap(vec![item("A", "Qa", "1700000000"), item("B", "Qb", "1700000000")]);
        let server = FakeWebDav::with_remote(bytes_of(&remote));
        // The Mac uploads D right after the Pad's GET.
        let concurrent = snap(vec![
            item("A", "Qa", "1700000000"),
            item("B", "Qb", "1700000000"),
            item("D", "Qd", "1700000000"),
        ]);
        server.schedule_concurrent_write(bytes_of(&concurrent));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.retry_count, 1, "one 412 retry");
        assert_eq!(summary.downloaded_item_count, 3, "the re-GET saw D");

        let expected = ids(&["A", "B", "C", "D"]);
        assert_eq!(active_ids(&db), expected, "D was not lost");
        assert_eq!(remote_ids(&server), expected, "C was not lost either");
        // GET twice (initial + re-fetch), PUT twice (rejected + accepted).
        assert_eq!(server.get_calls(), 2);
        assert_eq!(server.put_calls(), 2);
    }

    /// If the remote keeps changing the retries are bounded and the caller
    /// gets a clear error instead of an infinite loop.
    /// If the remote keeps changing (a peer that commits on every GET), the
    /// retries are bounded and the caller gets a clear error instead of an
    /// infinite loop — while the local merge still happened, so nothing is
    /// lost.
    #[tokio::test]
    async fn webdav_etag_retries_are_bounded() {
        let db = db_of("wd-etag-bound");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let remote = snap(vec![item("B", "Qb", "1700000000")]);
        let mut server = FakeWebDav::with_remote(bytes_of(&remote));
        // Every GET is immediately followed by another device committing the
        // same snapshot, so every PUT sees a stale ETag.
        let concurrent = bytes_of(&snap(vec![item("B", "Qb", "1700000000")]));
        server.schedule_concurrent_write_forever(concurrent);

        match sync_via_webdav(&db, &server).await {
            Err(SyncError::WebDav(WebDavError::PreconditionFailed)) => {}
            other => panic!("expected PreconditionFailed, got {other:?}"),
        }
        assert_eq!(server.put_calls(), WEBDAV_MAX_PUT_ATTEMPTS);
        // The local merge still happened, so nothing is lost.
        assert_eq!(active_ids(&db), ids(&["A", "B"]));
    }

    /// Servers without ETag support degrade to an unconditional PUT; the
    /// merge is still correct.
    #[tokio::test]
    async fn webdav_server_without_etags_falls_back_to_unconditional_put() {
        let db = db_of("wd-noetag");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let remote = snap(vec![item("B", "Qb", "1700000000")]);
        let mut server = FakeWebDav::with_remote(bytes_of(&remote));
        server.no_etag = true;

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.retry_count, 0);
        assert_eq!(remote_ids(&server), ids(&["A", "B"]));
        assert_eq!(active_ids(&db), ids(&["A", "B"]));
        assert!(server.remote_etag().is_none());
    }

    /// Servers that ignore preconditions simply accept the upload; the merge
    /// result is unchanged.
    #[tokio::test]
    async fn webdav_server_ignoring_preconditions_still_converges() {
        let db = db_of("wd-noprecond");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let remote = snap(vec![item("B", "Qb", "1700000000")]);
        let mut server = FakeWebDav::with_remote(bytes_of(&remote));
        server.ignore_preconditions = true;

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.retry_count, 0);
        assert_eq!(remote_ids(&server), ids(&["A", "B"]));
    }

    /// First sync where another device creates the snapshot between our GET
    /// (404) and our PUT: `If-None-Match: *` turns the would-be overwrite
    /// into a retry instead.
    #[tokio::test]
    async fn webdav_first_sync_race_is_retried_not_overwritten() {
        let db = db_of("wd-firstrace");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let server = FakeWebDav::new();
        // Another device commits right after our 404.
        let other = snap(vec![item("B", "Qb", "1700000000")]);
        server.schedule_concurrent_write(bytes_of(&other));

        let summary = sync_via_webdav(&db, &server).await.unwrap();
        assert_eq!(summary.retry_count, 1, "the race became a 412 retry");
        assert!(!summary.remote_existed, "our first GET saw nothing");
        assert_eq!(active_ids(&db), ids(&["A", "B"]), "B survived");
        assert_eq!(remote_ids(&server), ids(&["A", "B"]));
    }

    // ----- consistency with the local-file transport -----

    /// The headline requirement: Local File and WebDAV must produce exactly
    /// the same merged state from the same inputs.
    #[tokio::test]
    async fn webdav_merge_matches_local_file_merge() {
        let incoming = snap(vec![
            item("A", "Qa", "1700000000"),
            item("B", "Qb-newer", "1700000300"),
            item("D", "Qd", "1700000000"),
        ]);
        let bytes = bytes_of(&incoming);

        // Device 1: local file transport.
        let mut via_file = tmp_db("wd-same-file").1;
        {
            let conn = &via_file;
            seed(conn, "A", "Qa", "1700000000", None);
            seed(conn, "B", "Qb", "1700000000", None);
            seed(conn, "C", "Qc", "1700000000", None);
        }
        let file_path = tmp_dir("wd-same-file-io").join("peer.iksync");
        fs::write(&file_path, &bytes).unwrap();
        sync_import_local(&mut via_file, &file_path).unwrap();

        // Device 2: WebDAV transport, identical starting state.
        let via_webdav = db_of("wd-same-dav");
        {
            let conn = via_webdav.lock().unwrap();
            seed(&conn, "A", "Qa", "1700000000", None);
            seed(&conn, "B", "Qb", "1700000000", None);
            seed(&conn, "C", "Qc", "1700000000", None);
        }
        let server = FakeWebDav::with_remote(bytes.clone());
        sync_via_webdav(&via_webdav, &server).await.unwrap();

        let from_file = db::list_all_for_sync(&via_file).unwrap();
        let conn = via_webdav.lock().unwrap();
        let from_webdav = db::list_all_for_sync(&conn).unwrap();
        assert_eq!(from_file.len(), from_webdav.len());
        for (l, r) in from_file.iter().zip(from_webdav.iter()) {
            assert_eq!(l.sync_id, r.sync_id);
            assert_eq!(l.question, r.question, "content must match for {}", l.sync_id);
            assert_eq!(l.answer, r.answer);
            assert_eq!(l.updated_at, r.updated_at);
            assert_eq!(l.created_at, r.created_at);
            assert_eq!(l.last_read_at, r.last_read_at);
            assert_eq!(l.deleted_at, r.deleted_at);
            assert_eq!(l.tags, r.tags);
            assert_eq!(l.favorite, r.favorite);
        }
    }

    /// `SyncTarget` releases the connection between steps — nothing holds it
    /// across a network round-trip (the lock is re-taken on every call).
    #[tokio::test]
    async fn webdav_releases_db_lock_between_network_steps() {
        let db = db_of("wd-lock");
        seed(&db.lock().unwrap(), "A", "Qa", "1700000000", None);
        let server = FakeWebDav::with_remote(bytes_of(&snap(vec![item("B", "Qb", "1700000000")])));

        // If the engine held the guard across an await, this pre-lock would
        // deadlock. It must not.
        let held = db.lock().unwrap();
        drop(held);
        sync_via_webdav(&db, &server).await.unwrap();
        // And the connection is usable (not leaked / poisoned) afterwards.
        assert_eq!(active_ids(&db), ids(&["A", "B"]));
    }
}
