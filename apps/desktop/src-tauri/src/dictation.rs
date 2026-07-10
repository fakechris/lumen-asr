//! Recording + local ASR IPC (M2).

use crate::AppState;
use lumen_asr::{
    prepare_for_asr, sensevoice_status, whisper_status, AsrEngine, AsrRequest, AudioDeviceInfo,
    EngineKind, EngineStatus,
};
use lumen_core::{FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use serde::Serialize;
use tauri::State;

#[derive(Debug, Serialize)]
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
    let engine = *state
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;
    let sv = sensevoice_status();
    let wh = whisper_status();
    let active_ready = match engine {
        EngineKind::SenseVoice => sv.ready,
        EngineKind::Whisper => wh.ready,
    };
    Ok(AsrStatus {
        recording: state.audio.is_recording(),
        engine,
        sensevoice: sv,
        whisper: wh,
        active_ready,
    })
}

#[tauri::command]
pub fn start_recording(state: State<'_, AppState>) -> Result<(), String> {
    state.audio.start().map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeOutcome {
    pub text: String,
    pub engine: String,
    pub sample_rate: u32,
    pub num_samples: usize,
    pub duration_ms: u64,
    pub session: SessionRecord,
}

#[tauri::command]
pub async fn stop_and_transcribe(
    state: State<'_, AppState>,
    save: Option<bool>,
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

    // Clone engines (Arc) so MutexGuard is not held across .await
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

    let text = result.text.trim().to_string();
    let mut rec = SessionRecord::new();
    rec.status = SessionStatus::Completed;
    rec.insert_strategy = InsertStrategy::None;
    rec.asr_raw = Some(text.clone());
    rec.corrected = Some(text.clone());
    rec.pasted = Some(text.clone());
    rec.asr_engine = Some(engine_kind.as_str().into());
    rec.corrector_engine = Some("none".into());
    rec.focus = FocusInfo {
        app_name: Some("Lumen ASR".into()),
        bundle_id: None,
        window_title: None,
    };

    if save.unwrap_or(true) {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        if let Some(store) = store_guard.as_ref() {
            store.save_session(&rec).map_err(|e| e.to_string())?;
        }
    }

    Ok(TranscribeOutcome {
        text,
        engine: engine_kind.as_str().into(),
        sample_rate,
        num_samples,
        duration_ms,
        session: rec,
    })
}

#[tauri::command]
pub fn cancel_recording(state: State<'_, AppState>) -> Result<(), String> {
    if state.audio.is_recording() {
        let _ = state.audio.stop();
    }
    Ok(())
}
