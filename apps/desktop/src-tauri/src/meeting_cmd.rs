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
use lumen_core::{Meeting, MeetingDetail, MeetingStatus, MeetingSummary, SummaryKind};
use lumen_dictionary::split_for_injection;
use lumen_meeting::{
    export_meeting as render_export, process_meeting, CorrectionDict, DiarModels, ExportOutput,
    ExportPreset, MeetingOptions, MinutesConfig, DEFAULT_MAX_SPEAKERS,
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
        let reason = format!("could not create meetings dir: {e}");
        let _ = with_store(&state, |s| {
            s.fail_meeting(meeting_id, Some(&reason))
                .map_err(|e| e.to_string())
        });
        return Err(reason);
    }
    let out_path = dir.join(format!("{meeting_id}.wav"));

    // 4. Start the independent continuous recorder. When the real-time layer is
    //    engaged (macOS + streaming Paraformer installed), attach an audio
    //    fan-out so the streaming worker can transcribe live; otherwise record
    //    plainly (no sink → zero extra work in the audio callback).
    let device = preferred_device(&state);
    let streaming_dir = crate::meeting_live::streaming_dir_if_ready();
    let (sample_sink, sample_rx) = if streaming_dir.is_some() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let sample_rate = match state
        .meeting_recorder
        .start_with_sink(device, out_path, sample_sink)
    {
        Ok(rate) => rate,
        Err(e) => {
            // Roll back: mark failed and release the arbiter. No hotkey suspend
            // was applied yet, so nothing to restore. `sample_rx` drops here,
            // so no worker is left dangling.
            state.capture.force_idle();
            let reason = format!("could not start recording: {e}");
            let _ = with_store(&state, |s| {
                s.fail_meeting(meeting_id, Some(&reason))
                    .map_err(|e| e.to_string())
            });
            return Err(reason);
        }
    };

    // 5. Optional second track: system audio output (remote participants) via
    //    the macOS Core Audio process tap. Strictly best-effort and
    //    capability-gated — if the config opts out, the OS is too old
    //    (< 14.2), the permission is denied, or anything else fails, the
    //    session logs a warning and the meeting records mic-only exactly as
    //    before. The mic path above is already live and is never touched.
    let system_audio_enabled = state
        .config
        .lock()
        .map(|cfg| cfg.meeting.system_audio)
        .unwrap_or(true);
    if system_audio_enabled {
        let system_path = dir.join(format!("{meeting_id}.system.wav"));
        if state
            .meeting_system_audio
            .start(system_path.clone())
            .is_some()
        {
            // Record the path up front so crash recovery can salvage the
            // system track too. Best-effort: a store failure only loses the
            // system track, never the recording.
            let system_path = system_path.to_string_lossy().to_string();
            if let Err(e) = with_store(&state, |s| {
                s.set_meeting_system_audio_path(meeting_id, Some(&system_path))
                    .map_err(|e| e.to_string())
            }) {
                tracing::warn!(meeting_id = %meeting_id, error = %e, "could not record system audio path");
            }
        }
    }

    // 6. Recording is live — spawn the live-transcript worker (if streaming) and
    //    suspend the dictation hotkey. A live-worker failure never fails the
    //    recording: the worker itself degrades to "no live text" on any error.
    if let (Some(dir), Some(rx)) = (streaming_dir, sample_rx) {
        state.meeting_live.start(app.clone(), rx, sample_rate, dir);
    }
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

    // Stop the system-audio track (no-op when none is running): tears down the
    // tap and finalizes the second WAV. Best-effort — `None` here just means
    // the meeting is mic-only.
    let system_summary = state.meeting_system_audio.stop();

    // Stop the real-time worker (no-op if none was running). The recorder stop
    // above already dropped the audio fan-out sender, which ends the worker's
    // loop; this joins it (flushing the last live segment) so it never outlives
    // the recording. The authoritative transcript comes from the offline
    // pipeline below, which replaces the live preview.
    state.meeting_live.stop();

    // Unconditionally release the exclusive mode and restore the dictation
    // hotkey. If we bailed out on the `stop()` error above, `end_meeting` and
    // `resume` would never run — the app would be stuck in MeetingRecording with
    // the dictation hotkey unregistered forever.
    match state.capture.end_meeting() {
        Ok(action) => apply_hotkey_action(&app, action),
        Err(e) => tracing::warn!(error = %e, "arbiter end_meeting on stop"),
    }

    // Same finally semantics for meeting detection: whether the recorder stop
    // below succeeded or failed, the recording is over, so the policy must
    // leave `recording` — otherwise a stop failure would strand it there and
    // every future candidate would be rejected as busy. Harmless when this
    // recording was not started from a detection prompt (the policy no-ops
    // unless it is tracking an accepted recording). Also retracts a
    // still-visible "meeting seems over" suggestion, since the question is
    // now moot however the stop was initiated.
    state.meeting_detection.recording_finished(&app);

    // Now that the mic and hotkey are restored, surface any recorder failure.
    // Mark the meeting Failed so it does not linger in `Recording`.
    let summary = match stop_result {
        Ok(summary) => summary,
        Err(e) => {
            let reason = format!("recorder stop failed: {e}");
            let _ = with_store(&state, |s| {
                s.fail_meeting(id, Some(&reason)).map_err(|e| e.to_string())
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

    // Reconcile the system track: keep it only when it finalized with real
    // audio; an empty or failed track is removed and its path cleared so the
    // meeting reads back as plain mic-only.
    let system_wav = match system_summary {
        Some(system) if system.duration_seconds > 0.0 => Some(system.wav_path),
        other => {
            if let Some(system) = other {
                let _ = std::fs::remove_file(&system.wav_path);
            }
            let _ = with_store(&state, |s| {
                s.set_meeting_system_audio_path(id, None)
                    .map_err(|e| e.to_string())
            });
            None
        }
    };

    tracing::info!(
        meeting_id = %id,
        duration_seconds = summary.duration_seconds,
        dual_track = system_wav.is_some(),
        "meeting recording stopped → processing"
    );

    // Kick off transcription in the background so the stop command returns now.
    spawn_meeting_processing(app, id, summary.wav_path.clone(), system_wav);

    Ok(MeetingRecordingDto {
        id: meeting_id,
        audio_path,
        duration_seconds: summary.duration_seconds,
        sample_rate: summary.sample_rate,
        status: MeetingStatus::Processing.as_str().to_string(),
    })
}

/// Serialized meeting-detection status for the settings toggle.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingDetectionStatus {
    /// The user's opt-in preference (persisted).
    pub enabled: bool,
    /// Whether this OS exposes the audio-activity capability at all. When
    /// `false`, the toggle can explain the feature is unavailable here.
    pub capability_available: bool,
    /// Whether the detector poller is currently running.
    pub active: bool,
}

/// Read the meeting-detection preference plus runtime capability/active state.
#[tauri::command]
pub fn get_meeting_detection(state: State<'_, AppState>) -> Result<MeetingDetectionStatus, String> {
    let enabled = {
        let cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        cfg.meeting.detection_enabled
    };
    Ok(MeetingDetectionStatus {
        enabled,
        capability_available: meeting_detection_capability(),
        active: state.meeting_detection.is_active(),
    })
}

/// Toggle the opt-in meeting-detection preference. Persists it and starts/stops
/// the detector to match (starting only ever succeeds when the OS capability is
/// present). Returns the resulting status.
#[tauri::command]
pub fn set_meeting_detection_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<MeetingDetectionStatus, String> {
    {
        let mut cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        cfg.meeting.detection_enabled = enabled;
        cfg.save()?;
    }
    if enabled {
        state.meeting_detection.start(app);
    } else {
        // Stop the poller AND reset the policy: if a prompt is currently on
        // screen this emits `meeting-detection-cancelled` so the front-end
        // retracts it instead of leaving a stale prompt behind.
        state.meeting_detection.stop_and_reset(&app);
    }
    get_meeting_detection(state)
}

/// The user accepted a detection prompt: advance the policy and, if it says so,
/// start a meeting recording via the *existing* start path. Returns the new
/// meeting id, or an empty string if no recording was started (e.g. the prompt
/// was already stale).
#[tauri::command]
pub fn accept_meeting_detection(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    if !state.meeting_detection.accept() {
        return Ok(String::new());
    }
    // Reuse the proven start command (arbiter gate, hotkey suspend, recorder).
    // If it fails, tell the policy so it does not sit in `recording` forever
    // (which would reject every future candidate as busy).
    match start_meeting_recording(app.clone(), state.clone(), None) {
        Ok(id) => {
            // Remember which meeting this detection started so the
            // end-of-meeting stop suggestion can reference (and stop) it.
            state.meeting_detection.mark_recording_started(&id);
            Ok(id)
        }
        Err(e) => {
            state.meeting_detection.recording_failed(&app);
            Err(e)
        }
    }
}

/// The user dismissed a detection prompt (arms the per-app cooldown).
#[tauri::command]
pub fn dismiss_meeting_detection(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.meeting_detection.dismiss(&app);
    Ok(())
}

/// The user accepted an end-of-meeting stop suggestion: stop the
/// detection-started recording via the *existing* stop path (recorder
/// finalize, hotkey restore, offline pipeline). A stale click — the recording
/// already ended some other way — is a silent no-op, never an error toast.
#[tauri::command]
pub fn accept_meeting_detection_stop(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let Some(meeting_id) = state.meeting_detection.active_meeting_id() else {
        return Ok(());
    };
    state.meeting_detection.note_stop_accepted();
    stop_meeting_recording(app, state, meeting_id).map(|_| ())
}

/// The user declined an end-of-meeting stop suggestion ("继续录制"): the
/// recording keeps running and no further suggestion is made for it.
#[tauri::command]
pub fn decline_meeting_detection_stop(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.meeting_detection.decline_stop(&app);
    Ok(())
}

/// Serialized local detection counters (see `detection_stats.rs`). Everything
/// is counted and stored on this machine only.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingDetectionStatsDto {
    pub prompt_shown: u64,
    pub prompt_accepted: u64,
    pub prompt_dismissed: u64,
    pub stop_suggested: u64,
    pub stop_accepted: u64,
    pub stop_declined: u64,
}

/// Read the local meeting-detection counters (prompt/suggestion totals) so a
/// settings page can show how often detection fired and how often it was right.
#[tauri::command]
pub fn get_meeting_detection_stats(
    state: State<'_, AppState>,
) -> Result<MeetingDetectionStatsDto, String> {
    let c = state.meeting_detection.stats_snapshot();
    Ok(MeetingDetectionStatsDto {
        prompt_shown: c.prompt_shown,
        prompt_accepted: c.prompt_accepted,
        prompt_dismissed: c.prompt_dismissed,
        stop_suggested: c.stop_suggested,
        stop_accepted: c.stop_accepted,
        stop_declined: c.stop_declined,
    })
}

fn meeting_detection_capability() -> bool {
    #[cfg(target_os = "macos")]
    {
        lumen_platform_macos::meeting_detection_capability_available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Pause the active meeting recording. Paused audio is dropped (no silent gap).
#[tauri::command]
pub fn pause_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    state.meeting_recorder.pause().map_err(|e| e.to_string())?;
    // Keep the system track's timeline in lockstep with the mic's.
    state.meeting_system_audio.set_paused(true);
    Ok(())
}

/// Resume a paused meeting recording.
#[tauri::command]
pub fn resume_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    state.meeting_recorder.resume().map_err(|e| e.to_string())?;
    state.meeting_system_audio.set_paused(false);
    Ok(())
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
    let (audio_path, system_audio_path) = with_store(&state, |s| {
        let meeting = s
            .get_meeting(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "meeting not found".to_string())?;
        let audio = meeting
            .audio_path
            .ok_or_else(|| "meeting has no recorded audio".to_string())?;
        Ok((audio, meeting.system_audio_path))
    })?;
    // The system track is optional even when recorded: reprocess with it only
    // if the file is still on disk.
    let system_wav = system_audio_path.map(PathBuf::from).filter(|p| p.exists());
    spawn_meeting_processing(app, id, PathBuf::from(audio_path), system_wav);
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
fn spawn_meeting_processing(
    app: AppHandle,
    meeting_id: Uuid,
    wav: PathBuf,
    system_wav: Option<PathBuf>,
) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::warn!(meeting_id = %meeting_id, error = %e, "could not build meeting runtime");
                if let Err(e) =
                    mark_meeting_failed(meeting_id, Some("could not start meeting runtime"))
                {
                    tracing::warn!(meeting_id = %meeting_id, error = %e, "could not mark meeting failed");
                }
                return;
            }
        };
        runtime.block_on(async move {
            tracing::info!(meeting_id = %meeting_id, "meeting processing started");
            match process_meeting_pipeline(&app, meeting_id, &wav, system_wav.as_deref()).await {
                Ok(()) => tracing::info!(meeting_id = %meeting_id, "meeting processing finished"),
                Err(reason) => {
                    tracing::warn!(meeting_id = %meeting_id, error = %reason, "meeting processing failed");
                    // `process_meeting` records the reason for in-pipeline
                    // failures itself; this covers the pre-flight failures
                    // (engine/model/config build) that never reach it.
                    if let Err(e) = mark_meeting_failed(meeting_id, Some(&reason)) {
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

/// Upper bound on dictionary **terms** fed to the post-ASR correction pass. The
/// per-segment fuzzy scan is O(terms × text), so cap the list to keep correction
/// bounded on large dictionaries. `list_dictionary` returns most-recently-updated
/// first, so the cap keeps the freshest terms.
const MAX_MEETING_HOTWORDS: usize = 128;

/// Build the meeting's post-ASR correction view from the user's confirmed
/// dictionary (meeting "hotword" strategy A). Terms (names/jargon) drive the
/// fuzzy near-miss correction; replacement pairs (`from -> to`) are applied
/// verbatim. Replaces the old "forward terms to the ASR engine as hotwords"
/// behaviour, which was effectively a no-op on sherpa's offline Paraformer.
/// A store read error yields an empty dict so the meeting still runs (uncorrected).
fn meeting_correction_dict(store: &Store) -> CorrectionDict {
    let entries = store.list_dictionary().unwrap_or_default();
    let (mut terms, replacements) = split_for_injection(&entries);
    terms.truncate(MAX_MEETING_HOTWORDS);
    CorrectionDict::new(terms, replacements)
}

/// Build the pipeline's dependencies from app config and run
/// [`process_meeting`]. Errors are surfaced to [`spawn_meeting_processing`] which
/// logs them and ensures the meeting ends `failed`.
async fn process_meeting_pipeline(
    app: &AppHandle,
    meeting_id: Uuid,
    wav: &Path,
    system_wav: Option<&Path>,
) -> Result<(), String> {
    // A dedicated SQLite connection for the background worker. `process_meeting`
    // holds `&Store` across many `.await`s, which the UI's `std::sync::Mutex`
    // guard cannot span; a second connection (WAL + busy_timeout, see
    // `Store::open`) writes safely without contending the UI store lock.
    let store = Store::open(default_db_path()).map_err(|e| format!("open store: {e}"))?;

    // Build the ASR engine and (optional) minutes corrector from the user's
    // settings under brief locks, then drop the app-state handle before the long
    // async run below.
    let (asr_engine, corrector, minutes_model, cleanup_transcript) = {
        let state = app.state::<AppState>();
        let (corrector_cfg, cleanup_transcript) = {
            let cfg = state
                .config
                .lock()
                .map_err(|_| "config lock poisoned".to_string())?;
            (cfg.corrector.clone(), cfg.meeting.transcript_cleanup)
        };
        let asr_engine = crate::dictation::build_meeting_asr_engine(state.inner())?;
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
        (asr_engine, corrector, minutes_model, cleanup_transcript)
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

    // Feed the user's personal dictionary into the post-ASR correction pass
    // (meeting "hotword" strategy A): after each turn is transcribed, near-miss
    // mis-recognitions of the user's names/jargon are repaired in the transcript
    // (and thus in the minutes). Read from the background worker's own store
    // connection; a read failure just yields no correction rather than aborting.
    // The batched LLM transcript-cleanup pass runs only when the user opted in
    // AND an LLM corrector is available (no corrector → the pass is skipped inside
    // `process_meeting` regardless of this flag).
    let opts = MeetingOptions {
        max_speakers: Some(DEFAULT_MAX_SPEAKERS),
        correction: meeting_correction_dict(&store),
        cleanup_transcript,
        // Cross-meeting auto-identification: match diarized speakers against
        // the local voiceprint library and auto-assign enrolled names. The
        // library lives entirely on this machine (never uploaded); on builds
        // without diarization no embeddings exist, so this is naturally inert.
        identity_dir: Some(lumen_identity::default_identity_dir()),
        ..MeetingOptions::default()
    };

    // When no LLM is configured the minutes step is skipped (transcript-only →
    // ready). Remember that so we can leave a marker for the UI to prompt the
    // user to configure an LLM, instead of silently showing an empty 纪要 page.
    let no_llm = minutes_cfg.is_none();

    process_meeting(
        &store,
        meeting_id,
        wav,
        system_wav,
        &diar_models,
        asr_engine.as_ref(),
        minutes_cfg.as_ref(),
        &opts,
    )
    .await
    .map_err(|e| e.to_string())?;

    if no_llm {
        // Sentinel summary (kind=summary) the 纪要 page detects to show
        // "未配置 LLM，配置后可自动生成会议纪要" rather than a bare "暂无纪要".
        // Only written on the success path — a failed pipeline never reaches
        // here, so a failed meeting is not mistaken for a no-LLM one.
        let marker = MeetingSummary::new(
            meeting_id,
            SummaryKind::Summary,
            r#"{"skipped_no_llm":true}"#,
        );
        store
            .save_summary(&marker)
            .map_err(|e| format!("save no-llm marker: {e}"))?;
    }
    Ok(())
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

/// Reason recorded for an interrupted meeting we cannot salvage on launch.
const INTERRUPTED_NO_AUDIO: &str = "recording interrupted, no audio";

/// Recover meetings a previous run left mid-recording after a crash.
///
/// A meeting is written to storage as `Recording` the moment capture starts and
/// only advances when the `stop` command runs. If the app is killed mid-capture
/// the WAV's PCM samples are already on disk but its header length fields were
/// never patched (that happens on the recorder's finalize) and the row is stuck
/// at `Recording`. On the next launch we scan for those rows and, for each one:
///
/// - **Salvageable audio** (file exists with PCM data): back-fill the WAV header
///   the crash skipped, record the recovered duration + path (status →
///   `Processing`), and hand it to the same background transcription pipeline the
///   `stop` command uses. The already-captured audio (the first half of the
///   meeting) is preserved instead of lost.
/// - **No audio** (no path, a missing/empty file, or a header-only WAV): mark it
///   `failed` with a clear reason. We *keep* the row rather than delete it so the
///   interruption stays visible in the library instead of a meeting the user
///   started silently vanishing.
///
/// Runs on its own thread so it never blocks startup / the UI, and uses its own
/// SQLite connection (never the UI store lock), matching the background worker.
/// Platform gating is inherited from the pipeline: on non-macOS (or with the
/// diar models absent) the spawned `process_meeting` reports `Unsupported` and
/// the meeting ends `failed` — the same path a normal recording takes there.
pub fn recover_interrupted_meetings(app: AppHandle) {
    std::thread::spawn(move || {
        let store = match Store::open(default_db_path()) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(error = %e, "crash recovery: could not open store");
                return;
            }
        };
        let stale = match store.list_meetings_by_status(MeetingStatus::Recording) {
            Ok(stale) => stale,
            Err(e) => {
                tracing::warn!(error = %e, "crash recovery: could not list interrupted meetings");
                return;
            }
        };
        if stale.is_empty() {
            return;
        }
        tracing::info!(
            count = stale.len(),
            "crash recovery: found interrupted meeting(s) from a previous run"
        );
        for meeting in stale {
            recover_one_meeting(&app, &store, &meeting);
        }
    });
}

/// Recover a single interrupted meeting. See [`recover_interrupted_meetings`].
fn recover_one_meeting(app: &AppHandle, store: &Store, meeting: &Meeting) {
    let id = meeting.id;
    let Some(audio_path) = meeting.audio_path.clone() else {
        // Crashed before the stop path ever recorded an audio path.
        fail_interrupted(store, id);
        return;
    };
    let wav = PathBuf::from(&audio_path);
    // A missing or truly empty (0-byte) file has nothing to salvage.
    let has_bytes = std::fs::metadata(&wav)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !has_bytes {
        fail_interrupted(store, id);
        return;
    }

    match lumen_asr::repair_wav_header(&wav) {
        // Real PCM data recovered → advance to Processing and transcribe it.
        Ok(repaired) if repaired.data_bytes > 0 => {
            if let Err(e) = store.set_meeting_audio(
                id,
                &audio_path,
                repaired.duration_seconds,
                MeetingStatus::Processing,
            ) {
                tracing::warn!(meeting_id = %id, error = %e, "crash recovery: could not update recovered meeting");
                return;
            }
            let system_wav = recover_system_track(store, meeting);
            tracing::info!(
                meeting_id = %id,
                duration_seconds = repaired.duration_seconds,
                dual_track = system_wav.is_some(),
                "crash recovery: salvaged recording, header repaired → processing"
            );
            spawn_meeting_processing(app.clone(), id, wav, system_wav);
        }
        // Header-only WAV: repaired to a valid 0-length take — no audio to keep.
        Ok(_) => fail_interrupted(store, id),
        Err(e) => {
            // File exists but is not a repairable WAV (truncated / corrupt).
            let reason = format!("recording interrupted, audio unrecoverable: {e}");
            tracing::warn!(meeting_id = %id, error = %e, "crash recovery: wav unrepairable");
            if let Err(e) = store.fail_meeting(id, Some(&reason)) {
                tracing::warn!(meeting_id = %id, error = %e, "crash recovery: could not mark failed");
            }
        }
    }
}

/// Best-effort crash recovery for a salvaged meeting's optional **system**
/// track: repair its WAV header the same way as the mic track's. Any problem
/// (no path, missing/empty file, unrepairable header) clears the stored path
/// and drops the track — the meeting still recovers mic-only, mirroring the
/// live degrade contract.
fn recover_system_track(store: &Store, meeting: &Meeting) -> Option<PathBuf> {
    let path = meeting.system_audio_path.clone()?;
    let wav = PathBuf::from(&path);
    let recovered = match lumen_asr::repair_wav_header(&wav) {
        Ok(repaired) if repaired.data_bytes > 0 => Some(wav),
        Ok(_) => {
            // Header-only: nothing captured; remove the empty shell.
            let _ = std::fs::remove_file(&wav);
            None
        }
        Err(e) => {
            tracing::warn!(
                meeting_id = %meeting.id,
                path = %wav.display(),
                error = %e,
                "crash recovery: system track unrepairable, continuing mic-only"
            );
            None
        }
    };
    if recovered.is_none() {
        if let Err(e) = store.set_meeting_system_audio_path(meeting.id, None) {
            tracing::warn!(meeting_id = %meeting.id, error = %e, "crash recovery: could not clear system audio path");
        }
    }
    recovered
}

/// Mark an interrupted meeting `failed` with the standard "no audio" reason.
fn fail_interrupted(store: &Store, id: Uuid) {
    tracing::info!(meeting_id = %id, "crash recovery: no salvageable audio → failed");
    if let Err(e) = store.fail_meeting(id, Some(INTERRUPTED_NO_AUDIO)) {
        tracing::warn!(meeting_id = %id, error = %e, "crash recovery: could not mark failed");
    }
}

/// Mark a meeting `failed` (with an optional reason) from the background
/// worker, on its own connection (never touches the UI store lock).
fn mark_meeting_failed(meeting_id: Uuid, reason: Option<&str>) -> Result<(), String> {
    let store = Store::open(default_db_path()).map_err(|e| e.to_string())?;
    store
        .fail_meeting(meeting_id, reason)
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

/// Save (overwrite) the user's free-form notes for a meeting. The front-end
/// debounces writes; the backend does a plain last-write-wins update of the
/// `notes` column. Returns `true` if the meeting row was updated. These notes
/// are later fused into the minutes LLM pass as extra context.
#[tauri::command]
pub fn save_meeting_notes(
    state: State<'_, AppState>,
    meeting_id: String,
    notes: String,
) -> Result<bool, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    with_store(&state, |s| {
        s.set_meeting_notes(id, &notes).map_err(|e| e.to_string())
    })
}

/// Rename a meeting (edit its title). A blank title clears back to untitled so
/// the library shows the "未命名会议" fallback. Returns `true` if the meeting row
/// was updated.
#[tauri::command]
pub fn rename_meeting(
    state: State<'_, AppState>,
    meeting_id: String,
    title: String,
) -> Result<bool, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    with_store(&state, |s| {
        s.set_meeting_title(id, &title).map_err(|e| e.to_string())
    })
}

/// Delete a meeting and everything attached to it. The store cascade removes the
/// segments, speakers, and summaries; this command additionally deletes the
/// meeting's recorded WAV from disk (best-effort — a missing file is fine, and a
/// remove error is logged but does not fail the delete, since the row is already
/// gone). Returns `true` if a meeting row was deleted.
#[tauri::command]
pub fn delete_meeting(state: State<'_, AppState>, meeting_id: String) -> Result<bool, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    // Read the audio paths (mic + optional system track) before deleting the
    // row so we know which WAVs to remove.
    let (audio_path, system_audio_path) = with_store(&state, |s| {
        let meeting = s.get_meeting(id).map_err(|e| e.to_string())?;
        Ok((
            meeting.as_ref().and_then(|m| m.audio_path.clone()),
            meeting.and_then(|m| m.system_audio_path),
        ))
    })?;
    let deleted = with_store(&state, |s| s.delete_meeting(id).map_err(|e| e.to_string()))?;
    if deleted {
        for path in [audio_path, system_audio_path].into_iter().flatten() {
            let wav = Path::new(&path);
            if wav.exists() {
                if let Err(e) = std::fs::remove_file(wav) {
                    tracing::warn!(
                        meeting_id = %id,
                        path = %wav.display(),
                        error = %e,
                        "could not delete meeting audio file"
                    );
                }
            }
        }
    }
    Ok(deleted)
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

/// Edit the text of one transcript segment (manual correction from the review
/// page). Only the words change — the segment's timing and speaker attribution
/// are left untouched. Returns `true` if the segment row was updated.
#[tauri::command]
pub fn edit_meeting_segment(
    state: State<'_, AppState>,
    segment_id: String,
    text: String,
) -> Result<bool, String> {
    let id = parse_id(&segment_id, "segment")?;
    with_store(&state, |s| {
        s.update_segment_text(id, &text).map_err(|e| e.to_string())
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

// ----- speaker voiceprint enrollment (M5) --------------------------------
//
// The identity library is a local-only directory of JSON voiceprints
// (`lumen_identity::default_identity_dir()`); nothing here leaves the machine.

/// Serialized enrolled identity for the UI. The embedding itself is
/// deliberately not exposed over IPC — the front-end only needs name/metadata.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrolledSpeakerDto {
    pub id: String,
    pub name: String,
    pub enrolled_at: String,
    pub source_meeting_id: Option<String>,
}

impl From<&lumen_identity::EnrolledIdentity> for EnrolledSpeakerDto {
    fn from(identity: &lumen_identity::EnrolledIdentity) -> Self {
        Self {
            id: identity.id.to_string(),
            name: identity.name.clone(),
            enrolled_at: identity.enrolled_at.to_rfc3339(),
            source_meeting_id: identity.source_meeting_id.map(|id| id.to_string()),
        }
    }
}

/// Per-speaker voiceprint availability for one meeting: whether the diarization
/// pipeline stored a centroid embedding for each speaker (pre-v9 meetings and
/// non-diarized builds have none, so the enroll button can be hidden).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerVoiceprintDto {
    pub speaker_id: String,
    pub has_embedding: bool,
}

fn open_identity_store() -> Result<lumen_identity::IdentityStore, String> {
    lumen_identity::IdentityStore::open(lumen_identity::default_identity_dir())
        .map_err(|e| format!("open identity library: {e}"))
}

/// Enroll one confirmed meeting speaker into the local voiceprint library.
/// The name defaults to the speaker's `display_name`; passing `name` overrides
/// it (and confirms the speaker with that name when it was still unnamed).
/// Fails when the speaker has no stored embedding (meeting transcribed before
/// voiceprints existed → re-run transcription first).
#[tauri::command]
pub fn enroll_speaker(
    state: State<'_, AppState>,
    meeting_id: String,
    speaker_id: String,
    name: Option<String>,
) -> Result<EnrolledSpeakerDto, String> {
    let meeting = parse_id(&meeting_id, "meeting")?;
    let speaker_uuid = parse_id(&speaker_id, "speaker")?;

    let (speaker, embedding) = with_store(&state, |s| {
        let speaker = s
            .list_speakers(meeting)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|sp| sp.id == speaker_uuid)
            .ok_or_else(|| "speaker not found in this meeting".to_string())?;
        let embedding = s
            .get_speaker_embedding(speaker_uuid)
            .map_err(|e| e.to_string())?;
        Ok((speaker, embedding))
    })?;
    let embedding = embedding.ok_or_else(|| {
        "该说话人没有声纹数据（此会议在声纹功能之前转录，重新转录后即可注册）".to_string()
    })?;

    let name = name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| speaker.display_name.clone())
        .ok_or_else(|| "请先为该说话人设置真实姓名再注册声纹".to_string())?;

    // Keep the speaker row consistent when the enroll call supplied the name
    // (e.g. enrolling an unnamed speaker directly). Done *before* the identity
    // write: every fallible step precedes the enrollment persist, so a
    // reported error always means "not enrolled" — the command can no longer
    // fail after the voiceprint is already stored. (A confirmed-but-not-yet-
    // enrolled speaker is a normal state; the user just retries the enroll.)
    if speaker.display_name.as_deref() != Some(name.as_str()) {
        with_store(&state, |s| {
            s.rename_speaker(speaker_uuid, &name)
                .map_err(|e| e.to_string())
        })?;
    }

    let mut identities = open_identity_store()?;
    let enrolled = identities
        .enroll(&name, &embedding, Some(meeting))
        .map_err(|e| format!("enroll: {e}"))?;

    // The enrolled name is PII — log only the ids.
    tracing::info!(meeting_id = %meeting, speaker_id = %speaker_uuid, "speaker voiceprint enrolled");
    Ok(EnrolledSpeakerDto::from(&enrolled))
}

/// List every enrolled identity in the local voiceprint library (name-ordered).
#[tauri::command]
pub fn list_enrolled_speakers() -> Result<Vec<EnrolledSpeakerDto>, String> {
    let identities = open_identity_store()?;
    Ok(identities
        .list()
        .iter()
        .map(EnrolledSpeakerDto::from)
        .collect())
}

/// Remove one enrolled identity (its voiceprint file is deleted from disk).
/// Returns `true` if it existed. Existing meetings keep their display names —
/// removal only stops future auto-identification.
#[tauri::command]
pub fn remove_enrolled_speaker(identity_id: String) -> Result<bool, String> {
    let id = parse_id(&identity_id, "identity")?;
    let mut identities = open_identity_store()?;
    identities.remove(id).map_err(|e| format!("remove: {e}"))
}

/// Report which of a meeting's speakers have a stored voiceprint embedding, so
/// the UI can offer "注册声纹" only where enrollment is actually possible.
#[tauri::command]
pub fn get_meeting_voiceprints(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<Vec<SpeakerVoiceprintDto>, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    with_store(&state, |s| {
        let speakers = s.list_speakers(id).map_err(|e| e.to_string())?;
        speakers
            .iter()
            .map(|speaker| {
                let has_embedding = s
                    .get_speaker_embedding(speaker.id)
                    .map_err(|e| e.to_string())?
                    .is_some();
                Ok(SpeakerVoiceprintDto {
                    speaker_id: speaker.id.to_string(),
                    has_embedding,
                })
            })
            .collect()
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
