mod commands;

use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use std::sync::Mutex;

pub struct AppState {
    pub store: Mutex<Option<Store>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lumen_asr_desktop=info,lumen=info,warn".into()),
        )
        .init();

    let data_dir = default_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);

    let store = match Store::open(default_db_path()) {
        Ok(s) => {
            tracing::info!(path = %s.path().display(), "store opened");
            Some(s)
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to open store");
            None
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            store: Mutex::new(store),
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_health,
            commands::list_sessions,
            commands::get_session,
            commands::delete_session,
            commands::save_session,
            commands::seed_demo_session,
            commands::list_edit_events,
            commands::record_edit_event,
            commands::suggest_from_edit,
            commands::confirm_learn,
            commands::list_dictionary,
            commands::add_dictionary_term,
            commands::add_dictionary_replacement,
            commands::delete_dictionary_entry,
        ])
        .setup(|app| {
            tracing::info!(
                name = app.package_info().name,
                "Lumen ASR desktop starting (M1)"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
