use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use serde::Serialize;
use std::sync::Mutex;
pub struct AppState {
    pub store: Mutex<Option<Store>>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub app: String,
    pub version: String,
    pub data_dir: String,
    pub db_ok: bool,
}

#[tauri::command]
fn app_health(state: tauri::State<'_, AppState>) -> Health {
    let db_ok = state.store.lock().map(|g| g.is_some()).unwrap_or(false);
    Health {
        app: "Lumen ASR".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: default_data_dir().display().to_string(),
        db_ok,
    }
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
        .invoke_handler(tauri::generate_handler![app_health])
        .setup(|app| {
            tracing::info!(
                name = app.package_info().name,
                "Lumen ASR desktop starting"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
