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
use lumen_asr::{copy_pcm16_wav_range, live_tap_channel, WavRangeError, LIVE_TAP_CAPACITY};
use lumen_core::{
    LiveAnnotation, Meeting, MeetingDetail, MeetingStatus, MeetingSummary, SegmentChannel,
    SummaryKind,
};
use lumen_dictionary::split_for_injection;
use lumen_meeting::{
    export_meeting as render_export, process_meeting, CorrectionDict, DiarModels, ExportOutput,
    ExportPreset, MeetingOptions, MinutesConfig, ProcessingProgress, DEFAULT_MAX_SPEAKERS,
};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::Store;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

/// Unified-timeline sidecar written next to a meeting's WAVs as
/// `<meeting-id>.timeline.json`. Records where each track's WAV begins
/// relative to the meeting's shared `t0` (the `Instant` taken at recording
/// start) plus `t0`'s wall-clock, so downstream passes can line the two
/// tracks up without guessing.
///
/// The offsets are measured when each capture reports "started" — close, but
/// not sample-exact. Later work (echo suppression between the tracks,
/// aligning live-caption annotations with the offline transcript) reads this
/// sidecar and can tighten the alignment; the format stays additive metadata,
/// never a DB schema change.
#[derive(Debug, Deserialize, Serialize)]
struct MeetingTimeline {
    /// Seconds from `t0` until the mic recorder was capturing.
    mic_offset_seconds: f64,
    /// Seconds from `t0` until the system-audio tap was capturing; absent on
    /// mic-only meetings.
    #[serde(skip_serializing_if = "Option::is_none")]
    system_offset_seconds: Option<f64>,
    /// RFC 3339 wall-clock timestamp of `t0`.
    t0_wall_clock: String,
}

/// Read the per-track recording offsets used to map the optional system WAV
/// onto the mic player's timeline. Missing/old/corrupt metadata fails open to
/// the near-common-start assumption used elsewhere in the meeting pipeline.
fn read_timeline_offsets(path: &Path) -> (f64, Option<f64>) {
    let Ok(json) = std::fs::read_to_string(path) else {
        return (0.0, None);
    };
    let Ok(timeline) = serde_json::from_str::<MeetingTimeline>(&json) else {
        return (0.0, None);
    };
    let finite = |seconds: f64| seconds.is_finite().then_some(seconds);
    (
        finite(timeline.mic_offset_seconds).unwrap_or(0.0),
        timeline.system_offset_seconds.and_then(finite),
    )
}

/// Map a range selected on the mic player's timeline into system-WAV local
/// time. The third value is the system track's new positive start skew after
/// both tracks are cropped. `None` means the system track begins after the
/// entire kept interval and therefore contributes no audio.
fn system_trim_range(start: f64, end: f64, system_skew: f64) -> Option<(f64, f64, f64)> {
    let local_start = (start - system_skew).max(0.0);
    let local_end = end - system_skew;
    (local_end > local_start).then(|| {
        let new_skew = (local_start + system_skew - start).max(0.0);
        (local_start, local_end, new_skew)
    })
}

fn echo_diagnostics_sidecar(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("meeting");
    path.with_file_name(format!("{stem}.echo_suppression.json"))
}

/// Remove a mic WAV and the two sidecars derived from that exact recording.
/// Every path is explicit; callers validate that the WAV is app-owned first.
fn remove_mic_audio_artifacts(path: &Path) {
    for artifact in [
        path.to_path_buf(),
        path.with_extension("timeline.json"),
        echo_diagnostics_sidecar(path),
    ] {
        if artifact.exists() {
            if let Err(error) = std::fs::remove_file(&artifact) {
                tracing::warn!(path = %artifact.display(), %error, "could not remove replaced meeting artifact");
            }
        }
    }
}

fn remove_audio_file(path: &Path) {
    if path.exists() {
        if let Err(error) = std::fs::remove_file(path) {
            tracing::warn!(path = %path.display(), %error, "could not remove replaced meeting audio");
        }
    }
}

/// Resolve a stored meeting audio path and prove it is a direct WAV child of
/// Lumen's meetings directory before any destructive replacement is allowed.
fn owned_meeting_wav(path: &str) -> Result<PathBuf, String> {
    let meetings_dir = default_data_dir().join("meetings");
    owned_meeting_wav_in(&meetings_dir, Path::new(path))
}

fn owned_meeting_wav_in(meetings_dir: &Path, path: &Path) -> Result<PathBuf, String> {
    let owned_dir = meetings_dir
        .canonicalize()
        .map_err(|error| format!("无法读取会议音频目录：{error}"))?;
    let source = path
        .canonicalize()
        .map_err(|error| format!("无法读取会议音频：{error}"))?;
    if source.parent() != Some(owned_dir.as_path())
        || source.extension().and_then(|value| value.to_str()) != Some("wav")
        || !source.is_file()
    {
        return Err("会议音频不在 Lumen 管理的目录中，不能执行破坏性剪辑".to_string());
    }
    Ok(source)
}

/// Best-effort sidecar write: a failure only costs the alignment metadata,
/// never the recording.
fn write_timeline_sidecar(path: &Path, timeline: &MeetingTimeline) {
    let json = match serde_json::to_string_pretty(timeline) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "could not serialize meeting timeline sidecar");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, json) {
        tracing::warn!(path = %path.display(), error = %e, "could not write meeting timeline sidecar");
    }
}

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
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingRecordingDto {
    pub id: String,
    pub audio_path: String,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub status: String,
}

/// Ownership record for the process-global meeting capture engines.
///
/// The enclosing mutex is deliberately held for the whole start/stop command,
/// not only while changing this value: releasing the capture arbiter midway
/// through stop must not let a new start race with cleanup of the old global
/// recorder, watchdog, or power guard.
#[derive(Debug, Default)]
pub struct MeetingRecordingOwner {
    active_id: Option<Uuid>,
    last_completed: Option<MeetingRecordingDto>,
}

impl MeetingRecordingOwner {
    fn ensure_startable(&self) -> Result<(), String> {
        if let Some(active_id) = self.active_id {
            Err(format!("meeting {active_id} is already recording"))
        } else {
            Ok(())
        }
    }

    fn started(&mut self, id: Uuid) {
        self.active_id = Some(id);
    }

    /// `Ok(Some(_))` is an idempotent replay of the most recently completed
    /// stop. `Ok(None)` authorizes stopping the active global recorders.
    fn authorize_stop(&self, id: Uuid) -> Result<Option<MeetingRecordingDto>, String> {
        if self.active_id == Some(id) {
            return Ok(None);
        }
        if let Some(completed) = self
            .last_completed
            .as_ref()
            .filter(|completed| completed.id == id.to_string())
        {
            return Ok(Some(completed.clone()));
        }
        match self.active_id {
            Some(active_id) => Err(format!(
                "meeting {id} is not active; current recording is {active_id}"
            )),
            None => Err(format!("meeting {id} is not recording")),
        }
    }

    fn stopped_without_summary(&mut self, id: Uuid) {
        if self.active_id == Some(id) {
            self.active_id = None;
        }
    }

    fn completed(&mut self, id: Uuid, summary: MeetingRecordingDto) {
        self.stopped_without_summary(id);
        self.last_completed = Some(summary);
    }
}

/// Battery percent at or below which a meeting recording on battery power is
/// warned about (the machine may sleep / power off and cut the recording).
const LOW_BATTERY_PERCENT: u8 = 20;

/// How often the low-battery poll thread re-checks the battery while a meeting
/// records.
const BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Payload of the `meeting-power-warning` event the front-end listens for to
/// warn that a recording may be interrupted by power loss or system sleep.
/// `kind` is `"low-battery"` or `"will-sleep"`; `percent` is the battery level
/// for a low-battery warning and absent for a will-sleep warning.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingPowerWarning {
    pub kind: &'static str,
    pub percent: Option<u8>,
}

/// Emit a `meeting-power-warning` to the front-end. Best-effort: a failed emit
/// only costs the warning, never the recording.
pub fn emit_power_warning(app: &AppHandle, kind: &'static str, percent: Option<u8>) {
    let _ = app.emit(
        "meeting-power-warning",
        MeetingPowerWarning { kind, percent },
    );
}

/// A meeting is on battery power at or below the low threshold.
fn is_low_on_battery(status: Option<lumen_platform_macos::BatteryStatus>) -> bool {
    matches!(status, Some(s) if !s.on_ac && s.percent <= LOW_BATTERY_PERCENT)
}

/// Spawn a background thread that re-checks the battery every
/// [`BATTERY_POLL_INTERVAL`] while a meeting records and emits a low-battery
/// warning when the level newly crosses at/under the threshold on battery
/// power. Warns at most once per crossing: the `warned` latch is cleared once
/// the machine is back on AC or above the threshold, so a later dip warns
/// again. `initially_warned` seeds the latch from the start-of-recording check
/// so an already-low start does not warn twice.
///
/// Returns the stop flag + join handle so the stop command can signal and join
/// it. On non-macOS `battery_status()` is always `None`, so the thread simply
/// idles until stopped.
fn spawn_battery_poll(app: AppHandle, initially_warned: bool) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut warned = initially_warned;
        // Sleep in small slices so a stop request is honored promptly.
        let step = Duration::from_millis(500);
        while !stop_thread.load(Ordering::SeqCst) {
            let mut slept = Duration::ZERO;
            while slept < BATTERY_POLL_INTERVAL && !stop_thread.load(Ordering::SeqCst) {
                std::thread::sleep(step);
                slept += step;
            }
            if stop_thread.load(Ordering::SeqCst) {
                break;
            }
            let status = lumen_platform_macos::battery_status();
            if is_low_on_battery(status) {
                if !warned {
                    warned = true;
                    let percent = status.map(|s| s.percent);
                    tracing::warn!(
                        percent,
                        "meeting on low battery; recording may be interrupted by power loss"
                    );
                    emit_power_warning(&app, "low-battery", percent);
                }
            } else {
                // Back on AC or above the threshold → re-arm for the next dip.
                warned = false;
            }
        }
    });
    (stop, handle)
}

/// How often the silence watchdog re-checks captured audio while a meeting
/// records. One second keeps the visible countdown accurate while doing only
/// a handful of atomic reads per tick.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Grace period shown after prolonged silence is detected. Any captured sound
/// cancels it; otherwise the meeting is stopped when this countdown expires.
const SILENCE_COUNTDOWN_SECONDS: u32 = 20;

/// Handle stored in [`AppState`] for the one active silence watchdog.
pub struct MeetingWatchdogHandle {
    meeting_id: String,
    stop: Arc<AtomicBool>,
    continue_generation: Arc<AtomicU64>,
    handle: JoinHandle<()>,
}

impl MeetingWatchdogHandle {
    fn request_continue(&self, meeting_id: &str) -> Result<(), String> {
        if self.meeting_id != meeting_id {
            return Err("silence warning belongs to a different meeting".to_string());
        }
        self.continue_generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn stop_and_join(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// Payload of the `meeting-auto-stop` event: the front-end owns the real stop
/// path, so the watchdog only *asks* it to stop (it never calls the stop
/// command from the background thread). `reason` is `"silence"`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingAutoStop {
    pub meeting_id: String,
    pub reason: &'static str,
}

/// Payload emitted when all available recording tracks have stayed below the
/// physical-volume threshold long enough to begin the grace-period countdown.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSilenceWarning {
    pub meeting_id: String,
    pub countdown_seconds: u32,
}

/// Payload emitted when sound resumes or the user explicitly chooses to keep
/// recording, so every open UI can retract a stale countdown.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSilenceCleared {
    pub meeting_id: String,
}

/// Payload of the `meeting-calendar-ended` event: a linked calendar meeting's
/// end time has passed while it is still recording. The front-end shows a
/// *reminder* with a Stop button — never an auto-stop, since a calendar end is
/// not necessarily the real end.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingCalendarEnded {
    pub meeting_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SilenceWatchdogAction {
    None,
    Warn,
    Clear,
    Stop,
}

/// Pure state machine behind the watchdog thread. Time is measured by captured
/// samples rather than wall clock, so an intentional pause also pauses both the
/// silence threshold and the grace-period countdown.
struct SilenceWatchdogState {
    threshold_seconds: f64,
    countdown_seconds: f64,
    baseline_seconds: f64,
    last_silence_seconds: Option<f64>,
    warned_at_seconds: Option<f64>,
    continue_generation: u64,
}

impl SilenceWatchdogState {
    fn new(threshold_seconds: f64, countdown_seconds: f64) -> Self {
        Self {
            threshold_seconds,
            countdown_seconds,
            baseline_seconds: 0.0,
            last_silence_seconds: None,
            warned_at_seconds: None,
            continue_generation: 0,
        }
    }

    fn observe(
        &mut self,
        silence_seconds: Option<f64>,
        continue_generation: u64,
    ) -> SilenceWatchdogAction {
        if continue_generation != self.continue_generation {
            self.continue_generation = continue_generation;
            self.baseline_seconds = silence_seconds.unwrap_or(0.0);
            self.last_silence_seconds = silence_seconds;
            let warned = self.warned_at_seconds.take().is_some();
            return if warned {
                SilenceWatchdogAction::Clear
            } else {
                SilenceWatchdogAction::None
            };
        }

        let Some(silence_seconds) = silence_seconds.filter(|value| value.is_finite()) else {
            self.last_silence_seconds = None;
            self.baseline_seconds = 0.0;
            return if self.warned_at_seconds.take().is_some() {
                SilenceWatchdogAction::Clear
            } else {
                SilenceWatchdogAction::None
            };
        };

        // Each activity tracker is monotonic until a loud chunk resets it near
        // zero. A decrease therefore means real sound resumed on at least one
        // available track; re-arm from scratch and retract any warning.
        if self
            .last_silence_seconds
            .is_some_and(|last| silence_seconds + 0.001 < last)
        {
            self.baseline_seconds = 0.0;
            self.last_silence_seconds = Some(silence_seconds);
            return if self.warned_at_seconds.take().is_some() {
                SilenceWatchdogAction::Clear
            } else {
                SilenceWatchdogAction::None
            };
        }
        self.last_silence_seconds = Some(silence_seconds);

        let effective = (silence_seconds - self.baseline_seconds).max(0.0);
        match self.warned_at_seconds {
            Some(warned_at) if effective >= warned_at + self.countdown_seconds => {
                SilenceWatchdogAction::Stop
            }
            Some(_) => SilenceWatchdogAction::None,
            None if effective >= self.threshold_seconds => {
                self.warned_at_seconds = Some(effective);
                SilenceWatchdogAction::Warn
            }
            None => SilenceWatchdogAction::None,
        }
    }
}

/// Seconds since the most recent physical audio on any available meeting
/// track. `min` is deliberate: sound on either the mic or system-output track
/// means the meeting is still active. `None` is fail-open — no auto-stop when
/// capture activity cannot be measured.
fn active_meeting_silence_seconds(state: &AppState) -> Option<f64> {
    let mic = state
        .meeting_mic_aec
        .silence_seconds()
        .or_else(|| state.meeting_recorder.silence_seconds());
    let system = state.meeting_system_audio.silence_seconds();
    combine_track_silence_seconds(mic, system)
}

fn combine_track_silence_seconds(mic: Option<f64>, system: Option<f64>) -> Option<f64> {
    let valid = |seconds: Option<f64>| seconds.filter(|value| value.is_finite() && *value >= 0.0);
    let mic = valid(mic);
    let system = valid(system);
    match (mic, system) {
        (Some(mic), Some(system)) => Some(mic.min(system)),
        (Some(mic), None) => Some(mic),
        (None, Some(system)) => Some(system),
        (None, None) => None,
    }
}

/// Spawn a background thread that watches physical volume on every available
/// meeting track. Prolonged silence first emits a warning; after a 20-second
/// grace period of continued silence it emits `meeting-auto-stop`. Sound or an
/// explicit “continue recording” acknowledgement clears and re-arms it.
fn spawn_meeting_watchdog(
    app: AppHandle,
    meeting_id: String,
    minutes: u32,
) -> MeetingWatchdogHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let continue_generation = Arc::new(AtomicU64::new(0));
    let continue_thread = Arc::clone(&continue_generation);
    let threshold_seconds = f64::from(minutes) * 60.0;
    let thread_meeting_id = meeting_id.clone();
    let handle = std::thread::spawn(move || {
        let mut watchdog =
            SilenceWatchdogState::new(threshold_seconds, f64::from(SILENCE_COUNTDOWN_SECONDS));
        // Sleep in small slices so a stop request is honored promptly.
        let step = Duration::from_millis(250);
        while !stop_thread.load(Ordering::SeqCst) {
            let mut slept = Duration::ZERO;
            while slept < WATCHDOG_POLL_INTERVAL && !stop_thread.load(Ordering::SeqCst) {
                std::thread::sleep(step);
                slept += step;
            }
            if stop_thread.load(Ordering::SeqCst) {
                break;
            }
            let silence = active_meeting_silence_seconds(app.state::<AppState>().inner());
            let generation = continue_thread.load(Ordering::SeqCst);
            match watchdog.observe(silence, generation) {
                SilenceWatchdogAction::Warn => {
                    let seconds = silence.unwrap_or_default();
                    tracing::info!(
                        meeting_id = %thread_meeting_id,
                        silence_seconds = seconds,
                        countdown_seconds = SILENCE_COUNTDOWN_SECONDS,
                        "meeting silent past threshold; starting auto-stop countdown"
                    );
                    let _ = app.emit(
                        "meeting-silence-warning",
                        MeetingSilenceWarning {
                            meeting_id: thread_meeting_id.clone(),
                            countdown_seconds: SILENCE_COUNTDOWN_SECONDS,
                        },
                    );
                }
                SilenceWatchdogAction::Clear => {
                    let _ = app.emit(
                        "meeting-silence-cleared",
                        MeetingSilenceCleared {
                            meeting_id: thread_meeting_id.clone(),
                        },
                    );
                }
                SilenceWatchdogAction::Stop => {
                    tracing::info!(
                        meeting_id = %thread_meeting_id,
                        "meeting remained silent through countdown; asking UI to stop"
                    );
                    let _ = app.emit(
                        "meeting-auto-stop",
                        MeetingAutoStop {
                            meeting_id: thread_meeting_id.clone(),
                            reason: "silence",
                        },
                    );
                }
                SilenceWatchdogAction::None => {}
            }
        }
    });
    MeetingWatchdogHandle {
        meeting_id,
        stop,
        continue_generation,
        handle,
    }
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
    start_meeting_recording_with_targets(app, state, title, None)
}

/// Internal start path used by the detection flow after the backend has
/// matched and accepted a catalog entry. Keeping process targets out of the
/// public Tauri command prevents renderer callers from bypassing the external
/// catalog's capture policy.
fn start_meeting_recording_with_targets(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
    system_audio_bundle_ids: Option<Vec<String>>,
) -> Result<String, String> {
    // Hold this guard through the whole start. The matching stop command uses
    // the same guard, so no stop/start pair can overlap while operating on the
    // process-global recorder objects.
    let mut recording_owner = state
        .meeting_recording_owner
        .lock()
        .map_err(|_| "meeting recording owner lock poisoned".to_string())?;
    recording_owner.ensure_startable()?;

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
    //    engaged (macOS + streaming Paraformer installed), attach a bounded
    //    audio fan-out so the streaming worker can transcribe live; otherwise
    //    record plainly (no sink → zero extra work in the audio callback).
    //
    //    `t0` is the meeting's **unified timeline origin**: one `Instant`
    //    shared by every track. Fan-out packets are stamped against it at
    //    callback-arrival time, and each track's WAV start offset below is
    //    measured from it.
    let device = preferred_device(&state);
    let streaming_dir = crate::meeting_live::streaming_dir_if_ready();
    let t0 = Instant::now();
    let t0_wall_clock = chrono::Utc::now().to_rfc3339();
    let (mic_tap, mic_rx) = if streaming_dir.is_some() {
        let (tap, rx) = live_tap_channel("mic", t0, LIVE_TAP_CAPACITY);
        (Some(tap), Some(rx))
    } else {
        (None, None)
    };
    // Mic capture backend selection: prefer the system voice processor
    // (VoiceProcessingIO — OS-level echo cancellation, so speakerphone
    // meetings stop feeding the far-end voice back through the mic) when the
    // user has not opted out (`meeting.mic_aec`, default on) and the host
    // supports it. Any AEC start failure falls back to the plain cpal
    // recorder — the recording itself never fails because of AEC. Both
    // backends feed the identical WAV/fan-out contracts, so live preview,
    // pause, sidecars, and the offline pipeline are unaffected by the choice.
    // (Trade-off behind the opt-out: VPIO's bundled noise suppression may
    // attenuate quiet far-field speakers in a conference room.)
    let mic_aec_enabled = state
        .config
        .lock()
        .map(|cfg| cfg.meeting.mic_aec)
        .unwrap_or(true);
    let aec_rate = if mic_aec_enabled && crate::meeting_mic_aec::MeetingMicAec::is_supported() {
        let rate = state
            .meeting_mic_aec
            .start(device.clone(), out_path.clone(), mic_tap.clone());
        if rate.is_some() {
            tracing::info!("meeting mic path: VoiceProcessingIO (system AEC) engaged");
        } else {
            tracing::warn!("meeting mic path: VoiceProcessingIO failed, falling back to cpal");
        }
        rate
    } else {
        tracing::info!(
            mic_aec_enabled,
            "meeting mic path: cpal (AEC disabled or unsupported on this host)"
        );
        None
    };
    let sample_rate = match aec_rate {
        Some(rate) => rate,
        None => match state
            .meeting_recorder
            .start_with_sink(device, out_path.clone(), mic_tap)
        {
            Ok(rate) => rate,
            Err(e) => {
                // Roll back: mark failed and release the arbiter. No hotkey suspend
                // was applied yet, so nothing to restore. `mic_rx` drops here,
                // so no worker is left dangling.
                state.capture.force_idle();
                let reason = format!("could not start recording: {e}");
                let _ = with_store(&state, |s| {
                    s.fail_meeting(meeting_id, Some(&reason))
                        .map_err(|e| e.to_string())
                });
                return Err(reason);
            }
        },
    };
    let mic_offset_seconds = t0.elapsed().as_secs_f64();

    // Record the mic WAV path up front (mirroring the system track below) so
    // crash recovery can salvage it if the app is killed mid-recording — the
    // stop path is the only other place this is set, so without this an
    // interrupted meeting has no mic path and its audio, though on disk, is
    // reported unrecoverable. Best-effort: a store failure only weakens
    // recovery, never the recording.
    let mic_path = out_path.to_string_lossy().to_string();
    if let Err(e) = with_store(&state, |s| {
        s.set_meeting_audio_path(meeting_id, &mic_path)
            .map_err(|e| e.to_string())
    }) {
        tracing::warn!(meeting_id = %meeting_id, error = %e, "could not record mic audio path");
    }

    // 5. Optional second track: system audio output (remote participants) via
    //    the macOS Core Audio process tap. Strictly best-effort and
    //    capability-gated — if the config opts out, the OS is too old
    //    (< 14.2), the permission is denied, or anything else fails, the
    //    session logs a warning and the meeting records mic-only exactly as
    //    before. The mic path above is already live and is never touched.
    //    When the live layer is engaged (and `system_live_preview` opted in),
    //    the tap additionally fans out to the live worker as the "system"
    //    (远端) track.
    let (system_audio_enabled, system_live_preview) = state
        .config
        .lock()
        .map(|cfg| (cfg.meeting.system_audio, cfg.meeting.system_live_preview))
        .unwrap_or((true, true));
    let mut system_feed: Option<crate::meeting_live::LiveTrackFeed> = None;
    let mut system_offset_seconds: Option<f64> = None;
    if system_audio_enabled {
        // A detection-started recording supplies the exact accepted app (or an
        // empty list for browser mic-only). A manual recording uses every
        // native app explicitly enabled for capture in the external catalog.
        // There is intentionally no global-system-audio fallback.
        let targets = system_audio_bundle_ids
            .unwrap_or_else(|| state.meeting_detection.manual_capture_bundle_ids());
        let (system_tap, system_rx) = if streaming_dir.is_some() && system_live_preview {
            let (tap, rx) = live_tap_channel("system", t0, LIVE_TAP_CAPACITY);
            (Some(tap), Some(rx))
        } else {
            (None, None)
        };
        let system_path = dir.join(format!("{meeting_id}.system.wav"));
        if let Some(system_rate) =
            state
                .meeting_system_audio
                .start(system_path.clone(), targets, system_tap)
        {
            system_offset_seconds = Some(t0.elapsed().as_secs_f64());
            if let Some(rx) = system_rx {
                system_feed = Some(crate::meeting_live::LiveTrackFeed {
                    rx,
                    capture_rate: system_rate,
                });
            }
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
        // `system_rx` (when the tap failed to start) drops here, so the live
        // worker sees an immediately-disconnected system feed — i.e. none.
    }

    // Persist the unified-timeline sidecar next to the meeting WAVs. Purely
    // additive metadata (no schema change), best-effort like the tracks.
    write_timeline_sidecar(
        &out_path.with_extension("timeline.json"),
        &MeetingTimeline {
            mic_offset_seconds,
            system_offset_seconds,
            t0_wall_clock,
        },
    );

    // 6. Recording is live — spawn the live-transcript worker (if streaming) and
    //    suspend the dictation hotkey. A live-worker failure never fails the
    //    recording: the worker itself degrades to "no live text" on any error.
    if let (Some(streaming), Some(rx)) = (streaming_dir, mic_rx) {
        state.meeting_live.start(
            app.clone(),
            meeting_id.to_string(),
            streaming,
            crate::meeting_live::LiveTrackFeed {
                rx,
                capture_rate: sample_rate,
            },
            system_feed,
        );
    }
    apply_hotkey_action(&app, action);

    // 7. Best-effort calendar link (opt-out): look up the current / imminent
    //    calendar event on a background thread and, when one matches,
    //    auto-title an untitled meeting and note the attendee names. Spawned
    //    *after* the recorder is live so the start path is never delayed —
    //    a denied permission or no matching event changes nothing.
    let (calendar_link_enabled, calendar_end_reminder) = state
        .config
        .lock()
        .map(|cfg| (cfg.meeting.calendar_link, cfg.meeting.calendar_end_reminder))
        .unwrap_or((true, true));
    if calendar_link_enabled {
        spawn_calendar_link(app.clone(), meeting_id, calendar_end_reminder);
    }

    // Hold an activity that prevents idle system sleep and App Nap for the
    // duration of the recording, so the OS never suspends the audio capture
    // callbacks and drops meeting audio. Acquisition is infallible; guard the
    // lock defensively so a poisoned mutex can never fail the start command.
    if let Ok(mut guard) = state.meeting_power_guard.lock() {
        *guard = Some(lumen_platform_macos::MeetingPowerGuard::acquire());
        tracing::info!("holding off idle system sleep for the duration of the meeting");
    } else {
        tracing::warn!("meeting power guard lock poisoned; skipping idle-sleep hold");
    }

    // Proactive power warnings: the idle-sleep hold above cannot stop a drained
    // battery or a lid close, so warn the user while there is still time to plug
    // in. Check the battery once now, then poll it for the duration of the
    // recording. `battery_status()` is `None` on desktops / off-macOS, so this
    // is naturally inert there.
    let battery = lumen_platform_macos::battery_status();
    let low_at_start = is_low_on_battery(battery);
    if low_at_start {
        let percent = battery.map(|s| s.percent);
        tracing::warn!(
            percent,
            "meeting starting on low battery; recording may be interrupted by power loss"
        );
        emit_power_warning(&app, "low-battery", percent);
    }
    let poll = spawn_battery_poll(app.clone(), low_at_start);
    if let Ok(mut guard) = state.meeting_battery_poll.lock() {
        *guard = Some(poll);
    } else {
        // Poisoned lock: we cannot store the handle, so signal the thread to
        // stop rather than leak it (it exits at its next slice check).
        poll.0.store(true, Ordering::SeqCst);
        tracing::warn!("meeting battery poll lock poisoned; skipping low-battery monitoring");
    }

    // Silence watchdog: warn after N minutes with no physical audio on either
    // available track, then auto-stop after a 20-second grace period. Disabled
    // when `silence_auto_stop_minutes` is 0. The thread only emits events — the
    // front-end owns the real stop path. Same store-or-stop-on-poison handling
    // as the battery poll above.
    let silence_minutes = state
        .config
        .lock()
        .map(|cfg| cfg.meeting.silence_auto_stop_minutes)
        .unwrap_or(0);
    if silence_minutes > 0 {
        let watchdog = spawn_meeting_watchdog(app.clone(), meeting_id.to_string(), silence_minutes);
        if let Ok(mut guard) = state.meeting_watchdog.lock() {
            *guard = Some(watchdog);
        } else {
            watchdog.stop_and_join();
            tracing::warn!("meeting watchdog lock poisoned; skipping silence auto-stop monitoring");
        }
    }

    recording_owner.started(meeting_id);
    tracing::info!(meeting_id = %meeting_id, "meeting recording started");
    Ok(meeting_id.to_string())
}

/// Look-ahead window for linking a just-started recording to a calendar
/// event: an event starting within the next 15 minutes counts (the lookback
/// for already-running events lives in the platform layer).
#[cfg(target_os = "macos")]
const CALENDAR_LOOKAHEAD_MINUTES: u32 = 15;

/// Marker of the auto-inserted attendees line; its presence in the existing
/// notes means the line was already written (never duplicate it).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const ATTENDEES_MARKER: &str = "参会人:";

/// Build the notes text with the "参会人: …" line prepended, or `None` when
/// there is nothing to write: no attendees, or the notes already carry an
/// attendees line (manual or from a previous link).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn merge_attendees_into_notes(existing: &str, attendees: &[String]) -> Option<String> {
    if attendees.is_empty() || existing.contains(ATTENDEES_MARKER) {
        return None;
    }
    let line = format!("{ATTENDEES_MARKER} {}\n", attendees.join("、"));
    Some(format!("{line}{existing}"))
}

/// Query the calendar for the event this recording belongs to and fold it
/// into the meeting row (title when still untitled, attendees into notes).
/// Runs on its own thread with its own SQLite connection (never the UI store
/// lock), exactly like the processing pipeline: the start path has already
/// returned, and every failure here is log-and-drop — the recording itself
/// is untouched. On non-macOS there is no calendar bridge, so this is a no-op.
fn spawn_calendar_link(app: AppHandle, meeting_id: Uuid, end_reminder: bool) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || {
        // May block on the (first-use) permission prompt and the EventKit
        // fetch — harmless on this background thread. Denied permission or
        // no matching event → None, and the meeting stays as started.
        let Some(event) =
            lumen_platform_macos::calendar_current_or_upcoming_event(CALENDAR_LOOKAHEAD_MINUTES)
        else {
            return;
        };
        let store = match Store::open(default_db_path()) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(meeting_id = %meeting_id, error = %e, "calendar link: could not open store");
                return;
            }
        };
        if let Err(e) = apply_calendar_event(&store, meeting_id, &event) {
            tracing::warn!(meeting_id = %meeting_id, error = %e, "calendar link: could not apply event");
        }
        // Calendar-end reminder (opt-out): when the matched event has a future
        // end time, wait until it passes and — if the meeting is still
        // recording — prompt the user to stop. A reminder, never an auto-stop.
        if end_reminder && event.end_epoch_seconds > now_epoch_seconds() {
            spawn_calendar_end_reminder(
                app,
                meeting_id,
                event.title.trim().to_string(),
                event.end_epoch_seconds,
            );
        }
    });
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, meeting_id, end_reminder);
    }
}

/// Current wall-clock time as seconds since the Unix epoch (a backwards clock
/// clamps to 0). Used to schedule the calendar-end reminder relative to now.
#[cfg(target_os = "macos")]
fn now_epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Spawn a one-shot timer that fires when a linked calendar event's end time
/// passes: it sleeps until `end_epoch_seconds` (relative to now; a past end
/// fires ~immediately), re-opens the store, and — only if the meeting is STILL
/// `Recording` — emits `meeting-calendar-ended` so the front-end shows a Stop
/// reminder. Best-effort: any failure just drops the reminder.
#[cfg(target_os = "macos")]
fn spawn_calendar_end_reminder(
    app: AppHandle,
    meeting_id: Uuid,
    title: String,
    end_epoch_seconds: f64,
) {
    std::thread::spawn(move || {
        let store = match Store::open(default_db_path()) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(meeting_id = %meeting_id, error = %e, "calendar-end reminder: could not open store");
                return;
            }
        };
        let still_recording = |store: &Store| {
            matches!(
                store.get_meeting(meeting_id),
                Ok(Some(m)) if m.status == MeetingStatus::Recording
            )
        };
        // Sleep toward the event end in short slices, re-checking status each one,
        // so a meeting stopped before its scheduled end frees this thread within a
        // slice instead of parking it (possibly for hours).
        loop {
            let remaining = end_epoch_seconds - now_epoch_seconds();
            if remaining <= 0.0 {
                break;
            }
            if !still_recording(&store) {
                return;
            }
            std::thread::sleep(Duration::from_secs_f64(remaining.min(15.0)));
        }
        match store.get_meeting(meeting_id) {
            Ok(Some(meeting)) if meeting.status == MeetingStatus::Recording => {
                tracing::info!(meeting_id = %meeting_id, "calendar meeting end reached while recording; reminding to stop");
                let _ = app.emit(
                    "meeting-calendar-ended",
                    MeetingCalendarEnded {
                        meeting_id: meeting_id.to_string(),
                        title,
                    },
                );
            }
            // Already stopped / not found → the reminder is moot; drop it.
            _ => {}
        }
    });
}

/// Fold a matched calendar event into the meeting row: title the meeting
/// after the event when the user left it untitled, and prepend the attendee
/// line to the notes (skipped when one is already there). Reads the row
/// fresh so a title the user typed in the start dialog — or set in the UI in
/// the meantime — is never overwritten.
#[cfg(target_os = "macos")]
fn apply_calendar_event(
    store: &Store,
    meeting_id: Uuid,
    event: &lumen_platform_macos::CalendarEventInfo,
) -> Result<(), String> {
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "meeting not found".to_string())?;
    // Atomic conditional write: only title an *untitled* meeting, so a title the
    // user sets between our read above and this write (e.g. renaming during
    // recording) is never clobbered.
    let auto_titled = if event.title.trim().is_empty() {
        false
    } else {
        store
            .set_meeting_title_if_untitled(meeting_id, event.title.trim())
            .map_err(|e| e.to_string())?
    };
    if let Some(merged) = merge_attendees_into_notes(&meeting.notes, &event.attendee_names) {
        store
            .set_meeting_notes(meeting_id, &merged)
            .map_err(|e| e.to_string())?;
    }
    // Only ids and counts in the log — the event title / attendees are
    // personal data.
    tracing::info!(
        meeting_id = %meeting_id,
        auto_titled,
        attendees = event.attendee_names.len(),
        "calendar link applied to meeting"
    );
    Ok(())
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
    // Authorize the id before touching any global recorder. This also holds
    // start/stop serialization until every old-session resource is cleaned up.
    let mut recording_owner = state
        .meeting_recording_owner
        .lock()
        .map_err(|_| "meeting recording owner lock poisoned".to_string())?;
    if let Some(completed) = recording_owner.authorize_stop(id)? {
        return Ok(completed);
    }

    // Stop the watchdog before the capture engines. Otherwise it can poll a
    // just-finalized track once more and emit a stale auto-stop event while
    // this command is already stopping the meeting.
    if let Ok(mut guard) = state.meeting_watchdog.lock() {
        if let Some(watchdog) = guard.take() {
            watchdog.stop_and_join();
        }
    }

    // Try to stop + finalize the recorder, but *keep* the result — the recovery
    // below (restore hotkey + reset arbiter) must run on every path, success or
    // failure. This is finally/defer semantics: whatever happens to the
    // recorder or the store write, "the mic and hotkey always come back".
    //
    // The mic may have been captured by either backend: the AEC
    // (VoiceProcessingIO) session when one engaged at start, else the plain
    // cpal recorder. `meeting_mic_aec.stop()` returns `None` when it was not
    // the active path.
    let stop_result: Result<lumen_asr::RecordingSummary, String> =
        match state.meeting_mic_aec.stop() {
            Some(result) => result,
            None => state.meeting_recorder.stop().map_err(|e| e.to_string()),
        };

    // Stop the system-audio track (no-op when none is running): tears down the
    // tap and finalizes the second WAV. Best-effort — `None` here just means
    // the meeting is mic-only.
    let system_summary = state.meeting_system_audio.stop();

    // Surface any capture stalls (system sleep / App Nap suspended the audio
    // callback): the recorder padded them with silence to keep the timeline
    // honest, but the audio for those stretches was genuinely not captured, so
    // warn with the total across both tracks.
    let gap_seconds: f64 = stop_result
        .as_ref()
        .map(|s| s.gaps.iter().map(|g| g.duration_seconds).sum())
        .unwrap_or(0.0)
        + system_summary
            .as_ref()
            .map(|s| s.gaps.iter().map(|g| g.duration_seconds).sum::<f64>())
            .unwrap_or(0.0);
    if gap_seconds >= 1.0 {
        tracing::warn!(
            meeting_id = %id,
            gap_seconds,
            "meeting had capture stalls padded with silence (likely the Mac slept); \
             that audio was not recorded"
        );
    }

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

    // Release the idle-system-sleep / App-Nap hold acquired at start, on every
    // path regardless of stop success or failure. Dropping the guard ends the
    // activity; a poisoned lock just means it is already gone.
    let _ = state.meeting_power_guard.lock().map(|mut g| g.take());

    // Stop the low-battery poll thread started with the recording, on every
    // path. Signal it to exit and join (returns within a poll slice). A poisoned
    // lock just means it is already gone.
    if let Ok(mut guard) = state.meeting_battery_poll.lock() {
        if let Some((stop, handle)) = guard.take() {
            stop.store(true, Ordering::SeqCst);
            let _ = handle.join();
        }
    }

    let _ = app.emit(
        "meeting-silence-cleared",
        MeetingSilenceCleared {
            meeting_id: meeting_id.clone(),
        },
    );

    // Same finally semantics for meeting detection: whether the recorder stop
    // below succeeded or failed, the recording is over, so the policy must
    // leave `recording` — otherwise a stop failure would strand it there and
    // every future candidate would be rejected as busy. Harmless when this
    // recording was not started from a detection prompt (the policy no-ops
    // unless it is tracking an accepted recording). Also retracts a
    // still-visible "meeting seems over" suggestion, since the question is
    // now moot however the stop was initiated.
    let detection_enabled = state
        .config
        .lock()
        .map(|config| config.meeting.detection_enabled)
        .unwrap_or(false);
    state
        .meeting_detection
        .recording_finished(&app, detection_enabled);

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
            recording_owner.stopped_without_summary(id);
            return Err(e);
        }
    };
    let audio_path = summary.wav_path.to_string_lossy().to_string();

    if let Err(error) = with_store(&state, |s| {
        s.set_meeting_audio(
            id,
            &audio_path,
            summary.duration_seconds,
            MeetingStatus::Processing,
        )
        .map_err(|e| e.to_string())
    }) {
        recording_owner.stopped_without_summary(id);
        return Err(error);
    }

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

    let result = MeetingRecordingDto {
        id: meeting_id,
        audio_path,
        duration_seconds: summary.duration_seconds,
        sample_rate: summary.sample_rate,
        status: MeetingStatus::Processing.as_str().to_string(),
    };
    recording_owner.completed(id, result.clone());
    Ok(result)
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
    capture_system_audio: bool,
) -> Result<String, String> {
    let Some(accepted) = state.meeting_detection.accept() else {
        return Ok(String::new());
    };
    // Native apps need both the catalog authorization and the accepted prompt.
    // Browsers deliberately use per-prompt consent instead: their catalog
    // `capture = false` default means "never capture without asking", and the
    // prompt offers the explicit whole-browser vs mic-only choice every time.
    let should_capture = capture_system_audio
        && match accepted.app_class {
            lumen_core::AppClass::Browser => true,
            _ => state.meeting_detection.capture_enabled(&accepted.bundle_id),
        };
    let system_targets = should_capture
        .then(|| vec![accepted.bundle_id.clone()])
        .unwrap_or_default();
    // Reuse the proven start command (arbiter gate, hotkey suspend, recorder).
    // If it fails, tell the policy so it does not sit in `recording` forever
    // (which would reject every future candidate as busy).
    match start_meeting_recording_with_targets(
        app.clone(),
        state.clone(),
        None,
        Some(system_targets),
    ) {
        Ok(id) => {
            // Remember which meeting this detection started so the
            // end-of-meeting stop suggestion can reference (and stop) it.
            state.meeting_detection.mark_recording_started(&id);
            Ok(id)
        }
        Err(e) => {
            let detection_enabled = state
                .config
                .lock()
                .map(|config| config.meeting.detection_enabled)
                .unwrap_or(false);
            state
                .meeting_detection
                .recording_failed(&app, detection_enabled);
            Err(e)
        }
    }
}

/// Read the runtime meeting/recording app catalog and its editable user path.
#[tauri::command]
pub fn get_meeting_app_catalog(
    state: State<'_, AppState>,
) -> Result<crate::meeting_apps::MeetingAppCatalogDto, String> {
    Ok(state.meeting_detection.app_catalog())
}

/// Validate, atomically persist, and immediately activate a new app catalog.
#[tauri::command]
pub fn save_meeting_app_catalog(
    app: AppHandle,
    state: State<'_, AppState>,
    catalog: crate::meeting_apps::MeetingAppCatalog,
) -> Result<crate::meeting_apps::MeetingAppCatalogDto, String> {
    let saved = state.meeting_detection.save_app_catalog(catalog)?;
    let enabled = state
        .config
        .lock()
        .map(|config| config.meeting.detection_enabled)
        .unwrap_or(false);
    state
        .meeting_detection
        .restart_after_catalog_change(&app, enabled);
    Ok(saved)
}

/// Reload edits made directly to the external TOML file.
#[tauri::command]
pub fn reload_meeting_app_catalog(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<crate::meeting_apps::MeetingAppCatalogDto, String> {
    let reloaded = state.meeting_detection.reload_app_catalog()?;
    let enabled = state
        .config
        .lock()
        .map(|config| config.meeting.detection_enabled)
        .unwrap_or(false);
    state
        .meeting_detection
        .restart_after_catalog_change(&app, enabled);
    Ok(reloaded)
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

/// Serialized watchdog settings for the meeting settings UI.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingWatchdogConfig {
    /// Minutes of continuous mic silence before an unattended recording
    /// auto-stops. `0` disables the auto-stop.
    pub silence_auto_stop_minutes: u32,
    /// Prompt to stop when a calendar-linked meeting's end time passes.
    pub calendar_end_reminder: bool,
}

/// Read the meeting watchdog settings (silence auto-stop + calendar-end
/// reminder) for the settings UI.
#[tauri::command]
pub fn get_meeting_watchdog_config(
    state: State<'_, AppState>,
) -> Result<MeetingWatchdogConfig, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(MeetingWatchdogConfig {
        silence_auto_stop_minutes: cfg.meeting.silence_auto_stop_minutes,
        calendar_end_reminder: cfg.meeting.calendar_end_reminder,
    })
}

/// Persist the meeting watchdog settings and return the stored values. Takes
/// effect for the next recording (a running watchdog captured its threshold at
/// start).
#[tauri::command]
pub fn set_meeting_watchdog_config(
    state: State<'_, AppState>,
    silence_auto_stop_minutes: u32,
    calendar_end_reminder: bool,
) -> Result<MeetingWatchdogConfig, String> {
    {
        let mut cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        cfg.meeting.silence_auto_stop_minutes = silence_auto_stop_minutes;
        cfg.meeting.calendar_end_reminder = calendar_end_reminder;
        cfg.save()?;
    }
    get_meeting_watchdog_config(state)
}

/// The user confirmed that a silence warning is a real pause rather than an
/// abandoned meeting. Re-arm the watchdog from the current captured-silence
/// position, giving the recording a fresh full threshold before it can warn
/// again. The worker emits `meeting-silence-cleared` on its next tick.
#[tauri::command]
pub fn continue_meeting_after_silence(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<(), String> {
    let guard = state
        .meeting_watchdog
        .lock()
        .map_err(|_| "meeting watchdog lock poisoned".to_string())?;
    let watchdog = guard
        .as_ref()
        .ok_or_else(|| "meeting silence watchdog is not active".to_string())?;
    watchdog.request_continue(&meeting_id)
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
/// Routes to whichever mic backend is active: the AEC (VoiceProcessingIO)
/// session when one engaged at start, else the cpal recorder.
#[tauri::command]
pub fn pause_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    if !state.meeting_mic_aec.set_paused(true) {
        state.meeting_recorder.pause().map_err(|e| e.to_string())?;
    }
    // Keep the system track's timeline in lockstep with the mic's.
    state.meeting_system_audio.set_paused(true);
    Ok(())
}

/// Resume a paused meeting recording.
#[tauri::command]
pub fn resume_meeting_recording(state: State<'_, AppState>) -> Result<(), String> {
    if !state.meeting_mic_aec.set_paused(false) {
        state.meeting_recorder.resume().map_err(|e| e.to_string())?;
    }
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

/// Import an existing audio/video file into the meeting library and run the
/// same offline pipeline as a recorded meeting. `path` is used for drag-and-drop;
/// omit it to open a native file picker.
#[tauri::command]
pub async fn import_meeting_file(
    app: AppHandle,
    state: State<'_, AppState>,
    path: Option<String>,
) -> Result<String, String> {
    let source = match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => pick_meeting_audio_path(&app)?,
    };
    let meetings_dir = default_data_dir().join("meetings");
    let source_for_io = source.clone();
    let meetings_dir_for_io = meetings_dir.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_imported_meeting_audio(&source_for_io, &meetings_dir_for_io)
    })
    .await
    .map_err(|e| format!("导入中断：{e}"))??;

    let persist_error = with_store(&state, |s| {
        s.create_meeting(&prepared.meeting)
            .map_err(|e| format!("无法写入会议库：{e}"))
    });
    if let Err(error) = persist_error {
        let _ = std::fs::remove_file(&prepared.wav);
        return Err(error);
    }
    spawn_meeting_processing(app, prepared.meeting.id, prepared.wav, None);
    Ok(prepared.meeting.id.to_string())
}

#[derive(Debug)]
struct PreparedImport {
    meeting: Meeting,
    wav: PathBuf,
}

fn prepare_imported_meeting_audio(
    source: &Path,
    meetings_dir: &Path,
) -> Result<PreparedImport, String> {
    if !source.is_file() {
        return Err(format!("找不到音频文件：{}", source.display()));
    }
    if !crate::audio_convert::is_importable_meeting_audio(source) {
        return Err("仅支持 wav / mp3 / m4a / mp4".into());
    }
    let mut meeting = Meeting::new();
    meeting.status = MeetingStatus::Processing;
    meeting.title = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());
    let dest = meetings_dir.join(format!("{}.wav", meeting.id));
    crate::audio_convert::copy_or_convert_to_wav(source, &dest)?;
    meeting.audio_path = Some(dest.to_string_lossy().into_owned());
    Ok(PreparedImport { meeting, wav: dest })
}

fn pick_meeting_audio_path(app: &AppHandle) -> Result<PathBuf, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let picked = rfd::FileDialog::new()
            .add_filter("音视频", &["wav", "wave", "mp3", "m4a", "mp4"])
            .set_title("导入会议录音")
            .pick_file();
        let _ = tx.send(picked);
    })
    .map_err(|e| format!("无法打开文件对话框：{e}"))?;
    rx.recv()
        .map_err(|e| format!("文件对话框中断：{e}"))?
        .ok_or_else(|| "已取消导入".to_string())
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

/// The `meeting-processing-progress` event payload: which stage the offline
/// pipeline is on, its per-stage sub-progress (for the loop-heavy stages), and
/// an overall percent. The detail page filters by `meetingId` and renders a
/// friendly stage label + progress bar. Field names are camelCase for the JS
/// listener.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingProcessingProgress {
    meeting_id: String,
    stage: &'static str,
    track: Option<&'static str>,
    stage_index: u32,
    stage_total: u32,
    done: u32,
    total: u32,
    stage_percent: Option<f32>,
    overall_percent: f32,
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
    let (
        asr_engine,
        corrector,
        minutes_model,
        cleanup_transcript,
        echo_suppression,
        annotation_voiceprint_spread,
        auto_enroll_speakers,
    ) = {
        let state = app.state::<AppState>();
        let (
            corrector_cfg,
            cleanup_transcript,
            echo_suppression,
            annotation_voiceprint_spread,
            auto_enroll_speakers,
        ) = {
            let cfg = state
                .config
                .lock()
                .map_err(|_| "config lock poisoned".to_string())?;
            (
                cfg.corrector.clone(),
                cfg.meeting.transcript_cleanup,
                cfg.meeting.echo_suppression,
                cfg.meeting.annotation_voiceprint_spread,
                cfg.meeting.auto_enroll_speakers,
            )
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
        (
            asr_engine,
            corrector,
            minutes_model,
            cleanup_transcript,
            echo_suppression,
            annotation_voiceprint_spread,
            auto_enroll_speakers,
        )
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
        // Hide mic-track echo duplicates of remote speech (speakerphone
        // pickup) from the final transcript; multi-evidence and fail-open, so
        // headphone meetings are untouched. Config: `meeting.echo_suppression`.
        echo_suppression,
        // Cross-meeting auto-identification: match diarized speakers against
        // the local voiceprint library and auto-assign enrolled names. The
        // library lives entirely on this machine (never uploaded); on builds
        // without diarization no embeddings exist, so this is naturally inert.
        identity_dir: Some(lumen_identity::default_identity_dir()),
        // Spread manual speaker marks to unlabelled clusters by voiceprint so
        // one person's unmarked speech joins their name. Needs diarization
        // embeddings; inert without them. Config: `meeting.annotation_voiceprint_spread`.
        annotation_voiceprint_spread,
        // Enroll manually named speakers into the local identity library so
        // future meetings auto-identify them. Config: `meeting.auto_enroll_speakers`.
        auto_enroll_speakers,
        ..MeetingOptions::default()
    };

    // When no LLM is configured the minutes step is skipped (transcript-only →
    // ready). Remember that so we can leave a marker for the UI to prompt the
    // user to configure an LLM, instead of silently showing an empty 纪要 page.
    let no_llm = minutes_cfg.is_none();

    // Stream granular progress to the detail page: each stage boundary (and, for
    // the loop-heavy transcribe/cleanup stages, a throttled per-item tick) is
    // relayed as a `meeting-processing-progress` event, filtered client-side by
    // `meetingId`. Best-effort — a failed emit never affects the pipeline.
    let progress_app = app.clone();
    let emit_progress = move |p: ProcessingProgress| {
        let _ = progress_app.emit(
            "meeting-processing-progress",
            MeetingProcessingProgress {
                meeting_id: meeting_id.to_string(),
                stage: p.stage.as_str(),
                track: p.track.map(|t| t.as_str()),
                stage_index: p.stage_index,
                stage_total: p.stage_total,
                done: p.done,
                total: p.total,
                stage_percent: p.stage_percent,
                overall_percent: p.overall_percent,
            },
        );
    };

    process_meeting(
        &store,
        meeting_id,
        wav,
        system_wav,
        &diar_models,
        asr_engine.as_ref(),
        minutes_cfg.as_ref(),
        &opts,
        Some(&emit_progress),
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
    // Prefer the stored mic path; fall back to the conventional
    // `<meetings>/<id>.wav` when it is missing (older meetings interrupted
    // before the start path persisted it, or a lost store write) — the WAV is
    // still on disk under that name, so it can be salvaged instead of lost.
    let audio_path = meeting.audio_path.clone().unwrap_or_else(|| {
        default_data_dir()
            .join("meetings")
            .join(format!("{id}.wav"))
            .to_string_lossy()
            .to_string()
    });
    let wav = PathBuf::from(&audio_path);
    // A missing or truly empty (0-byte) file has nothing to salvage.
    let has_bytes = std::fs::metadata(&wav)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !has_bytes {
        fail_interrupted(app, store, meeting);
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
            emit_recovery(
                app,
                meeting,
                "recovered",
                Some(repaired.duration_seconds),
                None,
            );
            spawn_meeting_processing(app.clone(), id, wav, system_wav);
        }
        // Header-only WAV: repaired to a valid 0-length take — no audio to keep.
        Ok(_) => fail_interrupted(app, store, meeting),
        Err(e) => {
            // File exists but is not a repairable WAV (truncated / corrupt).
            let reason = format!("recording interrupted, audio unrecoverable: {e}");
            tracing::warn!(meeting_id = %id, error = %e, "crash recovery: wav unrepairable");
            if let Err(e) = store.fail_meeting(id, Some(&reason)) {
                tracing::warn!(meeting_id = %id, error = %e, "crash recovery: could not mark failed");
            }
            emit_recovery(app, meeting, "failed", None, Some(reason));
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
fn fail_interrupted(app: &AppHandle, store: &Store, meeting: &Meeting) {
    let id = meeting.id;
    tracing::info!(meeting_id = %id, "crash recovery: no salvageable audio → failed");
    if let Err(e) = store.fail_meeting(id, Some(INTERRUPTED_NO_AUDIO)) {
        tracing::warn!(meeting_id = %id, error = %e, "crash recovery: could not mark failed");
    }
    emit_recovery(
        app,
        meeting,
        "failed",
        None,
        Some(INTERRUPTED_NO_AUDIO.to_string()),
    );
}

/// Payload of the `meeting-recovery` event: the app found a recording that was
/// interrupted on a previous run (crash / kill / power loss) and either salvaged
/// it (`"recovered"`, now reprocessing) or could not (`"failed"`). The front-end
/// surfaces this so an interruption is never silent.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingRecoveryEvent {
    pub meeting_id: String,
    pub title: Option<String>,
    pub outcome: &'static str,
    pub duration_seconds: Option<f64>,
    pub reason: Option<String>,
}

fn emit_recovery(
    app: &AppHandle,
    meeting: &Meeting,
    outcome: &'static str,
    duration_seconds: Option<f64>,
    reason: Option<String>,
) {
    let event = MeetingRecoveryEvent {
        meeting_id: meeting.id.to_string(),
        title: meeting.title.clone(),
        outcome,
        duration_seconds,
        reason,
    };
    // Recovery runs on a startup background thread that can outrun the webview's
    // `listen()` registration, so buffer every outcome too: the front-end drains
    // the buffer on mount (see `take_recovery_notices`) and also listens live, so
    // a notice is never lost to that race. Keyed dedup happens client-side.
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut buf) = state.meeting_recovery_notices.lock() {
            buf.push(event.clone());
        }
    }
    let _ = app.emit("meeting-recovery", event);
}

/// Drain the buffered interrupted-recording recovery outcomes (see
/// [`emit_recovery`]). The front-end calls this once on mount to pick up any
/// notices emitted before its live listener was ready.
#[tauri::command]
pub fn take_recovery_notices(state: State<'_, AppState>) -> Vec<MeetingRecoveryEvent> {
    state
        .meeting_recovery_notices
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default()
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

struct PreparedMeetingTrim {
    mic: PathBuf,
    system: Option<PathBuf>,
    duration_seconds: f64,
}

fn prepare_meeting_trim(
    id: Uuid,
    mic_source: PathBuf,
    system_source: Option<PathBuf>,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<PreparedMeetingTrim, String> {
    let directory = mic_source
        .parent()
        .ok_or_else(|| "会议音频目录无效".to_string())?;
    let token = Uuid::new_v4().simple();
    let new_mic = directory.join(format!("{id}.trim-{token}.wav"));
    let new_system = directory.join(format!("{id}.trim-{token}.system.wav"));

    let mic_summary = copy_pcm16_wav_range(&mic_source, &new_mic, start_seconds, end_seconds)
        .map_err(|error| format!("剪辑麦克风录音失败：{error}"))?;

    let (mic_offset, system_offset) =
        read_timeline_offsets(&mic_source.with_extension("timeline.json"));
    let system_skew = system_offset.unwrap_or(mic_offset) - mic_offset;
    let mut kept_system = None;
    let mut new_system_skew = None;
    if let Some(system_source) = system_source.as_deref() {
        if let Some((local_start, local_end, kept_skew)) =
            system_trim_range(start_seconds, end_seconds, system_skew)
        {
            match copy_pcm16_wav_range(system_source, &new_system, local_start, local_end) {
                Ok(_) => {
                    kept_system = Some(new_system.clone());
                    // Usually both kept WAVs now begin together. When the
                    // selected range starts before the system capture did,
                    // preserve the remaining positive start skew.
                    new_system_skew = Some(kept_skew);
                }
                Err(WavRangeError::InvalidRange { .. }) => {
                    // The optional system track has no overlap with the kept
                    // interval; the mic track remains authoritative.
                }
                Err(error) => {
                    remove_mic_audio_artifacts(&new_mic);
                    remove_audio_file(&new_system);
                    return Err(format!("剪辑系统音频失败：{error}"));
                }
            }
        }
    }

    write_timeline_sidecar(
        &new_mic.with_extension("timeline.json"),
        &MeetingTimeline {
            mic_offset_seconds: 0.0,
            system_offset_seconds: new_system_skew,
            t0_wall_clock: chrono::Utc::now().to_rfc3339(),
        },
    );
    Ok(PreparedMeetingTrim {
        mic: new_mic,
        system: kept_system,
        duration_seconds: mic_summary.duration_seconds,
    })
}

/// Destructively keep one continuous range of a finished meeting and discard
/// everything before/after it. New WAVs are fully prepared first; only then is
/// the database atomically switched to the new sources and its time-aligned
/// derived data cleared. The old files are removed after that commit and the
/// normal offline pipeline regenerates the transcript and minutes.
#[tauri::command]
pub async fn trim_meeting_audio(
    app: AppHandle,
    state: State<'_, AppState>,
    meeting_id: String,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<f64, String> {
    const MIN_KEEP_SECONDS: f64 = 1.0;
    const RANGE_EPSILON_SECONDS: f64 = 0.25;

    let id = parse_id(&meeting_id, "meeting")?;
    let _audio_edit = state.meeting_audio_edit.lock().await;
    let meeting = with_store(&state, |store| {
        store
            .get_meeting(id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "会议不存在".to_string())
    })?;
    if meeting.status != MeetingStatus::Ready {
        return Err("只能剪辑已经处理完成的会议".to_string());
    }
    let duration = meeting
        .duration_seconds
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "会议没有可剪辑的有效时长".to_string())?;
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || start_seconds < 0.0
        || end_seconds - start_seconds < MIN_KEEP_SECONDS
        || end_seconds > duration + RANGE_EPSILON_SECONDS
    {
        return Err("请选择至少 1 秒、且位于录音时长内的保留区间".to_string());
    }
    if start_seconds <= RANGE_EPSILON_SECONDS && duration - end_seconds <= RANGE_EPSILON_SECONDS {
        return Err("当前选择包含整段录音，没有需要剪掉的内容".to_string());
    }

    let old_mic = owned_meeting_wav(
        meeting
            .audio_path
            .as_deref()
            .ok_or_else(|| "会议没有录音文件".to_string())?,
    )?;
    let old_system = match meeting.system_audio_path.as_deref() {
        Some(path) if Path::new(path).exists() => Some(owned_meeting_wav(path)?),
        _ => None,
    };
    let mic_for_copy = old_mic.clone();
    let system_for_copy = old_system.clone();

    let prepared = tauri::async_runtime::spawn_blocking(move || {
        prepare_meeting_trim(
            id,
            mic_for_copy,
            system_for_copy,
            start_seconds,
            end_seconds,
        )
    })
    .await
    .map_err(|error| format!("剪辑任务异常结束：{error}"))??;

    let new_mic_string = prepared.mic.to_string_lossy().to_string();
    let new_system_string = prepared
        .system
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let replaced = with_store(&state, |store| {
        store
            .replace_meeting_audio_after_trim(
                id,
                &new_mic_string,
                new_system_string.as_deref(),
                prepared.duration_seconds,
            )
            .map_err(|error| error.to_string())
    });
    match replaced {
        Ok(true) => {}
        Ok(false) => {
            remove_mic_audio_artifacts(&prepared.mic);
            if let Some(system) = prepared.system.as_deref() {
                remove_audio_file(system);
            }
            return Err("会议状态已经变化，请刷新后重试".to_string());
        }
        Err(error) => {
            remove_mic_audio_artifacts(&prepared.mic);
            if let Some(system) = prepared.system.as_deref() {
                remove_audio_file(system);
            }
            return Err(error);
        }
    }

    remove_mic_audio_artifacts(&old_mic);
    if let Some(system) = old_system.as_deref() {
        remove_audio_file(system);
    }
    spawn_meeting_processing(app, id, prepared.mic, prepared.system);
    Ok(prepared.duration_seconds)
}

/// Delete a meeting and everything attached to it. The store cascade removes the
/// segments, speakers, and summaries; this command additionally deletes the
/// meeting's recorded WAV from disk (best-effort — a missing file is fine, and a
/// remove error is logged but does not fail the delete, since the row is already
/// gone). Returns `true` if a meeting row was deleted.
#[tauri::command]
pub async fn delete_meeting(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<bool, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    let _audio_edit = state.meeting_audio_edit.lock().await;
    // Fetch both source paths and delete the row in one SQLite transaction.
    // The outer operation lock keeps trim's file preparation/DB swap/cleanup
    // from interleaving with the subsequent best-effort file removal.
    let deleted_paths = with_store(&state, |s| {
        s.delete_meeting_with_audio_paths(id)
            .map_err(|e| e.to_string())
    })?;
    let Some((audio_path, system_audio_path)) = deleted_paths else {
        return Ok(false);
    };
    if let Some(mic) = audio_path.as_deref() {
        match owned_meeting_wav(mic) {
            Ok(owned) => remove_mic_audio_artifacts(&owned),
            Err(error) => {
                tracing::warn!(path = %mic, %error, "skipping unowned meeting audio on delete")
            }
        }
    }
    if let Some(system) = system_audio_path.as_deref() {
        match owned_meeting_wav(system) {
            Ok(owned) => remove_audio_file(&owned),
            Err(error) => {
                tracing::warn!(path = %system, %error, "skipping unowned system audio on delete")
            }
        }
    }
    Ok(true)
}

// ----- live speaker annotations (L2) -------------------------------------
//
// While a meeting records, the user can mark "who is speaking" on individual
// live caption lines. Speaker rows do not exist yet (the offline pipeline
// creates them after stop), so each mark is persisted immediately as a
// `live_annotations` row anchored to a time range on the meeting's unified
// timeline plus its capture track. The offline pipeline reconciles them into
// speaker attribution after stop (manual always wins).

/// Annotate one live caption line with a speaker **boundary** on the meeting's
/// unified timeline. Appends a new `live_annotations` row (two boundaries at the
/// same `start_seconds` are resolved last-write-wins by `created_at` at
/// reconciliation time) and returns it. `segment_id` is the transient live
/// segment id (e.g. `mic-3`) — used for tracing only, never persisted. The
/// boundary opens a range from `start_seconds` until the next boundary on the
/// same track (the user's real pattern: one person speaks for a long stretch,
/// occasionally interrupted). When `unassigned` is set this is a "无" boundary —
/// from here on no manual speaker — and `identity_id`/`display_name` are
/// ignored; otherwise `display_name` is required (the name snapshot shown on the
/// chip) and `identity_id` optionally links an enrolled voiceprint identity.
/// `end_seconds` is retained for provenance only and is not used as a range end.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // the IPC payload is exactly these fields
pub fn annotate_live_segment(
    state: State<'_, AppState>,
    meeting_id: String,
    segment_id: String,
    start_seconds: f64,
    end_seconds: Option<f64>,
    channel: String,
    identity_id: Option<String>,
    display_name: String,
    unassigned: bool,
) -> Result<LiveAnnotation, String> {
    let meeting = parse_id(&meeting_id, "meeting")?;
    if !start_seconds.is_finite() || end_seconds.is_some_and(|end| !end.is_finite()) {
        return Err("invalid annotation time range".to_string());
    }
    let channel = SegmentChannel::from_str_or_mic(&channel);
    // A "无" boundary carries no name or identity; a named boundary requires a
    // non-empty name and may link an enrolled identity.
    let annotation = if unassigned {
        let mut a = LiveAnnotation::none_boundary(meeting, start_seconds, channel);
        a.end_seconds = end_seconds;
        a
    } else {
        let identity = identity_id
            .as_deref()
            .map(|id| parse_id(id, "identity"))
            .transpose()?;
        let name = display_name.trim();
        if name.is_empty() {
            return Err("说话人名字不能为空".to_string());
        }
        LiveAnnotation::new(meeting, start_seconds, end_seconds, channel, identity, name)
    };
    with_store(&state, |s| {
        s.add_live_annotation(&annotation)
            .map_err(|e| e.to_string())
    })?;
    // L3.5: let the running live worker seed a session voiceprint from this
    // annotation's audio, so later utterances by the same (unregistered)
    // person auto-label for the rest of the recording. Purely advisory —
    // silently a no-op when no worker is running. A "无" boundary carries no
    // speaker, so it never seeds a voiceprint.
    if !annotation.unassigned {
        state.meeting_live.notify_annotation(
            &meeting_id,
            crate::meeting_live::AnnotationNotice::Annotated {
                channel: annotation.channel.as_str().to_string(),
                start_seconds: annotation.start_seconds,
                end_seconds: annotation.end_seconds,
                identity_id: annotation.identity_id,
                display_name: annotation.display_name.clone(),
            },
        );
    }
    // Ids and times only — the annotated name is PII.
    tracing::info!(
        meeting_id = %meeting,
        live_segment = %segment_id,
        annotation_id = %annotation.id,
        start_seconds,
        "live speaker annotation saved"
    );
    Ok(annotation)
}

/// List a meeting's live annotations, oldest first — used by the recording
/// view to restore chip labels after a remount.
#[tauri::command]
pub fn list_live_annotations(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<Vec<LiveAnnotation>, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    with_store(&state, |s| {
        s.list_live_annotations(id).map_err(|e| e.to_string())
    })
}

/// Delete one live annotation (the chip's "清除" action). Returns `true` if a
/// row was deleted.
#[tauri::command]
pub fn delete_live_annotation(
    state: State<'_, AppState>,
    annotation_id: String,
) -> Result<bool, String> {
    let id = parse_id(&annotation_id, "annotation")?;
    // Read the row before deleting so the live worker can retract the
    // matching session voiceprint samples (L3.5). Best-effort: a failed read
    // never blocks the delete.
    let annotation = with_store(&state, |s| {
        s.get_live_annotation(id).map_err(|e| e.to_string())
    })
    .unwrap_or(None);
    let deleted = with_store(&state, |s| {
        s.delete_live_annotation(id).map_err(|e| e.to_string())
    })?;
    if deleted {
        if let Some(annotation) = annotation {
            state.meeting_live.notify_annotation(
                &annotation.meeting_id.to_string(),
                crate::meeting_live::AnnotationNotice::Cleared {
                    display_name: annotation.display_name,
                },
            );
        }
    }
    Ok(deleted)
}

/// Rename a mistyped live-annotation name across a whole meeting (the chip's
/// "重命名" action): every caption line marked `old_name` becomes `new_name`.
/// Returns the number of annotations updated.
#[tauri::command]
pub fn rename_live_annotations(
    state: State<'_, AppState>,
    meeting_id: String,
    old_name: String,
    new_name: String,
) -> Result<u64, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    let new_trimmed = new_name.trim().to_string();
    let changed = with_store(&state, |s| {
        s.rename_live_annotations(id, &old_name, &new_trimmed)
            .map_err(|e| e.to_string())
    })?;
    // Keep the live worker's session voiceprints in step (recording only): relabel
    // the group so the corrected name keeps matching instead of the old one
    // resurfacing in future live chips. No-op when no recording/worker is active.
    if changed > 0 {
        state.meeting_live.notify_annotation(
            &meeting_id,
            crate::meeting_live::AnnotationNotice::Renamed {
                old_name,
                new_name: new_trimmed,
            },
        );
    }
    Ok(changed)
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
    /// Every voiceprint sample, **oldest-first** — the same order
    /// `remove_speaker_sample` indexes into, so the manager can prune a
    /// specific recording by its position here.
    pub samples: Vec<EnrolledSampleDto>,
}

/// One voiceprint sample for the manager UI (the embedding never crosses IPC).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrolledSampleDto {
    pub enrolled_at: String,
    pub voiced_ms: u64,
    pub source_meeting_id: Option<String>,
    /// A short human label (e.g. what was said) for a recognizable list.
    pub source_label: Option<String>,
    /// Whether this sample maps to a playable recording (the file path itself
    /// stays server-side; playback goes through `read_voiceprint_sample_audio`).
    pub has_audio: bool,
}

impl From<&lumen_identity::EnrolledIdentity> for EnrolledSpeakerDto {
    fn from(identity: &lumen_identity::EnrolledIdentity) -> Self {
        // Identities hold multiple samples; the list header shows the most
        // recent one's metadata, the samples array drives per-sample pruning.
        let latest = identity.latest_sample();
        Self {
            id: identity.id.to_string(),
            name: identity.name.clone(),
            enrolled_at: latest
                .map(|s| s.enrolled_at.to_rfc3339())
                .unwrap_or_default(),
            source_meeting_id: latest
                .and_then(|s| s.source_meeting_id)
                .map(|id| id.to_string()),
            samples: identity
                .samples
                .iter()
                .map(|s| EnrolledSampleDto {
                    enrolled_at: s.enrolled_at.to_rfc3339(),
                    voiced_ms: s.voiced_ms,
                    source_meeting_id: s.source_meeting_id.map(|id| id.to_string()),
                    source_label: s.source_label.clone(),
                    has_audio: s
                        .source_audio_path
                        .as_deref()
                        .is_some_and(|p| std::path::Path::new(p).is_file()),
                })
                .collect(),
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

/// Read one meeting speaker's stored centroid plus its total voiced duration,
/// applying the same "no embedding" / "too short" gates `lumen_identity::enroll`
/// enforces so callers get an actionable message *before* anything is written.
/// Shared by direct enrollment and auto-enroll conflict resolution.
fn fetch_speaker_centroid(
    state: &State<'_, AppState>,
    meeting: Uuid,
    speaker_uuid: Uuid,
) -> Result<(lumen_core::Speaker, Vec<f32>, u64), String> {
    let (speaker, embedding, voiced_ms) = with_store(state, |s| {
        let speaker = s
            .list_speakers(meeting)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|sp| sp.id == speaker_uuid)
            .ok_or_else(|| "speaker not found in this meeting".to_string())?;
        let embedding = s
            .get_speaker_embedding(speaker_uuid)
            .map_err(|e| e.to_string())?;
        // Total voiced duration = the sum of this speaker's segment spans,
        // the same turns the centroid embedding was computed from.
        let voiced_ms: u64 = s
            .list_segments(meeting)
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|seg| seg.speaker_id == Some(speaker_uuid))
            .map(|seg| ((seg.end_seconds - seg.start_seconds).max(0.0) * 1000.0).round() as u64)
            .sum();
        Ok((speaker, embedding, voiced_ms))
    })?;
    let embedding = embedding.ok_or_else(|| {
        "该说话人没有声纹数据（此会议在声纹功能之前转录，重新转录后即可注册）".to_string()
    })?;
    if voiced_ms < lumen_identity::MIN_VOICED_MS {
        return Err(format!(
            "该说话人语音太短，无法注册声纹（有效语音约 {:.1} 秒，至少需要 {} 秒）",
            voiced_ms as f64 / 1000.0,
            lumen_identity::MIN_VOICED_MS / 1000
        ));
    }
    Ok((speaker, embedding, voiced_ms))
}

/// Enroll one confirmed meeting speaker into the local voiceprint library
/// (repeat enrollments of the same person accumulate samples, making future
/// auto-identification more robust). The name defaults to the speaker's
/// `display_name`; passing `name` overrides it (and confirms the speaker with
/// that name when it was still unnamed). Fails when the speaker has no stored
/// embedding (meeting transcribed before voiceprints existed → re-run
/// transcription first) or spoke for less than the minimum voiced duration
/// (`lumen_identity::MIN_VOICED_MS`).
#[tauri::command]
pub fn enroll_speaker(
    state: State<'_, AppState>,
    meeting_id: String,
    speaker_id: String,
    name: Option<String>,
) -> Result<EnrolledSpeakerDto, String> {
    let meeting = parse_id(&meeting_id, "meeting")?;
    let speaker_uuid = parse_id(&speaker_id, "speaker")?;

    let (speaker, embedding, voiced_ms) = fetch_speaker_centroid(&state, meeting, speaker_uuid)?;

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
        .enroll(&name, &embedding, voiced_ms, Some(meeting))
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

/// Rename an enrolled identity (its samples and auto-identification history are
/// kept). Renaming onto another identity's name is rejected — use
/// [`merge_enrolled_speakers`] for "these two are the same person".
#[tauri::command]
pub fn rename_enrolled_speaker(
    identity_id: String,
    name: String,
) -> Result<EnrolledSpeakerDto, String> {
    let id = parse_id(&identity_id, "identity")?;
    let mut identities = open_identity_store()?;
    let renamed = identities.rename(id, &name).map_err(|e| match e {
        lumen_identity::IdentityError::NameExists(n) => {
            format!("已存在名为“{n}”的声纹，若是同一个人请改用“合并”")
        }
        lumen_identity::IdentityError::EmptyName => "名字不能为空".to_string(),
        other => format!("rename: {other}"),
    })?;
    Ok(EnrolledSpeakerDto::from(&renamed))
}

/// Merge the `from` identity into `into` (all of `from`'s voiceprint samples
/// move onto `into`, then `from` is deleted). Resolves "same voice enrolled
/// under two names". The surviving identity keeps `into`'s name and id.
#[tauri::command]
pub fn merge_enrolled_speakers(
    from_id: String,
    into_id: String,
) -> Result<EnrolledSpeakerDto, String> {
    let from = parse_id(&from_id, "identity")?;
    let into = parse_id(&into_id, "identity")?;
    let mut identities = open_identity_store()?;
    let merged = identities
        .merge(from, into)
        .map_err(|e| format!("merge: {e}"))?;
    Ok(EnrolledSpeakerDto::from(&merged))
}

/// Delete a single voiceprint sample from an identity by its index in the
/// (oldest-first) sample list. Removing the last sample deletes the whole
/// identity. Returns the updated identity, or `None` when it no longer exists.
#[tauri::command]
pub fn remove_speaker_sample(
    identity_id: String,
    sample_index: usize,
) -> Result<Option<EnrolledSpeakerDto>, String> {
    let id = parse_id(&identity_id, "identity")?;
    let mut identities = open_identity_store()?;
    let removed = identities
        .remove_sample(id, sample_index)
        .map_err(|e| format!("remove sample: {e}"))?;
    if !removed {
        return Ok(None);
    }
    Ok(identities
        .list()
        .iter()
        .find(|i| i.id.to_string() == identity_id)
        .map(EnrolledSpeakerDto::from))
}

/// Return the raw WAV bytes of one voiceprint sample's source recording, so the
/// UI can play it back and the user can confirm the sample is really them. The
/// file path is taken from the *stored* sample (never the client) and must
/// still exist on disk. Fails when the sample has no playable source.
#[tauri::command]
pub fn read_voiceprint_sample_audio(
    identity_id: String,
    sample_index: usize,
) -> Result<tauri::ipc::Response, String> {
    let id = parse_id(&identity_id, "identity")?;
    let identities = open_identity_store()?;
    let path = identities
        .list()
        .iter()
        .find(|i| i.id == id)
        .and_then(|i| i.samples.get(sample_index))
        .and_then(|s| s.source_audio_path.clone())
        .ok_or_else(|| "该样本没有可播放的录音".to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("读取录音失败：{e}"))?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// One queued auto-enroll conflict for the manager UI: a meeting labelled a
/// speaker `labelName`, but that voice matched the already-enrolled
/// `existingName` (cosine `score`), so the enrollment was withheld.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollConflictDto {
    pub id: String,
    pub meeting_id: String,
    pub speaker_id: String,
    pub label_name: String,
    pub existing_name: String,
    pub score: f32,
    pub created_at: String,
}

impl From<&lumen_store::EnrollConflictRecord> for EnrollConflictDto {
    fn from(record: &lumen_store::EnrollConflictRecord) -> Self {
        Self {
            id: record.id.to_string(),
            meeting_id: record.meeting_id.to_string(),
            speaker_id: record.speaker_id.to_string(),
            label_name: record.label_name.clone(),
            existing_name: record.existing_name.clone(),
            score: record.score,
            created_at: record.created_at.clone(),
        }
    }
}

/// Every unresolved same-voice/different-name auto-enroll conflict, newest
/// first. Surfaced in the voiceprint manager for the user to resolve.
#[tauri::command]
pub fn list_enroll_conflicts(state: State<'_, AppState>) -> Result<Vec<EnrollConflictDto>, String> {
    with_store(&state, |s| {
        Ok(s.list_unresolved_enroll_conflicts()
            .map_err(|e| e.to_string())?
            .iter()
            .map(EnrollConflictDto::from)
            .collect())
    })
}

/// Resolve one auto-enroll conflict. When `enroll_as` names a person, the
/// conflicting speaker's centroid is enrolled under that name (either "同一个
/// 人" → the existing name, or "确实是另一个人" → the meeting's label); when it
/// is `None` the conflict is simply dismissed.
///
/// The meeting/speaker to act on come from the **stored** conflict record, not
/// the client, and the row is *claimed* (atomically flipped to resolved) before
/// enrolling: an already-resolved or unknown id is a no-op, and only the caller
/// that wins the claim enrolls — so a stale or concurrent request can never add
/// a duplicate sample.
#[tauri::command]
pub fn resolve_enroll_conflict(
    state: State<'_, AppState>,
    conflict_id: String,
    enroll_as: Option<String>,
) -> Result<(), String> {
    let conflict = parse_id(&conflict_id, "conflict")?;
    let Some(record) = with_store(&state, |s| {
        s.get_enroll_conflict(conflict).map_err(|e| e.to_string())
    })?
    else {
        return Ok(()); // already resolved or unknown — nothing to do
    };
    // Claim first, so at most one caller proceeds to enroll.
    let claimed = with_store(&state, |s| {
        s.resolve_enroll_conflict(conflict)
            .map_err(|e| e.to_string())
    })?;
    if !claimed {
        return Ok(());
    }
    if let Some(name) = enroll_as
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        let (_speaker, embedding, voiced_ms) =
            fetch_speaker_centroid(&state, record.meeting_id, record.speaker_id)?;
        let mut identities = open_identity_store()?;
        identities
            .enroll(name, &embedding, voiced_ms, Some(record.meeting_id))
            .map_err(|e| format!("enroll: {e}"))?;
    }
    tracing::info!(conflict_id = %conflict, "auto-enroll conflict resolved");
    Ok(())
}

/// Read the enrolled identity marked as *the user themself* ("这是我"), if
/// any. Rendering hint only: the UI shows "我" when attribution matches it.
#[tauri::command]
pub fn get_self_identity(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(cfg.meeting.self_identity_id.clone())
}

/// Set (or clear, with `None`) which enrolled identity is the user themself.
/// Validates the id shape; pointing at a since-removed identity is harmless
/// (it simply never matches). Returns the stored value.
#[tauri::command]
pub fn set_self_identity(
    state: State<'_, AppState>,
    identity_id: Option<String>,
) -> Result<Option<String>, String> {
    let identity_id = match identity_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(value) => Some(parse_id(value, "identity")?.to_string()),
    };
    let mut cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    cfg.meeting.self_identity_id = identity_id.clone();
    cfg.save()?;
    Ok(identity_id)
}

/// How many recent dictation recordings self-enrollment scans, and how many
/// good voiceprint samples it stops at — enough for a robust multi-sample "我"
/// identity without walking the whole history.
#[cfg(target_os = "macos")]
const SELF_ENROLL_SCAN_LIMIT: u32 = 40;
#[cfg(target_os = "macos")]
const SELF_ENROLL_TARGET_SAMPLES: usize = 6;

/// Outcome of a self-enrollment run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfEnrollDto {
    /// The identity samples were added to (`None` only when none qualified).
    pub identity_id: Option<String>,
    /// The name enrolled under (default "我").
    pub name: String,
    /// Voiceprint samples added this run.
    pub enrolled: usize,
    /// Recordings examined.
    pub scanned: usize,
    /// Recordings skipped (too little clear speech, unreadable, or rejected).
    pub skipped: usize,
}

/// Live progress of a self-enrollment run, emitted as `self-enroll-progress`
/// so the UI can show activity instead of an opaque wait.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelfEnrollProgress {
    scanned: usize,
    enrolled: usize,
    target: usize,
}

/// Register the user's own voice ("我") from the dictation recordings they
/// already made: scan the most recent recordings, embed each one's voiced
/// speech into a voiceprint sample, enroll them under `name` (default "我"),
/// and mark that identity as *self* so meetings auto-label them "我".
///
/// Recordings with less than the voiced-speech floor are skipped; the run stops
/// once enough samples are collected for a robust identity. Fails only when no
/// recording had enough clear speech, or the voiceprint model is unavailable.
#[tauri::command]
pub fn enroll_self_from_recordings(
    app: AppHandle,
    state: State<'_, AppState>,
    name: Option<String>,
) -> Result<SelfEnrollDto, String> {
    #[cfg(target_os = "macos")]
    {
        let emb_model = lumen_asr::lumen_models_dir().join("diar").join("emb.onnx");
        if !emb_model.is_file() {
            return Err("缺少声纹模型（diar/emb.onnx），无法从录音注册声纹".to_string());
        }
        let name = name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("我")
            .to_string();

        let mut embedder = lumen_meeting::LiveVoiceprintEmbedder::load(&emb_model)?;
        let sessions = with_store(&state, |s| {
            s.list_sessions(SELF_ENROLL_SCAN_LIMIT)
                .map_err(|e| e.to_string())
        })?;
        let mut identities = open_identity_store()?;
        // Recordings already sampled for this name — a re-scan tops up with new
        // recordings instead of re-embedding the same ones.
        let already_sampled: std::collections::HashSet<String> = identities
            .list()
            .iter()
            .find(|i| i.name == name)
            .map(|i| {
                i.samples
                    .iter()
                    .filter_map(|s| s.source_audio_path.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Whether this name already had an identity: decides if a later rollback
        // can safely delete it (only when this run created it from scratch).
        let existed_before = identities.list().iter().any(|i| i.name == name);

        let mut dto = SelfEnrollDto {
            identity_id: None,
            name: name.clone(),
            enrolled: 0,
            scanned: 0,
            skipped: 0,
        };
        for session in sessions {
            if dto.enrolled >= SELF_ENROLL_TARGET_SAMPLES {
                break;
            }
            let Some(path) = session.audio_path.clone() else {
                continue;
            };
            if already_sampled.contains(&path) {
                continue; // already a sample — don't re-add
            }
            dto.scanned += 1;
            // Emit activity before the heavy embed so the UI shows progress
            // rather than an opaque wait.
            let _ = app.emit(
                "self-enroll-progress",
                SelfEnrollProgress {
                    scanned: dto.scanned,
                    enrolled: dto.enrolled,
                    target: SELF_ENROLL_TARGET_SAMPLES,
                },
            );
            // A missing/corrupt recording is an expected per-file condition —
            // skip it. Only genuinely-rejected audio counts as skipped below;
            // model / store failures propagate rather than masquerading as "no
            // clear speech".
            let Ok((samples, sample_rate)) =
                crate::session_debug::read_wav_mono_f32(std::path::Path::new(&path))
            else {
                dto.skipped += 1;
                continue;
            };
            match lumen_meeting::embed_voiced_region(&mut embedder, &samples, sample_rate)
                .map_err(|e| format!("voiceprint model failed: {e}"))?
            {
                Some((embedding, voiced_ms)) => {
                    // A dictation session, not a meeting: record the WAV path so
                    // the sample plays back, and the transcript as its label.
                    let label = session
                        .corrected
                        .as_deref()
                        .or(session.asr_raw.as_deref())
                        .map(|t| t.trim().chars().take(40).collect::<String>())
                        .filter(|t| !t.is_empty());
                    let identity = identities
                        .enroll_sample(
                            &name,
                            &embedding,
                            voiced_ms,
                            lumen_identity::SampleSource {
                                meeting_id: None,
                                audio_path: Some(path),
                                label,
                            },
                        )
                        .map_err(|e| format!("enroll: {e}"))?;
                    dto.enrolled += 1;
                    dto.identity_id = Some(identity.id.to_string());
                }
                // Too little voiced speech in this recording — expected, skip.
                None => dto.skipped += 1,
            }
        }

        if dto.enrolled == 0 {
            // Nothing scanned means every recent recording was already a sample;
            // otherwise the recordings had too little clear speech.
            return Err(if dto.scanned == 0 {
                "没有新的听写录音可以补充声纹，先去做几次听写再回来".to_string()
            } else {
                "最近的听写录音里没有足够清晰的语音来注册声纹，多说几句再录一段听写试试".to_string()
            });
        }
        // Mark the freshly enrolled identity as the user themself. If persisting
        // the config fails and this run *created* the identity, roll the library
        // back so we don't leave a half-registered "我"; when it merely added
        // samples to a pre-existing identity, keep them (they're bounded by the
        // sample cap) and surface that the mark, not the enrollment, failed.
        if let Some(id) = dto.identity_id.clone() {
            let mut cfg = state
                .config
                .lock()
                .map_err(|_| "config lock poisoned".to_string())?;
            cfg.meeting.self_identity_id = Some(id.clone());
            if let Err(error) = cfg.save() {
                drop(cfg);
                if !existed_before {
                    if let Ok(uuid) = parse_id(&id, "identity") {
                        let _ = identities.remove(uuid);
                    }
                    return Err(format!("保存设置失败，已撤销注册：{error}"));
                }
                return Err(format!("声纹样本已注册，但标记「我」失败：{error}"));
            }
        }
        tracing::info!(
            enrolled = dto.enrolled,
            scanned = dto.scanned,
            "self-enrolled from dictation recordings"
        );
        Ok(dto)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, state, name);
        Err("当前构建不支持声纹注册".to_string())
    }
}

/// One retroactively re-identified speaker (cluster label → enrolled name).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReidentifyHitDto {
    pub label: String,
    pub name: String,
    pub score: f32,
}

/// Outcome of re-running voiceprint matching over a stored meeting.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReidentifyDto {
    /// Speakers newly named this run.
    pub updated: Vec<ReidentifyHitDto>,
    /// Still-unnamed speakers that were eligible to match.
    pub examined: usize,
}

/// Retroactively re-identify a stored meeting's speakers against the *current*
/// identity library ("回溯重认"): fill any still-unnamed 说话人N whose saved
/// centroid now matches an enrolled voiceprint, using the same policy as
/// processing-time auto-identification. Manual names (and names from an earlier
/// run) are never overridden. Uses the meeting's already-stored centroids — no
/// re-diarization or re-transcription — so it is fast and non-destructive.
#[tauri::command]
pub fn reidentify_meeting(
    state: State<'_, AppState>,
    meeting_id: String,
) -> Result<ReidentifyDto, String> {
    let id = parse_id(&meeting_id, "meeting")?;
    let (mut speakers, centroids, voiced) = with_store(&state, |s| {
        let speakers = s.list_speakers(id).map_err(|e| e.to_string())?;
        let segments = s.list_segments(id).map_err(|e| e.to_string())?;
        let mut centroids = std::collections::BTreeMap::new();
        let mut voiced = std::collections::BTreeMap::new();
        for speaker in &speakers {
            if let Some(embedding) = s
                .get_speaker_embedding(speaker.id)
                .map_err(|e| e.to_string())?
            {
                centroids.insert(speaker.id, embedding);
            }
            let ms: u64 = segments
                .iter()
                .filter(|seg| seg.speaker_id == Some(speaker.id))
                .map(|seg| ((seg.end_seconds - seg.start_seconds).max(0.0) * 1000.0).round() as u64)
                .sum();
            voiced.insert(speaker.id, ms);
        }
        Ok((speakers, centroids, voiced))
    })?;
    let examined = speakers.iter().filter(|s| s.display_name.is_none()).count();

    let identities = open_identity_store()?;
    let hits = lumen_meeting::reidentify_speakers(&mut speakers, &centroids, &voiced, &identities);

    // Persist only the speakers that changed.
    let changed: std::collections::HashSet<&str> = hits.iter().map(|h| h.label.as_str()).collect();
    with_store(&state, |s| {
        for speaker in &speakers {
            if changed.contains(speaker.label.as_str()) {
                s.upsert_speaker(speaker).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    })?;
    tracing::info!(meeting_id = %id, updated = hits.len(), "retroactive re-identification");
    Ok(ReidentifyDto {
        updated: hits
            .into_iter()
            .map(|h| ReidentifyHitDto {
                label: h.label,
                name: h.name,
                score: h.score,
            })
            .collect(),
        examined,
    })
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

#[cfg(test)]
mod tests {
    use super::{
        combine_track_silence_seconds, merge_attendees_into_notes, owned_meeting_wav_in,
        prepare_imported_meeting_audio, prepare_meeting_trim, read_timeline_offsets,
        remove_mic_audio_artifacts, system_trim_range, write_timeline_sidecar, MeetingRecordingDto,
        MeetingRecordingOwner, MeetingTimeline, SilenceWatchdogAction, SilenceWatchdogState,
    };
    use lumen_core::MeetingStatus;

    fn write_test_wav(path: &std::path::Path, sample_rate: u32, samples: usize) {
        let mut sink = lumen_asr::WavSink::create(path, sample_rate).unwrap();
        sink.write_samples(&vec![0.2; samples]).unwrap();
        sink.finalize().unwrap();
    }

    fn recording_result(id: uuid::Uuid) -> MeetingRecordingDto {
        MeetingRecordingDto {
            id: id.to_string(),
            audio_path: format!("/meetings/{id}.wav"),
            duration_seconds: 12.0,
            sample_rate: 16_000,
            status: "processing".into(),
        }
    }

    #[test]
    fn stale_stop_cannot_take_ownership_from_a_new_recording() {
        let old = uuid::Uuid::new_v4();
        let new = uuid::Uuid::new_v4();
        let mut owner = MeetingRecordingOwner::default();
        owner.started(old);
        owner.completed(old, recording_result(old));
        owner.ensure_startable().unwrap();
        owner.started(new);

        let replay = owner.authorize_stop(old).unwrap().unwrap();
        assert_eq!(replay.id, old.to_string());
        assert_eq!(owner.active_id, Some(new));
        assert!(owner.authorize_stop(new).unwrap().is_none());
    }

    #[test]
    fn unrelated_stop_is_rejected_without_changing_the_active_owner() {
        let active = uuid::Uuid::new_v4();
        let unrelated = uuid::Uuid::new_v4();
        let mut owner = MeetingRecordingOwner::default();
        owner.started(active);

        assert!(owner.authorize_stop(unrelated).is_err());
        assert_eq!(owner.active_id, Some(active));
    }

    #[test]
    fn system_trim_range_preserves_or_removes_capture_start_skew() {
        let assert_range = |actual: Option<(f64, f64, f64)>, expected: (f64, f64, f64)| {
            let actual = actual.expect("system range");
            assert!((actual.0 - expected.0).abs() < 1e-9);
            assert!((actual.1 - expected.1).abs() < 1e-9);
            assert!((actual.2 - expected.2).abs() < 1e-9);
        };
        assert_range(system_trim_range(10.0, 20.0, 0.4), (9.6, 19.6, 0.0));
        assert_range(system_trim_range(0.1, 1.0, 0.4), (0.0, 0.6, 0.3));
        assert_eq!(system_trim_range(0.0, 0.2, 0.4), None);
        assert_range(system_trim_range(0.0, 1.0, -0.2), (0.2, 1.2, 0.0));
    }

    #[test]
    fn removing_mic_audio_also_removes_every_derived_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let mic = directory.path().join("meeting.wav");
        let timeline = mic.with_extension("timeline.json");
        let echo = directory.path().join("meeting.echo_suppression.json");
        for path in [&mic, &timeline, &echo] {
            std::fs::write(path, b"fixture").unwrap();
        }

        remove_mic_audio_artifacts(&mic);
        assert!(!mic.exists());
        assert!(!timeline.exists());
        assert!(!echo.exists());
    }

    #[test]
    fn owned_meeting_wav_rejects_files_outside_or_below_the_meetings_root() {
        let directory = tempfile::tempdir().unwrap();
        let meetings = directory.path().join("meetings");
        let nested = meetings.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let owned = meetings.join("owned.wav");
        let outside = directory.path().join("outside.wav");
        let nested_wav = nested.join("nested.wav");
        for path in [&owned, &outside, &nested_wav] {
            std::fs::write(path, b"fixture").unwrap();
        }

        assert_eq!(
            owned_meeting_wav_in(&meetings, &owned).unwrap(),
            owned.canonicalize().unwrap()
        );
        assert!(owned_meeting_wav_in(&meetings, &outside).is_err());
        assert!(owned_meeting_wav_in(&meetings, &nested_wav).is_err());
    }

    #[test]
    fn imported_wav_copies_into_the_meetings_dir_as_processing() {
        let directory = tempfile::tempdir().unwrap();
        let meetings = directory.path().join("meetings");
        let source = directory.path().join("standup.wav");
        std::fs::write(&source, b"RIFF....WAVEfmt ").unwrap();

        let prepared = prepare_imported_meeting_audio(&source, &meetings).unwrap();
        assert_eq!(prepared.meeting.status, MeetingStatus::Processing);
        assert_eq!(prepared.meeting.title.as_deref(), Some("standup"));
        assert_eq!(
            prepared.wav,
            meetings.join(format!("{}.wav", prepared.meeting.id))
        );
        assert_eq!(std::fs::read(&prepared.wav).unwrap(), b"RIFF....WAVEfmt ");
    }

    #[test]
    fn imported_unsupported_extension_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("notes.txt");
        std::fs::write(&source, b"not audio").unwrap();
        let error = prepare_imported_meeting_audio(&source, directory.path()).unwrap_err();
        assert!(error.contains("wav"));
    }

    #[test]
    fn preparing_dual_track_trim_keeps_old_sources_and_writes_aligned_new_files() {
        let directory = tempfile::tempdir().unwrap();
        let mic = directory.path().join("meeting.wav");
        let system = directory.path().join("meeting.system.wav");
        write_test_wav(&mic, 10, 30);
        write_test_wav(&system, 10, 30);
        write_timeline_sidecar(
            &mic.with_extension("timeline.json"),
            &MeetingTimeline {
                mic_offset_seconds: 0.0,
                system_offset_seconds: Some(0.2),
                t0_wall_clock: "2026-01-01T00:00:00+00:00".into(),
            },
        );

        let prepared = prepare_meeting_trim(
            uuid::Uuid::new_v4(),
            mic.clone(),
            Some(system.clone()),
            0.5,
            2.5,
        )
        .unwrap();
        assert!((prepared.duration_seconds - 2.0).abs() < 1e-9);
        assert!(prepared.mic.exists());
        assert!(prepared.system.as_ref().is_some_and(|path| path.exists()));
        assert!(
            mic.exists(),
            "the coordinator owns old-file deletion after DB commit"
        );
        assert!(system.exists());
        assert_eq!(
            read_timeline_offsets(&prepared.mic.with_extension("timeline.json")),
            (0.0, Some(0.0))
        );
    }

    #[test]
    fn failed_system_trim_removes_every_prepared_file_but_keeps_old_sources() {
        let directory = tempfile::tempdir().unwrap();
        let mic = directory.path().join("meeting.wav");
        let system = directory.path().join("meeting.system.wav");
        write_test_wav(&mic, 10, 30);
        std::fs::write(&system, b"not a wav").unwrap();

        assert!(prepare_meeting_trim(
            uuid::Uuid::new_v4(),
            mic.clone(),
            Some(system.clone()),
            0.5,
            2.5,
        )
        .is_err());
        assert!(mic.exists());
        assert!(system.exists());
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".trim-")
        }));
    }

    #[test]
    fn silence_watchdog_warns_then_stops_only_after_grace_period() {
        let mut watchdog = SilenceWatchdogState::new(60.0, 20.0);
        assert_eq!(watchdog.observe(Some(59.9), 0), SilenceWatchdogAction::None);
        assert_eq!(watchdog.observe(Some(60.0), 0), SilenceWatchdogAction::Warn);
        assert_eq!(watchdog.observe(Some(79.9), 0), SilenceWatchdogAction::None);
        assert_eq!(watchdog.observe(Some(80.0), 0), SilenceWatchdogAction::Stop);
    }

    #[test]
    fn meeting_silence_uses_the_most_recent_activity_from_any_available_track() {
        assert_eq!(
            combine_track_silence_seconds(Some(90.0), Some(2.0)),
            Some(2.0)
        );
        assert_eq!(
            combine_track_silence_seconds(Some(90.0), Some(80.0)),
            Some(80.0)
        );
        assert_eq!(combine_track_silence_seconds(Some(12.0), None), Some(12.0));
        assert_eq!(combine_track_silence_seconds(None, Some(7.0)), Some(7.0));
        assert_eq!(combine_track_silence_seconds(None, None), None);
        assert_eq!(
            combine_track_silence_seconds(Some(f64::NAN), Some(4.0)),
            Some(4.0)
        );
    }

    #[test]
    fn sound_during_countdown_clears_and_rearms_warning() {
        let mut watchdog = SilenceWatchdogState::new(60.0, 20.0);
        assert_eq!(watchdog.observe(Some(60.0), 0), SilenceWatchdogAction::Warn);
        assert_eq!(watchdog.observe(Some(0.0), 0), SilenceWatchdogAction::Clear);
        assert_eq!(watchdog.observe(Some(59.0), 0), SilenceWatchdogAction::None);
        assert_eq!(watchdog.observe(Some(60.0), 0), SilenceWatchdogAction::Warn);
    }

    #[test]
    fn continue_acknowledgement_grants_a_fresh_full_silence_interval() {
        let mut watchdog = SilenceWatchdogState::new(60.0, 20.0);
        assert_eq!(watchdog.observe(Some(60.0), 0), SilenceWatchdogAction::Warn);
        assert_eq!(
            watchdog.observe(Some(65.0), 1),
            SilenceWatchdogAction::Clear
        );
        assert_eq!(
            watchdog.observe(Some(124.9), 1),
            SilenceWatchdogAction::None
        );
        assert_eq!(
            watchdog.observe(Some(125.0), 1),
            SilenceWatchdogAction::Warn
        );
    }

    #[test]
    fn losing_activity_measurement_fails_open_and_clears_warning() {
        let mut watchdog = SilenceWatchdogState::new(60.0, 20.0);
        assert_eq!(watchdog.observe(Some(60.0), 0), SilenceWatchdogAction::Warn);
        assert_eq!(watchdog.observe(None, 0), SilenceWatchdogAction::Clear);
        assert_eq!(watchdog.observe(None, 0), SilenceWatchdogAction::None);
    }

    #[test]
    fn timeline_sidecar_serializes_offsets_and_wall_clock() {
        let dual = serde_json::to_string(&MeetingTimeline {
            mic_offset_seconds: 0.012,
            system_offset_seconds: Some(0.45),
            t0_wall_clock: "2026-01-01T00:00:00+00:00".into(),
        })
        .unwrap();
        assert!(dual.contains("\"mic_offset_seconds\":0.012"));
        assert!(dual.contains("\"system_offset_seconds\":0.45"));
        assert!(dual.contains("\"t0_wall_clock\":\"2026-01-01T00:00:00+00:00\""));

        // Mic-only meetings omit the system offset instead of writing null.
        let mic_only = serde_json::to_string(&MeetingTimeline {
            mic_offset_seconds: 0.01,
            system_offset_seconds: None,
            t0_wall_clock: "2026-01-01T00:00:00+00:00".into(),
        })
        .unwrap();
        assert!(!mic_only.contains("system_offset_seconds"));
    }

    #[test]
    fn timeline_sidecar_write_is_best_effort_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.timeline.json");
        write_timeline_sidecar(
            &path,
            &MeetingTimeline {
                mic_offset_seconds: 0.02,
                system_offset_seconds: Some(0.3),
                t0_wall_clock: "2026-01-01T00:00:00+00:00".into(),
            },
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mic_offset_seconds"], 0.02);
        assert_eq!(parsed["system_offset_seconds"], 0.3);
        // A failing write (unwritable directory) must not panic.
        write_timeline_sidecar(
            &dir.path().join("missing").join("m.timeline.json"),
            &MeetingTimeline {
                mic_offset_seconds: 0.0,
                system_offset_seconds: None,
                t0_wall_clock: String::new(),
            },
        );
    }

    fn attendees(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn attendees_line_is_prepended_to_existing_notes() {
        let merged = merge_attendees_into_notes(
            "记得跟进预算",
            &attendees(&["张三", "李四 <li@example.com>"]),
        )
        .unwrap();
        assert_eq!(merged, "参会人: 张三、李四 <li@example.com>\n记得跟进预算");
    }

    #[test]
    fn empty_notes_get_just_the_attendees_line() {
        let merged = merge_attendees_into_notes("", &attendees(&["张三"])).unwrap();
        assert_eq!(merged, "参会人: 张三\n");
    }

    #[test]
    fn no_attendees_means_no_write() {
        assert_eq!(merge_attendees_into_notes("笔记", &[]), None);
    }

    #[test]
    fn existing_attendees_line_is_never_duplicated() {
        let existing = "参会人: 张三\n记得跟进预算";
        assert_eq!(
            merge_attendees_into_notes(existing, &attendees(&["王五"])),
            None
        );
    }
}
