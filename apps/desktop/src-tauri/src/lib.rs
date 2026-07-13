mod asr_models;
mod capsule;
mod commands;
mod config;
mod context_capture;
mod corrector_cmd;
mod corrector_probe;
mod corrector_svc;
mod dictation;
mod hotkey;
mod hotkey_validate;
mod inject_cmd;
mod learning;
mod mod_chord;
mod onboard;
mod permissions_cmd;
mod provider_presets;
mod session_debug;
mod volume_mon;

use config::AppConfig;
use lumen_asr::{
    default_sensevoice_dir, default_whisper_dir, AudioCapture, EngineKind, SenseVoiceSherpaAsr,
    WhisperAsr,
};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use std::sync::{Arc, Mutex};

pub struct AppState {
    pub store: Mutex<Option<Store>>,
    pub audio: AudioCapture,
    pub engine: Mutex<EngineKind>,
    pub sensevoice: Mutex<SenseVoiceSherpaAsr>,
    pub whisper: Mutex<WhisperAsr>,
    pub config: Mutex<AppConfig>,
    pub context: context_capture::ContextCaptureState,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let data_dir = default_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let _ = std::fs::create_dir_all(data_dir.join("models"));
    let _ = std::fs::create_dir_all(data_dir.join("debug"));
    let _ = std::fs::create_dir_all(data_dir.join("logs"));

    // File + stderr logging so we can debug "ASR died" / paste target issues.
    let log_path = data_dir.join("logs/lumen.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "lumen_asr_desktop=info,lumen=info,warn".into());
    match file {
        Ok(f) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            let writer = std::io::stderr.and(f);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .init();
            tracing::info!(path = %log_path.display(), "file logging enabled");
        }
        Err(e) => {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
            tracing::warn!(error = %e, "file logging unavailable");
        }
    }

    let app_config = AppConfig::load();
    tracing::info!(
        provider = %app_config.corrector.provider,
        model = %app_config.corrector.model,
        hotkey = %app_config.hotkey.toggle,
        onboarding_completed = app_config.onboarding.completed,
        "config loaded"
    );

    let audio = AudioCapture::new();
    let browser_provider: Option<Arc<dyn lumen_context::BrowserSnapshotProvider>> = if app_config
        .context
        .browser_enabled
        && !app_config.context.browser_extension_origins.is_empty()
    {
        let bridge_root = app_config.context.browser_bridge_root(&data_dir);
        let bridge = lumen_context::NativeBrowserBridgeConfig::new(
            "lumen-asr",
            bridge_root.join("bridge.sock"),
            bridge_root.join("bridge.token"),
            app_config.context.browser_extension_origins.clone(),
        );
        let host_config = data_dir.join("context-browser/host.json");
        let safari_host_config = bridge_root.join("browser-host.json");
        let provider = bridge
            .write_host_config(&host_config)
            .and_then(|_| bridge.write_host_config(&safari_host_config))
            .and_then(|_| {
                tauri::async_runtime::block_on(lumen_context::NativeBrowserProvider::bind(bridge))
            });
        match provider {
            Ok(provider) => Some(Arc::new(provider)),
            Err(error) => {
                tracing::warn!(error = %error, "browser context bridge initialization failed");
                None
            }
        }
    } else {
        if app_config.context.browser_enabled {
            tracing::warn!("browser context enabled without an extension origin allowlist");
        }
        None
    };
    let context = context_capture::ContextCaptureState::new_with_browser(
        &app_config.context,
        &data_dir,
        browser_provider,
    );
    if let Some(ref name) = app_config.audio.device_name {
        if !name.is_empty() {
            audio.set_device(Some(name.clone()));
        }
    }

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
    let store = Mutex::new(store);
    if let Err(error) = context.enforce_retention(&store) {
        tracing::warn!(error = %error, "context retention enforcement failed");
    }

    let sv_dir = default_sensevoice_dir();
    let wh_dir = default_whisper_dir();
    tracing::info!(dir = %sv_dir.display(), ready = lumen_asr::sensevoice_ready(&sv_dir), "SenseVoice model dir");
    tracing::info!(dir = %wh_dir.display(), ready = lumen_asr::whisper_ready(&wh_dir), "Whisper model dir");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(AppState {
            store,
            audio,
            engine: Mutex::new(EngineKind::SenseVoice),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(sv_dir)),
            whisper: Mutex::new(WhisperAsr::new(wh_dir)),
            config: Mutex::new(app_config),
            context,
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
            dictation::list_audio_devices,
            dictation::set_audio_device,
            dictation::set_asr_engine,
            dictation::get_asr_status,
            dictation::start_recording,
            dictation::stop_and_transcribe,
            dictation::cancel_recording,
            dictation::toggle_dictation_cmd,
            dictation::get_session_audio,
            dictation::retry_session_transcription,
            corrector_cmd::get_corrector_config,
            corrector_cmd::save_corrector_config,
            corrector_cmd::correct_text,
            corrector_cmd::default_corrector_config,
            corrector_cmd::list_llm_presets,
            corrector_cmd::list_asr_presets,
            corrector_cmd::get_asr_service_config,
            corrector_cmd::save_asr_service_config,
            permissions_cmd::get_permission_status,
            permissions_cmd::poll_permissions,
            permissions_cmd::open_microphone_settings,
            permissions_cmd::open_accessibility_settings,
            permissions_cmd::request_accessibility_access,
            permissions_cmd::request_microphone_access,
            inject_cmd::get_inject_config,
            inject_cmd::save_inject_config,
            inject_cmd::insert_text,
            hotkey::get_hotkey_config,
            hotkey::save_hotkey_config,
            hotkey::pause_hotkeys,
            hotkey::resume_hotkeys,
            learning::get_learning_config,
            learning::save_learning_config,
            learning::process_edit,
            onboard::get_onboarding_state,
            onboard::set_onboarding_step,
            onboard::skip_onboarding,
            onboard::complete_onboarding,
            onboard::reopen_onboarding,
            volume_mon::start_volume_monitoring_cmd,
            volume_mon::stop_volume_monitoring_cmd,
            asr_models::check_asr_model_status,
            asr_models::list_local_asr_models,
            asr_models::use_existing_asr_model,
            asr_models::start_asr_model_download,
            asr_models::cancel_asr_model_download,
            corrector_probe::probe_corrector,
            corrector_probe::ollama_list_models,
            corrector_probe::ollama_pull_model,
            corrector_probe::cancel_ollama_pull,
            corrector_probe::apply_corrector_suggestion,
            hotkey_validate::validate_hotkey,
            context_capture::clear_context_data,
        ])
        .setup(|app| {
            // Keep Regular activation policy. Focus preservation: non-focusable
            // capsule + restore typing-target only when we stole frontmost.

            if let Err(e) = capsule::ensure_capsule(app.handle()) {
                tracing::warn!(error = %e, "capsule window create failed");
            }

            // Log AX status only — wizard/settings open System Settings on demand.
            permissions_cmd::bootstrap_permissions();

            if let Err(e) = hotkey::setup_hotkeys(app.handle()) {
                tracing::warn!(error = %e, "hotkey setup failed");
            }
            if !lumen_platform_macos::is_accessibility_trusted() {
                tracing::warn!(
                    "hotkey event-tap needs Accessibility; using fallback monitors until granted"
                );
            }

            let debug_dir = session_debug::debug_root();
            let _ = std::fs::create_dir_all(&debug_dir);
            let log_path = lumen_platform::default_data_dir().join("logs/lumen.log");
            tracing::info!(
                name = app.package_info().name,
                debug = %debug_dir.display(),
                log = %log_path.display(),
                accessibility = lumen_platform_macos::is_accessibility_trusted(),
                "Lumen ASR desktop starting (session debug enabled)"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
