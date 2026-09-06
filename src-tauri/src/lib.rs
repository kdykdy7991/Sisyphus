mod db;
mod commands;
mod config;
mod llm;
mod vision;
mod similarity;
mod chat;
mod tokenizer;
mod backup;
mod sync;
mod webdav;

use tauri::Manager;
use config::{ApiConfigState, WebDavConfigState};
use db::Db;

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
            let api_cfg = config::load(&data_dir, &db_location);
            app.manage(ApiConfigState {
                data_dir: data_dir.clone(),
                inner: std::sync::Mutex::new(api_cfg),
                db_path: path.clone(),
            });
            // WebDAV lives in its own file so the transport credentials can
            // never end up in a snapshot or a backup.
            let webdav_cfg = config::load_webdav(&data_dir);
            app.manage(WebDavConfigState {
                data_dir: data_dir.clone(),
                inner: std::sync::Mutex::new(webdav_cfg),
            });
            eprintln!("[interview-kit] database ready at {} (trigram_fts={})", db_location, trigram);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::knowledge_list,
            commands::knowledge_get,
            commands::knowledge_search,
            commands::knowledge_save,
            commands::knowledge_recent,
            commands::knowledge_clear,
            commands::settings_get,
            commands::settings_save,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Interview Kit");
}