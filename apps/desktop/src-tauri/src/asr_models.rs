//! Local ASR model discovery + SenseVoice package download (onboarding Stage C).

use crate::AppState;
use lumen_asr::{
    default_qwen_dir, default_sensevoice_dir, default_whisper_dir, download_sensevoice_package,
    lumen_models_dir, qwen_ready, scan_model_candidates, sensevoice_ready, whisper_ready,
    EngineKind, SenseVoiceSherpaAsr, WhisperAsr, SENSEVOICE_ARCHIVE_URL,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager, State};

static DOWNLOAD_CANCEL: AtomicBool = AtomicBool::new(false);
static DOWNLOAD_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrModelCandidate {
    pub engine: String,
    pub path: String,
    pub label: String,
    pub ready: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrModelStatus {
    pub sensevoice_ready: bool,
    pub sensevoice_dir: String,
    pub whisper_ready: bool,
    pub whisper_dir: String,
    pub qwen_ready: bool,
    pub qwen_dir: String,
    pub models_root: String,
    pub active_engine: String,
    pub active_model_dir: String,
    pub candidates: Vec<AsrModelCandidate>,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrDownloadProgress {
    pub phase: String,
    pub message: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub percent: Option<f32>,
}

fn scan_candidates() -> Vec<AsrModelCandidate> {
    scan_model_candidates()
        .into_iter()
        .map(|candidate| AsrModelCandidate {
            engine: candidate.engine,
            path: candidate.path.display().to_string(),
            label: candidate.label,
            ready: candidate.ready,
            source: candidate.source,
        })
        .collect()
}

#[tauri::command]
pub fn check_asr_model_status(state: State<'_, AppState>) -> Result<AsrModelStatus, String> {
    let engine = state
        .engine
        .lock()
        .map(|g| g.as_str().to_string())
        .unwrap_or_else(|_| "sensevoice".into());
    let sv = default_sensevoice_dir();
    let wh = default_whisper_dir();
    let qw = default_qwen_dir();
    // Prefer live engine dirs if already loaded.
    let sv_live = state
        .sensevoice
        .lock()
        .ok()
        .map(|g| g.model_dir().to_path_buf())
        .unwrap_or_else(|| sv.clone());
    let wh_live = state
        .whisper
        .lock()
        .ok()
        .map(|g| g.model_dir().to_path_buf())
        .unwrap_or_else(|| wh.clone());
    let qw_live = state
        .qwen
        .lock()
        .ok()
        .map(|g| g.model_dir().to_path_buf())
        .unwrap_or_else(|| qw.clone());

    let active_model_dir = match engine.as_str() {
        "qwen" => qw_live.display().to_string(),
        "whisper" => wh_live.display().to_string(),
        _ => sv_live.display().to_string(),
    };
    Ok(AsrModelStatus {
        sensevoice_ready: sensevoice_ready(&sv_live) || sensevoice_ready(&sv),
        sensevoice_dir: if sensevoice_ready(&sv_live) {
            sv_live.display().to_string()
        } else {
            sv.display().to_string()
        },
        whisper_ready: whisper_ready(&wh_live) || whisper_ready(&wh),
        whisper_dir: if whisper_ready(&wh_live) {
            wh_live.display().to_string()
        } else {
            wh.display().to_string()
        },
        qwen_ready: qwen_ready(&qw_live) || qwen_ready(&qw),
        qwen_dir: if qwen_ready(&qw_live) {
            qw_live.display().to_string()
        } else {
            qw.display().to_string()
        },
        models_root: lumen_models_dir().display().to_string(),
        active_engine: engine,
        active_model_dir,
        candidates: scan_candidates(),
        download_url: SENSEVOICE_ARCHIVE_URL.into(),
    })
}

#[tauri::command]
pub fn list_local_asr_models() -> Result<Vec<AsrModelCandidate>, String> {
    Ok(scan_candidates())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UseAsrModelInput {
    pub path: String,
    pub engine: Option<String>,
}

/// Point runtime at an existing model directory and persist it for the next launch.
#[tauri::command]
pub fn use_existing_asr_model(
    app: AppHandle,
    state: State<'_, AppState>,
    input: UseAsrModelInput,
) -> Result<AsrModelStatus, String> {
    let path = PathBuf::from(input.path.trim());
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    let engine = crate::dictation::canonical_asr_provider(
        &input.engine.unwrap_or_else(|| "sensevoice".into()),
    );
    match engine.as_str() {
        "local_qwen" => {
            if !qwen_ready(&path) {
                return Err(
                    "folder is not a valid Qwen3-ASR MLX model dir (need config, safetensors and tokenizer assets)"
                        .into(),
                );
            }
            crate::dictation::unload_qwen(&state);
            let asr_config = {
                let mut config = state
                    .config
                    .lock()
                    .map_err(|_| "config lock poisoned".to_string())?;
                config.asr.set_model_dir_for(EngineKind::Qwen, &path);
                config.asr.provider = "local_qwen".into();
                config.save()?;
                config.asr.clone()
            };
            *state
                .qwen
                .lock()
                .map_err(|_| "qwen lock poisoned".to_string())? =
                crate::qwen_engine_from_config(&asr_config);
            *state
                .engine
                .lock()
                .map_err(|_| "engine lock poisoned".to_string())? = EngineKind::Qwen;
            crate::schedule_qwen_runtime_refresh(app)?;
        }
        "local_whisper" => {
            if !whisper_ready(&path) {
                return Err("folder is not a valid Whisper (sherpa) model dir".into());
            }
            crate::dictation::unload_qwen(&state);
            *state
                .whisper
                .lock()
                .map_err(|_| "whisper lock poisoned".to_string())? = WhisperAsr::new(path.clone());
            *state
                .engine
                .lock()
                .map_err(|_| "engine lock poisoned".to_string())? = EngineKind::Whisper;
            persist_model_selection(&state, &path, EngineKind::Whisper)?;
        }
        _ => {
            if !sensevoice_ready(&path) {
                return Err(
                    "folder is not a valid SenseVoice model dir (need model*.onnx + tokens.txt)"
                        .into(),
                );
            }
            crate::dictation::unload_qwen(&state);
            *state
                .sensevoice
                .lock()
                .map_err(|_| "sensevoice lock poisoned".to_string())? =
                SenseVoiceSherpaAsr::new(path.clone());
            *state
                .engine
                .lock()
                .map_err(|_| "engine lock poisoned".to_string())? = EngineKind::SenseVoice;
            persist_model_selection(&state, &path, EngineKind::SenseVoice)?;
        }
    }
    tracing::info!(path = %path.display(), %engine, "ASR model path selected");
    check_asr_model_status(state)
}

fn persist_model_selection(
    state: &AppState,
    path: &Path,
    engine: EngineKind,
) -> Result<(), String> {
    let mut config = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    config.asr.set_model_dir_for(engine, path);
    config.asr.provider = match engine {
        EngineKind::Qwen => "local_qwen",
        EngineKind::Whisper => "local_whisper",
        // Only local engines reach this persistence path.
        _ => "local_sensevoice",
    }
    .into();
    config.save()
}

#[tauri::command]
pub fn cancel_asr_model_download() -> Result<(), String> {
    DOWNLOAD_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub async fn start_asr_model_download(app: AppHandle) -> Result<AsrModelStatus, String> {
    if DOWNLOAD_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("download already running".into());
    }
    DOWNLOAD_CANCEL.store(false, Ordering::SeqCst);

    let app_for_dl = app.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || download_sensevoice(&app_for_dl)).await;
    DOWNLOAD_RUNNING.store(false, Ordering::SeqCst);

    match result {
        Ok(Ok(dir)) => {
            // Reload into app state
            let state = app.state::<AppState>();
            crate::dictation::unload_qwen(&state);
            *state
                .sensevoice
                .lock()
                .map_err(|_| "sensevoice lock poisoned".to_string())? =
                SenseVoiceSherpaAsr::new(dir.clone());
            *state
                .engine
                .lock()
                .map_err(|_| "engine lock poisoned".to_string())? = EngineKind::SenseVoice;
            persist_model_selection(&state, &dir, EngineKind::SenseVoice)?;
            check_asr_model_status(state)
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("download task failed: {e}")),
    }
}

fn emit_progress(app: &AppHandle, phase: &str, message: &str, bytes: u64, total: Option<u64>) {
    let percent = total.map(|t| {
        if t == 0 {
            0.0
        } else {
            (bytes as f32 / t as f32) * 100.0
        }
    });
    let _ = app.emit(
        "asr-download-progress",
        AsrDownloadProgress {
            phase: phase.into(),
            message: message.into(),
            bytes,
            total,
            percent,
        },
    );
}

fn download_sensevoice(app: &AppHandle) -> Result<PathBuf, String> {
    // The shared installer handles the cross-process install lock, cancel
    // checks, curl download, extraction, and atomic publish.
    let installed = download_sensevoice_package(&lumen_models_dir(), &DOWNLOAD_CANCEL, |p| {
        emit_progress(app, &p.phase, &p.message, p.bytes, p.total)
    })
    .map_err(|error| error.to_string())?;
    tracing::info!(dir = %installed.display(), "SenseVoice model installed");
    Ok(installed)
}
