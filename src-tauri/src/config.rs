// Settings / LLM-config persistence.
//
// The API configuration (base URL, key, model names) is stored as a small JSON
// file in the platform app-data directory, separate from the SQLite database.
// The file is created with owner-only permissions (0600 on Unix) so the API key
// stays as restricted as practical without pulling in a full Keychain. The key
// is never written to the DB, never logged, and never echoed back to the UI.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Mirror of the frontend `Settings` type. `database_location` is informational
/// only (resolved from the runtime); the writable model settings live here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiConfig {
    pub api_base_url: String,
    pub api_key: String,
    pub chat_model: String,
    pub vision_model: String,
    pub database_location: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            chat_model: "gpt-5".to_string(),
            vision_model: "gpt-5".to_string(),
            database_location: String::new(),
        }
    }
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

/// Load the saved config, or return a default when the file is missing/corrupt.
/// The DB location is filled in by the caller so it always reflects reality even
/// if a stale value was persisted.
pub fn load(data_dir: &Path, database_location: &str) -> ApiConfig {
    let mut cfg = match fs::read_to_string(config_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ApiConfig::default(),
    };
    cfg.database_location = database_location.to_string();
    cfg
}

/// Persist the config. Creates the file with 0600 perms (Unix) so the API key
/// is not world-readable; on platforms without chmod this is a plain write.
pub fn save(data_dir: &Path, cfg: &ApiConfig) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = config_path(data_dir);
    let json = serde_json::to_string_pretty(cfg).map_err(io_err)?;
    fs::write(&path, json)?;
    restrict_permissions(&path);
    Ok(())
}

use std::io;

fn io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Best-effort owner-only permissions on Unix. Ignored elsewhere.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

/// Shared in-memory handle so commands can read/write config without re-reading
/// the file every call. Mutex for interior mutability across Tauri commands.
pub struct ApiConfigState {
    pub data_dir: PathBuf,
    pub inner: Mutex<ApiConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "interview-kit-config-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tmp_dir("roundtrip");
        let cfg = ApiConfig {
            api_base_url: "https://example.com/v1".to_string(),
            api_key: "secret-key".to_string(),
            chat_model: "cm".to_string(),
            vision_model: "vm".to_string(),
            database_location: String::new(),
        };
        save(&dir, &cfg).unwrap();
        let loaded = load(&dir, "/db/location.db");
        assert_eq!(loaded.api_base_url, "https://example.com/v1");
        assert_eq!(loaded.api_key, "secret-key");
        assert_eq!(loaded.vision_model, "vm");
        assert_eq!(loaded.database_location, "/db/location.db");
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tmp_dir("missing");
        let loaded = load(&dir, "/db/location.db");
        assert_eq!(loaded.api_base_url, "https://api.openai.com/v1");
        assert_eq!(loaded.database_location, "/db/location.db");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perms");
        let cfg = ApiConfig {
            api_key: "k".to_string(),
            ..Default::default()
        };
        save(&dir, &cfg).unwrap();
        let meta = fs::metadata(config_path(&dir)).unwrap();
        let mode = meta.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config should be owner-only");
    }
}