//! Tauri IPC for meeting recording (Stage M3).
//!
//! These commands drive the **independent** continuous recorder
//! (`lumen_asr::MeetingRecorder`) — they never touch the dictation
//! `AudioCapture` / hold-to-talk path. Starting a meeting acquires the
//! [`CaptureArbiter`], which suspends the dictation global hotkey; stopping it
//! restores the hotkey and moves the meeting to `Processing`.
//!
//! M3 stops at `Processing`: the offline diarize+transcribe pipeline (M2b)
//! needs the model and is wired up in a later stage.

use crate::mode_arbiter::HotkeyAction;
use crate::AppState;
use lumen_core::{Meeting, MeetingStatus};
use lumen_platform::default_data_dir;
use serde::Serialize;
use tauri::{AppHandle, State};
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
/// returns the recording summary. Does **not** run transcription (that needs the
/// model and lands in a later stage).
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
