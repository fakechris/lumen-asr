//! Recording + local ASR + model corrector IPC (M2–M5).

use crate::config::AsrServiceConfig;
use crate::context_capture::{ActiveContextCapture, StageUsageInput};
use crate::pipeline_attempt::{
    apply_asr_result, build_pipeline_identity, elapsed_ms, mark_attempt_failed, persist_attempt,
    run_corrector_stage, write_attempt_debug, AttemptDebug,
};
use crate::session_debug;
use crate::AppState;
use lumen_asr::{
    prepare_for_asr, qwen_ready, sensevoice_ready, whisper_ready, AsrEngine, AsrRequest, AsrResult,
    AudioDeviceInfo, EngineKind, EngineStatus, OpenAiAudioAsr, OpenAiAudioConfig,
    QwenShadowRequest, QwenShadowTerm,
};
use lumen_context::TargetHint;
use lumen_core::{DictEntryKind, FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use lumen_dictionary::DictionaryEntry;
use lumen_platform_macos::{
    activate_target, frontmost_app_name, frontmost_target, is_self_app_name, is_self_target,
    FrontmostTarget,
};
use lumen_prompts::IntentSpec;
use lumen_store::{
    AttemptStatus, ContextStageUsage, DictationAttemptRecord, InsertionOutcome, PipelineIssueKind,
    PipelineStage, PipelineStageIssue,
};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// Frontmost app captured when hotkey dictation starts — restored before paste.
static TARGET: Mutex<Option<FrontmostTarget>> = Mutex::new(None);

/// Intent bound to the current dictation session (default / translate / raw).
static SESSION_INTENT: Mutex<IntentSpec> = Mutex::new(IntentSpec::Default);
/// UI-facing copy of session intent; kept until capsule goes idle so processing
/// phase still knows “翻译” after take_session_intent() for the corrector.
static UI_SESSION_INTENT: Mutex<IntentSpec> = Mutex::new(IntentSpec::Default);
const QWEN_SHADOW_TERM_LIMIT: usize = 64;

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
/// Reject only a missing/invalid signal; low-gain speech must still reach ASR.
const ABSOLUTE_SILENCE_PEAK: f32 = 1.0e-6;

/// Snapshot frontmost app into process-local cache (sync, preferred at press).
fn remember_target_app() -> Option<FrontmostTarget> {
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
    target
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
    pub qwen_runtime_path: String,
    pub qwen_runtime_ready: bool,
    pub qwen_runtime_checking: bool,
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

fn ensure_audible_capture(peak: f32) -> Result<(), &'static str> {
    if peak.is_finite() && peak > ABSOLUTE_SILENCE_PEAK {
        Ok(())
    } else {
        Err("未检测到麦克风信号。请检查麦克风权限、输入设备或静音状态后重试。")
    }
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
    if kind == EngineKind::Qwen {
        state
            .qwen
            .lock()
            .map_err(|_| "qwen lock poisoned".to_string())?
            .activate();
        crate::schedule_qwen_runtime_refresh(app)?;
    }
    Ok(kind)
}

pub(crate) fn unload_qwen(state: &AppState) {
    if let Ok(engine) = state.qwen.lock() {
        if !engine.unload() {
            tracing::warn!("Qwen worker is busy and could not be unloaded during engine switch");
        }
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
            EngineKind::SenseVoice => "local_sensevoice".into(),
            EngineKind::Qwen => "local_qwen".into(),
            EngineKind::Whisper => "local_whisper".into(),
        }
    } else {
        canonical_asr_provider(&asr_cfg.provider)
    };
    let sv = state
        .sensevoice
        .lock()
        .map(|engine| {
            let path = engine.model_dir();
            EngineStatus {
                kind: EngineKind::SenseVoice,
                ready: sensevoice_ready(path),
                model_dir: path.display().to_string(),
            }
        })
        .unwrap_or_else(|_| lumen_asr::sensevoice_status());
    let wh = state
        .whisper
        .lock()
        .map(|engine| {
            let path = engine.model_dir();
            EngineStatus {
                kind: EngineKind::Whisper,
                ready: whisper_ready(path),
                model_dir: path.display().to_string(),
            }
        })
        .unwrap_or_else(|_| lumen_asr::whisper_status());
    let (qwen, qwen_runtime_path) = state
        .qwen
        .lock()
        .map(|engine| {
            let model_dir = engine.model_dir();
            (
                EngineStatus {
                    kind: EngineKind::Qwen,
                    ready: qwen_ready(model_dir),
                    model_dir: model_dir.display().to_string(),
                },
                engine.python_executable().display().to_string(),
            )
        })
        .unwrap_or_else(|_| {
            let status = lumen_asr::qwen_status();
            (
                status,
                asr_cfg.qwen_python_executable().display().to_string(),
            )
        });
    let (qwen_runtime_ready, qwen_runtime_checking) = if qwen.ready {
        state
            .qwen_runtime
            .lock()
            .map(|runtime| {
                let current = runtime.executable == std::path::PathBuf::from(&qwen_runtime_path);
                (current && runtime.ready, current && runtime.checking)
            })
            .unwrap_or((false, false))
    } else {
        (false, false)
    };
    let active_ready = match provider.as_str() {
        "local_sensevoice" => sv.ready,
        "local_qwen" => qwen.ready && qwen_runtime_ready,
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
        qwen_runtime_path,
        qwen_runtime_ready,
        qwen_runtime_checking,
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
    checking: bool,
) -> Result<(), String> {
    if ready {
        return Ok(());
    }
    if checking {
        return Err(format!(
            "{provider_label} 正在检查本地运行环境，请稍后再试。"
        ));
    }
    let guidance = match canonical_asr_provider(provider).as_str() {
        "local_qwen" => {
            "请先选择有效的 Qwen MLX 模型目录和能够导入 mlx_qwen3_asr.Session 的 Python。"
        }
        "local_sensevoice" => "请先安装或选择有效的 SenseVoice 模型。",
        "local_whisper" => "请先选择有效的 Whisper 模型。",
        "openai_audio" | "custom" => "请先完成在线 ASR 的地址与凭据配置。",
        _ => "当前 ASR 尚未接入可运行的识别客户端。",
    };
    Err(format!("{provider_label} 未就绪。{guidance}"))
}

pub fn start_recording_inner(state: &AppState) -> Result<(), String> {
    if state.audio.is_recording() {
        return Ok(());
    }
    let status = asr_status_from(state);
    ensure_active_asr_ready(
        &status.provider,
        &status.provider_label,
        status.active_ready,
        status.qwen_runtime_checking,
    )?;
    let target = remember_target_app();
    let hint = target.as_ref().map(|target| TargetHint {
        app_name: target.name.clone(),
        bundle_id: target.bundle_id.clone(),
        ..TargetHint::default()
    });
    state.context.begin(hint);
    state.audio.start().map_err(|error| {
        state.context.clear_active();
        error.to_string()
    })
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeOutcome {
    pub text: String,
    pub asr_text: String,
    pub corrected_text: String,
    pub model_applied: bool,
    pub asr_engine: String,
    pub corrector_engine: String,
    pub sample_rate: u32,
    pub num_samples: usize,
    pub duration_ms: u64,
    pub session: SessionRecord,
    /// When true, UI/backend should start post-paste edit watch.
    pub watch_post_paste: bool,
    pub post_paste_seconds: u64,
}

#[tauri::command]
pub async fn stop_and_transcribe(
    app: AppHandle,
    state: State<'_, AppState>,
    save: Option<bool>,
) -> Result<TranscribeOutcome, String> {
    let outcome = stop_and_transcribe_inner(&state, save.unwrap_or(true), Some(&app)).await?;
    if outcome.watch_post_paste {
        crate::learning::spawn_post_paste_watch(
            app,
            outcome.session.id,
            outcome.corrected_text.clone(),
            outcome.post_paste_seconds,
        );
    }
    Ok(outcome)
}

async fn attach_frozen_context(
    state: &AppState,
    active: Option<&ActiveContextCapture>,
    attempt: &mut DictationAttemptRecord,
    app: Option<&AppHandle>,
) {
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
        return;
    };

    match active.freeze(&state.store).await {
        Ok(input_ref) => {
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
            if should_archive {
                if let Some(app) = app {
                    let active = active.clone();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let state = app.state::<AppState>();
                        if let Err(error) = active.archive(&state.store).await {
                            tracing::warn!(error = %error, "late context archive failed");
                        }
                    });
                }
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
        }
    }
}

pub async fn stop_and_transcribe_inner(
    state: &AppState,
    save: bool,
    app: Option<&AppHandle>,
) -> Result<TranscribeOutcome, String> {
    let pipeline_started = Instant::now();
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
    attach_frozen_context(state, active_context.as_ref(), &mut attempt, app).await;
    let capture = match capture_result {
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
            if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
                tracing::warn!(error = %persist_error, "failed to persist capture stop failure");
            }
            return Err(error);
        }
    };
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
        let error = "no audio captured (0 samples) — hold longer or check mic".to_string();
        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Capture,
            &error,
            pipeline_started,
        );
        rec.status = SessionStatus::Failed;
        rec.asr_engine = Some(engine_kind.as_str().into());
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
        if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
            tracing::warn!(error = %persist_error, "failed to persist capture failure");
        }
        return Err(error);
    }

    let preprocess_started = Instant::now();
    let samples_16k = prepare_for_asr(&capture);
    attempt.pipeline_metrics.preprocess_ms = elapsed_ms(preprocess_started);
    let (rms, peak) = session_debug::audio_stats(&samples_16k);
    if let Err(error) = ensure_audible_capture(peak) {
        tracing::error!(peak, rms, "audio capture rejected before ASR");
        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Capture,
            error,
            pipeline_started,
        );
        rec.status = SessionStatus::Failed;
        rec.asr_engine = Some(engine_kind.as_str().into());
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
        if let Err(persist_error) = persist_attempt(state, save, &rec, attempt) {
            tracing::warn!(error = %persist_error, "failed to persist silent capture failure");
        }
        return Err(error.to_string());
    }

    // Clone samples for debug dump (after ASR we still have this).
    let samples_for_debug = samples_16k.clone();

    let asr_started = Instant::now();
    let result = match run_asr(state, engine_kind, &asr_cfg, samples_16k, &mut attempt).await {
        Ok(result) => result,
        Err(error) => {
            attempt.pipeline_metrics.asr_ms = elapsed_ms(asr_started);
            attempt.pipeline_metrics.set_asr_rtf();
            mark_attempt_failed(&mut attempt, PipelineStage::Asr, &error, pipeline_started);
            rec.status = SessionStatus::Failed;
            rec.asr_engine = Some(engine_kind.as_str().into());
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
            return Err(error);
        }
    };
    let (asr_text, enhanced_text) = apply_asr_result(&mut attempt, &result, asr_started);
    tracing::info!(
        attempt_id = %attempt.id,
        asr_chars = asr_text.chars().count(),
        engine = %asr_engine_str,
        "ASR result"
    );

    tracing::info!(?intent, "running corrector with session intent");
    let correction =
        run_corrector_stage(state, &cfg, &enhanced_text, intent.clone(), &mut attempt).await?;
    let corrected_text = correction.text;
    let corrector_engine = correction.engine;
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

    let mut insert_strategy = InsertStrategy::None;
    let mut did_insert = false;
    let mut insertion_outcome = InsertionOutcome::NotRequested;
    let mut frontmost_before_insert = None;
    let insert_started = Instant::now();
    if cfg.inject.auto_insert && !corrected_text.is_empty() {
        let ax_ok = lumen_platform_macos::is_accessibility_trusted();
        if !ax_ok {
            // Without Accessibility, synthetic keys only hit *this* process — copy instead.
            tracing::error!(
                "Accessibility not granted; cannot inject into other apps. Open System Settings → Privacy & Security → Accessibility and enable this process"
            );
            let clipboard_copied = match crate::inject_cmd::copy_only(&corrected_text).await {
                Ok(()) => {
                    insert_strategy = InsertStrategy::CopyOnly;
                    insertion_outcome = InsertionOutcome::Copied;
                    notes.push(
                        "accessibility denied — text copied to clipboard; enable Accessibility for insert"
                            .into(),
                    );
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
            if let Some(app) = app {
                let message = if clipboard_copied {
                    "需要「辅助功能」权限才能插入到其他 App。请到 系统设置 → 隐私与安全性 → 辅助功能 打开 Lumen（或 lumen-asr-desktop），然后重试。文字已复制到剪贴板。"
                } else {
                    "需要「辅助功能」权限才能插入到其他 App，并且复制到剪贴板也失败了。请先手动复制结果，并到 系统设置 → 隐私与安全性 → 辅助功能 打开 Lumen（或 lumen-asr-desktop）后重试。"
                };
                emit_dictation(
                    app,
                    DictationUiEvent::Error {
                        message: message.into(),
                    },
                );
            }
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

            match crate::inject_cmd::insert_with_config(&cfg.inject, &corrected_text).await {
                Ok(out) => {
                    insert_strategy = out.strategy;
                    insertion_outcome = insertion_outcome_for_strategy(insert_strategy);
                    did_insert = insertion_outcome == InsertionOutcome::Inserted;
                    tracing::info!(
                        ?insert_strategy,
                        frontmost = ?frontmost_app_name(),
                        "auto-insert done"
                    );
                }
                Err(e) => {
                    insertion_outcome = InsertionOutcome::Failed;
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
                }
            }
        }
    }
    attempt.pipeline_metrics.insert_ms = elapsed_ms(insert_started);
    attempt
        .pipeline_metrics
        .set_insertion_outcome(insertion_outcome);
    attempt.inserted = did_insert.then(|| corrected_text.clone());
    attempt.status = AttemptStatus::Completed;
    attempt.pipeline_metrics.total_ms = elapsed_ms(pipeline_started);

    rec.status = SessionStatus::Completed;
    rec.insert_strategy = insert_strategy;
    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(engine_kind.as_str().into());
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
        asr_engine: engine_kind.as_str().into(),
        corrector_engine,
        sample_rate,
        num_samples,
        duration_ms,
        session: rec,
        watch_post_paste: did_insert && cfg.learning.post_paste_capture,
        post_paste_seconds: cfg.learning.post_paste_seconds,
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
    attempt.pipeline_inputs.context = state.store.lock().ok().and_then(|guard| {
        guard.as_ref().and_then(|store| {
            store
                .list_dictation_attempts(rec.id, 1, None)
                .ok()
                .and_then(|attempts| {
                    attempts
                        .into_iter()
                        .next()
                        .and_then(|prior| prior.pipeline_inputs.context)
                })
        })
    });
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
    let result = match run_asr(&state, engine_kind, &cfg.asr, samples_16k, &mut attempt).await {
        Ok(result) => result,
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
    let (asr_text, enhanced_text) = apply_asr_result(&mut attempt, &result, asr_started);
    let correction = run_corrector_stage(
        &state,
        &cfg,
        &enhanced_text,
        IntentSpec::Default,
        &mut attempt,
    )
    .await?;
    let corrected_text = correction.text;
    let corrector_engine = correction.engine;
    attempt.status = AttemptStatus::Completed;
    attempt.pipeline_metrics.total_ms = elapsed_ms(pipeline_started);

    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(engine_kind.as_str().into());
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
        asr_engine: engine_kind.as_str().into(),
        corrector_engine,
        model_applied: correction.model_applied,
    })
}

#[tauri::command]
pub fn cancel_recording(state: State<'_, AppState>) -> Result<(), String> {
    cancel_recording_inner(&state)
}

pub fn cancel_recording_inner(state: &AppState) -> Result<(), String> {
    state.context.clear_active();
    if state.audio.is_recording() {
        let _ = state.audio.stop();
    }
    Ok(())
}

async fn run_asr(
    state: &AppState,
    engine_kind: EngineKind,
    asr_cfg: &AsrServiceConfig,
    samples_16k: Vec<f32>,
    attempt: &mut DictationAttemptRecord,
) -> Result<AsrResult, String> {
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
            timeout: std::time::Duration::from_secs(asr_cfg.timeout_secs.max(30)),
            language: if asr_cfg.language.trim().is_empty() {
                None
            } else {
                Some(asr_cfg.language.clone())
            },
        })
        .map_err(|e| e.to_string())?;
        return eng
            .transcribe(AsrRequest {
                samples: samples_16k,
                sample_rate: 16_000,
                hotwords: vec![],
            })
            .await
            .map_err(|e| e.to_string());
    }

    let selected_local_engine = engine_kind_for_provider(provider).unwrap_or(engine_kind);
    if selected_local_engine == EngineKind::Whisper {
        let eng = state
            .whisper
            .lock()
            .map_err(|_| "asr lock poisoned".to_string())?
            .clone();
        return eng
            .transcribe(AsrRequest {
                samples: samples_16k,
                sample_rate: 16_000,
                hotwords: vec![],
            })
            .await
            .map_err(|e| e.to_string());
    }

    if selected_local_engine == EngineKind::Qwen {
        let eng = state
            .qwen
            .lock()
            .map_err(|_| "asr lock poisoned".to_string())?
            .clone();
        let (shadow, dictionary_captured) =
            qwen_shadow_request_from_store(state, asr_cfg.qwen_shadow_enabled);
        let selected = shadow.enabled && !shadow.terms.is_empty();
        let projection = serde_json::to_vec(&shadow).map_err(|error| error.to_string())?;
        let capture_id = attempt
            .pipeline_inputs
            .context
            .as_ref()
            .map(|input| input.capture_id);
        let not_used_reason = if !shadow.enabled {
            Some("qwen_shadow_disabled".to_owned())
        } else if !dictionary_captured {
            Some("personal_dictionary_unavailable".to_owned())
        } else if shadow.terms.is_empty() {
            Some("no_confirmed_personal_terms".to_owned())
        } else {
            None
        };
        match state.context.record_stage_usage(StageUsageInput {
            capture_id,
            attempt_id: attempt.id,
            stage: PipelineStage::Enhancement,
            sources: vec!["personal_dictionary".into()],
            projection: Some(&projection),
            captured: dictionary_captured,
            selected,
            consumed: selected,
            sent: selected,
            not_used_reason,
        }) {
            Ok(usage) => attempt.pipeline_inputs.stage_usages.push(usage),
            Err(error) => {
                tracing::warn!(error = %error, "failed to persist Qwen shadow input provenance");
                attempt
                    .pipeline_metrics
                    .stage_issues
                    .push(PipelineStageIssue {
                        stage: PipelineStage::Enhancement,
                        kind: PipelineIssueKind::InputUnavailable,
                        message: "qwen shadow input provenance unavailable".into(),
                    });
            }
        }
        return eng
            .transcribe_with_shadow(
                AsrRequest {
                    samples: samples_16k,
                    sample_rate: 16_000,
                    hotwords: vec![],
                },
                Some(shadow),
            )
            .await
            .map_err(|e| e.to_string());
    }

    let eng = state
        .sensevoice
        .lock()
        .map_err(|_| "asr lock poisoned".to_string())?
        .clone();
    eng.transcribe(AsrRequest {
        samples: samples_16k,
        sample_rate: 16_000,
        hotwords: vec![],
    })
    .await
    .map_err(|e| e.to_string())
}

fn qwen_shadow_request_from_store(state: &AppState, enabled: bool) -> (QwenShadowRequest, bool) {
    if !enabled {
        return (
            QwenShadowRequest {
                enabled: false,
                ..QwenShadowRequest::default()
            }
            .bounded(),
            false,
        );
    }
    let (entries, captured) = match state.store.lock() {
        Ok(store) => match store.as_ref().map(|store| store.list_dictionary()) {
            Some(Ok(entries)) => (entries, true),
            Some(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "dictionary unavailable; Qwen shadow will run without personal terms"
                );
                (Vec::new(), false)
            }
            None => (Vec::new(), false),
        },
        Err(_) => {
            tracing::warn!(
                "dictionary store lock poisoned; Qwen shadow will run without personal terms"
            );
            (Vec::new(), false)
        }
    };
    (build_qwen_shadow_request(&entries, enabled), captured)
}

fn build_qwen_shadow_request(entries: &[DictionaryEntry], enabled: bool) -> QwenShadowRequest {
    if !enabled {
        return QwenShadowRequest {
            enabled: false,
            ..QwenShadowRequest::default()
        }
        .bounded();
    }
    let mut terms = Vec::new();
    for entry in entries.iter().filter(|entry| entry.confirmed) {
        if terms.len() >= QWEN_SHADOW_TERM_LIMIT {
            break;
        }
        let surface = match entry.kind {
            DictEntryKind::Term => entry.term.as_deref(),
            DictEntryKind::Replacement => entry.to_text.as_deref(),
        };
        let Some(surface) = surface.map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        if terms
            .iter()
            .any(|term: &QwenShadowTerm| term.surface == surface)
        {
            continue;
        }
        terms.push(QwenShadowTerm {
            surface: surface.to_owned(),
            source: "personal_dictionary".into(),
        });
    }
    QwenShadowRequest {
        enabled,
        terms,
        ..QwenShadowRequest::default()
    }
    .bounded()
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
    Cancelled,
}

fn intent_ui_label(intent: &IntentSpec) -> (String, Option<String>, String) {
    match intent {
        IntentSpec::Translate { target_language } => (
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

/// Start recording if idle (push-to-talk press / toggle start).
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
    if PHASE
        .compare_exchange(
            PHASE_RECORDING,
            PHASE_PROCESSING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
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
            if outcome.watch_post_paste {
                crate::learning::spawn_post_paste_watch(
                    app.clone(),
                    outcome.session.id,
                    outcome.corrected_text.clone(),
                    outcome.post_paste_seconds,
                );
            }
            crate::capsule::set_capsule_visible(&app, false, "idle");
            emit_dictation(&app, DictationUiEvent::Done { outcome });
            emit_dictation(&app, DictationUiEvent::Idle);
            PHASE.store(PHASE_IDLE, Ordering::SeqCst);
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
    use crate::{AppState, QwenRuntimeStatus};
    use lumen_asr::{AudioCapture, SenseVoiceSherpaAsr, WhisperAsr};
    use lumen_dictionary::DictionaryEntry;
    use lumen_store::{Store, MAX_ATTEMPT_PAGE_SIZE};
    use std::sync::Mutex;

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
    fn near_silent_capture_threshold_rejects_invalid_or_inaudible_peaks() {
        assert!(ensure_audible_capture(0.0).is_err());
        assert!(ensure_audible_capture(f32::NAN).is_err());
        assert!(ensure_audible_capture(1.0e-7).is_err());
        assert!(ensure_audible_capture(1.0e-5).is_ok());
        assert!(ensure_audible_capture(0.005).is_ok());
    }

    #[test]
    fn qwen_shadow_uses_only_confirmed_personal_dictionary_surfaces() {
        let confirmed_term = DictionaryEntry::term("Codex");
        let confirmed_replacement = DictionaryEntry::replacement("cotex", "Codex CLI");
        let mut unconfirmed = DictionaryEntry::term("private draft");
        unconfirmed.confirmed = false;

        let request = build_qwen_shadow_request(
            &[
                confirmed_term,
                confirmed_replacement,
                unconfirmed,
                DictionaryEntry::term("Codex"),
            ],
            true,
        );

        assert!(request.enabled);
        assert_eq!(
            request
                .terms
                .iter()
                .map(|term| term.surface.as_str())
                .collect::<Vec<_>>(),
            ["Codex", "Codex CLI"]
        );
        assert!(request
            .terms
            .iter()
            .all(|term| term.source == "personal_dictionary"));

        let disabled =
            build_qwen_shadow_request(&[DictionaryEntry::term("must not leave the app")], false);
        assert!(!disabled.enabled);
        assert!(disabled.terms.is_empty());
    }

    #[tokio::test]
    async fn capture_stop_runs_and_failure_is_persisted_after_snapshot_lock_poisoning() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig::default();
        let context = crate::context_capture::ContextRecorder::new(&config.context, dir.path());
        let qwen = crate::qwen_engine_from_config(&config.asr);
        let qwen_executable = qwen.python_executable().to_path_buf();
        let state = AppState {
            store: Mutex::new(Some(
                Store::open(dir.path().join("capture.sqlite")).unwrap(),
            )),
            audio: AudioCapture::new(),
            engine: Mutex::new(EngineKind::SenseVoice),
            sensevoice: Mutex::new(SenseVoiceSherpaAsr::new(dir.path().join("sensevoice"))),
            qwen: Mutex::new(qwen),
            qwen_runtime: Mutex::new(QwenRuntimeStatus {
                executable: qwen_executable,
                ready: false,
                checking: false,
                generation: 0,
            }),
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
}
