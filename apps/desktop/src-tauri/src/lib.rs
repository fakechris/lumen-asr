mod asr_models;
mod capsule;
mod commands;
mod config;
#[cfg(target_os = "macos")]
mod context_capture;
#[cfg(not(target_os = "macos"))]
#[path = "context_capture_stub.rs"]
mod context_capture;
mod corrector_cmd;
mod corrector_probe;
mod corrector_svc;
mod detection_stats;
mod dictation;
mod edit_learning_runtime;
mod headless;
mod hotkey;
mod hotkey_validate;
mod inject_cmd;
mod learning;
mod meeting_cmd;
mod meeting_detection;
mod meeting_live;
mod meeting_mic_aec;
mod meeting_system_audio;
mod mod_chord;
mod mode_arbiter;
mod onboard;
mod pane_observer;
mod permissions_cmd;
mod pipeline_attempt;
mod provider_presets;
mod session_debug;
mod volume_mon;

pub use headless::maybe_run_cli;

#[cfg(test)]
pub(crate) static MACOS_LIVE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use config::AppConfig;
use lumen_asr::{
    default_qwen_dir, default_sensevoice_dir, default_whisper_dir, qwen_ready,
    resolve_qwen_asr_dir, resolve_sensevoice_dir, sensevoice_ready, whisper_ready, AudioCapture,
    EngineKind, MeetingRecorder, QwenAsr, QwenAsrConfig, SenseVoiceSherpaAsr, WhisperAsr,
};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::{SessionArtifactPaths, Store};
use mode_arbiter::CaptureArbiter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Manager;

const QWEN_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Render filesystem paths with native separators in user-facing DTOs.
/// `PathBuf` accepts `/` on Windows, but exposing mixed separators in Settings
/// made a valid model directory look broken.
pub(crate) fn display_path(path: &Path) -> String {
    let value = path.display().to_string();
    #[cfg(target_os = "windows")]
    {
        value.replace('/', "\\")
    }
    #[cfg(not(target_os = "windows"))]
    {
        value
    }
}

/// Keep Windows installs on the documented shared model root even while the
/// pinned lumen-models revision still treats every non-macOS platform as Unix.
/// An explicit user/environment override always wins.
#[cfg(target_os = "windows")]
fn configure_windows_models_root() {
    let configured = std::env::var_os("LUMEN_MODELS_DIR")
        .filter(|value| !value.to_string_lossy().trim().is_empty());
    if configured.is_some() {
        return;
    }
    if let Some(local_app_data) =
        std::env::var_os("LOCALAPPDATA").filter(|value| !value.to_string_lossy().trim().is_empty())
    {
        std::env::set_var(
            "LUMEN_MODELS_DIR",
            PathBuf::from(local_app_data).join("Lumen").join("models"),
        );
    }
}

fn remove_session_artifacts(
    data_dir: &Path,
    artifacts: &SessionArtifactPaths,
) -> Result<(), String> {
    if let Some(audio_path) = artifacts.audio_path.as_deref() {
        let audio_path = Path::new(audio_path);
        let artifact_exists = audio_path.exists() || audio_path.parent().is_some_and(Path::exists);
        match session_debug::remove_session_debug_artifacts_from(
            data_dir,
            audio_path,
            &artifacts.session_id.to_string(),
        ) {
            Ok(true) => {}
            Ok(false) if !artifact_exists => {}
            Ok(false) => return Err(format!("refused to remove debug artifact: {audio_path:?}")),
            Err(error) => return Err(format!("remove debug artifact: {error}")),
        }
    }
    for artifact in &artifacts.context_artifacts {
        if artifact.manifest_path.is_empty() {
            continue;
        }
        let manifest_path = Path::new(&artifact.manifest_path);
        let artifact_exists =
            manifest_path.exists() || manifest_path.parent().is_some_and(Path::exists);
        match context_capture::remove_context_manifest_artifact(
            data_dir,
            artifact.capture_id,
            &artifact.manifest_path,
        ) {
            Ok(true) => {}
            Ok(false) if !artifact_exists => {}
            Ok(false) => {
                return Err(format!(
                    "refused to remove context artifact: {}",
                    artifact.manifest_path
                ))
            }
            Err(error) => return Err(format!("remove context artifact: {error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
fn purge_legacy_short_silent_sessions(store: &Store, data_dir: &Path) -> Result<usize, String> {
    let discarded = store
        .hidden_short_silent_session_artifacts()
        .map_err(|error| error.to_string())?;
    let mut removable = Vec::new();
    for artifacts in &discarded {
        match remove_session_artifacts(data_dir, artifacts) {
            Ok(()) => removable.push(artifacts.session_id),
            Err(error) => tracing::warn!(
                session_id = %artifacts.session_id,
                %error,
                "legacy silent capture cleanup will retry on next startup"
            ),
        }
    }
    store
        .delete_sessions(&removable)
        .map_err(|error| error.to_string())
}

fn schedule_legacy_short_silent_session_purge(app: tauri::AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        let data_dir = default_data_dir();
        let mut after_session_id = String::new();
        let mut purged_count = 0;
        loop {
            let batch = {
                let state = app.state::<AppState>();
                let guard = match state.store.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        tracing::warn!("store lock poisoned during silent capture cleanup");
                        return;
                    }
                };
                let Some(store) = guard.as_ref() else {
                    return;
                };
                match store.hidden_short_silent_session_artifact_batch(&after_session_id, 128) {
                    Ok(batch) => batch,
                    Err(error) => {
                        tracing::warn!(%error, "failed to list legacy short silent captures");
                        return;
                    }
                }
            };
            let Some(last_scanned_session_id) = batch.last_scanned_session_id else {
                break;
            };
            after_session_id = last_scanned_session_id;

            let mut removable = Vec::new();
            for artifacts in &batch.artifacts {
                match remove_session_artifacts(&data_dir, artifacts) {
                    Ok(()) => removable.push(artifacts.session_id),
                    Err(error) => tracing::warn!(
                        session_id = %artifacts.session_id,
                        %error,
                        "legacy silent capture cleanup will retry on next startup"
                    ),
                }
            }
            if removable.is_empty() {
                continue;
            }
            let state = app.state::<AppState>();
            let guard = match state.store.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::warn!("store lock poisoned while deleting silent capture rows");
                    return;
                }
            };
            let Some(store) = guard.as_ref() else {
                return;
            };
            match store.delete_sessions(&removable) {
                Ok(deleted) => purged_count += deleted,
                Err(error) => tracing::warn!(
                    %error,
                    "failed to delete cleaned legacy short silent capture rows"
                ),
            }
        }
        if purged_count > 0 {
            tracing::info!(
                count = purged_count,
                "purged legacy short silent captures and local artifacts"
            );
        }
    });
}

pub(crate) fn delete_session_with_artifacts(
    store: &Store,
    data_dir: &Path,
    session_id: uuid::Uuid,
) -> Result<bool, String> {
    let Some(artifacts) = store
        .session_artifacts(session_id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    remove_session_artifacts(data_dir, &artifacts)?;
    store
        .delete_session(session_id)
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone)]
pub struct QwenRuntimeStatus {
    pub executable: PathBuf,
    pub ready: bool,
    pub checking: bool,
    pub generation: u64,
}

pub struct AppState {
    pub store: Arc<Mutex<Option<Store>>>,
    pub(crate) edit_learning: edit_learning_runtime::DesktopEditLearning,
    pub audio: AudioCapture,
    /// Independent continuous recorder for meetings (never touches `audio`).
    pub meeting_recorder: MeetingRecorder,
    /// Held while a meeting is recording to prevent idle system sleep and App
    /// Nap so the audio capture callbacks are not suspended. `None` when no
    /// recording is active. Best-effort — acquiring never blocks recording.
    pub meeting_power_guard: std::sync::Mutex<Option<lumen_platform_macos::MeetingPowerGuard>>,
    /// Low-battery poll thread for the active recording: its stop flag and join
    /// handle, so `stop_meeting_recording` can signal + join it. `None` when no
    /// recording is active. All `Send`/`Sync` — never holds an objc2 object.
    pub meeting_battery_poll: std::sync::Mutex<
        Option<(
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::thread::JoinHandle<()>,
        )>,
    >,
    /// Silence-watchdog poll thread for the active recording: its stop flag and
    /// join handle, so `stop_meeting_recording` can signal + join it. `None`
    /// when no recording is active (or the silence auto-stop is disabled). All
    /// `Send`/`Sync` — never holds an objc2 object.
    pub meeting_watchdog: std::sync::Mutex<
        Option<(
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::thread::JoinHandle<()>,
        )>,
    >,
    /// Interrupted-recording recovery outcomes buffered at startup until the
    /// front-end drains them (it can miss the live event if recovery runs before
    /// its listener is ready). See `meeting_cmd::take_recovery_notices`.
    pub meeting_recovery_notices: std::sync::Mutex<Vec<meeting_cmd::MeetingRecoveryEvent>>,
    /// Real-time (P3) streaming-Paraformer live-transcript worker for the
    /// active recording. Idle (no worker) unless a recording is streaming.
    pub meeting_live: meeting_live::MeetingLive,
    /// Optional system-audio (remote participants) track for the active
    /// meeting recording. Capability-gated (macOS 14.2+ process tap);
    /// best-effort — idle everywhere else.
    pub meeting_system_audio: meeting_system_audio::MeetingSystemAudio,
    /// System-AEC (VoiceProcessingIO) mic capture for the active meeting
    /// recording. Opt-out via `meeting.mic_aec`; on any init failure the
    /// meeting falls back to `meeting_recorder` (plain cpal). macOS-only —
    /// idle everywhere else. Dictation never touches this.
    pub meeting_mic_aec: meeting_mic_aec::MeetingMicAec,
    /// Mutual-exclusion arbiter between dictation and meeting recording.
    pub capture: CaptureArbiter,
    /// Opt-in, capability-gated meeting activity detection + prompt policy.
    pub meeting_detection: meeting_detection::MeetingDetectionService,
    pub engine: Mutex<EngineKind>,
    pub sensevoice: Mutex<SenseVoiceSherpaAsr>,
    pub qwen: Mutex<QwenAsr>,
    pub qwen_runtime: Mutex<QwenRuntimeStatus>,
    pub whisper: Mutex<WhisperAsr>,
    pub config: Mutex<AppConfig>,
    pub context: context_capture::ContextRecorder,
}

/// Shared model-dir resolution policy for local engines (dictation + startup).
///
/// Backward compatible: a non-empty, valid `configured` override always wins.
/// When the user's config is empty or points at an unready dir, defer to the
/// shared cluster resolver (`shared` — shared root plus read-only legacy dirs),
/// and finally to the shared-root `default` (which may not exist yet) so
/// "not installed" reporting and downloads keep targeting the right place.
fn resolve_local_model_dir(
    configured: PathBuf,
    ready: impl Fn(&Path) -> bool,
    shared: impl FnOnce() -> Option<PathBuf>,
    default: impl FnOnce() -> PathBuf,
) -> PathBuf {
    if !configured.as_os_str().is_empty() && ready(&configured) {
        configured
    } else {
        shared().unwrap_or_else(default)
    }
}

fn qwen_engine_from_config(config: &config::AsrServiceConfig) -> QwenAsr {
    let model_dir = resolve_local_model_dir(
        config.model_dir_for(EngineKind::Qwen),
        qwen_ready,
        || resolve_qwen_asr_dir(None),
        default_qwen_dir,
    );
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
    #[cfg(target_os = "windows")]
    configure_windows_models_root();

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
    let context = context_capture::ContextRecorder::new(&app_config.context, &data_dir);
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

    let store = Arc::new(Mutex::new(store));
    let edit_learning = edit_learning_runtime::DesktopEditLearning::new(
        store.clone(),
        app_config.learning.persist_edit_evidence_text,
    );

    let initial_engine = dictation::engine_kind_for_provider(&app_config.asr.provider)
        .unwrap_or(EngineKind::SenseVoice);
    // Backward compatible: an explicit, valid config override wins; otherwise the
    // shared resolver finds the first ready SenseVoice across the shared cluster
    // root and legacy dirs (incl. Shandianshuo `sensevoice-small`, read-only), and
    // finally the shared-root default so "not installed" reporting / downloads work.
    let sv_dir = resolve_local_model_dir(
        app_config.asr.model_dir_for(EngineKind::SenseVoice),
        sensevoice_ready,
        || resolve_sensevoice_dir(None),
        default_sensevoice_dir,
    );
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
            store,
            edit_learning,
            audio,
            meeting_recorder: MeetingRecorder::new(),
            meeting_power_guard: Mutex::new(None),
            meeting_battery_poll: Mutex::new(None),
            meeting_watchdog: Mutex::new(None),
            meeting_recovery_notices: Mutex::new(Vec::new()),
            meeting_live: meeting_live::MeetingLive::default(),
            meeting_system_audio: meeting_system_audio::MeetingSystemAudio::default(),
            meeting_mic_aec: meeting_mic_aec::MeetingMicAec::default(),
            capture: CaptureArbiter::new(),
            meeting_detection: meeting_detection::MeetingDetectionService::new(),
            engine: Mutex::new(initial_engine),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(sv_dir)),
            qwen: Mutex::new(qwen),
            qwen_runtime: Mutex::new(qwen_runtime),
            whisper: Mutex::new(WhisperAsr::new(wh_dir)),
            config: Mutex::new(app_config),
            context,
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_health,
            commands::build_info,
            commands::list_sessions,
            commands::get_session,
            commands::list_session_attempts,
            commands::list_context_snapshots,
            commands::delete_session,
            commands::export_session_transcript,
            commands::save_session,
            commands::seed_demo_session,
            commands::list_edit_events,
            commands::list_edit_observations,
            commands::record_edit_event,
            commands::suggest_from_edit,
            commands::confirm_learn,
            commands::list_dictionary,
            commands::add_dictionary_term,
            commands::add_dictionary_replacement,
            commands::delete_dictionary_entry,
            dictation::list_audio_devices,
            dictation::get_audio_device,
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
            hotkey::start_fn_capture,
            hotkey::stop_fn_capture,
            meeting_cmd::start_meeting_recording,
            meeting_cmd::stop_meeting_recording,
            meeting_cmd::pause_meeting_recording,
            meeting_cmd::resume_meeting_recording,
            meeting_cmd::get_meeting_detection,
            meeting_cmd::set_meeting_detection_enabled,
            meeting_cmd::accept_meeting_detection,
            meeting_cmd::dismiss_meeting_detection,
            meeting_cmd::accept_meeting_detection_stop,
            meeting_cmd::decline_meeting_detection_stop,
            meeting_cmd::get_meeting_detection_stats,
            meeting_cmd::get_meeting_watchdog_config,
            meeting_cmd::set_meeting_watchdog_config,
            meeting_cmd::process_meeting_now,
            meeting_cmd::list_meetings,
            meeting_cmd::get_meeting_detail,
            meeting_cmd::save_meeting_notes,
            meeting_cmd::rename_meeting,
            meeting_cmd::delete_meeting,
            meeting_cmd::edit_meeting_segment,
            meeting_cmd::annotate_live_segment,
            meeting_cmd::list_live_annotations,
            meeting_cmd::delete_live_annotation,
            meeting_cmd::rename_live_annotations,
            meeting_cmd::take_recovery_notices,
            meeting_cmd::rename_speaker,
            meeting_cmd::reassign_segment_speaker,
            meeting_cmd::merge_speakers,
            meeting_cmd::enroll_speaker,
            meeting_cmd::list_enrolled_speakers,
            meeting_cmd::remove_enrolled_speaker,
            meeting_cmd::rename_enrolled_speaker,
            meeting_cmd::merge_enrolled_speakers,
            meeting_cmd::remove_speaker_sample,
            meeting_cmd::read_voiceprint_sample_audio,
            meeting_cmd::list_enroll_conflicts,
            meeting_cmd::resolve_enroll_conflict,
            meeting_cmd::get_meeting_voiceprints,
            meeting_cmd::reidentify_meeting,
            meeting_cmd::get_self_identity,
            meeting_cmd::set_self_identity,
            meeting_cmd::enroll_self_from_recordings,
            meeting_cmd::export_meeting,
            learning::get_learning_config,
            learning::save_learning_config,
            learning::process_edit,
            edit_learning_runtime::get_edit_learning_observability,
            edit_learning_runtime::list_edit_learning_feedback,
            edit_learning_runtime::acknowledge_edit_learning_feedback,
            edit_learning_runtime::list_edit_learning_proposals,
            edit_learning_runtime::decide_edit_learning_proposal,
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
            asr_models::start_paraformer_offline_download,
            asr_models::start_paraformer_streaming_download,
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

            app.state::<AppState>()
                .edit_learning
                .attach_app_handle(app.handle().clone());

            // Log AX status only — wizard/settings open System Settings on demand.
            permissions_cmd::bootstrap_permissions();

            if let Err(e) = hotkey::setup_hotkeys(app.handle()) {
                tracing::warn!(error = %e, "hotkey setup failed");
            }

            // Crash recovery: if a previous run was killed mid-recording, its
            // meeting is stuck in `Recording` with an un-finalized WAV on disk.
            // Salvage it (repair the header, transcribe the captured audio) or
            // mark it failed — off the launch path so the UI never blocks.
            meeting_cmd::recover_interrupted_meetings(app.handle().clone());
            schedule_legacy_short_silent_session_purge(app.handle().clone());

            // Register the imminent-sleep observer once, on the main thread
            // (Tauri setup runs on the main thread). It fires whenever the system
            // is about to sleep (lid close / forced sleep). Only warn when a
            // meeting is actually recording, so a routine sleep with no meeting
            // stays silent. The observer lives for the app's lifetime (its token
            // is leaked inside the platform layer).
            {
                let handle = app.handle().clone();
                lumen_platform_macos::install_will_sleep_observer(move || {
                    if handle
                        .try_state::<AppState>()
                        .is_some_and(|state| state.capture.is_meeting_recording())
                    {
                        meeting_cmd::emit_power_warning(&handle, "will-sleep", None);
                    }
                });
            }

            // Opt-in meeting detection: only start when the user enabled it AND
            // the OS capability is present. Off by default; failure to start
            // (unavailable capability) is silent — the feature just stays dark.
            let detection_enabled = app
                .state::<AppState>()
                .config
                .lock()
                .map(|cfg| cfg.meeting.detection_enabled)
                .unwrap_or(false);
            if detection_enabled {
                app.state::<AppState>()
                    .meeting_detection
                    .start(app.handle().clone());
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
            #[cfg(target_os = "macos")]
            if !lumen_platform_macos::is_accessibility_trusted() {
                tracing::warn!(
                    "hotkey event-tap needs Accessibility; using fallback monitors until granted"
                );
            }

            let debug_dir = session_debug::debug_root();
            let _ = std::fs::create_dir_all(&debug_dir);
            let log_path = lumen_platform::default_data_dir().join("logs/lumen.log");
            #[cfg(target_os = "macos")]
            tracing::info!(
                name = app.package_info().name,
                debug = %debug_dir.display(),
                log = %log_path.display(),
                accessibility = lumen_platform_macos::is_accessibility_trusted(),
                "Lumen ASR desktop starting (session debug enabled)"
            );
            #[cfg(not(target_os = "macos"))]
            tracing::info!(
                name = app.package_info().name,
                debug = %debug_dir.display(),
                log = %log_path.display(),
                "Lumen ASR desktop starting (copy-only platform mode)"
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use chrono::Utc;
    use lumen_core::{SessionRecord, SessionStatus};
    use lumen_store::{
        AttemptStatus, ContextSnapshotRecord, DictationAttemptRecord, PipelineIssueKind,
        PipelineStage, PipelineStageIssue,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn short_silent_marker(store: &Store, audio_path: Option<PathBuf>) -> Uuid {
        let mut session = SessionRecord::new();
        session.status = SessionStatus::Failed;
        session.audio_path = audio_path.map(|path| path.display().to_string());
        let mut attempt = DictationAttemptRecord::new(session.id);
        attempt.status = AttemptStatus::Failed;
        attempt.failed_stage = Some(PipelineStage::Capture);
        attempt.pipeline_metrics.audio_duration_ms = 500;
        attempt
            .pipeline_metrics
            .stage_issues
            .push(PipelineStageIssue {
                stage: PipelineStage::Capture,
                kind: PipelineIssueKind::AbsoluteSilence,
                message: "absolute_silence".into(),
            });
        store
            .save_short_silent_cleanup_marker(&session, &attempt)
            .unwrap();
        session.id
    }

    #[test]
    fn legacy_silence_purge_deletes_artifacts_and_retries_refused_paths() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("data");
        let store = Store::open(directory.path().join("history.sqlite")).unwrap();

        let valid_session_dir = data_dir.join("debug/valid-session");
        let valid_audio = valid_session_dir.join("audio_16k.wav");
        std::fs::create_dir_all(&valid_session_dir).unwrap();
        std::fs::write(&valid_audio, b"wav").unwrap();
        let valid_id = short_silent_marker(&store, Some(valid_audio));
        std::fs::write(
            valid_session_dir.join("meta.json"),
            format!(r#"{{"sessionId":"{valid_id}"}}"#),
        )
        .unwrap();
        let capture_id = Uuid::new_v4();
        let capture_dir = data_dir.join("context").join(capture_id.to_string());
        let manifest_path = capture_dir.join("manifest.r0001.v1.sealed.json");
        std::fs::create_dir_all(&capture_dir).unwrap();
        std::fs::write(&manifest_path, b"sealed").unwrap();
        let now = Utc::now();
        store
            .save_context_snapshot(&ContextSnapshotRecord {
                capture_id,
                session_id: valid_id,
                revision: 1,
                schema_version: 1,
                profile: "metadata".into(),
                target_generation: 1,
                started_at: now,
                frozen_at: now,
                completed_at: Some(now),
                manifest_path: manifest_path.display().to_string(),
                source_presence_bitmap: 0,
                source_status_json: "{}".into(),
                sanitized_hash: "hash".into(),
                encryption: "none".into(),
                status: "complete".into(),
            })
            .unwrap();

        let missing_id = short_silent_marker(
            &store,
            Some(data_dir.join("debug/missing-session/audio_16k.wav")),
        );

        let outside_dir = directory.path().join("outside");
        let outside_audio = outside_dir.join("audio_16k.wav");
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(&outside_audio, b"wav").unwrap();
        let refused_id = short_silent_marker(&store, Some(outside_audio));
        std::fs::write(
            outside_dir.join("meta.json"),
            format!(r#"{{"sessionId":"{refused_id}"}}"#),
        )
        .unwrap();

        assert_eq!(
            purge_legacy_short_silent_sessions(&store, &data_dir).unwrap(),
            2
        );
        assert!(!valid_session_dir.exists());
        assert!(!capture_dir.exists());
        assert!(store.get_session(valid_id).unwrap().is_none());
        assert!(store.get_session(missing_id).unwrap().is_none());
        assert!(store.get_session(refused_id).unwrap().is_some());
        assert!(outside_dir.exists());
    }

    #[test]
    fn explicit_session_deletion_removes_owned_audio_and_context() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("data");
        let store = Store::open(directory.path().join("history.sqlite")).unwrap();
        let mut session = SessionRecord::new();
        session.status = SessionStatus::Completed;
        session.asr_raw = Some("useful transcript".into());
        let debug_dir = data_dir.join("debug/visible-session");
        let audio_path = debug_dir.join("audio_16k.wav");
        std::fs::create_dir_all(&debug_dir).unwrap();
        std::fs::write(&audio_path, b"wav").unwrap();
        std::fs::write(
            debug_dir.join("meta.json"),
            format!(r#"{{"sessionId":"{}"}}"#, session.id),
        )
        .unwrap();
        session.audio_path = Some(audio_path.display().to_string());
        store.save_session(&session).unwrap();

        let capture_id = Uuid::new_v4();
        let capture_dir = data_dir.join("context").join(capture_id.to_string());
        let manifest_path = capture_dir.join("manifest.r0001.v1.sealed.json");
        std::fs::create_dir_all(&capture_dir).unwrap();
        std::fs::write(&manifest_path, b"sealed").unwrap();
        let now = Utc::now();
        store
            .save_context_snapshot(&ContextSnapshotRecord {
                capture_id,
                session_id: session.id,
                revision: 1,
                schema_version: 1,
                profile: "metadata".into(),
                target_generation: 1,
                started_at: now,
                frozen_at: now,
                completed_at: Some(now),
                manifest_path: manifest_path.display().to_string(),
                source_presence_bitmap: 0,
                source_status_json: "{}".into(),
                sanitized_hash: "hash".into(),
                encryption: "none".into(),
                status: "complete".into(),
            })
            .unwrap();

        assert!(delete_session_with_artifacts(&store, &data_dir, session.id).unwrap());
        assert!(!debug_dir.exists());
        assert!(!capture_dir.exists());
        assert!(store.get_session(session.id).unwrap().is_none());
        assert!(store.list_context_snapshots(session.id).unwrap().is_empty());
    }

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

/// Cross-platform (incl. Windows) unit tests for the shared model-dir resolution
/// policy that backs both dictation-engine startup and Qwen construction.
#[cfg(test)]
mod resolution_tests {
    use super::resolve_local_model_dir;
    use std::path::PathBuf;

    #[test]
    fn valid_config_override_wins_over_shared_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("user-model");
        let picked = resolve_local_model_dir(
            configured.clone(),
            |_| true, // configured dir is "ready"
            || panic!("shared resolver must not run when config override is valid"),
            || panic!("default must not run when config override is valid"),
        );
        assert_eq!(picked, configured);
    }

    #[test]
    fn empty_config_falls_back_to_shared_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared-root-model");
        let shared_for_closure = shared.clone();
        let picked = resolve_local_model_dir(
            PathBuf::new(), // no config override
            |_| true,
            move || Some(shared_for_closure),
            || panic!("default must not run when shared resolution succeeds"),
        );
        assert_eq!(picked, shared);
    }

    #[test]
    fn unready_config_override_is_ignored_for_shared_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("stale-user-model");
        let shared = dir.path().join("shared-root-model");
        let shared_for_closure = shared.clone();
        let picked = resolve_local_model_dir(
            configured,
            |_| false, // configured dir exists in config but is not ready
            move || Some(shared_for_closure),
            || panic!("default must not run when shared resolution succeeds"),
        );
        assert_eq!(picked, shared);
    }

    #[test]
    fn nothing_installed_falls_back_to_default_for_not_installed_status() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("shared-download-target");
        let default_for_closure = default.clone();
        let picked = resolve_local_model_dir(
            PathBuf::new(),
            |_| true,
            || None, // shared resolver finds nothing ready anywhere
            move || default_for_closure,
        );
        assert_eq!(picked, default);
    }
}
