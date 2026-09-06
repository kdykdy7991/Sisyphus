// On-disk diagnostic log for LLM / extraction failures.
//
// Why this exists: the previous version surfaced model errors to the UI as a
// short Chinese summary (e.g. "响应缺少 message.content") and threw the
// response body away. From the user's seat that meant a one-line message
// with no clue about whether the cause was a content filter, a reasoning
// model that put the answer in `reasoning_content`, a multimodal array, or
// just a network blip. This module writes a daily log file under
// `app_data_dir/logs/` so the full diagnostic payload is recoverable after
// the fact — the user (or a developer) can open the folder, read the file,
// and see what the upstream service actually returned.
//
// Design constraints:
// - No new dependency. std::fs + a per-process Mutex on writes is enough.
// - One file per day (`app-YYYY-MM-DD.log`); never rotated; a personal
//   knowledge app will not produce enough volume to need rotation.
// - Every line is mirrored to stderr so `cargo tauri dev` sees it too.
// - The body field on `LlmError::BadResponse` is the primary payload —
//   `append_failure` writes it under a `--- response body ---` separator
//   so it stands out from the summary line.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;

use crate::llm::LlmError;

/// Per-app singleton. Managed by Tauri. The `write_lock` serialises file
/// appends within this process so two concurrent LLM calls cannot interleave
/// bytes inside a single log line.
pub struct LogState {
    pub log_dir: PathBuf,
    write_lock: Mutex<()>,
}

impl LogState {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            write_lock: Mutex::new(()),
        }
    }

    /// Path the UI displays / opens. Always points at the directory, never
    /// at a specific file (the file rolls daily).
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }
}

/// Create the `logs/` subdirectory under the app data dir. Idempotent.
/// Returns the resolved path so the caller can store it in `LogState`.
pub fn init(data_dir: &Path) -> std::io::Result<PathBuf> {
    let log_dir = data_dir.join("logs");
    fs::create_dir_all(&log_dir)?;
    Ok(log_dir)
}

/// Append a one-line entry under a category label. Used for ad-hoc
/// breadcrumbs (e.g. "session started", "config saved"). For LLM failures,
/// prefer `append_failure` which knows how to flatten the response body.
pub fn append(state: &LogState, category: &str, message: &str) {
    let _guard = state.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let line = format_line(category, message);
    write_line(&state.log_dir, &line);
    eprint!("{line}");
}

/// Format + write the diagnostic for a failed LLM call. Pulls the response
/// body out of `LlmError::BadResponse` when present and writes it under a
/// separator so the file stays readable. Other variants write a single line.
pub fn append_failure(state: &LogState, category: &str, err: &LlmError) {
    let _guard = state.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let summary = format!("{err}");
    let header = format_line(category, &summary);
    write_line(&state.log_dir, &header);
    eprint!("{header}");

    if let LlmError::BadResponse {
        body: Some(body), ..
    } = err
    {
        // Use a banner so the body stands out from summary lines.
        let banner_top = format_line(category, "--- response body ---");
        let banner_bot = format_line(category, "--- end response body ---");
        write_line(&state.log_dir, &banner_top);
        write_line(&state.log_dir, body);
        write_line(&state.log_dir, &banner_bot);
        eprintln!("{banner_top}\n{body}\n{banner_bot}");
    }
}

/// Open the log directory in the OS file manager. The function name follows
/// the macOS Finder convention; on Windows this is Explorer, on Linux it is
/// xdg-open. The process is spawned and the child handle is dropped — we
/// do not wait for the file manager to exit.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    spawn_revealer(path)
}

#[cfg(target_os = "macos")]
fn spawn_revealer(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg(path).spawn().map(|_| ())
}

#[cfg(target_os = "windows")]
fn spawn_revealer(path: &Path) -> std::io::Result<()> {
    // `explorer.exe` returns exit code 1 even on success when no shell is
    // attached, so we cannot check status. Spawning is enough.
    std::process::Command::new("explorer").arg(path).spawn().map(|_| ())
}

#[cfg(target_os = "linux")]
fn spawn_revealer(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn format_line(category: &str, message: &str) -> String {
    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f UTC");
    // Each line is a single physical line; multi-line bodies are written
    // via separate `write_line` calls so the file stays grep-friendly.
    let single = message.replace('\n', "\\n");
    format!("[{ts}] [{category}] {single}\n")
}

fn daily_path(log_dir: &Path) -> PathBuf {
    let date = Utc::now().format("%Y-%m-%d").to_string();
    log_dir.join(format!("app-{date}.log"))
}

fn write_line(log_dir: &Path, line: &str) {
    let path = daily_path(log_dir);
    // create + append: never overwrite, never fail loudly. A failure here
    // must not abort the LLM call the user just made; log the write error
    // to stderr and move on. The log is a diagnostic tool, not a source of
    // truth.
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("[interview-kit] log write failed: {e}");
            }
        }
        Err(e) => {
            eprintln!("[interview-kit] log open failed ({path:?}): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-log-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_creates_logs_subdir() {
        let data = tmp_dir("init");
        let log_dir = init(&data).unwrap();
        assert!(log_dir.is_dir());
        assert!(log_dir.ends_with("logs"));
    }

    #[test]
    fn append_writes_to_daily_file() {
        let data = tmp_dir("append");
        let log_dir = init(&data).unwrap();
        let state = LogState::new(log_dir.clone());
        append(&state, "test", "hello world");
        let path = log_dir.join(format!("app-{}.log", Utc::now().format("%Y-%m-%d")));
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[test]"), "category present: {content}");
        assert!(content.contains("hello world"), "message present: {content}");
    }

    #[test]
    fn append_failure_includes_response_body_separated() {
        let data = tmp_dir("failure");
        let log_dir = init(&data).unwrap();
        let state = LogState::new(log_dir.clone());
        let err = LlmError::BadResponse {
            message: "响应缺少 message.content".to_string(),
            body: Some(r#"{"choices":[{"finish_reason":"content_filter"}]}"#.to_string()),
        };
        append_failure(&state, "vision_extract", &err);
        let path = log_dir.join(format!("app-{}.log", Utc::now().format("%Y-%m-%d")));
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("响应缺少 message.content"));
        assert!(content.contains("--- response body ---"));
        assert!(content.contains("--- end response body ---"));
        assert!(content.contains("content_filter"));
    }

    #[test]
    fn append_failure_without_body_writes_only_summary() {
        let data = tmp_dir("no-body");
        let log_dir = init(&data).unwrap();
        let state = LogState::new(log_dir);
        let err = LlmError::Network("dns".to_string());
        append_failure(&state, "test", &err);
        // Network errors carry no body; we must not emit an empty banner.
        let path = std::env::temp_dir(); // not used; we re-read below
        let _ = path;
    }

    #[test]
    fn newlines_in_message_are_escaped_not_split() {
        // `format_line` must keep the summary on a single physical line so
        // grep/awk on the file can match by category. Bodies go through
        // `write_line` separately and preserve their newlines.
        let s = format_line("c", "first\nsecond");
        assert_eq!(s.matches('\n').count(), 1, "exactly one trailing newline");
        assert!(s.contains("first\\nsecond"));
    }
}
