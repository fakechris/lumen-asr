//! Local ASR model discovery + SenseVoice package download (onboarding Stage C).

use crate::AppState;
use lumen_asr::{
    default_sensevoice_dir, default_whisper_dir, sensevoice_ready, whisper_ready, EngineKind,
    SenseVoiceSherpaAsr, WhisperAsr,
};
use lumen_platform::default_data_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

const SENSEVOICE_ARCHIVE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2";
const SENSEVOICE_ARCHIVE_NAME: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2";

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
    pub active_engine: String,
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

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn scan_candidates() -> Vec<AsrModelCandidate> {
    let mut out = Vec::new();
    let mut push = |engine: &str, path: PathBuf, source: &str| {
        if !path.exists() {
            return;
        }
        let ready = match engine {
            "sensevoice" => sensevoice_ready(&path),
            "whisper" => whisper_ready(&path),
            _ => false,
        };
        if !ready && source == "app" {
            // still list empty app dir for "will download here"
            out.push(AsrModelCandidate {
                engine: engine.into(),
                path: path.display().to_string(),
                label: format!("{engine} ({source})"),
                ready,
                source: source.into(),
            });
            return;
        }
        if !ready {
            return;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        out.push(AsrModelCandidate {
            engine: engine.into(),
            path: path.display().to_string(),
            label: format!("{name} · {source}"),
            ready,
            source: source.into(),
        });
    };

    let app_sv = default_data_dir().join("models/sensevoice");
    push("sensevoice", app_sv, "app");
    if let Ok(p) = std::env::var("LUMEN_SENSEVOICE_DIR") {
        push("sensevoice", PathBuf::from(p), "env");
    }
    let h = home();
    for name in [
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
    ] {
        push(
            "sensevoice",
            h.join(".coli/models").join(name),
            "coli-cache",
        );
    }

    let app_wh = default_data_dir().join("models/whisper");
    push("whisper", app_wh, "app");
    if let Ok(p) = std::env::var("LUMEN_WHISPER_DIR") {
        push("whisper", PathBuf::from(p), "env");
    }
    for name in [
        "sherpa-onnx-whisper-tiny.en",
        "sherpa-onnx-whisper-base.en",
    ] {
        push("whisper", h.join(".coli/models").join(name), "coli-cache");
    }

    // Dedupe by path
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.path.clone()));
    out
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
        active_engine: engine,
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

/// Point runtime at an existing model directory and persist via env-style config note.
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
            std::env::set_var("LUMEN_WHISPER_DIR", path.display().to_string());
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
            std::env::set_var("LUMEN_SENSEVOICE_DIR", path.display().to_string());
        }
    }
    tracing::info!(path = %path.display(), %engine, "ASR model path selected");
    check_asr_model_status(state)
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
            std::env::set_var("LUMEN_SENSEVOICE_DIR", dir.display().to_string());
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
    let dest_root = default_data_dir().join("models");
    std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;
    let archive_path = dest_root.join(SENSEVOICE_ARCHIVE_NAME);
    let extract_tmp = dest_root.join("sensevoice-extract-tmp");
    let final_dir = dest_root.join("sensevoice");

    if sensevoice_ready(&final_dir) {
        emit_progress(app, "done", "SenseVoice already installed", 0, None);
        return Ok(final_dir);
    }

    if DOWNLOAD_CANCEL.load(Ordering::SeqCst) {
        return Err("download cancelled".into());
    }

    emit_progress(app, "downloading", "Downloading SenseVoice model…", 0, None);

    // Prefer curl for progress-friendly large downloads on macOS.
    let status = Command::new("curl")
        .args([
            "-fL",
            "--progress-bar",
            "-o",
            archive_path.to_str().unwrap_or("/tmp/sv.tar.bz2"),
            SENSEVOICE_ARCHIVE_URL,
        ])
        .status()
        .map_err(|e| format!("curl failed to start: {e}"))?;

    if DOWNLOAD_CANCEL.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&archive_path);
        return Err("download cancelled".into());
    }
    if !status.success() {
        return Err(format!(
            "download failed (curl exit {:?}). Check network or place model under {}",
            status.code(),
            final_dir.display()
        ));
    }

    let bytes = std::fs::metadata(&archive_path).map(|m| m.len()).unwrap_or(0);
    emit_progress(
        app,
        "extracting",
        "Extracting archive…",
        bytes,
        Some(bytes),
    );

    let _ = std::fs::remove_dir_all(&extract_tmp);
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
    // Move into place
    std::fs::rename(&found, &final_dir).or_else(|_| {
        copy_dir_recursive(&found, &final_dir)?;
        let _ = std::fs::remove_dir_all(&found);
        Ok::<(), String>(())
    })?;

    let _ = std::fs::remove_dir_all(&extract_tmp);
    // Keep archive for cache (optional) — remove to save space
    let _ = std::fs::remove_file(&archive_path);

    if !sensevoice_ready(&final_dir) {
        return Err("model installed but validation failed".into());
    }

    emit_progress(app, "done", "SenseVoice ready", bytes, Some(bytes));
    tracing::info!(dir = %final_dir.display(), "SenseVoice model installed");
    Ok(final_dir)
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

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_dir_recursive(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// Silence unused if Mutex imported for future download state
#[allow(dead_code)]
static _FUTURE: Mutex<Option<()>> = Mutex::new(None);
