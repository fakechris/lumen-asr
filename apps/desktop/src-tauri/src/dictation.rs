//! Recording + local ASR + model corrector IPC (M2–M5).

use crate::config::{AsrServiceConfig, VadConfig};
use crate::context_capture::{
    ActiveContextCapture, CorrectorContextProjection, StageUsageInput, TargetHint,
};
use crate::pipeline_attempt::{
    apply_asr_result, build_pipeline_identity, elapsed_ms, mark_attempt_failed, persist_attempt,
    run_corrector_stage, write_attempt_debug, AttemptDebug,
};
use crate::session_debug;
use crate::AppState;
use lumen_asr::{
    lumen_models_dir, paraformer_offline_ready, prepare_for_asr, probe_status, sensevoice_ready,
    whisper_ready, AsrEngine, AsrRequest, AsrResult, AudioDeviceInfo, EngineKind, EngineStatus,
    OpenAiAudioAsr, OpenAiAudioConfig, ParaformerAsr,
};
use lumen_core::{FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use lumen_platform_macos::{
    activate_target, frontmost_app_name, frontmost_target, is_self_app_name, is_self_target,
    FrontmostTarget,
};
use lumen_prompts::IntentSpec;
use lumen_store::{
    should_discard_short_silent_capture, AttemptStatus, ContextStageUsage, DictationAttemptRecord,
    InsertionOutcome, PipelineIssueKind, PipelineStage, PipelineStageIssue,
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// Frontmost app captured when hotkey dictation starts — restored before paste.
static TARGET: Mutex<Option<FrontmostTarget>> = Mutex::new(None);
static PANE_TARGET: Mutex<PaneTargetState> = Mutex::new(PaneTargetState {
    generation: 0,
    completed: true,
    deadline: None,
    cancellation: None,
    pane: None,
});
static PANE_TARGET_READY: Condvar = Condvar::new();
const PANE_DISCOVERY_BUDGET: Duration = Duration::from_secs(2);

struct PaneTargetState {
    generation: u64,
    completed: bool,
    deadline: Option<Instant>,
    cancellation: Option<Arc<AtomicBool>>,
    pane: Option<crate::pane_observer::LockedPane>,
}

struct PaneDiscoveryStart {
    generation: u64,
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
}

/// Intent bound to the current dictation session (default / translate / raw).
static SESSION_INTENT: Mutex<IntentSpec> = Mutex::new(IntentSpec::Default);
/// UI-facing copy of session intent; kept until capsule goes idle so processing
/// phase still knows “翻译” after take_session_intent() for the corrector.
static UI_SESSION_INTENT: Mutex<IntentSpec> = Mutex::new(IntentSpec::Default);

pub fn set_session_intent(intent: IntentSpec) {
    if let Ok(mut g) = SESSION_INTENT.lock() {
        *g = intent.clone();
    }
    if let Ok(mut g) = UI_SESSION_INTENT.lock() {
        *g = intent;
    }
}

fn take_session_intent() -> IntentSpec {
    SESSION_INTENT
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or(IntentSpec::Default)
}

fn peek_ui_intent() -> IntentSpec {
    UI_SESSION_INTENT
        .lock()
        .map(|g| g.clone())
        .unwrap_or(IntentSpec::Default)
}

fn clear_ui_intent() {
    if let Ok(mut g) = UI_SESSION_INTENT.lock() {
        *g = IntentSpec::Default;
    }
}

/// Serial dictation lifecycle — prevents overlapping start/stop thrash (felt like crash).
const PHASE_IDLE: u8 = 0;
const PHASE_RECORDING: u8 = 1;
const PHASE_PROCESSING: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);
static UI_NOTICE_EPOCH: AtomicU64 = AtomicU64::new(0);
static UI_TRANSITION: Mutex<()> = Mutex::new(());
static RECORD_STARTED: Mutex<Option<Instant>> = Mutex::new(None);

/// Only discard as bounce if shorter than this *and* almost no audio.
const BOUNCE_MS: u128 = 80;
/// Reject only digital silence / sub-quantization noise. A peak above 1e-6 is
/// still sent to ASR, so quiet human speech remains fail-open.
const ABSOLUTE_SILENCE_PEAK: f32 = 1.0e-6;
const ABSOLUTE_SILENCE_ISSUE: &str = "absolute_silence";
const ABSOLUTE_SILENCE_MESSAGE: &str =
    "未检测到麦克风信号。请检查麦克风权限、输入设备或静音状态后重试。";
const INVALID_CAPTURE_MESSAGE: &str = "录音数据无效，请检查输入设备后重试。";
const INVALID_CAPTURE_ISSUE: &str = "invalid_audio_signal";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureSignalIssue {
    AbsoluteSilence,
    InvalidSignal,
}

impl CaptureSignalIssue {
    fn message(self) -> &'static str {
        match self {
            Self::AbsoluteSilence => ABSOLUTE_SILENCE_MESSAGE,
            Self::InvalidSignal => INVALID_CAPTURE_MESSAGE,
        }
    }
}

/// Snapshot frontmost app into process-local cache (sync, preferred at press).
fn remember_target_app() -> (Option<FrontmostTarget>, PaneDiscoveryStart) {
    let t = frontmost_target();
    let target = match &t {
        Some(t) if !is_self_target(t) => {
            tracing::info!(
                name = ?t.name,
                bundle = ?t.bundle_id,
                "dictation target remembered"
            );
            Some(t.clone())
        }
        other => {
            tracing::warn!(?other, "could not remember non-self frontmost target");
            None
        }
    };
    // Never reuse a target from an earlier dictation generation.
    if let Ok(mut current) = TARGET.lock() {
        *current = target.clone();
    }
    let deadline = Instant::now() + PANE_DISCOVERY_BUDGET;
    let cancellation = Arc::new(AtomicBool::new(false));
    let generation = PANE_TARGET
        .lock()
        .map(|mut state| {
            if let Some(previous) = state.cancellation.take() {
                previous.store(true, Ordering::Release);
            }
            state.generation = state.generation.wrapping_add(1);
            state.completed = false;
            state.deadline = Some(deadline);
            state.cancellation = Some(cancellation.clone());
            state.pane = None;
            state.generation
        })
        .unwrap_or_default();
    (
        target,
        PaneDiscoveryStart {
            generation,
            deadline,
            cancellation,
        },
    )
}

fn discover_pane_target(
    target: Option<crate::pane_observer::PaneDiscoveryTarget>,
    generation: u64,
) {
    std::thread::spawn(move || {
        let pane = match target.map(crate::pane_observer::identify_pane) {
            Some(Ok(pane)) => pane,
            Some(Err(reason)) => {
                tracing::warn!(
                    %reason,
                    "terminal pane discovery failed; edit observation may use Accessibility fallback"
                );
                None
            }
            None => None,
        };
        if let Some(pane) = pane.as_ref() {
            tracing::info!(
                observer = pane.observer_id(),
                "terminal pane target remembered"
            );
        }
        if let Ok(mut state) = PANE_TARGET.lock() {
            if state.generation != generation {
                return;
            }
            state.pane = pane;
            state.completed = true;
            state.deadline = None;
            PANE_TARGET_READY.notify_all();
        }
    });
}

fn take_discovered_pane_target() -> Option<crate::pane_observer::LockedPane> {
    let mut state = PANE_TARGET.lock().ok()?;
    while !state.completed {
        let Some(remaining) = state
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
        else {
            break;
        };
        let waited = PANE_TARGET_READY.wait_timeout(state, remaining);
        let (next_state, timeout) = match waited {
            Ok(result) => result,
            Err(poisoned) => poisoned.into_inner(),
        };
        state = next_state;
        if timeout.timed_out() {
            break;
        }
    }
    let pane = state.pane.clone();
    if let Some(cancellation) = state.cancellation.take() {
        cancellation.store(true, Ordering::Release);
    }
    state.generation = state.generation.wrapping_add(1);
    state.completed = true;
    state.deadline = None;
    pane
}

fn cancel_pane_target_discovery() {
    if let Ok(mut state) = PANE_TARGET.lock() {
        if let Some(cancellation) = state.cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
        state.generation = state.generation.wrapping_add(1);
        state.completed = true;
        state.deadline = None;
        state.pane = None;
        PANE_TARGET_READY.notify_all();
    }
}

/// Prepare for insert:
/// - Hide our UI
/// - Only re-activate cached target if *we* became frontmost
/// - Never force-activate when the typing target is already frontmost
///   (avoids dropping the text-field caret)
fn restore_target_app_before_insert(app: Option<&AppHandle>) -> Option<String> {
    if let Some(app) = app {
        crate::capsule::set_capsule_visible(app, false, "pre-insert");
        crate::capsule::ensure_main_stays_background(app);
    }

    let target = TARGET.lock().ok().and_then(|g| g.clone());
    let current = frontmost_app_name();
    tracing::info!(
        target = ?target.as_ref().and_then(|t| t.name.clone()),
        frontmost = ?current,
        "pre-insert focus state"
    );

    let need_activate = match &current {
        Some(c) if is_self_app_name(c) => true,
        None => true,
        Some(_) => false,
    };

    if need_activate {
        if let Some(ref t) = target {
            if !is_self_target(t) {
                tracing::info!(
                    name = ?t.name,
                    bundle = ?t.bundle_id,
                    "Lumen stole frontmost — restoring target"
                );
                let ok = activate_target(t);
                tracing::info!(ok, "activate_target result");
                std::thread::sleep(std::time::Duration::from_millis(180));
            }
        }
    } else {
        tracing::info!("target already frontmost — skip activate (preserve caret)");
    }

    frontmost_app_name()
}

fn insertion_outcome_for_strategy(strategy: InsertStrategy) -> InsertionOutcome {
    match strategy {
        InsertStrategy::Paste | InsertStrategy::Ax | InsertStrategy::Type => {
            InsertionOutcome::Inserted
        }
        InsertStrategy::CopyOnly => InsertionOutcome::Copied,
        InsertStrategy::None => InsertionOutcome::Failed,
    }
}

/// Capsule / overlay copy confirmation. Keep this short — the HUD is a pill.
fn copied_toast_notice() -> String {
    "已复制".into()
}

fn copy_only_fallback_notice(clipboard_copied: bool) -> String {
    #[cfg(target_os = "macos")]
    {
        if clipboard_copied {
            "已复制 · 请开启辅助功能后可自动插入".into()
        } else {
            "需要「辅助功能」权限才能插入到其他 App，并且复制到剪贴板也失败了。请先从历史记录复制结果，并到 系统设置 → 隐私与安全性 → 辅助功能 打开 Lumen（或 lumen-asr-desktop）后重试。"
                .into()
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if clipboard_copied {
            copied_toast_notice()
        } else {
            "未能插入，也无法写入剪贴板。请从历史记录复制结果。".into()
        }
    }
}

fn insert_failure_notice(error: &str, copied: bool) -> String {
    let elevated = error.to_ascii_lowercase().contains("elevated");
    match (copied, elevated) {
        (true, true) => "已复制 · 目标窗口可能以管理员运行".into(),
        (true, false) => copied_toast_notice(),
        (false, true) => {
            "目标窗口拒绝输入（可能正在以管理员身份运行）。也无法写入剪贴板，请从历史记录复制结果。"
                .into()
        }
        (false, false) => "未能插入，也无法写入剪贴板。请从历史记录复制结果。".into(),
    }
}

fn capsule_notice_hold(outcome: &TranscribeOutcome) -> Duration {
    let copy_only = outcome
        .insert_notice
        .as_deref()
        .is_some_and(|notice| notice.starts_with("已复制"));
    if copy_only && outcome.fallback_reason.is_none() {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(4)
    }
}

/// When cloud ASR is selected and a local engine is ready, do not wait out the
/// full HTTP timeout before giving the user text.
const CLOUD_ASR_HEDGE: Duration = Duration::from_secs(8);

fn cloud_asr_hedge_deadline(configured: Duration, local_ready: bool) -> Duration {
    if local_ready {
        configured.min(CLOUD_ASR_HEDGE)
    } else {
        configured
    }
}

fn cloud_asr_error_is_hedgeable(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("timeout")
        || error.contains("timed out")
        || error.contains("error sending request")
        || error.contains("connection")
        || error.contains("connect error")
        || error.contains("dns error")
        || error.contains("network")
}

fn asr_engine_hedge_label(selected_provider: &str, local_engine: EngineKind) -> String {
    format!("{selected_provider}→{}", local_engine.as_str())
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AsrStatus {
    pub recording: bool,
    /// Local runtime engine when the selected provider runs on-device.
    pub engine: EngineKind,
    /// Settings provider id (local_sensevoice | local_qwen | openai_audio | …).
    pub provider: String,
    pub sensevoice: EngineStatus,
    pub qwen: EngineStatus,
    pub whisper: EngineStatus,
    pub active_ready: bool,
    /// Short label for UI (e.g. "OpenAI Audio · whisper-1").
    pub provider_label: String,
}

#[tauri::command]
pub fn list_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    lumen_asr::AudioCapture::list_devices().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_audio_device(state: State<'_, AppState>) -> Result<Option<String>, String> {
    state
        .config
        .lock()
        .map(|cfg| cfg.audio.device_name.clone())
        .map_err(|_| "config lock poisoned".to_string())
}

#[tauri::command]
pub fn set_audio_device(state: State<'_, AppState>, name: Option<String>) -> Result<(), String> {
    state.audio.set_device(name.clone());
    // Persist preferred device for onboarding + next launch.
    if let Ok(mut cfg) = state.config.lock() {
        cfg.audio.device_name = name.filter(|s| !s.is_empty());
        let _ = cfg.save();
    }
    Ok(())
}

fn ensure_audible_capture(rms: f32, peak: f32) -> Result<(), CaptureSignalIssue> {
    if !rms.is_finite() || !peak.is_finite() {
        Err(CaptureSignalIssue::InvalidSignal)
    } else if peak > ABSOLUTE_SILENCE_PEAK {
        Ok(())
    } else {
        Err(CaptureSignalIssue::AbsoluteSilence)
    }
}

fn record_capture_signal_issue(attempt: &mut DictationAttemptRecord, issue: CaptureSignalIssue) {
    let (kind, message) = match issue {
        CaptureSignalIssue::AbsoluteSilence => {
            (PipelineIssueKind::AbsoluteSilence, ABSOLUTE_SILENCE_ISSUE)
        }
        CaptureSignalIssue::InvalidSignal => {
            (PipelineIssueKind::InputUnavailable, INVALID_CAPTURE_ISSUE)
        }
    };
    attempt
        .pipeline_metrics
        .stage_issues
        .push(PipelineStageIssue {
            stage: PipelineStage::Capture,
            kind,
            message: message.into(),
        });
}

pub(crate) fn canonical_asr_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "sensevoice" | "local_sensevoice" => "local_sensevoice".into(),
        "qwen" | "qwen3_asr" | "local_qwen" => "local_qwen".into(),
        "whisper" | "local_whisper" => "local_whisper".into(),
        other => other.into(),
    }
}

pub(crate) fn engine_kind_for_provider(provider: &str) -> Option<EngineKind> {
    match canonical_asr_provider(provider).as_str() {
        "local_sensevoice" => Some(EngineKind::SenseVoice),
        "local_qwen" => Some(EngineKind::Qwen),
        "local_whisper" => Some(EngineKind::Whisper),
        _ => None,
    }
}

#[tauri::command]
pub fn set_asr_engine(
    app: AppHandle,
    state: State<'_, AppState>,
    engine: String,
) -> Result<EngineKind, String> {
    let _ = &app;
    // Accept either local engine names or full provider ids from Settings.
    let provider_id = canonical_asr_provider(&engine);
    let kind = match provider_id.as_str() {
        "local_sensevoice" => EngineKind::SenseVoice,
        "local_qwen" => EngineKind::Qwen,
        "local_whisper" => EngineKind::Whisper,
        "openai_audio" | "custom" => EngineKind::SenseVoice,
        other if other.starts_with("local_") => {
            return Err(format!("unknown local engine: {engine}"));
        }
        other => {
            // Cloud / config_only providers: store in asr config; local engine unchanged.
            if let Ok(mut cfg) = state.config.lock() {
                if let Some(p) = crate::provider_presets::asr_preset_by_id(other) {
                    cfg.asr.provider = p.id;
                    if !p.base_url.is_empty() {
                        cfg.asr.base_url = p.base_url;
                    }
                    if !p.default_model.is_empty() {
                        cfg.asr.model = p.default_model;
                    }
                    let _ = cfg.save();
                } else {
                    cfg.asr.provider = other.to_string();
                    let _ = cfg.save();
                }
            }
            unload_qwen(&state);
            return Ok(state
                .engine
                .lock()
                .map(|g| *g)
                .unwrap_or(EngineKind::SenseVoice));
        }
    };
    *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())? = kind;
    if kind != EngineKind::Qwen {
        unload_qwen(&state);
    }
    if let Ok(mut cfg) = state.config.lock() {
        cfg.asr.provider = provider_id.clone();
        if provider_id == "openai_audio" {
            if cfg.asr.base_url.is_empty() {
                cfg.asr.base_url = "https://api.openai.com/v1".into();
            }
            if cfg.asr.model.is_empty() {
                cfg.asr.model = "whisper-1".into();
            }
        }
        let _ = cfg.save();
    }
    Ok(kind)
}

pub(crate) fn unload_qwen(state: &AppState) {
    // sherpa-onnx Qwen3-ASR: dropping the cached recognizer releases the model;
    // the next request reloads it lazily. False just means nothing was loaded.
    if let Ok(engine) = state.qwen.lock() {
        engine.unload();
    }
}

#[tauri::command]
pub fn get_asr_status(state: State<'_, AppState>) -> Result<AsrStatus, String> {
    Ok(asr_status_from(&state))
}

pub fn asr_status_from(state: &AppState) -> AsrStatus {
    let engine = state
        .engine
        .lock()
        .map(|g| *g)
        .unwrap_or(EngineKind::SenseVoice);
    let asr_cfg = state
        .config
        .lock()
        .map(|c| c.asr.clone())
        .unwrap_or_default();
    let provider = if asr_cfg.provider.is_empty() {
        match engine {
            EngineKind::Qwen => "local_qwen".into(),
            EngineKind::Whisper => "local_whisper".into(),
            // The runtime engine slot only ever holds a local engine.
            _ => "local_sensevoice".into(),
        }
    } else {
        canonical_asr_provider(&asr_cfg.provider)
    };
    let mut sv = state
        .sensevoice
        .lock()
        .map(|engine| probe_status(EngineKind::SenseVoice, Some(&engine.model_dir())))
        .unwrap_or_else(|_| lumen_asr::sensevoice_status());
    let mut wh = state
        .whisper
        .lock()
        .map(|engine| probe_status(EngineKind::Whisper, Some(&engine.model_dir())))
        .unwrap_or_else(|_| lumen_asr::whisper_status());
    let mut qwen = state
        .qwen
        .lock()
        .map(|engine| probe_status(EngineKind::Qwen, Some(engine.model_dir())))
        .unwrap_or_else(|_| lumen_asr::qwen_status());
    sv.model_dir = crate::display_path(std::path::Path::new(&sv.model_dir));
    wh.model_dir = crate::display_path(std::path::Path::new(&wh.model_dir));
    qwen.model_dir = crate::display_path(std::path::Path::new(&qwen.model_dir));
    let active_ready = match provider.as_str() {
        "local_sensevoice" => sv.ready,
        "local_qwen" => qwen.ready,
        "local_whisper" => wh.ready,
        "openai_audio" | "custom" => !asr_cfg.api_key.is_empty() || !asr_cfg.base_url.is_empty(),
        // config_only: selectable but not runnable yet
        _ => false,
    };
    let provider_label = crate::provider_presets::asr_preset_by_id(&provider)
        .map(|p| {
            if provider.starts_with("local_") || asr_cfg.model.is_empty() {
                p.label
            } else {
                format!("{} · {}", p.label, asr_cfg.model)
            }
        })
        .unwrap_or_else(|| provider.clone());
    AsrStatus {
        recording: state.audio.is_recording(),
        engine,
        provider,
        sensevoice: sv,
        qwen,
        whisper: wh,
        active_ready,
        provider_label,
    }
}

#[tauri::command]
pub fn start_recording(state: State<'_, AppState>) -> Result<(), String> {
    start_recording_inner(&state)
}

pub(crate) fn ensure_active_asr_ready(
    provider: &str,
    provider_label: &str,
    ready: bool,
) -> Result<(), String> {
    if ready {
        return Ok(());
    }
    let guidance = match canonical_asr_provider(provider).as_str() {
        "local_qwen" => "请先在「设置 → 语音识别」下载或选择有效的 Qwen3-ASR（sherpa-onnx）模型。",
        "local_sensevoice" => "请先安装或选择有效的 SenseVoice 模型。",
        "local_whisper" => "请先选择有效的 Whisper 模型。",
        "openai_audio" | "custom" => "请先完成在线 ASR 的地址与凭据配置。",
        _ => "当前 ASR 尚未接入可运行的识别客户端。",
    };
    Err(format!("{provider_label} 未就绪。{guidance}"))
}

/// Arm the configured VAD backend for the upcoming dictation recording.
///
/// Must run before `state.audio.start()`: silero mode attaches the silero VAD
/// (the model loads here, off the audio thread, on first use); rms / disabled
/// / model-unavailable all detach it so the capture callback does zero extra
/// work. Fail-open throughout — a silero problem logs and leaves the RMS path
/// in charge, recording is never affected.
fn configure_session_vad(state: &AppState) {
    let vad = state.config.lock().ok().map(|config| config.vad.clone());
    let Some(vad) = vad else { return };
    if !vad.enabled || vad.mode != "silero" {
        let _ = state.audio.set_silero_vad(None);
        return;
    }
    let Some(model_path) = resolve_silero_model_path(&vad) else {
        tracing::warn!("vad.mode=silero but the model is not installed; using rms this session");
        let _ = state.audio.set_silero_vad(None);
        kick_silero_model_download();
        return;
    };
    match state.audio.set_silero_vad(Some(&model_path)) {
        Ok(()) => tracing::info!(path = %model_path.display(), "silero vad armed"),
        Err(error) => {
            tracing::warn!(%error, "silero vad unavailable; using rms this session");
            let _ = state.audio.set_silero_vad(None);
        }
    }
}

/// Configured silero model path wins; empty config resolves the shared
/// lumen-models install. Returns `None` when no usable model file exists.
fn resolve_silero_model_path(vad: &VadConfig) -> Option<PathBuf> {
    let configured = vad.silero_model_path.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    lumen_asr::silero_vad_model_path(&lumen_asr::default_silero_vad_dir())
}

/// First silero session downloads the model in the background (~2 MB); the
/// current session runs RMS, the next one picks silero up. Runs at most once
/// per process.
fn kick_silero_model_download() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let cancel = AtomicBool::new(false);
        match lumen_asr::download_silero_vad_package(
            &lumen_asr::lumen_models_dir(),
            &cancel,
            |p| tracing::debug!(phase = %p.phase, "silero vad model download"),
        ) {
            Ok(dir) => tracing::info!(dir = %dir.display(), "silero vad model installed"),
            Err(error) => tracing::warn!(%error, "silero vad model download failed"),
        }
    });
}

pub fn start_recording_inner(state: &AppState) -> Result<(), String> {
    if state.audio.is_recording() {
        return Ok(());
    }
    let status = asr_status_from(state);
    ensure_active_asr_ready(&status.provider, &status.provider_label, status.active_ready)?;
    let (target, pane_discovery) = remember_target_app();
    let hint = target.as_ref().map(|target| TargetHint {
        app_name: target.name.clone(),
        bundle_id: target.bundle_id.clone(),
        ..TargetHint::default()
    });
    state.context.begin(hint);
    configure_session_vad(state);
    state.audio.start().map_err(|error| {
        cancel_pane_target_discovery();
        state.context.clear_active();
        error.to_string()
    })?;
    crate::permissions_cmd::mark_microphone_capture_started();
    // Notify the capture arbiter that a dictation is now live (CaptureMode::
    // Dictation). Recording is already running here, so this is a state-only
    // signal — it never touches the audio/hotkey path. A meeting suspends the
    // dictation hotkey, so this normally can't collide; log if it ever does.
    if let Err(e) = state.capture.begin_dictation() {
        tracing::warn!(error = %e, "arbiter begin_dictation (meeting active?)");
    }
    let pane_observation_enabled = state
        .config
        .lock()
        .map(|config| config.inject.auto_insert && config.learning.post_paste_capture)
        .unwrap_or(false);
    if pane_observation_enabled {
        let reserved = state.edit_learning.reserve_target(target.as_ref());
        tracing::info!(
            target_bundle_id = ?target.as_ref().and_then(|value| value.bundle_id.as_deref()),
            reserved,
            "edit-learning target reservation attempted at recording start"
        );
        // Recording is already live while we synchronously lock the exact outer
        // surface; provider-specific probing then continues in the background.
        let pane_target = target.as_ref().and_then(|target| {
            crate::pane_observer::capture_pane_target(
                target,
                pane_discovery.deadline,
                pane_discovery.cancellation,
            )
        });
        // A generation guard discards any result that arrives after this
        // dictation has moved on.
        discover_pane_target(pane_target, pane_discovery.generation);
    } else {
        state
            .edit_learning
            .clear_pending("post_paste_capture_disabled");
        cancel_pane_target_discovery();
    }
    Ok(())
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeOutcome {
    pub text: String,
    pub asr_text: String,
    pub corrected_text: String,
    pub model_applied: bool,
    pub fallback_reason: Option<String>,
    pub asr_engine: String,
    pub corrector_engine: String,
    pub sample_rate: u32,
    pub num_samples: usize,
    pub duration_ms: u64,
    pub session: SessionRecord,
    /// True when the backend successfully anchored and started post-paste edit watch.
    pub watch_post_paste: bool,
    pub post_paste_seconds: u64,
    /// Human-readable insert fallback. Distinct from ASR failure and corrector fallback.
    pub insert_notice: Option<String>,
}

#[tauri::command]
pub async fn stop_and_transcribe(
    app: AppHandle,
    state: State<'_, AppState>,
    save: Option<bool>,
) -> Result<TranscribeOutcome, String> {
    stop_and_transcribe_inner(&state, save.unwrap_or(true), Some(&app)).await
}

struct FrozenContextAttachment {
    corrector_projection: Option<CorrectorContextProjection>,
    late_archive: Option<ActiveContextCapture>,
}

fn schedule_late_context_archive(active: Option<ActiveContextCapture>, app: Option<&AppHandle>) {
    let (Some(active), Some(app)) = (active, app) else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if let Err(error) = active.archive(&state.store).await {
            tracing::warn!(error = %error, "late context archive failed");
        }
    });
}

async fn attach_frozen_context(
    state: &AppState,
    active: Option<&ActiveContextCapture>,
    attempt: &mut DictationAttemptRecord,
) -> FrozenContextAttachment {
    let Some(active) = active else {
        attempt
            .pipeline_inputs
            .stage_usages
            .push(ContextStageUsage {
                stage: PipelineStage::Asr,
                sources: vec!["captured_context".into()],
                captured: false,
                not_used_reason: Some("capture_session_missing".into()),
                ..ContextStageUsage::default()
            });
        return FrozenContextAttachment {
            corrector_projection: None,
            late_archive: None,
        };
    };

    match active.freeze(&state.store).await {
        Ok(frozen) => {
            let input_ref = frozen.input_ref;
            let captured = input_ref.source_presence_bitmap != 0;
            let should_archive = input_ref.source_status_summary == "partial";
            attempt.pipeline_inputs.context = Some(input_ref);
            match state.context.record_stage_usage(StageUsageInput {
                capture_id: Some(active.capture_id.0),
                attempt_id: attempt.id,
                stage: PipelineStage::Asr,
                sources: vec!["captured_context".into()],
                projection: None,
                captured,
                selected: false,
                consumed: false,
                sent: false,
                not_used_reason: Some("captured_context_not_projected_to_asr".into()),
            }) {
                Ok(usage) => attempt.pipeline_inputs.stage_usages.push(usage),
                Err(error) => tracing::warn!(error = %error, "failed to record ASR context usage"),
            }
            FrozenContextAttachment {
                corrector_projection: frozen.corrector_projection,
                late_archive: should_archive.then(|| active.clone()),
            }
        }
        Err(error) => {
            attempt
                .pipeline_metrics
                .stage_issues
                .push(PipelineStageIssue {
                    stage: PipelineStage::Capture,
                    kind: PipelineIssueKind::InputUnavailable,
                    message: error.clone(),
                });
            attempt
                .pipeline_inputs
                .stage_usages
                .push(ContextStageUsage {
                    stage: PipelineStage::Asr,
                    sources: vec!["captured_context".into()],
                    captured: false,
                    not_used_reason: Some("context_persistence_failed".into()),
                    ..ContextStageUsage::default()
                });
            tracing::warn!(error = %error, "failed to freeze context input");
            FrozenContextAttachment {
                corrector_projection: None,
                late_archive: None,
            }
        }
    }
}

pub async fn stop_and_transcribe_inner(
    state: &AppState,
    save: bool,
    app: Option<&AppHandle>,
) -> Result<TranscribeOutcome, String> {
    let pipeline_started = Instant::now();
    let clear_unconsumed_edit_target = || {
        state
            .edit_learning
            .clear_pending("dictation_ended_before_observed_insertion");
        cancel_pane_target_discovery();
    };
    let active_context = state.context.take_active();
    let target = TARGET.lock().ok().and_then(|guard| guard.clone());
    let mut rec = SessionRecord::new();
    if let Some(active) = active_context.as_ref() {
        rec.id = active.session_id;
    }
    rec.focus = FocusInfo {
        app_name: target.as_ref().and_then(|value| value.name.clone()),
        bundle_id: target.as_ref().and_then(|value| value.bundle_id.clone()),
        window_title: None,
    };
    let engine_kind = *state.engine.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("engine lock poisoned before capture stop; recovering snapshot");
        poisoned.into_inner()
    });
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|poisoned| {
            tracing::warn!("config lock poisoned before capture stop; recovering snapshot");
            poisoned.into_inner()
        })
        .clone();
    let asr_cfg = cfg.asr.clone();
    let asr_engine_str = if asr_cfg.provider.starts_with("local") {
        engine_kind.as_str().to_string()
    } else {
        asr_cfg.provider.clone()
    };
    let intent = take_session_intent();
    let mut attempt = DictationAttemptRecord::new(rec.id);
    attempt.pipeline_identity = build_pipeline_identity(
        state,
        &cfg,
        engine_kind,
        &asr_engine_str,
        "not_run",
        intent.clone(),
    );

    let capture_result = state.audio.stop();
    // Dictation capture is done — return the arbiter to Idle so a meeting can
    // start again. State-only signal; the audio path already stopped above.
    state.capture.end_dictation();
    let FrozenContextAttachment {
        corrector_projection: captured_context,
        late_archive: mut late_context_archive,
    } = attach_frozen_context(state, active_context.as_ref(), &mut attempt).await;
    let mut capture = match capture_result {
        Ok(capture) => capture,
        Err(error) => {
            let error = error.to_string();
            mark_attempt_failed(
                &mut attempt,
                PipelineStage::Capture,
                &error,
                pipeline_started,
            );
            rec.status = SessionStatus::Failed;
            rec.asr_engine = Some(engine_kind.as_str().into());
            schedule_late_context_archive(late_context_archive.take(), app);
            if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist capture stop failure");
            }
            clear_unconsumed_edit_target();
            return Err(error);
        }
    };
    // VAD: drop the silent tail so ASR does not scan dead air (300ms padding
    // kept so the last syllable is never clipped).
    if cfg.vad.enabled && cfg.vad.trim_trailing {
        let cut = lumen_asr::trim_trailing_silence(
            &capture.samples,
            capture.sample_rate,
            cfg.vad.end_threshold,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        if cut < capture.samples.len() {
            tracing::info!(
                dropped_samples = capture.samples.len() - cut,
                "vad trimmed trailing silence"
            );
            capture.samples.truncate(cut);
        }
    }
    let num_samples = capture.samples.len();
    let sample_rate = capture.sample_rate;
    let duration_ms = if sample_rate > 0 {
        (num_samples as u64 * 1000) / sample_rate as u64
    } else {
        0
    };
    let (rms_cap, peak_cap) = session_debug::audio_stats(&capture.samples);
    tracing::info!(
        num_samples,
        sample_rate,
        duration_ms,
        rms = rms_cap,
        peak = peak_cap,
        "audio capture stopped"
    );

    attempt.pipeline_metrics.audio_duration_ms = duration_ms;

    if capture.samples.is_empty() {
        record_capture_signal_issue(&mut attempt, CaptureSignalIssue::AbsoluteSilence);
        let error = "no audio captured (0 samples) — hold longer or check mic".to_string();
        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Capture,
            &error,
            pipeline_started,
        );
        rec.status = SessionStatus::Failed;
        rec.asr_engine = Some(engine_kind.as_str().into());
        let discard = should_discard_short_silent_capture(&rec, &attempt);
        if !discard {
            write_attempt_debug(
                &mut rec,
                &attempt,
                AttemptDebug {
                    target: target.as_ref(),
                    frontmost_before_insert: None,
                    sample_rate_capture: sample_rate,
                    num_samples_capture: num_samples,
                    samples_asr: &[],
                    rms: 0.0,
                    peak: 0.0,
                    notes: vec!["empty capture".into()],
                },
            );
            schedule_late_context_archive(late_context_archive.take(), app);
        }
        if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
            tracing::warn!(error = %persist_error, "failed to persist capture failure");
        }
        clear_unconsumed_edit_target();
        return Err(error);
    }

    let preprocess_started = Instant::now();
    let samples_16k = prepare_for_asr(&capture.samples, capture.sample_rate);
    attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
    let (rms, peak) = session_debug::audio_stats(&samples_16k);
    if let Err(issue) = ensure_audible_capture(rms, peak) {
        let error = issue.message();
        tracing::error!(peak, rms, ?issue, "audio capture rejected before ASR");
        record_capture_signal_issue(&mut attempt, issue);
        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Capture,
            error,
            pipeline_started,
        );
        rec.status = SessionStatus::Failed;
        rec.asr_engine = Some(engine_kind.as_str().into());
        let discard = should_discard_short_silent_capture(&rec, &attempt);
        if !discard {
            write_attempt_debug(
                &mut rec,
                &attempt,
                AttemptDebug {
                    target: target.as_ref(),
                    frontmost_before_insert: None,
                    sample_rate_capture: sample_rate,
                    num_samples_capture: num_samples,
                    samples_asr: &samples_16k,
                    rms,
                    peak,
                    notes: vec!["near-silent capture rejected before ASR".into()],
                },
            );
            schedule_late_context_archive(late_context_archive.take(), app);
        }
        if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
            tracing::warn!(error = %persist_error, "failed to persist silent capture failure");
        }
        clear_unconsumed_edit_target();
        return Err(error.to_string());
    }

    // Clone samples for debug dump (after ASR we still have this).
    let samples_for_debug = samples_16k.clone();
    schedule_late_context_archive(late_context_archive.take(), app);

    let asr_started = Instant::now();
    let asr_run = match run_asr(state, engine_kind, &asr_cfg, samples_16k, &mut attempt).await {
        Ok(run) => run,
        Err(error) => {
            attempt.pipeline_metrics.asr_ms = elapsed_ms(asr_started);
            attempt.pipeline_metrics.set_asr_rtf();
            mark_attempt_failed(&mut attempt, PipelineStage::Asr, &error, pipeline_started);
            rec.status = SessionStatus::Failed;
            rec.asr_engine = Some(asr_engine_str.clone());
            write_attempt_debug(
                &mut rec,
                &attempt,
                AttemptDebug {
                    target: target.as_ref(),
                    frontmost_before_insert: None,
                    sample_rate_capture: sample_rate,
                    num_samples_capture: num_samples,
                    samples_asr: &samples_for_debug,
                    rms,
                    peak,
                    notes: vec!["ASR failed".into()],
                },
            );
            if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist ASR failure");
            }
            clear_unconsumed_edit_target();
            return Err(error);
        }
    };
    let asr_engine_label = asr_run.engine_label;
    attempt.pipeline_identity.asr_engine = asr_engine_label.clone();
    let (asr_text, enhanced_text) = apply_asr_result(&mut attempt, &asr_run.result, asr_started);
    tracing::info!(
        attempt_id = %attempt.id,
        asr_chars = asr_text.chars().count(),
        engine = %asr_engine_label,
        "ASR result"
    );

    tracing::info!(?intent, "running corrector with session intent");
    let correction = match run_corrector_stage(
        state,
        &cfg,
        &enhanced_text,
        intent.clone(),
        captured_context.as_ref(),
        &mut attempt,
    )
    .await
    {
        Ok(correction) => correction,
        Err(error) => {
            clear_unconsumed_edit_target();
            return Err(error);
        }
    };
    let corrected_text = correction.text;
    let corrector_engine = correction.engine;
    let fallback_reason = correction.fallback_reason;
    if !correction.model_applied && matches!(intent, IntentSpec::Translate { .. }) {
        tracing::warn!(
            %corrector_engine,
            "translate intent but model not applied — output stays ASR language"
        );
    }

    let mut notes: Vec<String> = Vec::new();
    if asr_text.is_empty() || asr_text == "." {
        notes.push("empty/dot ASR".into());
    }
    if matches!(intent, IntentSpec::Translate { .. }) && !correction.model_applied {
        notes.push(format!(
            "翻译未执行：模型未响应（{}）。请在「AI 修正」里确认 Ollama 模型名可用（当前机器常见 qwen3.5:9b）",
            corrector_engine
        ));
    }

    // Discovery started with recording. Join it before insertion so the
    // post-insert anchor itself is never delayed by provider probing.
    let mut pane_target = if cfg.inject.auto_insert
        && !corrected_text.is_empty()
        && cfg.learning.post_paste_capture
    {
        take_discovered_pane_target()
    } else {
        state
            .edit_learning
            .clear_pending("observed_insertion_not_requested");
        cancel_pane_target_discovery();
        None
    };

    let mut insert_strategy = InsertStrategy::None;
    let mut did_insert = false;
    let mut observation_started = false;
    let mut insertion_outcome = InsertionOutcome::NotRequested;
    let mut insert_notice: Option<String> = None;
    let mut frontmost_before_insert = None;
    let insert_started = Instant::now();
    if cfg.inject.auto_insert && !corrected_text.is_empty() {
        #[cfg(target_os = "windows")]
        let insert_available = true;
        #[cfg(target_os = "macos")]
        let insert_available = lumen_platform_macos::is_accessibility_trusted();
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let insert_available = false;
        if !insert_available {
            state
                .edit_learning
                .clear_pending("cross_app_insertion_unavailable");
            #[cfg(target_os = "macos")]
            tracing::error!(
                "Accessibility not granted; cannot inject into other apps. Open System Settings → Privacy & Security → Accessibility and enable this process"
            );
            #[cfg(not(target_os = "macos"))]
            tracing::info!("platform uses copy-only output; skipping cross-app injection");
            let clipboard_copied = match crate::inject_cmd::copy_only(&corrected_text).await {
                Ok(()) => {
                    insert_strategy = InsertStrategy::CopyOnly;
                    insertion_outcome = InsertionOutcome::Copied;
                    #[cfg(target_os = "macos")]
                    notes.push(
                        "accessibility denied — text copied to clipboard; enable Accessibility for insert"
                            .into(),
                    );
                    #[cfg(not(target_os = "macos"))]
                    notes.push("platform copy-only mode — text copied to clipboard".into());
                    tracing::info!("copied result to clipboard (no AX)");
                    true
                }
                Err(e) => {
                    insertion_outcome = InsertionOutcome::Failed;
                    notes.push(format!("clipboard copy failed: {e}"));
                    attempt
                        .pipeline_metrics
                        .stage_issues
                        .push(PipelineStageIssue {
                            stage: PipelineStage::Insert,
                            kind: PipelineIssueKind::ClipboardFailure,
                            message: e.to_string(),
                        });
                    false
                }
            };
            insert_notice = Some(copy_only_fallback_notice(clipboard_copied));
        } else {
            frontmost_before_insert = restore_target_app_before_insert(app);
            // Let focus settle after capsule hide; modifiers clear inside inject.
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;

            if let Some(cur) = frontmost_app_name() {
                if is_self_app_name(&cur) {
                    notes.push(format!("frontmost is self before insert: {cur}"));
                    tracing::warn!(%cur, "frontmost still Lumen — one restore attempt");
                    if let Some(ref t) = target {
                        let _ = activate_target(t);
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }

            let insertion = if cfg.learning.post_paste_capture {
                state
                    .edit_learning
                    .insert(
                        &cfg.inject,
                        rec.id,
                        attempt.id,
                        &corrected_text,
                        target.as_ref(),
                        pane_target.take(),
                    )
                    .await
                    .map(|outcome| {
                        observation_started = outcome.observation_started;
                        outcome.insertion
                    })
            } else {
                crate::inject_cmd::insert_with_config(&cfg.inject, &corrected_text).await
            };
            match insertion {
                Ok(out) => {
                    insert_strategy = out.strategy;
                    insertion_outcome = insertion_outcome_for_strategy(insert_strategy);
                    did_insert = insertion_outcome == InsertionOutcome::Inserted;
                    if insertion_outcome == InsertionOutcome::Copied {
                        insert_notice = Some(copied_toast_notice());
                    }
                    tracing::info!(
                        ?insert_strategy,
                        observation_started,
                        frontmost = ?frontmost_app_name(),
                        "auto-insert done"
                    );
                }
                Err(e) => {
                    notes.push(format!("insert error: {e}"));
                    attempt
                        .pipeline_metrics
                        .stage_issues
                        .push(PipelineStageIssue {
                            stage: PipelineStage::Insert,
                            kind: PipelineIssueKind::InjectionFailure,
                            message: e.to_string(),
                        });
                    tracing::warn!(error = %e, "auto-insert failed");
                    match crate::inject_cmd::copy_only(&corrected_text).await {
                        Ok(()) => {
                            insert_strategy = InsertStrategy::CopyOnly;
                            insertion_outcome = InsertionOutcome::Copied;
                            insert_notice = Some(insert_failure_notice(&e, true));
                            tracing::info!("copied result to clipboard after insert failure");
                        }
                        Err(copy_err) => {
                            insertion_outcome = InsertionOutcome::Failed;
                            notes.push(format!("clipboard copy failed: {copy_err}"));
                            attempt
                                .pipeline_metrics
                                .stage_issues
                                .push(PipelineStageIssue {
                                    stage: PipelineStage::Insert,
                                    kind: PipelineIssueKind::ClipboardFailure,
                                    message: copy_err,
                                });
                            insert_notice = Some(insert_failure_notice(&e, false));
                        }
                    }
                }
            }
        }
    }
    attempt.pipeline_metrics.insert_ms = elapsed_ms(insert_started);
    attempt
        .pipeline_metrics
        .set_insertion_outcome(insertion_outcome);
    let watch_post_paste = did_insert && observation_started;
    attempt.inserted = did_insert.then(|| corrected_text.clone());
    attempt.status = AttemptStatus::Completed;
    attempt.pipeline_metrics.total_ms = elapsed_ms(pipeline_started);

    rec.status = SessionStatus::Completed;
    rec.insert_strategy = insert_strategy;
    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(asr_engine_label.clone());
    rec.corrector_engine = Some(corrector_engine.clone());
    write_attempt_debug(
        &mut rec,
        &attempt,
        AttemptDebug {
            target: target.as_ref(),
            frontmost_before_insert,
            sample_rate_capture: sample_rate,
            num_samples_capture: num_samples,
            samples_asr: &samples_for_debug,
            rms,
            peak,
            notes,
        },
    );
    persist_attempt(state, save, &rec, attempt)?;

    Ok(TranscribeOutcome {
        text: corrected_text.clone(),
        asr_text,
        corrected_text,
        model_applied: correction.model_applied,
        fallback_reason,
        asr_engine: asr_engine_label,
        corrector_engine,
        sample_rate,
        num_samples,
        duration_ms,
        session: rec,
        watch_post_paste,
        post_paste_seconds: cfg.learning.post_paste_seconds,
        insert_notice,
    })
}

/// Load session WAV as raw bytes for frontend playback (Blob URL).
#[tauri::command]
pub fn get_session_audio(state: State<'_, AppState>, id: String) -> Result<Vec<u8>, String> {
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let rec = {
        let guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        let store = guard
            .as_ref()
            .ok_or_else(|| "database not available".to_string())?;
        store
            .get_session(uuid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "session not found".to_string())?
    };
    let path = rec
        .audio_path
        .as_ref()
        .ok_or_else(|| "此会话没有保存音频".to_string())?;
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return Err(format!("音频文件不存在: {path}"));
    }
    std::fs::read(p).map_err(|e| format!("read audio: {e}"))
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RetryOutcome {
    pub session: SessionRecord,
    pub asr_text: String,
    pub corrected_text: String,
    pub asr_engine: String,
    pub corrector_engine: String,
    pub model_applied: bool,
    pub fallback_reason: Option<String>,
}

/// Re-run ASR + corrector from saved session audio (no re-record, no auto-insert).
#[tauri::command]
pub async fn retry_session_transcription(
    state: State<'_, AppState>,
    id: String,
) -> Result<RetryOutcome, String> {
    let pipeline_started = Instant::now();
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let mut rec = {
        let guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        let store = guard
            .as_ref()
            .ok_or_else(|| "database not available".to_string())?;
        store
            .get_session(uuid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "session not found".to_string())?
    };
    let engine_kind = *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();
    let asr_engine_str = if cfg.asr.provider.starts_with("local") {
        engine_kind.as_str().to_string()
    } else {
        cfg.asr.provider.clone()
    };
    let mut attempt = DictationAttemptRecord::new(rec.id);
    attempt.pipeline_identity = build_pipeline_identity(
        &state,
        &cfg,
        engine_kind,
        &asr_engine_str,
        "not_run",
        IntentSpec::Default,
    );
    let prior_attempts = state
        .store
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .and_then(|store| store.list_dictation_attempts(rec.id, 100, None).ok())
        })
        .unwrap_or_default();
    let mut retry_context_projection = None;
    for prior in &prior_attempts {
        let Some(context_ref) = prior.pipeline_inputs.context.as_ref() else {
            continue;
        };
        let Some(usage) = prior.pipeline_inputs.stage_usages.iter().find(|usage| {
            usage.stage == PipelineStage::Corrector
                && usage.sent
                && usage.projection_path.is_some()
                && !usage
                    .sources
                    .iter()
                    .any(|source| source == "personal_dictionary")
        }) else {
            continue;
        };
        match state
            .context
            .load_stage_projection(Some(context_ref.capture_id), prior.id, usage)
        {
            Ok(projection) => {
                match serde_json::from_slice::<crate::context_capture::CorrectorContextProjection>(
                    &projection,
                ) {
                    Ok(projection) => {
                        attempt.pipeline_inputs.context = Some(context_ref.clone());
                        retry_context_projection = Some(projection);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            prior_attempt_id = %prior.id,
                            error = %error,
                            "historical context projection could not be decoded"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    prior_attempt_id = %prior.id,
                    %error,
                    "historical context projection could not be opened"
                );
            }
        }
    }
    if attempt.pipeline_inputs.context.is_none() {
        attempt.pipeline_inputs.context = prior_attempts
            .iter()
            .find_map(|prior| prior.pipeline_inputs.context.clone());
    }
    attempt
        .pipeline_inputs
        .stage_usages
        .push(ContextStageUsage {
            stage: PipelineStage::Asr,
            sources: vec!["captured_context".into()],
            captured: attempt
                .pipeline_inputs
                .context
                .as_ref()
                .is_some_and(|input| input.source_presence_bitmap != 0),
            not_used_reason: Some("captured_context_not_projected_to_asr".into()),
            ..ContextStageUsage::default()
        });

    let preprocess_started = Instant::now();
    let path = match rec.audio_path.clone() {
        Some(path) => path,
        None => {
            let error = "此会话没有音频，无法重新转写".to_string();
            attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
            mark_attempt_failed(
                &mut attempt,
                PipelineStage::Preprocess,
                &error,
                pipeline_started,
            );
            if let Err(persist_error) = persist_attempt(&state, true, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist retry failure");
            }
            return Err(error);
        }
    };
    let (samples, sample_rate) = match session_debug::read_wav_mono_f32(std::path::Path::new(&path))
    {
        Ok(audio) => audio,
        Err(error) => {
            attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
            mark_attempt_failed(
                &mut attempt,
                PipelineStage::Preprocess,
                &error,
                pipeline_started,
            );
            if let Err(persist_error) = persist_attempt(&state, true, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist retry failure");
            }
            return Err(error);
        }
    };
    if samples.is_empty() {
        let error = "音频为空".to_string();
        attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Preprocess,
            &error,
            pipeline_started,
        );
        if let Err(persist_error) = persist_attempt(&state, true, &rec, attempt) {
            tracing::warn!(error = %persist_error, "failed to persist retry failure");
        }
        return Err(error);
    }

    let samples_16k = if sample_rate == 16_000 {
        samples
    } else {
        lumen_asr::resample_linear(&samples, sample_rate, 16_000)
    };
    attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
    attempt.pipeline_metrics.audio_duration_ms = (samples_16k.len() as u64 * 1_000) / 16_000;

    tracing::info!(
        %id,
        engine = engine_kind.as_str(),
        samples = samples_16k.len(),
        "retry transcription start"
    );

    let asr_started = Instant::now();
    let asr_run = match run_asr(&state, engine_kind, &cfg.asr, samples_16k, &mut attempt).await {
        Ok(run) => run,
        Err(error) => {
            attempt.pipeline_metrics.asr_ms = elapsed_ms(asr_started);
            attempt.pipeline_metrics.set_asr_rtf();
            mark_attempt_failed(&mut attempt, PipelineStage::Asr, &error, pipeline_started);
            if let Err(persist_error) = persist_attempt(&state, true, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist retry ASR failure");
            }
            return Err(error);
        }
    };
    let asr_engine_label = asr_run.engine_label;
    attempt.pipeline_identity.asr_engine = asr_engine_label.clone();
    let (asr_text, enhanced_text) = apply_asr_result(&mut attempt, &asr_run.result, asr_started);
    let correction = run_corrector_stage(
        &state,
        &cfg,
        &enhanced_text,
        IntentSpec::Default,
        retry_context_projection.as_ref(),
        &mut attempt,
    )
    .await?;
    let corrected_text = correction.text;
    let corrector_engine = correction.engine;
    let fallback_reason = correction.fallback_reason;
    attempt.status = AttemptStatus::Completed;
    attempt.pipeline_metrics.total_ms = elapsed_ms(pipeline_started);

    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(asr_engine_label.clone());
    rec.corrector_engine = Some(corrector_engine.clone());
    rec.status = SessionStatus::Completed;

    // The original debug text files remain immutable. The retry result is a
    // new attempt row rather than overwriting the first attempt's sidecars.
    persist_attempt(&state, true, &rec, attempt)?;

    tracing::info!(
        %id,
        asr_chars = asr_text.chars().count(),
        corrected_chars = corrected_text.chars().count(),
        "retry transcription done"
    );
    Ok(RetryOutcome {
        session: rec,
        asr_text,
        corrected_text,
        asr_engine: asr_engine_label,
        corrector_engine,
        model_applied: correction.model_applied,
        fallback_reason,
    })
}

#[tauri::command]
pub fn cancel_recording(state: State<'_, AppState>) -> Result<(), String> {
    cancel_recording_inner(&state)
}

pub fn cancel_recording_inner(state: &AppState) -> Result<(), String> {
    state.context.clear_active();
    state
        .edit_learning
        .clear_pending("dictation_cancelled_before_insertion");
    cancel_pane_target_discovery();
    if state.audio.is_recording() {
        let _ = state.audio.stop();
    }
    // Cancelled dictation still owns the arbiter — release it back to Idle.
    state.capture.end_dictation();
    Ok(())
}

/// Build the offline meeting-transcription ASR engine.
///
/// Meeting transcription is **diarization-first**: the audio is segmented into
/// speaker turns and each turn is transcribed independently, so speaker
/// attribution comes from the diarizer and does not depend on Paraformer's
/// word-level timestamps. Given that, SenseVoice is preferred for the final
/// transcript — it produces punctuation and handles multiple languages, so its
/// quality is higher, and it is the same model dictation already provisions
/// (shared via the cluster resolver). Post-ASR dictionary correction still runs
/// afterward. This is the meeting path only; dictation keeps its own configured
/// provider (SenseVoice/Whisper/Qwen/cloud).
///
/// Resolution:
/// 1. Use the already-provisioned SenseVoice engine (`state.sensevoice`), whose
///    model dir was resolved at startup via the shared cluster resolver (shared
///    root + legacy dirs incl. Shandianshuo). Preferred when ready.
/// 2. Legacy fallback: if SenseVoice is somehow unprovisioned but an offline
///    Paraformer model exists under `<lumen_models_dir>/paraformer/offline/`,
///    transcribe with [`ParaformerAsr`] rather than fail.
/// 3. Otherwise return an error — no usable meeting ASR model is installed.
///
/// The returned engine is `Send + Sync` (trait requirement), so it can be moved
/// into the background meeting-processing task.
pub(crate) fn build_meeting_asr_engine(state: &AppState) -> Result<Box<dyn AsrEngine>, String> {
    // Preferred: the shared SenseVoice engine (punctuation + multilingual),
    // resolved at startup from the shared cluster root / legacy dirs.
    let sensevoice = state
        .sensevoice
        .lock()
        .map_err(|_| "asr lock poisoned".to_string())?
        .clone();
    if sensevoice_ready(&sensevoice.model_dir()) {
        tracing::info!(
            dir = %sensevoice.model_dir().display(),
            "meeting ASR engine: SenseVoice (punctuation + multilingual, shared with dictation)"
        );
        return Ok(Box::new(sensevoice));
    }

    // Legacy fallback: SenseVoice not provisioned, but an offline Paraformer model
    // is installed under the shared convention `<root>/paraformer/offline/`. Kept
    // so meetings still transcribe rather than fail; no longer the preferred path.
    let paraformer_dir = lumen_models_dir().join("paraformer").join("offline");
    if paraformer_offline_ready(&paraformer_dir) {
        tracing::warn!(
            dir = %paraformer_dir.display(),
            "SenseVoice model not provisioned; meeting falls back to offline Paraformer"
        );
        return Ok(Box::new(ParaformerAsr::new(paraformer_dir)));
    }

    Err(
        "meeting transcription needs a provisioned SenseVoice model, but none was found \
         (install SenseVoice or select an existing model dir)"
            .to_string(),
    )
}

struct AsrRun {
    result: AsrResult,
    engine_label: String,
}

async fn run_asr(
    state: &AppState,
    engine_kind: EngineKind,
    asr_cfg: &AsrServiceConfig,
    samples_16k: Vec<f32>,
    attempt: &mut DictationAttemptRecord,
) -> Result<AsrRun, String> {
    let provider = canonical_asr_provider(&asr_cfg.provider);
    let provider = provider.as_str();

    if matches!(
        provider,
        "aliyun_qwen" | "volcengine" | "soniox" | "stepfun" | "mimo"
    ) {
        return Err(format!(
            "ASR「{provider}」仅预置了 endpoint，完整流式客户端尚未接入。请改用本地 SenseVoice 或 OpenAI Audio。"
        ));
    }

    if matches!(provider, "openai_audio" | "custom") {
        return run_cloud_asr_with_local_hedge(state, provider, asr_cfg, samples_16k, attempt)
            .await;
    }

    let selected_local_engine = engine_kind_for_provider(provider).unwrap_or(engine_kind);
    let result = run_local_asr(state, selected_local_engine, samples_16k).await?;
    Ok(AsrRun {
        result,
        engine_label: selected_local_engine.as_str().into(),
    })
}

async fn run_cloud_asr_with_local_hedge(
    state: &AppState,
    provider: &str,
    asr_cfg: &AsrServiceConfig,
    samples_16k: Vec<f32>,
    attempt: &mut DictationAttemptRecord,
) -> Result<AsrRun, String> {
    let local_kind = local_hedge_engine_kind(state);
    let configured = Duration::from_secs(asr_cfg.timeout_secs.max(30));
    let deadline = cloud_asr_hedge_deadline(configured, local_kind.is_some());
    let cloud = transcribe_openai_audio(asr_cfg, samples_16k.clone());
    let cloud_outcome = match tokio::time::timeout(deadline, cloud).await {
        Ok(outcome) => outcome,
        Err(_) => Err("timeout".into()),
    };
    match cloud_outcome {
        Ok(result) => Ok(AsrRun {
            result,
            engine_label: provider.to_owned(),
        }),
        Err(error) => {
            let Some(local_kind) = local_kind.filter(|_| cloud_asr_error_is_hedgeable(&error))
            else {
                return Err(if error == "timeout" {
                    "在线 ASR 超时".into()
                } else {
                    error
                });
            };
            tracing::warn!(
                %provider,
                hedge = %local_kind.as_str(),
                error = %error,
                "cloud ASR timed out or dropped; falling back to local engine"
            );
            match run_local_asr(state, local_kind, samples_16k).await {
                Ok(result) => {
                    attempt
                        .pipeline_metrics
                        .stage_issues
                        .push(PipelineStageIssue {
                            stage: PipelineStage::Asr,
                            kind: PipelineIssueKind::Fallback,
                            message: format!("{provider} timeout; used {}", local_kind.as_str()),
                        });
                    Ok(AsrRun {
                        result,
                        engine_label: asr_engine_hedge_label(provider, local_kind),
                    })
                }
                Err(local_error) => Err(format!(
                    "在线 ASR 超时，本地 {} 也失败：{local_error}",
                    local_kind.as_str()
                )),
            }
        }
    }
}

fn local_hedge_engine_kind(state: &AppState) -> Option<EngineKind> {
    if let Ok(engine) = state.sensevoice.lock() {
        if sensevoice_ready(&engine.model_dir()) {
            return Some(EngineKind::SenseVoice);
        }
    }
    if let Ok(engine) = state.whisper.lock() {
        if whisper_ready(&engine.model_dir()) {
            return Some(EngineKind::Whisper);
        }
    }
    None
}

async fn transcribe_openai_audio(
    asr_cfg: &AsrServiceConfig,
    samples_16k: Vec<f32>,
) -> Result<AsrResult, String> {
    let base = if asr_cfg.base_url.trim().is_empty() {
        "https://api.openai.com/v1".into()
    } else {
        asr_cfg.base_url.clone()
    };
    let model = if asr_cfg.model.trim().is_empty() {
        "whisper-1".into()
    } else {
        asr_cfg.model.clone()
    };
    let eng = OpenAiAudioAsr::new(OpenAiAudioConfig {
        base_url: base,
        api_key: asr_cfg.api_key.clone(),
        model,
        timeout: Duration::from_secs(asr_cfg.timeout_secs.max(30)),
        language: if asr_cfg.language.trim().is_empty() {
            None
        } else {
            Some(asr_cfg.language.clone())
        },
        // Keep the shared engine's defaults for the new knobs
        // (8 MiB request cap, "openai_audio" transcript label).
        ..OpenAiAudioConfig::default()
    })
    .map_err(|e| e.to_string())?;
    eng.transcribe(AsrRequest::new(samples_16k, 16_000))
        .await
        .map_err(|e| e.to_string())
}

async fn run_local_asr(
    state: &AppState,
    engine_kind: EngineKind,
    samples_16k: Vec<f32>,
) -> Result<AsrResult, String> {
    if engine_kind == EngineKind::Whisper {
        let eng = state
            .whisper
            .lock()
            .map_err(|_| "asr lock poisoned".to_string())?
            .clone();
        return eng
            .transcribe(AsrRequest::new(samples_16k, 16_000))
            .await
            .map_err(|e| e.to_string());
    }

    if engine_kind == EngineKind::Qwen {
        let eng = state
            .qwen
            .lock()
            .map_err(|_| "asr lock poisoned".to_string())?
            .clone();
        // sherpa-onnx Qwen3-ASR auto-detects the language and has no per-request
        // hotword/shadow channel — a plain transcribe is the whole contract.
        return eng
            .transcribe(AsrRequest::new(samples_16k, 16_000))
            .await
            .map_err(|e| e.to_string());
    }

    let eng = state
        .sensevoice
        .lock()
        .map_err(|_| "asr lock poisoned".to_string())?
        .clone();
    eng.transcribe(AsrRequest::new(samples_16k, 16_000))
        .await
        .map_err(|e| e.to_string())
}

/// Capsule / hotkey lifecycle events for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "phase")]
pub enum DictationUiEvent {
    Idle,
    Listening {
        message: String,
        /// default | translate | raw — for capsule styling
        intent: String,
        /// e.g. "en" when translating
        target_language: Option<String>,
    },
    Processing {
        message: String,
        intent: String,
        target_language: Option<String>,
    },
    Done {
        outcome: TranscribeOutcome,
    },
    Error {
        message: String,
    },
    Notice {
        message: String,
    },
    Cancelled,
}

fn intent_ui_label(intent: &IntentSpec) -> (String, Option<String>, String) {
    match intent {
        IntentSpec::Translate {
            target_language, ..
        } => (
            "translate".into(),
            Some(target_language.clone()),
            format!("翻译→{target_language}"),
        ),
        IntentSpec::Raw => ("raw".into(), None, "仅原文".into()),
        // Never call normal path “录音” during processing — user confuses with translate.
        IntentSpec::Default | IntentSpec::PolishOverride => ("default".into(), None, "整理".into()),
    }
}

pub fn emit_dictation(app: &AppHandle, event: DictationUiEvent) {
    let _ = app.emit("dictation", &event);
}

/// Shows background edit-learning feedback in the capsule only while the
/// dictation UI is idle. Active recording/processing feedback always wins.
pub(crate) fn show_transient_background_notice(
    app: &AppHandle,
    message: String,
    is_error: bool,
) -> bool {
    let notice_epoch = {
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if PHASE.load(Ordering::SeqCst) != PHASE_IDLE {
            tracing::debug!(
                is_error,
                "edit-learning capsule notice deferred because dictation UI is active"
            );
            return false;
        }
        let notice_epoch = UI_NOTICE_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        crate::capsule::set_capsule_visible(app, true, "edit-learning");
        let event = if is_error {
            DictationUiEvent::Error { message }
        } else {
            DictationUiEvent::Notice { message }
        };
        emit_dictation(app, event);
        notice_epoch
    };

    let app_for_notice = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if UI_NOTICE_EPOCH.load(Ordering::SeqCst) == notice_epoch
            && PHASE.load(Ordering::SeqCst) == PHASE_IDLE
        {
            emit_dictation(&app_for_notice, DictationUiEvent::Idle);
            crate::capsule::set_capsule_visible(&app_for_notice, false, "idle");
        }
    });
    true
}

fn finish_with_transient_error(app: &AppHandle, message: String) {
    let notice_epoch = {
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
        let notice_epoch = UI_NOTICE_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        crate::capsule::set_capsule_visible(app, true, "error");
        emit_dictation(app, DictationUiEvent::Error { message });
        notice_epoch
    };

    let app_for_notice = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if UI_NOTICE_EPOCH.load(Ordering::SeqCst) == notice_epoch
            && PHASE.load(Ordering::SeqCst) == PHASE_IDLE
        {
            emit_dictation(&app_for_notice, DictationUiEvent::Idle);
            crate::capsule::set_capsule_visible(&app_for_notice, false, "idle");
        }
    });
}

fn finish_with_transient_fallback(app: &AppHandle, outcome: TranscribeOutcome) {
    let hold = capsule_notice_hold(&outcome);
    let notice_epoch = {
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
        let notice_epoch = UI_NOTICE_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        crate::capsule::set_capsule_visible(app, true, "fallback");
        emit_dictation(app, DictationUiEvent::Done { outcome });
        notice_epoch
    };

    let app_for_notice = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(hold).await;
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if UI_NOTICE_EPOCH.load(Ordering::SeqCst) == notice_epoch
            && PHASE.load(Ordering::SeqCst) == PHASE_IDLE
        {
            emit_dictation(&app_for_notice, DictationUiEvent::Idle);
            crate::capsule::set_capsule_visible(&app_for_notice, false, "idle");
        }
    });
}

/// Start recording if idle (push-to-talk press / toggle start).
/// When `[vad] enabled`, end the dictation after a sustained silent stretch.
/// Uses the same stop path as a hotkey release — `dictation_stop` is
/// phase-guarded, so a racing manual stop wins cleanly.
///
/// Two backends, same "stop only after a sustained silent stretch" policy:
/// silero (`AudioCapture` feeds the VAD from the capture callback; this
/// watcher reads its last-speech timestamp) or rms (polls `latest_rms` into
/// `SilenceAutoStop`). silero configured but not active (model missing /
/// failed to load) falls back to rms with a warning.
fn spawn_vad_autostop(app: &AppHandle) {
    let state = app.state::<AppState>();
    let vad = state.config.lock().ok().map(|config| config.vad.clone());
    let Some(vad) = vad else { return };
    if !vad.enabled {
        return;
    }
    let timeout = Duration::from_millis(vad.silence_timeout_ms.clamp(300, 30_000));
    let silero_active = state.audio.silero_vad_active();
    if vad.mode == "silero" && silero_active {
        let watcher = lumen_asr::TimestampAutoStop::new(timeout);
        let app = app.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let state = app.state::<AppState>();
                let Some(elapsed_ms) = state.audio.silero_elapsed_ms() else {
                    break; // recording already ended (hotkey release, error, …)
                };
                let last_speech = state.audio.silero_last_speech_at_ms();
                if watcher.update(last_speech, elapsed_ms) == lumen_asr::VadAction::AutoStop {
                    tracing::info!("silero vad silence timeout — auto-stopping dictation");
                    if let Err(error) = dictation_stop(app.clone()).await {
                        tracing::warn!(error = %error, "vad auto-stop failed");
                    }
                    break;
                }
            }
        });
        return;
    }
    if vad.mode == "silero" {
        tracing::warn!("vad.mode=silero but the silero backend is not active, falling back to rms");
    } else if vad.mode != "rms" {
        tracing::warn!(mode = %vad.mode, "vad mode not implemented, falling back to rms");
    }
    let mut watcher =
        lumen_asr::SilenceAutoStop::new(vad.start_threshold, vad.end_threshold, timeout);
    let app = app.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            let state = app.state::<AppState>();
            let Some(rms) = state.audio.latest_rms() else {
                break; // recording already ended (hotkey release, error, …)
            };
            if watcher.update(rms, Instant::now()) == lumen_asr::VadAction::AutoStop {
                tracing::info!("vad silence timeout — auto-stopping dictation");
                if let Err(error) = dictation_stop(app.clone()).await {
                    tracing::warn!(error = %error, "vad auto-stop failed");
                }
                break;
            }
        }
    });
}

pub async fn dictation_start(app: AppHandle) -> Result<(), String> {
    dictation_start_with_intent(app, IntentSpec::Default).await
}

pub async fn dictation_start_with_intent(app: AppHandle, intent: IntentSpec) -> Result<(), String> {
    // Only one session at a time.
    let phase_transition = {
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PHASE.compare_exchange(
            PHASE_IDLE,
            PHASE_RECORDING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
    };
    if phase_transition.is_err() {
        tracing::info!(
            phase = PHASE.load(Ordering::SeqCst),
            "dictation_start ignored (not idle)"
        );
        return Ok(());
    }
    UI_NOTICE_EPOCH.fetch_add(1, Ordering::SeqCst);

    set_session_intent(intent.clone());
    let (intent_kind, target_lang, intent_label) = intent_ui_label(&intent);

    // Stamp immediately so a racing stop does not see held_ms=0.
    if let Ok(mut g) = RECORD_STARTED.lock() {
        *g = Some(Instant::now());
    }

    let state = app.state::<AppState>();
    if state.audio.is_recording() {
        // Already capturing — stay in RECORDING.
        return Ok(());
    }

    // Always show capsule while recording — primary UX feedback for hotkey users.
    let show_capsule = state
        .config
        .lock()
        .map(|c| c.hotkey.show_capsule)
        .unwrap_or(true);

    match start_recording_inner(&state) {
        Ok(()) => {
            // Serialize the final state check and UI publication with stop's
            // RECORDING -> PROCESSING transition. A quick release can finish
            // while pane discovery is still returning from start_recording_inner.
            let _transition = UI_TRANSITION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if PHASE.load(Ordering::SeqCst) != PHASE_RECORDING || !state.audio.is_recording() {
                tracing::debug!("recording ended before start feedback was published");
                return Ok(());
            }
            tracing::info!(%intent_kind, ?target_lang, "dictation recording live");
            // Force-show capsule on hotkey start so user always sees feedback.
            crate::capsule::set_capsule_visible(&app, true, "listening");
            if !show_capsule {
                tracing::debug!(
                    "config show_capsule=false but forcing visible for hotkey feedback"
                );
            }
            emit_dictation(
                &app,
                DictationUiEvent::Listening {
                    message: format!("按住·{intent_label}"),
                    intent: intent_kind,
                    target_language: target_lang,
                },
            );
            spawn_vad_autostop(&app);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "start_recording failed");
            if let Ok(mut g) = RECORD_STARTED.lock() {
                *g = None;
            }
            finish_with_transient_error(&app, e.clone());
            Err(e)
        }
    }
}

/// Stop recording + ASR + correct + paste into target (push-to-talk release / toggle stop).
pub async fn dictation_stop(app: AppHandle) -> Result<(), String> {
    // Only stop if we are actively recording.
    let phase_transition = {
        let _transition = UI_TRANSITION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PHASE.compare_exchange(
            PHASE_RECORDING,
            PHASE_PROCESSING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
    };
    if phase_transition.is_err() {
        tracing::info!(
            phase = PHASE.load(Ordering::SeqCst),
            "dictation_stop ignored (not recording)"
        );
        return Ok(());
    }

    let held_ms = RECORD_STARTED
        .lock()
        .ok()
        .and_then(|g| g.map(|t| t.elapsed().as_millis()))
        .unwrap_or(0);

    let state = app.state::<AppState>();

    // True bounce only: very short + nothing useful yet.
    if held_ms < BOUNCE_MS && !state.audio.is_recording() {
        tracing::info!(held_ms, "bounce stop — nothing to process");
        if let Ok(mut g) = RECORD_STARTED.lock() {
            *g = None;
        }
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
        emit_dictation(&app, DictationUiEvent::Idle);
        return Ok(());
    }

    if !state.audio.is_recording() {
        tracing::warn!(held_ms, "stop but audio not recording — reset idle");
        if let Ok(mut g) = RECORD_STARTED.lock() {
            *g = None;
        }
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
        return Ok(());
    }

    // UI intent survives take_session_intent inside stop_and_transcribe_inner.
    let intent_peek = peek_ui_intent();
    let (intent_kind, target_lang, intent_label) = intent_ui_label(&intent_peek);
    tracing::info!(held_ms, %intent_kind, "dictation stop → ASR");
    let processing_msg = if intent_kind == "translate" {
        format!("正在翻译 → {}…", target_lang.as_deref().unwrap_or("en"))
    } else if intent_kind == "raw" {
        "转写中（不整理）…".into()
    } else {
        "转写与整理中…".into()
    };
    let _ = intent_label;
    emit_dictation(
        &app,
        DictationUiEvent::Processing {
            message: processing_msg,
            intent: intent_kind,
            target_language: target_lang,
        },
    );
    // Keep capsule visible during processing so user sees work in progress.
    crate::capsule::set_capsule_visible(&app, true, "processing");

    let result = stop_and_transcribe_inner(&state, true, Some(&app)).await;
    if let Ok(mut g) = RECORD_STARTED.lock() {
        *g = None;
    }

    clear_ui_intent();
    match result {
        Ok(outcome) => {
            if outcome.fallback_reason.is_some() || outcome.insert_notice.is_some() {
                finish_with_transient_fallback(&app, outcome);
            } else {
                crate::capsule::set_capsule_visible(&app, false, "idle");
                emit_dictation(&app, DictationUiEvent::Done { outcome });
                emit_dictation(&app, DictationUiEvent::Idle);
                PHASE.store(PHASE_IDLE, Ordering::SeqCst);
            }
            Ok(())
        }
        Err(e) => {
            finish_with_transient_error(&app, e.clone());
            Err(e)
        }
    }
}

/// Legacy toggle: start if idle, stop if recording (UI button / toggle mode).
pub async fn toggle_dictation(app: AppHandle) -> Result<(), String> {
    match PHASE.load(Ordering::SeqCst) {
        PHASE_RECORDING => dictation_stop(app).await,
        PHASE_IDLE => dictation_start(app).await,
        _ => {
            tracing::debug!("toggle ignored (processing)");
            Ok(())
        }
    }
}

#[tauri::command]
pub async fn toggle_dictation_cmd(app: AppHandle) -> Result<(), String> {
    toggle_dictation(app).await
}

#[cfg(test)]
mod attempt_metric_tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::AppState;
    use lumen_asr::{AsrEngineId, AudioCapture, SenseVoiceSherpaAsr, WhisperAsr};
    use lumen_store::{Store, MAX_ATTEMPT_PAGE_SIZE};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[test]
    fn background_notice_has_a_capsule_safe_serialization_contract() {
        let value = serde_json::to_value(DictationUiEvent::Notice {
            message: "已记录修改".into(),
        })
        .unwrap();

        assert_eq!(value["phase"], "notice");
        assert_eq!(value["message"], "已记录修改");
    }

    /// Minimal `AppState` for meeting-engine selection tests, pointing SenseVoice
    /// at `sensevoice_dir`. All other engines/dirs are placeholders under `dir`.
    fn test_state_with_sensevoice(dir: &Path, sensevoice_dir: std::path::PathBuf) -> AppState {
        let config = AppConfig::default();
        let context = crate::context_capture::ContextRecorder::new(&config.context, dir);
        let qwen = crate::qwen_engine_from_config(&config.asr);
        let store = Arc::new(Mutex::new(Some(
            Store::open(dir.join("capture.sqlite")).unwrap(),
        )));
        AppState {
            edit_learning: crate::edit_learning_runtime::DesktopEditLearning::new(
                store.clone(),
                false,
            ),
            store,
            audio: AudioCapture::new(),
            meeting_recorder: lumen_asr::MeetingRecorder::new(),
            meeting_power_guard: std::sync::Mutex::new(None),
            meeting_battery_poll: std::sync::Mutex::new(None),
            meeting_watchdog: std::sync::Mutex::new(None),
            meeting_max_duration_watchdog: std::sync::Mutex::new(None),
            meeting_recording_owner: std::sync::Mutex::new(
                crate::meeting_cmd::MeetingRecordingOwner::default(),
            ),
            meeting_audio_edit: tokio::sync::Mutex::new(()),
            meeting_recovery_notices: std::sync::Mutex::new(Vec::new()),
            meeting_live: crate::meeting_live::MeetingLive::default(),
            meeting_system_audio: crate::meeting_system_audio::MeetingSystemAudio::default(),
            meeting_mic_aec: crate::meeting_mic_aec::MeetingMicAec::default(),
            capture: crate::mode_arbiter::CaptureArbiter::new(),
            meeting_detection: crate::meeting_detection::MeetingDetectionService::new(),
            engine: Mutex::new(EngineKind::SenseVoice),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(sensevoice_dir)),
            qwen: Mutex::new(qwen),
            whisper: Mutex::new(WhisperAsr::new(dir.join("whisper"))),
            config: Mutex::new(config),
            context,
        }
    }

    #[test]
    fn meeting_engine_prefers_sensevoice_when_provisioned() {
        let dir = tempfile::tempdir().unwrap();
        // Make the SenseVoice dir "ready": model + tokens files present.
        let sensevoice_dir = dir.path().join("sensevoice");
        std::fs::create_dir_all(&sensevoice_dir).unwrap();
        std::fs::write(sensevoice_dir.join("model.int8.onnx"), b"model").unwrap();
        std::fs::write(sensevoice_dir.join("tokens.txt"), b"tokens").unwrap();

        let state = test_state_with_sensevoice(dir.path(), sensevoice_dir);
        let engine = build_meeting_asr_engine(&state).unwrap();
        // Meeting final transcript now uses SenseVoice (punctuation + multilingual),
        // not Paraformer offline. SenseVoice being ready short-circuits before any
        // real shared-root Paraformer lookup, so this stays environment-isolated.
        assert_eq!(engine.id(), AsrEngineId::SenseVoiceSherpa);
    }

    #[test]
    fn insertion_strategies_map_to_distinct_metric_outcomes() {
        for strategy in [
            InsertStrategy::Paste,
            InsertStrategy::Ax,
            InsertStrategy::Type,
        ] {
            assert_eq!(
                insertion_outcome_for_strategy(strategy),
                InsertionOutcome::Inserted
            );
        }
        assert_eq!(
            insertion_outcome_for_strategy(InsertStrategy::CopyOnly),
            InsertionOutcome::Copied
        );
        assert_eq!(
            insertion_outcome_for_strategy(InsertStrategy::None),
            InsertionOutcome::Failed
        );
    }

    #[test]
    fn copy_only_feedback_matches_platform_contract() {
        let copied = copy_only_fallback_notice(true);
        assert!(copied.starts_with("已复制"));
        #[cfg(target_os = "windows")]
        {
            assert!(!copied.contains("辅助功能"));
            let failure = copy_only_fallback_notice(false);
            assert!(failure.contains("剪贴板"));
            assert!(!failure.contains("辅助功能"));
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert!(copied.contains("辅助功能"));
        }
    }

    #[test]
    fn insert_failure_notice_separates_elevated_targets_from_generic_failures() {
        let elevated = insert_failure_notice(
            "Windows blocked simulated keyboard input; elevated apps cannot receive input from a non-elevated Lumen process",
            true,
        );
        assert!(elevated.starts_with("已复制"));
        assert!(elevated.contains("管理员"));
        let generic = insert_failure_notice("paste failed", true);
        assert_eq!(generic, "已复制");
        let both_failed = insert_failure_notice("paste failed", false);
        assert!(both_failed.contains("历史记录"));
    }

    #[test]
    fn cloud_asr_hedge_prefers_a_short_deadline_when_local_is_ready() {
        let configured = Duration::from_secs(120);
        assert_eq!(
            cloud_asr_hedge_deadline(configured, true),
            Duration::from_secs(8)
        );
        assert_eq!(cloud_asr_hedge_deadline(configured, false), configured);
        assert_eq!(
            cloud_asr_hedge_deadline(Duration::from_secs(5), true),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn cloud_asr_timeouts_are_hedgeable_but_auth_rejects_are_not() {
        assert!(cloud_asr_error_is_hedgeable("timeout"));
        assert!(cloud_asr_error_is_hedgeable("http: error sending request"));
        assert!(cloud_asr_error_is_hedgeable("timed out"));
        assert!(!cloud_asr_error_is_hedgeable(
            "provider rejected request with status 401 Unauthorized"
        ));
        assert!(!cloud_asr_error_is_hedgeable(
            "provider rejected request with status 400: bad request"
        ));
    }

    #[test]
    fn asr_engine_hedge_label_records_the_path_that_actually_ran() {
        assert_eq!(
            asr_engine_hedge_label("openai_audio", EngineKind::SenseVoice),
            "openai_audio→sensevoice"
        );
    }

    #[test]
    fn copy_toast_holds_the_capsule_briefly() {
        let outcome = TranscribeOutcome {
            text: "你好".into(),
            asr_text: "你好".into(),
            corrected_text: "你好".into(),
            model_applied: true,
            fallback_reason: None,
            asr_engine: "sensevoice".into(),
            corrector_engine: "none".into(),
            sample_rate: 16_000,
            num_samples: 0,
            duration_ms: 0,
            session: SessionRecord::new(),
            watch_post_paste: false,
            post_paste_seconds: 0,
            insert_notice: Some("已复制".into()),
        };
        assert_eq!(capsule_notice_hold(&outcome), Duration::from_secs(2));
        let mut with_fallback = outcome.clone();
        with_fallback.fallback_reason = Some("timeout".into());
        assert_eq!(capsule_notice_hold(&with_fallback), Duration::from_secs(4));
    }

    #[test]
    fn near_silent_capture_threshold_rejects_invalid_or_inaudible_peaks() {
        assert_eq!(
            ensure_audible_capture(0.0, 0.0),
            Err(CaptureSignalIssue::AbsoluteSilence)
        );
        assert_eq!(
            ensure_audible_capture(f32::NAN, 0.0),
            Err(CaptureSignalIssue::InvalidSignal)
        );
        assert_eq!(
            ensure_audible_capture(0.0, f32::INFINITY),
            Err(CaptureSignalIssue::InvalidSignal)
        );
        assert_eq!(
            ensure_audible_capture(1.0e-7, 1.0e-7),
            Err(CaptureSignalIssue::AbsoluteSilence)
        );
        assert_eq!(
            ensure_audible_capture(1.0e-7, ABSOLUTE_SILENCE_PEAK),
            Err(CaptureSignalIssue::AbsoluteSilence)
        );
        assert!(ensure_audible_capture(1.0e-7, 1.000_001e-6).is_ok());
        assert!(ensure_audible_capture(1.0e-6, 1.0e-5).is_ok());
        assert!(ensure_audible_capture(0.001, 0.005).is_ok());
    }

    #[test]
    fn absolute_silence_uses_a_stable_structured_issue() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        record_capture_signal_issue(&mut attempt, CaptureSignalIssue::AbsoluteSilence);

        assert_eq!(attempt.pipeline_metrics.stage_issues.len(), 1);
        let issue = &attempt.pipeline_metrics.stage_issues[0];
        assert_eq!(issue.stage, PipelineStage::Capture);
        assert_eq!(issue.kind, PipelineIssueKind::AbsoluteSilence);
        assert_eq!(issue.message, ABSOLUTE_SILENCE_ISSUE);
    }

    #[test]
    fn invalid_audio_signal_is_not_classified_as_absolute_silence() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        record_capture_signal_issue(&mut attempt, CaptureSignalIssue::InvalidSignal);

        let issue = &attempt.pipeline_metrics.stage_issues[0];
        assert_eq!(issue.stage, PipelineStage::Capture);
        assert_eq!(issue.kind, PipelineIssueKind::InputUnavailable);
        assert_eq!(issue.message, INVALID_CAPTURE_ISSUE);
    }

    #[tokio::test]
    async fn capture_stop_runs_and_failure_is_persisted_after_snapshot_lock_poisoning() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig::default();
        let context = crate::context_capture::ContextRecorder::new(&config.context, dir.path());
        let qwen = crate::qwen_engine_from_config(&config.asr);
        let store = Arc::new(Mutex::new(Some(
            Store::open(dir.path().join("capture.sqlite")).unwrap(),
        )));
        let state = AppState {
            edit_learning: crate::edit_learning_runtime::DesktopEditLearning::new(
                store.clone(),
                false,
            ),
            store,
            audio: AudioCapture::new(),
            meeting_recorder: lumen_asr::MeetingRecorder::new(),
            meeting_power_guard: std::sync::Mutex::new(None),
            meeting_battery_poll: std::sync::Mutex::new(None),
            meeting_watchdog: std::sync::Mutex::new(None),
            meeting_max_duration_watchdog: std::sync::Mutex::new(None),
            meeting_recording_owner: std::sync::Mutex::new(
                crate::meeting_cmd::MeetingRecordingOwner::default(),
            ),
            meeting_audio_edit: tokio::sync::Mutex::new(()),
            meeting_recovery_notices: std::sync::Mutex::new(Vec::new()),
            meeting_live: crate::meeting_live::MeetingLive::default(),
            meeting_system_audio: crate::meeting_system_audio::MeetingSystemAudio::default(),
            meeting_mic_aec: crate::meeting_mic_aec::MeetingMicAec::default(),
            capture: crate::mode_arbiter::CaptureArbiter::new(),
            meeting_detection: crate::meeting_detection::MeetingDetectionService::new(),
            engine: Mutex::new(EngineKind::SenseVoice),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(dir.path().join("sensevoice"))),
            qwen: Mutex::new(qwen),
            whisper: Mutex::new(WhisperAsr::new(dir.path().join("whisper"))),
            config: Mutex::new(config),
            context,
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.engine.lock().unwrap();
            panic!("poison engine snapshot lock");
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.config.lock().unwrap();
            panic!("poison config snapshot lock");
        }));

        let error = stop_and_transcribe_inner(&state, true, None)
            .await
            .unwrap_err();

        assert!(error.contains("not recording"));
        let store = state.store.lock().unwrap();
        let store = store.as_ref().unwrap();
        let sessions = store.list_sessions(1).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Failed);
        let attempts = store
            .list_dictation_attempts(sessions[0].id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, AttemptStatus::Failed);
        assert_eq!(attempts[0].failed_stage, Some(PipelineStage::Capture));
    }

    #[test]
    fn silero_model_path_prefers_configured_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("silero_vad.onnx");
        std::fs::write(&model, b"model").unwrap();
        let vad = VadConfig {
            silero_model_path: model.to_string_lossy().into_owned(),
            ..VadConfig::default()
        };
        assert_eq!(resolve_silero_model_path(&vad), Some(model));
    }

    #[test]
    fn silero_model_path_rejects_missing_configured_file() {
        // An explicit but wrong path must not silently fall through to the
        // shared install — surfacing the typo beats picking another model.
        let vad = VadConfig {
            silero_model_path: "/nonexistent/silero_vad.onnx".into(),
            ..VadConfig::default()
        };
        assert_eq!(resolve_silero_model_path(&vad), None);
    }
}
