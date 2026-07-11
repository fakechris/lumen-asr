//! Recording + local ASR + model corrector IPC (M2–M5).

use crate::corrector_svc::{engine_label, run_correct_with_intent};
use crate::session_debug::{self, SessionDebugMeta};
use crate::AppState;
use crate::config::AsrServiceConfig;
use lumen_asr::{
    prepare_for_asr, sensevoice_status, whisper_status, AsrEngine, AsrRequest, AsrResult,
    AudioDeviceInfo, EngineKind, EngineStatus, OpenAiAudioAsr, OpenAiAudioConfig,
};
use lumen_core::{FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use lumen_platform_macos::{
    activate_target, frontmost_app_name, frontmost_target, is_self_app_name, is_self_target,
    FrontmostTarget,
};
use lumen_prompts::IntentSpec;
use serde::Serialize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

/// Frontmost app captured when hotkey dictation starts — restored before paste.
static TARGET: Mutex<Option<FrontmostTarget>> = Mutex::new(None);

/// Intent bound to the current dictation session (default / translate / raw).
static SESSION_INTENT: Mutex<IntentSpec> = Mutex::new(IntentSpec::Default);

pub fn set_session_intent(intent: IntentSpec) {
    if let Ok(mut g) = SESSION_INTENT.lock() {
        *g = intent;
    }
}

fn take_session_intent() -> IntentSpec {
    SESSION_INTENT
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or(IntentSpec::Default)
}

/// Serial dictation lifecycle — prevents overlapping start/stop thrash (felt like crash).
const PHASE_IDLE: u8 = 0;
const PHASE_RECORDING: u8 = 1;
const PHASE_PROCESSING: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);
static RECORD_STARTED: Mutex<Option<Instant>> = Mutex::new(None);

/// Only discard as bounce if shorter than this *and* almost no audio.
const BOUNCE_MS: u128 = 80;

/// Snapshot frontmost app into process-local cache (sync, preferred at press).
fn remember_target_app() {
    let t = frontmost_target();
    match &t {
        Some(t) if !is_self_target(t) => {
            tracing::info!(
                name = ?t.name,
                bundle = ?t.bundle_id,
                "dictation target remembered"
            );
            if let Ok(mut g) = TARGET.lock() {
                *g = Some(t.clone());
            }
        }
        other => {
            tracing::warn!(?other, "could not remember non-self frontmost target");
            // Keep previous good target if we briefly saw ourselves.
            if let Ok(mut g) = TARGET.lock() {
                if g.is_none() {
                    *g = t.filter(|x| !is_self_target(x));
                }
            }
        }
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

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AsrStatus {
    pub recording: bool,
    pub engine: EngineKind,
    pub sensevoice: EngineStatus,
    pub whisper: EngineStatus,
    pub active_ready: bool,
}

#[tauri::command]
pub fn list_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    lumen_asr::AudioCapture::list_devices().map_err(|e| e.to_string())
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

#[tauri::command]
pub fn set_asr_engine(state: State<'_, AppState>, engine: String) -> Result<EngineKind, String> {
    let kind = EngineKind::parse(&engine).ok_or_else(|| format!("unknown engine: {engine}"))?;
    *state.engine.lock().map_err(|_| "engine lock poisoned".to_string())? = kind;
    Ok(kind)
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
    let sv = sensevoice_status();
    let wh = whisper_status();
    let active_ready = match engine {
        EngineKind::SenseVoice => sv.ready,
        EngineKind::Whisper => wh.ready,
    };
    AsrStatus {
        recording: state.audio.is_recording(),
        engine,
        sensevoice: sv,
        whisper: wh,
        active_ready,
    }
}

#[tauri::command]
pub fn start_recording(state: State<'_, AppState>) -> Result<(), String> {
    start_recording_inner(&state)
}

pub fn start_recording_inner(state: &AppState) -> Result<(), String> {
    state.audio.start().map_err(|e| e.to_string())
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

pub async fn stop_and_transcribe_inner(
    state: &AppState,
    save: bool,
    app: Option<&AppHandle>,
) -> Result<TranscribeOutcome, String> {
    let capture = state.audio.stop().map_err(|e| e.to_string())?;
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

    if capture.samples.is_empty() {
        return Err("no audio captured (0 samples) — hold longer or check mic".into());
    }

    let samples_16k = prepare_for_asr(&capture);
    let (rms, peak) = session_debug::audio_stats(&samples_16k);
    if peak < 0.005 {
        tracing::warn!(peak, rms, "audio nearly silent — ASR likely empty");
    }

    let engine_kind = *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;

    // Clone samples for debug dump (after ASR we still have this).
    let samples_for_debug = samples_16k.clone();

    let asr_cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .asr
        .clone();

    let result = run_asr(state, engine_kind, &asr_cfg, samples_16k).await?;

    let asr_text = result.text.trim().to_string();
    let asr_engine_str = if asr_cfg.provider.starts_with("local") {
        engine_kind.as_str().to_string()
    } else {
        asr_cfg.provider.clone()
    };
    tracing::info!(%asr_text, engine = %asr_engine_str, "ASR result");

    let entries = {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        match store_guard.as_ref() {
            Some(s) => s.list_dictionary().unwrap_or_default(),
            None => vec![],
        }
    };

    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();

    let intent = take_session_intent();
    tracing::info!(?intent, "running corrector with session intent");
    let corr = run_correct_with_intent(&cfg, &asr_text, &entries, intent.clone()).await;
    let corrected_text = corr.text.trim().to_string();
    let corrector_engine = if corr.model_applied {
        engine_label(&cfg)
    } else if !cfg.corrector.enabled || cfg.corrector.provider == "none" {
        "none".into()
    } else {
        format!("{}:fallback", engine_label(&cfg))
    };
    if !corr.model_applied {
        if matches!(intent, IntentSpec::Translate { .. }) {
            tracing::warn!(
                %corrector_engine,
                "translate intent but model not applied — output stays ASR language"
            );
        }
    }
    tracing::info!(%corrected_text, %corrector_engine, model_applied = corr.model_applied, "corrector result");

    let target = TARGET.lock().ok().and_then(|g| g.clone());
    let mut notes: Vec<String> = Vec::new();
    if peak < 0.005 {
        notes.push("near-silent audio".into());
    }
    if asr_text.is_empty() || asr_text == "." {
        notes.push("empty/dot ASR".into());
    }
    if matches!(intent, IntentSpec::Translate { .. }) && !corr.model_applied {
        notes.push(format!(
            "翻译未执行：模型未响应（{}）。请在「AI 修正」里确认 Ollama 模型名可用（当前机器常见 qwen3.5:9b）",
            corrector_engine
        ));
    }

    let mut insert_strategy = InsertStrategy::None;
    let mut did_insert = false;
    let mut frontmost_before_insert = None;
    if cfg.inject.auto_insert && !corrected_text.is_empty() {
        let ax_ok = lumen_platform_macos::is_accessibility_trusted();
        if !ax_ok {
            // Without Accessibility, synthetic keys only hit *this* process — copy instead.
            notes.push(
                "accessibility denied — text copied to clipboard; enable Accessibility for insert"
                    .into(),
            );
            tracing::error!(
                "Accessibility not granted; cannot inject into other apps. Open System Settings → Privacy & Security → Accessibility and enable this process"
            );
            match crate::inject_cmd::copy_only(&corrected_text).await {
                Ok(()) => {
                    insert_strategy = InsertStrategy::CopyOnly;
                    tracing::info!("copied result to clipboard (no AX)");
                }
                Err(e) => {
                    notes.push(format!("clipboard copy failed: {e}"));
                }
            }
            if let Some(app) = app {
                emit_dictation(
                    app,
                    DictationUiEvent::Error {
                        message: "需要「辅助功能」权限才能插入到其他 App。请到 系统设置 → 隐私与安全性 → 辅助功能 打开 Lumen（或 lumen-asr-desktop），然后重试。文字已复制到剪贴板。".into(),
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
                    did_insert = !matches!(
                        insert_strategy,
                        InsertStrategy::None | InsertStrategy::CopyOnly
                    );
                    tracing::info!(
                        ?insert_strategy,
                        frontmost = ?frontmost_app_name(),
                        "auto-insert done"
                    );
                }
                Err(e) => {
                    notes.push(format!("insert error: {e}"));
                    tracing::warn!(error = %e, "auto-insert failed");
                }
            }
        }
    }

    let mut rec = SessionRecord::new();
    rec.status = SessionStatus::Completed;
    rec.insert_strategy = insert_strategy;
    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(engine_kind.as_str().into());
    rec.corrector_engine = Some(corrector_engine.clone());
    rec.focus = FocusInfo {
        app_name: target.as_ref().and_then(|t| t.name.clone()),
        bundle_id: target.as_ref().and_then(|t| t.bundle_id.clone()),
        window_title: None,
    };

    // Always write debug dump (audio + texts) for analysis.
    let debug_dir = session_debug::new_session_dir(&rec.id.to_string());
    let meta = SessionDebugMeta {
        session_id: rec.id.to_string(),
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        target_app: target.as_ref().and_then(|t| t.name.clone()),
        target_bundle_id: target.as_ref().and_then(|t| t.bundle_id.clone()),
        frontmost_before_insert,
        sample_rate_capture: sample_rate,
        num_samples_capture: num_samples,
        sample_rate_asr: 16_000,
        num_samples_asr: samples_for_debug.len(),
        duration_ms,
        rms,
        peak,
        asr_engine: engine_kind.as_str().into(),
        corrector_engine: corrector_engine.clone(),
        asr_text: asr_text.clone(),
        corrected_text: corrected_text.clone(),
        insert_strategy: format!("{insert_strategy:?}"),
        insert_ok: did_insert,
        notes,
    };
    if let Err(e) = session_debug::write_session_debug(&debug_dir, &meta, &samples_for_debug) {
        tracing::warn!(error = %e, "failed to write session debug");
    } else {
        rec.audio_path = Some(debug_dir.join("audio_16k.wav").display().to_string());
    }

    if save {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        if let Some(store) = store_guard.as_ref() {
            store.save_session(&rec).map_err(|e| e.to_string())?;
        }
    }

    Ok(TranscribeOutcome {
        text: corrected_text.clone(),
        asr_text,
        corrected_text,
        model_applied: corr.model_applied,
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
        let store = guard.as_ref().ok_or_else(|| "database not available".to_string())?;
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
    let uuid = uuid::Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let mut rec = {
        let guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        let store = guard.as_ref().ok_or_else(|| "database not available".to_string())?;
        store
            .get_session(uuid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "session not found".to_string())?
    };
    let path = rec
        .audio_path
        .clone()
        .ok_or_else(|| "此会话没有音频，无法重新转写".to_string())?;
    let (samples, sample_rate) = session_debug::read_wav_mono_f32(std::path::Path::new(&path))?;
    if samples.is_empty() {
        return Err("音频为空".into());
    }

    // Resample to 16 kHz if needed
    let samples_16k = if sample_rate == 16_000 {
        samples
    } else {
        lumen_asr::resample_linear(&samples, sample_rate, 16_000)
    };

    let engine_kind = *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;

    tracing::info!(
        %id,
        engine = engine_kind.as_str(),
        samples = samples_16k.len(),
        "retry transcription start"
    );

    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();
    let result = run_asr(&*state, engine_kind, &cfg.asr, samples_16k.clone()).await?;

    let asr_text = result.text.trim().to_string();
    let entries = {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        match store_guard.as_ref() {
            Some(s) => s.list_dictionary().unwrap_or_default(),
            None => vec![],
        }
    };
    let corr = run_correct_with_intent(&cfg, &asr_text, &entries, IntentSpec::Default).await;
    let corrected_text = corr.text.trim().to_string();
    let corrector_engine = if corr.model_applied {
        engine_label(&cfg)
    } else if !cfg.corrector.enabled || cfg.corrector.provider == "none" {
        "none".into()
    } else {
        format!("{}:fallback", engine_label(&cfg))
    };

    rec.asr_raw = Some(asr_text.clone());
    rec.corrected = Some(corrected_text.clone());
    rec.pasted = Some(corrected_text.clone());
    rec.asr_engine = Some(engine_kind.as_str().into());
    rec.corrector_engine = Some(corrector_engine.clone());
    rec.status = SessionStatus::Completed;

    // Refresh sidecar text dumps if debug dir is the parent of audio_path.
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::write(parent.join("asr.txt"), &asr_text);
        let _ = std::fs::write(parent.join("corrected.txt"), &corrected_text);
    }

    {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        if let Some(store) = store_guard.as_ref() {
            store.save_session(&rec).map_err(|e| e.to_string())?;
        }
    }

    tracing::info!(%id, %asr_text, %corrected_text, "retry transcription done");
    Ok(RetryOutcome {
        session: rec,
        asr_text,
        corrected_text,
        asr_engine: engine_kind.as_str().into(),
        corrector_engine,
        model_applied: corr.model_applied,
    })
}

#[tauri::command]
pub fn cancel_recording(state: State<'_, AppState>) -> Result<(), String> {
    cancel_recording_inner(&state)
}

pub fn cancel_recording_inner(state: &AppState) -> Result<(), String> {
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
) -> Result<AsrResult, String> {
    let provider = asr_cfg.provider.as_str();

    if matches!(
        provider,
        "aliyun_qwen" | "volcengine" | "soniox" | "stepfun" | "mimo"
    ) {
        return Err(format!(
            "ASR「{provider}」已收录预设（对齐闪电说 endpoint），完整流式客户端下一阶段接入。请改用本地 SenseVoice 或 OpenAI Audio。"
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

    if provider == "local_whisper" || matches!(engine_kind, EngineKind::Whisper) {
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
    Done { outcome: TranscribeOutcome },
    Error { message: String },
    Cancelled,
}

fn intent_ui_label(intent: &IntentSpec) -> (String, Option<String>, String) {
    match intent {
        IntentSpec::Translate { target_language } => (
            "translate".into(),
            Some(target_language.clone()),
            format!("翻译→{target_language}"),
        ),
        IntentSpec::Raw => ("raw".into(), None, "原文".into()),
        IntentSpec::Default | IntentSpec::PolishOverride => {
            ("default".into(), None, "录音".into())
        }
    }
}

pub fn emit_dictation(app: &AppHandle, event: DictationUiEvent) {
    let _ = app.emit("dictation", &event);
}

/// Start recording if idle (push-to-talk press / toggle start).
pub async fn dictation_start(app: AppHandle) -> Result<(), String> {
    dictation_start_with_intent(app, IntentSpec::Default).await
}

pub async fn dictation_start_with_intent(
    app: AppHandle,
    intent: IntentSpec,
) -> Result<(), String> {
    // Only one session at a time.
    if PHASE
        .compare_exchange(
            PHASE_IDLE,
            PHASE_RECORDING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        tracing::info!(
            phase = PHASE.load(Ordering::SeqCst),
            "dictation_start ignored (not idle)"
        );
        return Ok(());
    }

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

    // Capture typing target first (NSWorkspace is ~ms) so insert restores correctly.
    remember_target_app();

    // Then start mic; never show UI before we know the target.
    match start_recording_inner(&state) {
        Ok(()) => {
            tracing::info!(%intent_kind, ?target_lang, "dictation recording live");
            // Force-show capsule on hotkey start so user always sees feedback.
            crate::capsule::set_capsule_visible(&app, true, "listening");
            if !show_capsule {
                tracing::debug!("config show_capsule=false but forcing visible for hotkey feedback");
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
            PHASE.store(PHASE_IDLE, Ordering::SeqCst);
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

    // Peek intent for UI (don't take — stop_and_transcribe still needs it).
    let intent_peek = SESSION_INTENT
        .lock()
        .map(|g| g.clone())
        .unwrap_or(IntentSpec::Default);
    let (intent_kind, target_lang, intent_label) = intent_ui_label(&intent_peek);
    tracing::info!(held_ms, %intent_kind, "dictation stop → ASR");
    let processing_msg = if intent_kind == "translate" {
        format!("转写并翻译→{}…", target_lang.as_deref().unwrap_or("en"))
    } else {
        format!("转写与修正中…（{intent_label}）")
    };
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
            crate::capsule::set_capsule_visible(&app, false, "idle");
            emit_dictation(
                &app,
                DictationUiEvent::Error {
                    message: e.clone(),
                },
            );
            emit_dictation(&app, DictationUiEvent::Idle);
            PHASE.store(PHASE_IDLE, Ordering::SeqCst);
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
