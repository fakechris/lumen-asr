mod asr_models;
mod capsule;
mod commands;
mod config;
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
mod pipeline_attempt;
mod provider_presets;
mod session_debug;
mod volume_mon;

use config::AppConfig;
use lumen_asr::{
    default_qwen_dir, default_sensevoice_dir, default_whisper_dir, qwen_ready, sensevoice_ready,
    whisper_ready, AudioCapture, EngineKind, QwenAsr, QwenAsrConfig, SenseVoiceSherpaAsr,
    WhisperAsr,
};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::Manager;

const QWEN_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct QwenRuntimeStatus {
    pub executable: PathBuf,
    pub ready: bool,
    pub checking: bool,
    pub generation: u64,
}

pub struct AppState {
    pub store: Mutex<Option<Store>>,
    pub audio: AudioCapture,
    pub engine: Mutex<EngineKind>,
    pub sensevoice: Mutex<SenseVoiceSherpaAsr>,
    pub qwen: Mutex<QwenAsr>,
    pub qwen_runtime: Mutex<QwenRuntimeStatus>,
    pub whisper: Mutex<WhisperAsr>,
    pub config: Mutex<AppConfig>,
}

fn qwen_engine_from_config(config: &config::AsrServiceConfig) -> QwenAsr {
    let selected = config.model_dir_for(EngineKind::Qwen);
    let model_dir = if !selected.as_os_str().is_empty() && qwen_ready(&selected) {
        selected
    } else {
        default_qwen_dir()
    };
    QwenAsr::new(QwenAsrConfig::product(
        config.qwen_python_executable(),
        model_dir,
        (!config.language.trim().is_empty()).then(|| config.language.clone()),
        std::time::Duration::from_secs(config.timeout_secs.max(30)),
    ))
}

fn qwen_runtime_available(path: &Path) -> bool {
    qwen_runtime_available_with_timeout(path, QWEN_RUNTIME_PROBE_TIMEOUT)
}

fn qwen_runtime_available_with_timeout(path: &Path, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(path)
        .args([
            "-c",
            "import sys;from mlx_qwen3_asr import Session;sys.exit(0 if callable(Session) and callable(getattr(Session,'transcribe',None)) else 1)",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub(crate) fn schedule_qwen_runtime_refresh(app: tauri::AppHandle) -> Result<(), String> {
    let (executable, model_ready) = app
        .state::<AppState>()
        .qwen
        .lock()
        .map(|engine| {
            (
                engine.python_executable().to_path_buf(),
                qwen_ready(engine.model_dir()),
            )
        })
        .map_err(|_| "qwen lock poisoned".to_string())?;
    let generation = {
        let state = app.state::<AppState>();
        let mut runtime = state
            .qwen_runtime
            .lock()
            .map_err(|_| "qwen runtime lock poisoned".to_string())?;
        runtime.generation = runtime.generation.wrapping_add(1);
        runtime.executable = executable.clone();
        runtime.ready = false;
        runtime.checking = model_ready;
        runtime.generation
    };
    if !model_ready {
        return Ok(());
    }

    tauri::async_runtime::spawn(async move {
        let probe_executable = executable.clone();
        let ready = tokio::task::spawn_blocking(move || qwen_runtime_available(&probe_executable))
            .await
            .unwrap_or(false);
        let state = app.state::<AppState>();
        let Ok(mut runtime) = state.qwen_runtime.lock() else {
            tracing::warn!("qwen runtime lock poisoned after probe");
            return;
        };
        if runtime.generation == generation && runtime.executable == executable {
            runtime.ready = ready;
            runtime.checking = false;
        }
    });
    Ok(())
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

    let initial_engine = dictation::engine_kind_for_provider(&app_config.asr.provider)
        .unwrap_or(EngineKind::SenseVoice);
    let selected_sensevoice_dir = app_config.asr.model_dir_for(EngineKind::SenseVoice);
    let sv_dir = (!selected_sensevoice_dir.as_os_str().is_empty()
        && sensevoice_ready(&selected_sensevoice_dir))
    .then_some(selected_sensevoice_dir)
    .unwrap_or_else(default_sensevoice_dir);
    let selected_whisper_dir = app_config.asr.model_dir_for(EngineKind::Whisper);
    let wh_dir = (!selected_whisper_dir.as_os_str().is_empty()
        && whisper_ready(&selected_whisper_dir))
    .then_some(selected_whisper_dir)
    .unwrap_or_else(default_whisper_dir);
    let qwen = qwen_engine_from_config(&app_config.asr);
    let qwen_runtime = QwenRuntimeStatus {
        executable: qwen.python_executable().to_path_buf(),
        ready: false,
        checking: false,
        generation: 0,
    };
    tracing::info!(dir = %sv_dir.display(), ready = lumen_asr::sensevoice_ready(&sv_dir), "SenseVoice model dir");
    tracing::info!(dir = %wh_dir.display(), ready = lumen_asr::whisper_ready(&wh_dir), "Whisper model dir");
    tracing::info!(
        dir = %qwen.model_dir().display(),
        python = %qwen.python_executable().display(),
        ready = lumen_asr::qwen_ready(qwen.model_dir()),
        "Qwen model config"
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(AppState {
            store: Mutex::new(store),
            audio,
            engine: Mutex::new(initial_engine),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(sv_dir)),
            qwen: Mutex::new(qwen),
            qwen_runtime: Mutex::new(qwen_runtime),
            whisper: Mutex::new(WhisperAsr::new(wh_dir)),
            config: Mutex::new(app_config),
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_health,
            commands::list_sessions,
            commands::get_session,
            commands::list_session_attempts,
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
            let qwen_selected = app
                .state::<AppState>()
                .engine
                .lock()
                .map(|engine| *engine == EngineKind::Qwen)
                .unwrap_or(false);
            if qwen_selected {
                if let Err(error) = schedule_qwen_runtime_refresh(app.handle().clone()) {
                    tracing::warn!(%error, "could not schedule Qwen runtime probe");
                }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn probe_script(name: &str, body: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lumen-qwen-probe-{name}-{nonce}"));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn qwen_runtime_probe_handles_success_failure_missing_and_timeout() {
        let success = probe_script("success", "exit 0");
        let failure = probe_script("failure", "exit 7");
        let hanging = probe_script("hanging", "exec sleep 5");

        assert!(qwen_runtime_available_with_timeout(
            &success,
            Duration::from_secs(1)
        ));
        assert!(!qwen_runtime_available_with_timeout(
            &failure,
            Duration::from_secs(1)
        ));
        assert!(!qwen_runtime_available_with_timeout(
            Path::new("/does/not/exist"),
            Duration::from_secs(1)
        ));
        assert!(!qwen_runtime_available_with_timeout(
            &hanging,
            Duration::from_millis(50)
        ));

        let _ = std::fs::remove_file(success);
        let _ = std::fs::remove_file(failure);
        let _ = std::fs::remove_file(hanging);
    }

    #[test]
    fn qwen_provider_aliases_share_one_canonical_engine_contract() {
        for alias in ["qwen", "qwen3_asr", "local_qwen"] {
            assert_eq!(dictation::canonical_asr_provider(alias), "local_qwen");
            assert_eq!(
                dictation::engine_kind_for_provider(alias),
                Some(EngineKind::Qwen)
            );
        }
    }

    #[test]
    fn backend_recording_gate_rejects_unready_qwen() {
        let error = dictation::ensure_active_asr_ready(
            "local_qwen",
            "本地 Qwen3-ASR 0.6B 8-bit（高准确率）",
            false,
            false,
        )
        .unwrap_err();
        assert!(error.contains("Qwen"));
        assert!(error.contains("未就绪"));
        let checking =
            dictation::ensure_active_asr_ready("local_qwen", "Qwen", false, true).unwrap_err();
        assert!(checking.contains("正在检查"));
        assert!(dictation::ensure_active_asr_ready("local_qwen", "Qwen", true, false).is_ok());
    }
}
