//! Recording + local ASR + model corrector IPC (M2–M5).

use crate::corrector_svc::{engine_label, run_correct};
use crate::AppState;
use lumen_asr::{
    prepare_for_asr, sensevoice_status, whisper_status, AsrEngine, AsrRequest, AudioDeviceInfo,
    EngineKind, EngineStatus,
};
use lumen_core::{FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

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
    state.audio.set_device(name);
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
    let outcome = stop_and_transcribe_inner(&state, save.unwrap_or(true)).await?;
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
) -> Result<TranscribeOutcome, String> {
    let capture = state.audio.stop().map_err(|e| e.to_string())?;
    let num_samples = capture.samples.len();
    let sample_rate = capture.sample_rate;
    let duration_ms = if sample_rate > 0 {
        (num_samples as u64 * 1000) / sample_rate as u64
    } else {
        0
    };

    if capture.samples.is_empty() {
        return Err("no audio captured".into());
    }

    let samples_16k = prepare_for_asr(&capture);
    let engine_kind = *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;

    let result = match engine_kind {
        EngineKind::SenseVoice => {
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
            .map_err(|e| e.to_string())?
        }
        EngineKind::Whisper => {
            let eng = state
                .whisper
                .lock()
                .map_err(|_| "asr lock poisoned".to_string())?
                .clone();
            eng.transcribe(AsrRequest {
                samples: samples_16k,
                sample_rate: 16_000,
                hotwords: vec![],
            })
            .await
            .map_err(|e| e.to_string())?
        }
    };

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

    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();

    let corr = run_correct(&cfg, &asr_text, &entries).await;
    let corrected_text = corr.text.trim().to_string();
    let corrector_engine = if corr.model_applied {
        engine_label(&cfg)
    } else if !cfg.corrector.enabled || cfg.corrector.provider == "none" {
        "none".into()
    } else {
        format!("{}:fallback", engine_label(&cfg))
    };

    let mut insert_strategy = InsertStrategy::None;
    let mut did_insert = false;
    if cfg.inject.auto_insert && !corrected_text.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        match crate::inject_cmd::insert_with_config(&cfg.inject, &corrected_text).await {
            Ok(out) => {
                insert_strategy = out.strategy;
                did_insert = !matches!(insert_strategy, InsertStrategy::None | InsertStrategy::CopyOnly);
                tracing::info!(?insert_strategy, "auto-insert done");
            }
            Err(e) => {
                tracing::warn!(error = %e, "auto-insert failed; text still available in UI");
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
        app_name: Some("Lumen ASR".into()),
        bundle_id: None,
        window_title: None,
    };

    if save {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        if let Some(store) = store_guard.as_ref() {
            store.save_session(&rec).map_err(|e| e.to_string())?;
        }
    }

    // M6: optional post-paste watch for user corrections in the target app.
    if did_insert && cfg.learning.post_paste_capture {
        // Need AppHandle — not available here. Caller with AppHandle should spawn.
        // Stored flag on outcome for UI/hotkey path.
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

/// Capsule / hotkey lifecycle events for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "phase")]
pub enum DictationUiEvent {
    Idle,
    Listening { message: String },
    Processing { message: String },
    Done { outcome: TranscribeOutcome },
    Error { message: String },
    Cancelled,
}

pub fn emit_dictation(app: &AppHandle, event: DictationUiEvent) {
    let _ = app.emit("dictation", &event);
}

pub async fn toggle_dictation(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let show_capsule = state
        .config
        .lock()
        .map(|c| c.hotkey.show_capsule)
        .unwrap_or(true);

    if state.audio.is_recording() {
        emit_dictation(
            &app,
            DictationUiEvent::Processing {
                message: "转写与修正中…".into(),
            },
        );
        crate::capsule::set_capsule_visible(&app, show_capsule, "processing");
        match stop_and_transcribe_inner(&state, true).await {
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
                return Err(e);
            }
        }
    } else {
        start_recording_inner(&state)?;
        crate::capsule::set_capsule_visible(&app, show_capsule, "listening");
        emit_dictation(
            &app,
            DictationUiEvent::Listening {
                message: "正在录音…".into(),
            },
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn toggle_dictation_cmd(app: AppHandle) -> Result<(), String> {
    toggle_dictation(app).await
}
