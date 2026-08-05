//! Local ASR model discovery + SenseVoice package download (onboarding Stage C).

use crate::AppState;
use lumen_asr::{
    default_paraformer_offline_dir, default_paraformer_streaming_dir, default_qwen_dir,
    default_sensevoice_dir, default_whisper_dir, download_paraformer_offline_package,
    download_paraformer_streaming_package, download_sensevoice_package, lumen_models_dir,
    paraformer_offline_ready, paraformer_streaming_ready, qwen_ready, scan_model_candidates,
    sensevoice_ready, whisper_ready, EngineKind, SenseVoiceSherpaAsr, WhisperAsr,
    PARAFORMER_OFFLINE_ARCHIVE_URL, PARAFORMER_STREAMING_ARCHIVE_URL, SENSEVOICE_ARCHIVE_URL,
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
    /// Offline Paraformer (meeting transcription): word-level timestamps.
    pub paraformer_offline_ready: bool,
    pub paraformer_offline_dir: String,
    /// Streaming Paraformer (meeting real-time transcription).
    pub paraformer_streaming_ready: bool,
    pub paraformer_streaming_dir: String,
    pub qwen_runtime_supported: bool,
    pub qwen_fallback_reason: Option<String>,
    pub recommended_engine: String,
    pub total_memory_mb: Option<u64>,
    pub models_root: String,
    pub active_engine: String,
    pub active_model_dir: String,
    pub candidates: Vec<AsrModelCandidate>,
    pub download_url: String,
    pub paraformer_offline_download_url: String,
    pub paraformer_streaming_download_url: String,
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
            path: crate::display_path(&candidate.path),
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
    let pf_offline = default_paraformer_offline_dir();
    let pf_streaming = default_paraformer_streaming_dir();
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
        "qwen" => crate::display_path(&qw_live),
        "whisper" => crate::display_path(&wh_live),
        _ => crate::display_path(&sv_live),
    };
    let qwen_runtime_supported = cfg!(target_os = "macos");
    let total_memory_mb = total_memory_mb();
    let qwen_fallback_reason = if !qwen_runtime_supported {
        Some(
            "Qwen3-ASR local runtime currently uses Apple MLX and is unavailable on Windows; SenseVoice was selected."
                .into(),
        )
    } else if total_memory_mb.is_some_and(|memory| memory < 8 * 1024) {
        Some("Available system memory is below the 8 GB Qwen safety threshold; SenseVoice was selected.".into())
    } else {
        None
    };
    let recommended_engine = if qwen_runtime_supported
        && qwen_fallback_reason.is_none()
        && (qwen_ready(&qw_live) || qwen_ready(&qw))
    {
        "qwen"
    } else {
        "sensevoice"
    };
    Ok(AsrModelStatus {
        sensevoice_ready: sensevoice_ready(&sv_live) || sensevoice_ready(&sv),
        sensevoice_dir: if sensevoice_ready(&sv_live) {
            crate::display_path(&sv_live)
        } else {
            crate::display_path(&sv)
        },
        whisper_ready: whisper_ready(&wh_live) || whisper_ready(&wh),
        whisper_dir: if whisper_ready(&wh_live) {
            crate::display_path(&wh_live)
        } else {
            crate::display_path(&wh)
        },
        qwen_ready: qwen_ready(&qw_live) || qwen_ready(&qw),
        qwen_dir: if qwen_ready(&qw_live) {
            crate::display_path(&qw_live)
        } else {
            crate::display_path(&qw)
        },
        paraformer_offline_ready: paraformer_offline_ready(&pf_offline),
        paraformer_offline_dir: crate::display_path(&pf_offline),
        paraformer_streaming_ready: paraformer_streaming_ready(&pf_streaming),
        paraformer_streaming_dir: crate::display_path(&pf_streaming),
        qwen_runtime_supported,
        qwen_fallback_reason,
        recommended_engine: recommended_engine.into(),
        total_memory_mb,
        models_root: crate::display_path(&lumen_models_dir()),
        active_engine: engine,
        active_model_dir,
        candidates: scan_candidates(),
        download_url: SENSEVOICE_ARCHIVE_URL.into(),
        paraformer_offline_download_url: PARAFORMER_OFFLINE_ARCHIVE_URL.into(),
        paraformer_streaming_download_url: PARAFORMER_STREAMING_ARCHIVE_URL.into(),
    })
}

#[cfg(target_os = "windows")]
fn total_memory_mb() -> Option<u64> {
    use std::ffi::c_void;

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut c_void) -> i32;
    }

    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    (unsafe { GlobalMemoryStatusEx((&mut status as *mut MemoryStatusEx).cast()) } != 0)
        .then_some(status.total_phys / (1024 * 1024))
}

#[cfg(not(target_os = "windows"))]
fn total_memory_mb() -> Option<u64> {
    None
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

/// Install the offline Paraformer model (meeting transcription, word-level
/// timestamps). Unlike the SenseVoice command this does **not** switch the
/// active dictation engine — Paraformer is an optional meeting model — so it
/// just downloads and returns refreshed model status.
#[tauri::command]
pub async fn start_paraformer_offline_download(app: AppHandle) -> Result<AsrModelStatus, String> {
    run_paraformer_download(app, PfVariant::Offline).await
}

/// Install the streaming Paraformer model (meeting real-time transcription).
#[tauri::command]
pub async fn start_paraformer_streaming_download(app: AppHandle) -> Result<AsrModelStatus, String> {
    run_paraformer_download(app, PfVariant::Streaming).await
}

#[derive(Clone, Copy)]
enum PfVariant {
    Offline,
    Streaming,
}

async fn run_paraformer_download(
    app: AppHandle,
    variant: PfVariant,
) -> Result<AsrModelStatus, String> {
    // Shares the single-download guard + cancel flag with the SenseVoice
    // installer; the cluster install lock serializes any cross-process races.
    if DOWNLOAD_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("download already running".into());
    }
    DOWNLOAD_CANCEL.store(false, Ordering::SeqCst);

    let app_for_dl = app.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || download_paraformer(&app_for_dl, variant))
            .await;
    DOWNLOAD_RUNNING.store(false, Ordering::SeqCst);

    match result {
        // Meeting models are not the active dictation engine, so there is no
        // engine/state reload here — just report the new readiness.
        Ok(Ok(_dir)) => check_asr_model_status(app.state::<AppState>()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("download task failed: {e}")),
    }
}

fn download_paraformer(app: &AppHandle, variant: PfVariant) -> Result<PathBuf, String> {
    let root = lumen_models_dir();
    let on_progress =
        |p: lumen_asr::DownloadProgress| emit_progress(app, &p.phase, &p.message, p.bytes, p.total);
    let (installed, label) = match variant {
        PfVariant::Offline => (
            download_paraformer_offline_package(&root, &DOWNLOAD_CANCEL, on_progress)
                .map_err(|error| error.to_string())?,
            "Paraformer (offline)",
        ),
        PfVariant::Streaming => (
            download_paraformer_streaming_package(&root, &DOWNLOAD_CANCEL, on_progress)
                .map_err(|error| error.to_string())?,
            "Paraformer (streaming)",
        ),
    };
    tracing::info!(dir = %installed.display(), model = label, "Paraformer model installed");
    Ok(installed)
}
