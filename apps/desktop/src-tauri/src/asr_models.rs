//! Local ASR model discovery + SenseVoice package download (onboarding Stage C).

use crate::AppState;
use lumen_asr::{
    default_sensevoice_dir, default_whisper_dir, lumen_models_dir, scan_model_candidates,
    sensevoice_ready, whisper_ready, EngineKind, ModelInstallLock, SenseVoiceSherpaAsr, WhisperAsr,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

const SENSEVOICE_ARCHIVE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2";
const SENSEVOICE_ARCHIVE_NAME: &str =
    "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2";

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

    let active_model_dir = if engine == "whisper" {
        wh_live.display().to_string()
    } else {
        sv_live.display().to_string()
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
    state: State<'_, AppState>,
    input: UseAsrModelInput,
) -> Result<AsrModelStatus, String> {
    let path = PathBuf::from(input.path.trim());
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    let engine = input
        .engine
        .unwrap_or_else(|| "sensevoice".into())
        .to_ascii_lowercase();
    match engine.as_str() {
        "whisper" => {
            if !whisper_ready(&path) {
                return Err("folder is not a valid Whisper (sherpa) model dir".into());
            }
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
    config.asr.provider = match engine {
        EngineKind::SenseVoice => "local_sensevoice",
        EngineKind::Whisper => "local_whisper",
    }
    .into();
    config.asr.model_dir = path.display().to_string();
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
    let dest_root = lumen_models_dir();
    std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;
    let final_dir = dest_root.join("sensevoice");

    if sensevoice_ready(&final_dir) {
        emit_progress(app, "done", "SenseVoice already installed", 0, None);
        return Ok(final_dir);
    }

    let _install_lock = acquire_install_lock(app, &dest_root)?;
    if sensevoice_ready(&final_dir) {
        emit_progress(
            app,
            "done",
            "SenseVoice installed by another Lumen app",
            0,
            None,
        );
        return Ok(final_dir);
    }

    if DOWNLOAD_CANCEL.load(Ordering::SeqCst) {
        return Err("download cancelled".into());
    }

    let process_id = std::process::id();
    let archive_path = dest_root.join(format!(".{SENSEVOICE_ARCHIVE_NAME}.{process_id}.part"));
    let extract_tmp = dest_root.join(format!(".sensevoice-extract-{process_id}"));
    let _scratch = DownloadScratch::new(archive_path.clone(), extract_tmp.clone());

    emit_progress(app, "downloading", "Downloading SenseVoice model…", 0, None);

    // Prefer curl for progress-friendly large downloads on macOS.
    let mut child = Command::new("curl")
        .args([
            "-fL",
            "--progress-bar",
            "-o",
            archive_path.to_str().ok_or("bad archive path")?,
            SENSEVOICE_ARCHIVE_URL,
        ])
        .spawn()
        .map_err(|e| format!("curl failed to start: {e}"))?;
    let status = loop {
        if DOWNLOAD_CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("download cancelled".into());
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(100)),
        }
    };
    if !status.success() {
        return Err(format!(
            "download failed (curl exit {:?}). Check network or place model under {}",
            status.code(),
            final_dir.display()
        ));
    }

    let bytes = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);
    emit_progress(app, "extracting", "Extracting archive…", bytes, Some(bytes));

    std::fs::create_dir_all(&extract_tmp).map_err(|e| e.to_string())?;

    let tar_status = Command::new("tar")
        .args([
            "-xjf",
            archive_path.to_str().ok_or("bad archive path")?,
            "-C",
            extract_tmp.to_str().ok_or("bad extract path")?,
        ])
        .status()
        .map_err(|e| format!("tar failed: {e}"))?;
    if !tar_status.success() {
        return Err("failed to extract model archive".into());
    }

    // Find directory containing model + tokens
    let found = find_sensevoice_dir(&extract_tmp).ok_or_else(|| {
        "extracted archive but could not find model.int8.onnx + tokens.txt".to_string()
    })?;

    if final_dir.exists() {
        let _ = std::fs::remove_dir_all(&final_dir);
    }
    std::fs::rename(&found, &final_dir)
        .map_err(|error| format!("publish model atomically: {error}"))?;

    if !sensevoice_ready(&final_dir) {
        return Err("model installed but validation failed".into());
    }

    emit_progress(app, "done", "SenseVoice ready", bytes, Some(bytes));
    tracing::info!(dir = %final_dir.display(), "SenseVoice model installed");
    Ok(final_dir)
}

fn acquire_install_lock(app: &AppHandle, models_root: &Path) -> Result<ModelInstallLock, String> {
    let mut announced = false;
    loop {
        if DOWNLOAD_CANCEL.load(Ordering::SeqCst) {
            return Err("download cancelled".into());
        }
        match ModelInstallLock::try_acquire(models_root).map_err(|error| error.to_string())? {
            Some(lock) => return Ok(lock),
            None => {
                if !announced {
                    emit_progress(
                        app,
                        "waiting",
                        "Another Lumen app is installing SenseVoice…",
                        0,
                        None,
                    );
                    announced = true;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

struct DownloadScratch {
    archive: PathBuf,
    extract_dir: PathBuf,
}

impl DownloadScratch {
    fn new(archive: PathBuf, extract_dir: PathBuf) -> Self {
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_dir_all(&extract_dir);
        Self {
            archive,
            extract_dir,
        }
    }
}

impl Drop for DownloadScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.archive);
        let _ = std::fs::remove_dir_all(&self.extract_dir);
    }
}

fn find_sensevoice_dir(root: &Path) -> Option<PathBuf> {
    if sensevoice_ready(root) {
        return Some(root.to_path_buf());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if sensevoice_ready(&dir) {
            return Some(dir);
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                }
            }
        }
    }
    None
}
