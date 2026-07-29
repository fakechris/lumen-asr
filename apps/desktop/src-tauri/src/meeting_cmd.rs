//! Tauri IPC for meeting recording.
//!
//! These commands drive the **independent** continuous recorder
//! (`lumen_asr::MeetingRecorder`) — they never touch the dictation
//! `AudioCapture` / hold-to-talk path. Starting a meeting acquires the
//! [`CaptureArbiter`], which suspends the dictation global hotkey; stopping it
//! restores the hotkey and moves the meeting to `Processing`.
//!
//! Stopping a recording then spawns the offline diarize + transcribe + minutes
//! pipeline ([`lumen_meeting::process_meeting`]) in the background: the stop
//! command returns immediately while the meeting advances
//! `processing → transcribing → summarizing → ready` (or `failed`). The UI polls
//! [`get_meeting_detail`] for the status. The real diarization step is macOS-only
//! (see the platform gating in `lumen-meeting`); on other platforms the pipeline
//! reports `Unsupported` and the meeting is left `failed`.

use crate::mode_arbiter::HotkeyAction;
use crate::AppState;
use lumen_core::{Meeting, MeetingDetail, MeetingStatus};
use lumen_meeting::{
    export_meeting as render_export, process_meeting, DiarModels, ExportOutput, ExportPreset,
    MeetingOptions, MinutesConfig, DEFAULT_MAX_SPEAKERS,
};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

fn with_store<T>(
    state: &State<'_, AppState>,
    f: impl FnOnce(&lumen_store::Store) -> Result<T, String>,
) -> Result<T, String> {
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;
    f(store)
}

fn apply_hotkey_action(app: &AppHandle, action: HotkeyAction) {
    // Implementation choice: we suspend by *unregistering* the global shortcut
    // (via the existing `pause_hotkeys`/`resume_hotkeys` mechanism already used
    // for hotkey capture) rather than a no-op check inside the dictation
    // handler. Unregistering fully removes the shortcut, so there is zero chance
    // dictation fires mid-meeting, and it reuses a proven, cross-platform path.
    match action {
        HotkeyAction::Suspend => {
            if let Err(e) = crate::hotkey::pause_hotkeys(app.clone()) {
                tracing::warn!(error = %e, "failed to suspend dictation hotkeys for meeting");
            }
        }
        HotkeyAction::Resume => {
            if let Err(e) = crate::hotkey::resume_hotkeys(app.clone()) {
                tracing::warn!(error = %e, "failed to resume dictation hotkeys after meeting");
            }
        }
        HotkeyAction::None => {}
    }
}

fn preferred_device(state: &State<'_, AppState>) -> Option<String> {
    let guard = state.config.lock().ok()?;
    guard
        .audio
        .device_name
        .as_ref()
        .filter(|n| !n.is_empty())
        .cloned()
}

/// Serialized meeting recording result returned to the UI.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingRecordingDto {
    pub id: String,
    pub audio_path: String,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub status: String,
}

/// Start a new meeting recording. Creates the meeting row (`Recording`), begins
/// the continuous recorder writing to `<data_dir>/meetings/<id>.wav`, and
/// suspends the dictation hotkey. Returns the new meeting id.
#[tauri::command]
pub fn start_meeting_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<String, String> {
    // 1. Arbiter gate (rejects if a dictation capture or meeting is active).
    let action = state.capture.begin_meeting().map_err(|e| e.to_string())?;

    // 2. Persist the meeting row up front (status = Recording).
    let mut meeting = Meeting::new();
    meeting.title = title.and_then(|t| {
        let trimmed = t.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });
    let meeting_id = meeting.id;
    if let Err(e) = with_store(&state, |s| {
        s.create_meeting(&meeting).map_err(|e| e.to_string())
    }) {
        state.capture.force_idle();
        return Err(e);
    }

    // 3. Prepare the output path under the app data dir.
    let dir = default_data_dir().join("meetings");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        state.capture.force_idle();
        let _ = with_store(&state, |s| {
            s.update_meeting_status(meeting_id, MeetingStatus::Failed)
                .map_err(|e| e.to_string())
        });
        return Err(format!("could not create meetings dir: {e}"));
    }
    let out_path = dir.join(format!("{meeting_id}.wav"));

    // 4. Start the independent continuous recorder.
    let device = preferred_device(&state);
    if let Err(e) = state.meeting_recorder.start(device, out_path) {
        // Roll back: mark failed and release the arbiter. No hotkey suspend was
        // applied yet, so nothing to restore.
        state.capture.force_idle();
        let _ = with_store(&state, |s| {
            s.update_meeting_status(meeting_id, MeetingStatus::Failed)
                .map_err(|e| e.to_string())
        });
        return Err(format!("could not start recording: {e}"));
    }

    // 5. Recording is live — now suspend the dictation hotkey.
    apply_hotkey_action(&app, action);

    tracing::info!(meeting_id = %meeting_id, "meeting recording started");
    Ok(meeting_id.to_string())
}

/// Stop the active meeting recording. Finalizes the WAV, records the audio path
/// and duration (status → `Processing`), restores the dictation hotkey, and
/// returns the recording summary. Then spawns the offline transcription + minutes
/// pipeline in the background (the meeting advances to `ready`/`failed` on its
/// own); the stop command itself returns without waiting for transcription.
#[tauri::command]
pub fn stop_meeting_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<MeetingRecordingDto, String> {
    let id = Uuid::parse_str(&meeting_id).map_err(|e| format!("invalid meeting id: {e}"))?;

    // Try to stop + finalize the recorder, but *keep* the result — the recovery
    // below (restore hotkey + reset arbiter) must run on every path, success or
    // failure. This is finally/defer semantics: whatever happens to the
    // recorder or the store write, "the mic and hotkey always come back".
    let stop_result = state.meeting_recorder.stop();

    // Unconditionally release the exclusive mode and restore the dictation
    // hotkey. If we bailed out on the `stop()` error above, `end_meeting` and
    // `resume` would never run — the app would be stuck in MeetingRecording with
    // the dictation hotkey unregistered forever.
    match state.capture.end_meeting() {
        Ok(action) => apply_hotkey_action(&app, action),
        Err(e) => tracing::warn!(error = %e, "arbiter end_meeting on stop"),
    }

    // Now that the mic and hotkey are restored, surface any recorder failure.
    // Mark the meeting Failed so it does not linger in `Recording`.
    let summary = match stop_result {
        Ok(summary) => summary,
        Err(e) => {
            let _ = with_store(&state, |s| {
                s.update_meeting_status(id, MeetingStatus::Failed)
                    .map_err(|e| e.to_string())
            });
            tracing::warn!(meeting_id = %id, error = %e, "meeting recorder stop failed");
            return Err(e.to_string());
        }
    };
    let audio_path = summary.wav_path.to_string_lossy().to_string();

    with_store(&state, |s| {
        s.set_meeting_audio(
            id,
            &audio_path,
            summary.duration_seconds,
            MeetingStatus::Processing,
        )
        .map_err(|e| e.to_string())
    })?;

    tracing::info!(
        meeting_id = %id,
        duration_seconds = summary.duration_seconds,
        "meeting recording stopped → processing"
    );

    // Kick off transcription in the background so the stop command returns now.
    spawn_meeting_processing(app, id, summary.wav_path.clone());

    Ok(MeetingRecordingDto {
        id: meeting_id,
        audio_path,
        duration_seconds: summary.duration_seconds,
        sample_rate: summary.sample_rate,
        status: MeetingStatus::Processing.as_str().to_string(),
    })
}

/// Pause the active meeting recording. Paused audio is dropped (no silent gap).
#[tauri::command]
pub fn pause_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    state.meeting_recorder.pause().map_err(|e| e.to_string())
}

/// Resume a paused meeting recording.
#[tauri::command]
pub fn resume_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    state.meeting_recorder.resume().map_err(|e| e.to_string())
}

/// Manually (re)run the offline transcription + minutes pipeline for a recorded
/// meeting — a debug / retry entry point. Returns immediately; the meeting
/// advances through the lifecycle in the background and the UI polls
/// [`get_meeting_detail`] for the result.
#[tauri::command]
pub fn process_meeting_now(
    app: AppHandle,
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<(), String> {
    let id = parse_id(&meeting_id, "meeting")?;
    let audio_path = with_store(&state, |s| {
        s.get_meeting(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "meeting not found".to_string())?
            .audio_path
            .ok_or_else(|| "meeting has no recorded audio".to_string())
    })?;
    spawn_meeting_processing(app, id, PathBuf::from(audio_path));
    Ok(())
}

/// Spawn the offline meeting pipeline off the IPC path so the caller returns
/// immediately. Transcription is slow (diarize + per-turn ASR + minutes LLM), so
/// it must never block the stop command.
///
/// It runs on a dedicated OS thread with its own **current-thread** Tokio
/// runtime, not `tauri::async_runtime::spawn`: `process_meeting` holds a
/// thread-affine SQLite connection (`&Store`, which is `!Send`) across `.await`s,
/// so its future cannot run on a multi-thread executor (whose `spawn` requires
/// `Send`). A private single-thread runtime keeps the connection and the future
/// thread-local.
///
/// `process_meeting` advances the meeting status itself and marks it `failed` on
/// any in-pipeline error; this wrapper additionally covers pre-flight failures
/// (engine/model/config) that never reach it.
fn spawn_meeting_processing(app: AppHandle, meeting_id: Uuid, wav: PathBuf) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::warn!(meeting_id = %meeting_id, error = %e, "could not build meeting runtime");
                if let Err(e) = mark_meeting_failed(meeting_id) {
                    tracing::warn!(meeting_id = %meeting_id, error = %e, "could not mark meeting failed");
                }
                return;
            }
        };
        runtime.block_on(async move {
            tracing::info!(meeting_id = %meeting_id, "meeting processing started");
            match process_meeting_pipeline(&app, meeting_id, &wav).await {
                Ok(()) => tracing::info!(meeting_id = %meeting_id, "meeting processing finished"),
                Err(reason) => {
                    tracing::warn!(meeting_id = %meeting_id, error = %reason, "meeting processing failed");
                    if let Err(e) = mark_meeting_failed(meeting_id) {
                        tracing::warn!(
                            meeting_id = %meeting_id,
                            error = %e,
                            "could not mark meeting failed"
                        );
                    }
                }
            }
        });
    });
}

/// Build the pipeline's dependencies from app config and run
/// [`process_meeting`]. Errors are surfaced to [`spawn_meeting_processing`] which
/// logs them and ensures the meeting ends `failed`.
async fn process_meeting_pipeline(
    app: &AppHandle,
    meeting_id: Uuid,
    wav: &Path,
) -> Result<(), String> {
    // A dedicated SQLite connection for the background worker. `process_meeting`
    // holds `&Store` across many `.await`s, which the UI's `std::sync::Mutex`
    // guard cannot span; a second connection (WAL + busy_timeout, see
    // `Store::open`) writes safely without contending the UI store lock.
    let store = Store::open(default_db_path()).map_err(|e| format!("open store: {e}"))?;

    // Build the ASR engine and (optional) minutes corrector from the user's
    // settings under brief locks, then drop the app-state handle before the long
    // async run below.
    let (asr_engine, corrector, minutes_model) = {
        let state = app.state::<AppState>();
        let engine_kind = *state
            .engine
            .lock()
            .map_err(|_| "engine lock poisoned".to_string())?;
        let (asr_cfg, corrector_cfg) = {
            let cfg = state
                .config
                .lock()
                .map_err(|_| "config lock poisoned".to_string())?;
            (cfg.asr.clone(), cfg.corrector.clone())
        };
        let asr_engine =
            crate::dictation::build_meeting_asr_engine(state.inner(), engine_kind, &asr_cfg)?;
        // Only build a corrector when an LLM is actually configured. With none,
        // the minutes step is skipped (transcript-only → ready) rather than
        // failing on an unparseable non-LLM response.
        let corrector = if corrector_cfg.enabled && corrector_cfg.provider != "none" {
            Some(crate::corrector_svc::build_corrector(&corrector_cfg)?)
        } else {
            None
        };
        let minutes_model = corrector.as_ref().and_then(|_| {
            let model = corrector_cfg.model.trim();
            (!model.is_empty()).then(|| model.to_string())
        });
        (asr_engine, corrector, minutes_model)
    };

    // Diarization models under `<lumen_models_dir>/diar/{seg.onnx,emb.onnx,plda}`.
    let diar_models = DiarModels::under_root(lumen_asr::lumen_models_dir().join("diar"));
    #[cfg(target_os = "macos")]
    ensure_diar_models_present(&diar_models)?;

    let minutes_cfg = corrector.as_ref().map(|corrector| MinutesConfig {
        corrector: corrector.as_ref(),
        model: minutes_model,
        max_tokens: None,
    });

    let opts = MeetingOptions {
        max_speakers: Some(DEFAULT_MAX_SPEAKERS),
        ..MeetingOptions::default()
    };

    process_meeting(
        &store,
        meeting_id,
        wav,
        &diar_models,
        asr_engine.as_ref(),
        minutes_cfg.as_ref(),
        &opts,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Verify the three diarization model artifacts exist before running, so a
/// missing install fails with a clear, actionable reason instead of a cryptic
/// model-load error. macOS-only: it is the only platform that runs real
/// diarization (elsewhere the pipeline reports `Unsupported`).
#[cfg(target_os = "macos")]
fn ensure_diar_models_present(models: &DiarModels) -> Result<(), String> {
    for (label, path) in [
        ("segmentation", models.segmentation.as_path()),
        ("embedding", models.embedding.as_path()),
        ("plda", models.plda_dir.as_path()),
    ] {
        if !path.exists() {
            return Err(format!(
                "diar models not found: missing {label} at {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Mark a meeting `failed` from the background worker, on its own connection
/// (never touches the UI store lock).
fn mark_meeting_failed(meeting_id: Uuid) -> Result<(), String> {
    let store = Store::open(default_db_path()).map_err(|e| e.to_string())?;
    store
        .update_meeting_status(meeting_id, MeetingStatus::Failed)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// Meeting library / detail / speaker-ops / export commands. These are
// model-free and cross-platform: they only read or mutate already-stored
// meetings. Offline transcription (macOS-only diarization) and the
// structured-minutes LLM pass live in `lumen_meeting::process_meeting`.

fn parse_id(value: &str, what: &str) -> Result<Uuid, String> {
    Uuid::parse_str(value).map_err(|e| format!("invalid {what} id: {e}"))
}

fn parse_status_filter(token: &str) -> Option<MeetingStatus> {
    match token.trim().to_ascii_lowercase().as_str() {
        "recording" => Some(MeetingStatus::Recording),
        "processing" => Some(MeetingStatus::Processing),
        "transcribing" => Some(MeetingStatus::Transcribing),
        "summarizing" => Some(MeetingStatus::Summarizing),
        "ready" => Some(MeetingStatus::Ready),
        "failed" => Some(MeetingStatus::Failed),
        _ => None,
    }
}

/// List meetings newest first, optionally filtered by lifecycle `status` and/or
/// a title substring `query`. Unknown `status` tokens are ignored (treated as
/// no filter). `limit` defaults to 200.
#[tauri::command]
pub fn list_meetings(
    state: State<'_, AppState>,
    status: Option<String>,
    query: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Meeting>, String> {
    let status = status.as_deref().and_then(parse_status_filter);
    let limit = limit.unwrap_or(200);
    with_store(&state, |s| {
        s.list_meetings_filtered(status, query.as_deref(), limit)
            .map_err(|e| e.to_string())
    })
}

/// Read one meeting with its speakers, `seq`-ordered segments, and summaries.
#[tauri::command]
pub fn get_meeting_detail(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<MeetingDetail, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    with_store(&state, |s| {
        s.get_meeting_detail(id).map_err(|e| e.to_string())
    })?
    .ok_or_else(|| "meeting not found".to_string())
}

/// Rename a speaker cluster (Speaker 3 → 李明). Returns `true` if updated.
#[tauri::command]
pub fn rename_speaker(
    state: State<'_, AppState>,
    speaker_id: String,
    display_name: String,
) -> Result<bool, String> {
    let id = parse_id(&speaker_id, "speaker")?;
    with_store(&state, |s| {
        s.rename_speaker(id, display_name.trim())
            .map_err(|e| e.to_string())
    })
}

/// Reassign a single mis-attributed segment to another speaker.
#[tauri::command]
pub fn reassign_segment_speaker(
    state: State<'_, AppState>,
    segment_id: String,
    speaker_id: String,
) -> Result<bool, String> {
    let seg = parse_id(&segment_id, "segment")?;
    let spk = parse_id(&speaker_id, "speaker")?;
    with_store(&state, |s| {
        s.reassign_segment_speaker(seg, spk)
            .map_err(|e| e.to_string())
    })
}

/// Merge two speaker clusters (they are the same person): `from` is folded into
/// `into` and deleted. Returns the number of segments moved.
#[tauri::command]
pub fn merge_speakers(
    state: State<'_, AppState>,
    meeting_id: String,
    from_speaker_id: String,
    into_speaker_id: String,
) -> Result<u64, String> {
    let mid = parse_id(&meeting_id, "meeting")?;
    let from = parse_id(&from_speaker_id, "from speaker")?;
    let into = parse_id(&into_speaker_id, "into speaker")?;
    with_store(&state, |s| {
        s.merge_speakers(mid, from, into).map_err(|e| e.to_string())
    })
}

/// Export a meeting into one of the four presets: `minutes_md`, `transcript_md`,
/// `subtitles_srt`, `data_json`. Returns the filename + text content.
#[tauri::command]
pub fn export_meeting(
    state: State<'_, AppState>,
    meeting_id: String,
    preset: String,
) -> Result<ExportOutput, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    let preset =
        ExportPreset::parse(&preset).ok_or_else(|| format!("unknown export preset: {preset}"))?;
    let detail = with_store(&state, |s| {
        s.get_meeting_detail(id).map_err(|e| e.to_string())
    })?
    .ok_or_else(|| "meeting not found".to_string())?;
    render_export(&detail, preset).map_err(|e| e.to_string())
}
