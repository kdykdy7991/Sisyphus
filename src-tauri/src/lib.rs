mod backup;
mod chat;
mod commands;
mod config;
mod db;
mod llm;
mod log;
mod similarity;
mod sync;
mod tokenizer;
mod vision;
mod webdav;

use tauri::Manager;
use config::{ApiConfigState, WebDavConfigState};
use db::Db;
use log::LogState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let path = db::resolve_db_path(app.path().app_data_dir()?)?;
            let conn = db::open(&path)?;
            let trigram = db::init(&conn)?;
            app.manage(Db {
                conn: std::sync::Mutex::new(conn),
                trigram,
            });
            let data_dir = app.path().app_data_dir()?;
            let db_location = path.display().to_string();
            let profiles = config::load_profiles(&data_dir, &db_location);
            let api_cfg = profiles.profiles.iter().find(|p| p.id == profiles.active_id).unwrap().config.clone();
            app.manage(ApiConfigState {
                data_dir: data_dir.clone(),
                inner: std::sync::Mutex::new(api_cfg),
                profiles: std::sync::Mutex::new(profiles),
                db_path: path.clone(),
            });
            // WebDAV lives in its own file so the transport credentials can
            // never end up in a snapshot or a backup.
            let webdav_cfg = config::load_webdav(&data_dir);
            app.manage(WebDavConfigState {
                data_dir: data_dir.clone(),
                inner: std::sync::Mutex::new(webdav_cfg),
            });
            // Diagnostic log directory under app data dir. Managed as state
            // so Tauri commands can write to it without re-resolving the
            // path on every call.
            let log_dir = log::init(&data_dir)?;
            let log_state = LogState::new(log_dir.clone());
            log::append(&log_state, "session", &format!("database ready at {db_location} (trigram_fts={trigram})"));
            app.manage(log_state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::knowledge_list,
            commands::knowledge_export_markdown,
            commands::knowledge_get,
            commands::topic_list,
            commands::topic_create,
            commands::topic_delete,
            commands::knowledge_search,
            commands::knowledge_save,
            commands::knowledge_recent,
            commands::knowledge_clear,
            commands::settings_get,
            commands::settings_save,
            commands::settings_profiles,
            commands::settings_profile_create,
            commands::settings_profile_switch,
            commands::settings_profile_delete,
            commands::settings_test_connection,
            commands::vision_extract,
            commands::analyze_similarity,
            commands::knowledge_chat,
            commands::backup_create,
            commands::backup_inspect,
            commands::backup_restore,
            commands::sync_export_local,
            commands::sync_inspect,
            commands::sync_import_local,
            commands::webdav_config_get,
            commands::webdav_config_save,
            commands::webdav_test_connection,
            commands::sync_webdav,
            commands::log_open_dir,
            commands::log_get_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Interview Kit");
}
