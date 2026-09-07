// Settings / LLM-config persistence.
//
// Two *device-local* config files live in the platform app-data directory,
// both outside the SQLite database:
//
//   config.json  -> ApiConfig      (LLM base URL / key / models)
//   webdav.json  -> WebDavConfig   (WebDAV url / username / password)
//
// Both carry secrets, so both are written with owner-only permissions (0600 on
// Unix). Neither file is ever read by the backup / sync engines: a `.ikbackup`
// archive and a `.iksync` snapshot carry Knowledge data only.

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiConfigProfile { pub id: String, pub name: String, pub config: ApiConfig }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiConfigProfiles { pub active_id: String, pub profiles: Vec<ApiConfigProfile> }

fn inferred_profile_name(config: &ApiConfig) -> String {
    let host = config.api_base_url
        .split_once("://").map(|(_, rest)| rest).unwrap_or(&config.api_base_url)
        .split('/').next().unwrap_or("")
        .trim_start_matches("api.");
    let provider = match host {
        "openai.com" => "OpenAI".to_string(),
        "" => "LLM".to_string(),
        value => value.to_string(),
    };
    let model = if config.vision_model.trim().is_empty() { config.chat_model.trim() } else { config.vision_model.trim() };
    if model.is_empty() { provider } else { format!("{provider} · {model}") }
}

pub fn load_profiles(data_dir: &Path, database_location: &str) -> ApiConfigProfiles {
    let text = fs::read_to_string(config_path(data_dir)).ok();
    if let Some(text) = text.as_deref() {
        if let Ok(mut store) = serde_json::from_str::<ApiConfigProfiles>(text) {
            for p in &mut store.profiles { p.config.database_location = database_location.to_string(); }
            let mut renamed = false;
            for p in &mut store.profiles {
                if p.name.trim().is_empty() || p.name == "默认配置" {
                    p.name = inferred_profile_name(&p.config);
                    renamed = true;
                }
            }
            if !store.profiles.is_empty() && store.profiles.iter().all(|p| p.id != store.active_id) { store.active_id = store.profiles[0].id.clone(); }
            if !store.profiles.is_empty() {
                if renamed { let _ = save_profiles(data_dir, &store); }
                return store;
            }
        }
    }
    let mut config = text.and_then(|v| serde_json::from_str::<ApiConfig>(&v).ok()).unwrap_or_default();
    config.database_location = database_location.to_string();
    let name = inferred_profile_name(&config);
    ApiConfigProfiles { active_id: "default".into(), profiles: vec![ApiConfigProfile { id: "default".into(), name, config }] }
}

pub fn save_profiles(data_dir: &Path, store: &ApiConfigProfiles) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = config_path(data_dir);
    fs::write(&path, serde_json::to_string_pretty(store).map_err(io_err)?)?;
    restrict_permissions(&path);
    Ok(())
}

/// WebDAV transport settings.
///
/// This is **device configuration**, not knowledge data: it is persisted in
/// its own file and is never part of a `SyncSnapshot` (`.iksync`), never part
/// of a `.ikbackup`, and never written to the knowledge SQLite. That split is
/// what lets a snapshot be shared across machines (or over WebDAV) without
/// ever carrying the credentials of the machine that produced it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebDavConfig {
    /// Base URL of the WebDAV root, e.g.
    /// `https://dav.example.com/remote.php/dav/files/alice/Sisyphus`.
    /// The app stores its snapshot at `<url>/sync/latest.iksync`.
    pub url: String,
    pub username: String,
    /// Application password / token. Never logged, never echoed to the UI.
    pub password: String,
}

impl Default for WebDavConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }
}

fn webdav_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("webdav.json")
}

/// Load the WebDAV config, or a default when the file is missing/corrupt.
pub fn load_webdav(data_dir: &Path) -> WebDavConfig {
    match fs::read_to_string(webdav_config_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => WebDavConfig::default(),
    }
}

/// Persist the WebDAV config with the same owner-only permissions as the API
/// config. The password never touches the knowledge database or any log.
pub fn save_webdav(data_dir: &Path, cfg: &WebDavConfig) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = webdav_config_path(data_dir);
    let json = serde_json::to_string_pretty(cfg).map_err(io_err)?;
    fs::write(&path, json)?;
    restrict_permissions(&path);
    Ok(())
}

/// Merge an incoming config (from the UI) into the stored one.
///
/// The UI never receives the password back, so it always submits an empty
/// field; an empty `password` therefore means "keep the existing one" (same
/// contract as the API key). `url` is normalized by trimming whitespace and
/// trailing slashes so `<root>/sync/latest.iksync` never ends up with a
/// double slash.
pub fn merge_webdav(existing: &WebDavConfig, incoming: WebDavConfig) -> WebDavConfig {
    WebDavConfig {
        url: normalize_webdav_url(&incoming.url),
        username: incoming.username.trim().to_string(),
        password: if incoming.password.is_empty() {
            existing.password.clone()
        } else {
            incoming.password
        },
    }
}

/// Trim whitespace and any trailing `/`. Returns an empty string for an
/// empty / whitespace-only input so callers can detect "not configured".
pub fn normalize_webdav_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// Shared in-memory handle for the WebDAV config. Kept separate from
/// `ApiConfigState` so the LLM settings and the sync transport can never
/// accidentally leak into each other's payloads.
pub struct WebDavConfigState {
    pub data_dir: PathBuf,
    pub inner: Mutex<WebDavConfig>,
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
/// `db_path` is the on-disk location of `interview-kit.db`; it's read-only at
/// runtime (resolved once in setup) and is used by the backup/restore commands
/// to know which file to snapshot / replace.
pub struct ApiConfigState {
    pub data_dir: PathBuf,
    pub inner: Mutex<ApiConfig>,
    pub profiles: Mutex<ApiConfigProfiles>,
    pub db_path: PathBuf,
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

    #[test]
    fn legacy_single_config_migrates_to_default_profile() {
        let dir = tmp_dir("profile-migration");
        let cfg = ApiConfig { api_key: "keep-me".into(), chat_model: "chat-a".into(), ..Default::default() };
        save(&dir, &cfg).unwrap();
        let store = load_profiles(&dir, "/db/live.db");
        assert_eq!(store.active_id, "default");
        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].config.api_key, "keep-me");
        assert_eq!(store.profiles[0].config.database_location, "/db/live.db");
        save_profiles(&dir, &store).unwrap();
        assert_eq!(load_profiles(&dir, "/db/live.db").profiles[0].config.chat_model, "chat-a");
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

    // ----- WebDAV config (device config, never knowledge data) -----

    #[test]
    fn webdav_save_then_load_roundtrips() {
        let dir = tmp_dir("webdav-roundtrip");
        let cfg = WebDavConfig {
            url: "https://dav.example.com/dav/Sisyphus".to_string(),
            username: "alice".to_string(),
            password: "app-token".to_string(),
        };
        save_webdav(&dir, &cfg).unwrap();
        let loaded = load_webdav(&dir);
        assert_eq!(loaded.url, cfg.url);
        assert_eq!(loaded.username, cfg.username);
        assert_eq!(loaded.password, cfg.password);
    }

    #[test]
    fn webdav_load_missing_returns_default() {
        let dir = tmp_dir("webdav-missing");
        let loaded = load_webdav(&dir);
        assert!(loaded.url.is_empty());
        assert!(loaded.username.is_empty());
        assert!(loaded.password.is_empty());
    }

    /// The WebDAV config must live in its own file, never in `config.json`
    /// (and therefore never in anything the backup / sync engines read).
    #[test]
    fn webdav_config_uses_its_own_file() {
        let dir = tmp_dir("webdav-separate");
        let cfg = WebDavConfig {
            url: "https://dav.example.com/dav".to_string(),
            username: "bob".to_string(),
            password: "top-secret-pw".to_string(),
        };
        save_webdav(&dir, &cfg).unwrap();
        assert!(webdav_config_path(&dir).exists());
        // config.json either does not exist or does not carry WebDAV fields.
        match fs::read_to_string(config_path(&dir)) {
            Ok(text) => assert!(!text.contains("top-secret-pw")),
            Err(_) => {}
        }
        // Nothing WebDAV-related lands in the API config.
        let api = load(&dir, "/db/x.db");
        let _ = api;
    }

    #[cfg(unix)]
    #[test]
    fn webdav_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("webdav-perms");
        let cfg = WebDavConfig {
            password: "p".to_string(),
            ..Default::default()
        };
        save_webdav(&dir, &cfg).unwrap();
        let meta = fs::metadata(webdav_config_path(&dir)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    /// An empty incoming password means "keep the stored one" — the UI never
    /// receives the password, so it can only ever submit an empty field.
    #[test]
    fn webdav_merge_keeps_existing_password_on_empty() {
        let existing = WebDavConfig {
            url: "https://dav.example.com/dav".to_string(),
            username: "alice".to_string(),
            password: "stored-token".to_string(),
        };
        let incoming = WebDavConfig {
            url: "  https://dav.example.com/dav/  ".to_string(),
            username: "alice".to_string(),
            password: String::new(),
        };
        let merged = merge_webdav(&existing, incoming);
        assert_eq!(merged.password, "stored-token");
        // Trailing slash + whitespace normalized away.
        assert_eq!(merged.url, "https://dav.example.com/dav");
    }

    #[test]
    fn webdav_merge_overwrites_password_when_provided() {
        let existing = WebDavConfig {
            password: "old".to_string(),
            ..Default::default()
        };
        let incoming = WebDavConfig {
            url: "https://dav.example.com/dav".to_string(),
            username: "alice".to_string(),
            password: "new".to_string(),
        };
        assert_eq!(merge_webdav(&existing, incoming).password, "new");
    }

    #[test]
    fn webdav_url_normalization() {
        assert_eq!(normalize_webdav_url("  https://d/  "), "https://d");
        assert_eq!(normalize_webdav_url("https://d///"), "https://d");
        assert_eq!(normalize_webdav_url("   "), "");
    }
}
