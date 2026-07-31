//! App-level meeting-detection wiring: platform detector → pure policy → UI.
//!
//! This is the glue between the macOS [`MeetingActivityDetector`] (which reports
//! audio-input activity) and the pure [`lumen_core::MeetingDetectionPolicy`]
//! (which decides whether to prompt). It owns the policy behind a lock, converts
//! detector signals into policy inputs (attaching a monotonic timestamp), and
//! turns policy outputs into Tauri events / actions:
//!
//! - `ShowPrompt`  → emit `meeting-detected` (the app shows a lightweight prompt)
//! - `CancelPrompt`→ emit `meeting-detection-cancelled` (retract the prompt)
//! - `SuggestStop` → emit `meeting-detection-stop-suggested` (the app asks
//!   "meeting seems over — stop recording?"; never stops on its own)
//! - `CancelStopPrompt` → emit `meeting-detection-stop-cancelled` (retract it)
//! - `Decision`    → local `tracing` log only (never uploaded)
//! - `StartRecording` is produced *only* by the user-accept path
//!   ([`Self::accept`]), which then reuses the existing `start_meeting_recording`
//!   command — detection never records on its own.
//!
//! Prompt/suggestion counters are additionally tallied into a local JSON file
//! (see [`crate::detection_stats`]) so false-positive rates can be quantified;
//! purely local, no network.
//!
//! The detector half is macOS + capability gated; on every other platform the
//! service exists (so commands compile) but [`Self::start`] does nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lumen_core::{DetectionConfig, DetectionInput, DetectionOutput, MeetingDetectionPolicy};
use lumen_platform::default_data_dir;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::detection_stats::{DetectionStats, DetectionStatsCounters, StatCounter};

/// Payload of the `meeting-detected` event the front-end listens for.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingDetectedEvent {
    /// Normalized bundle id of the app that looks like a meeting.
    bundle_id: String,
    /// Class token (`native_meeting` today) for labelling.
    app_class: String,
}

/// Payload of the `meeting-detection-stop-suggested` event: the candidate that
/// triggered `meeting_id`'s recording has been gone for the stop-stability
/// window, so the app should ask the user whether to stop.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingStopSuggestedEvent {
    /// Id of the detection-started meeting the suggestion is about (`None` in
    /// the theoretical case where tracking was already cleared — the front-end
    /// stop action goes through a backend command that re-checks anyway).
    meeting_id: Option<String>,
    /// Normalized bundle id of the app whose input disappeared (for labelling).
    bundle_id: String,
}

/// State shared between the service handle and the detector callback thread.
struct DetectionShared {
    policy: Mutex<MeetingDetectionPolicy>,
    /// Local prompt/suggestion counters (JSON file + tracing; never uploaded).
    stats: DetectionStats,
    /// Meeting id of the recording started from a detection prompt; `None`
    /// when idle or when the current recording was started manually. This is
    /// what scopes the stop suggestion to detection-started meetings only.
    active_meeting: Mutex<Option<String>>,
}

/// Holds the detection policy and the (macOS) background detector.
pub struct MeetingDetectionService {
    shared: Arc<DetectionShared>,
    #[cfg(target_os = "macos")]
    detector: Mutex<lumen_platform_macos::MeetingActivityDetector>,
    /// Monotonic origin; policy timestamps are ms since this instant.
    origin: Instant,
    active: AtomicBool,
}

impl Default for MeetingDetectionService {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingDetectionService {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(DetectionShared {
                policy: Mutex::new(MeetingDetectionPolicy::new(DetectionConfig::default())),
                stats: DetectionStats::load(default_data_dir().join("detection_stats.json")),
                active_meeting: Mutex::new(None),
            }),
            #[cfg(target_os = "macos")]
            detector: Mutex::new(lumen_platform_macos::MeetingActivityDetector::new()),
            origin: Instant::now(),
            active: AtomicBool::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// True when the poller is running.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Start the detector if the OS capability is present. No-op off macOS, if
    /// already running, or if the capability is unavailable. Returns whether a
    /// poller was actually started.
    #[allow(unused_variables)]
    pub fn start(&self, app: AppHandle) -> bool {
        #[cfg(target_os = "macos")]
        {
            if self.active.load(Ordering::SeqCst) {
                return false;
            }
            let shared = self.shared.clone();
            let origin = self.origin;
            let app_for_cb = app.clone();
            let started = {
                let mut detector = match self.detector.lock() {
                    Ok(d) => d,
                    Err(_) => return false,
                };
                detector.start(
                    lumen_platform_macos::MEETING_DETECTION_DEFAULT_POLL,
                    move |signal| {
                        handle_signal(&shared, &app_for_cb, origin, signal);
                    },
                )
            };
            if started {
                self.active.store(true, Ordering::SeqCst);
                tracing::info!("meeting detection started (capability present)");
            } else {
                tracing::info!("meeting detection not started (capability unavailable)");
            }
            started
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Stop the detector. Idempotent.
    pub fn stop(&self) {
        #[cfg(target_os = "macos")]
        {
            if let Ok(mut detector) = self.detector.lock() {
                detector.stop();
            }
        }
        self.active.store(false, Ordering::SeqCst);
    }

    /// Disable path: stop the detector *and* reset the policy, retracting any
    /// prompt or stop suggestion that is currently on screen. Without the
    /// reset, turning the setting off while a prompt is showing would leave
    /// the front-end prompt stranded and the policy stuck in `prompted`.
    pub fn stop_and_reset(&self, app: &AppHandle) {
        self.stop();
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.reset()
        };
        apply_outputs(app, &self.shared, &outputs);
    }

    /// The user accepted the prompt. Advances the policy (arming cooldown and
    /// moving to `recording`) and reports whether a recording should now begin.
    /// The caller performs the actual `start_meeting_recording`.
    pub fn accept(&self) -> bool {
        let now = self.now_ms();
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return false,
            };
            policy.handle(DetectionInput::UserAccepted { now_ms: now })
        };
        let mut should_start = false;
        for out in &outputs {
            match out {
                DetectionOutput::StartRecording { bundle_id } => {
                    tracing::info!(bundle_id = %bundle_id, "meeting detection accepted → start");
                    should_start = true;
                }
                DetectionOutput::Decision(d) => log_decision(d),
                _ => {}
            }
        }
        if should_start {
            self.shared.stats.increment(StatCounter::PromptAccepted);
        }
        should_start
    }

    /// Record which meeting the accepted prompt actually started, so the
    /// end-of-meeting stop suggestion knows what it is about. Called by the
    /// accept command once `start_meeting_recording` succeeds.
    pub fn mark_recording_started(&self, meeting_id: &str) {
        if let Ok(mut active) = self.shared.active_meeting.lock() {
            *active = Some(meeting_id.to_string());
        }
    }

    /// The detection-started meeting currently recording, if any.
    pub fn active_meeting_id(&self) -> Option<String> {
        self.shared
            .active_meeting
            .lock()
            .ok()
            .and_then(|active| active.clone())
    }

    /// The user accepted a stop suggestion (counted just before the caller
    /// runs the ordinary stop command).
    pub fn note_stop_accepted(&self) {
        self.shared.stats.increment(StatCounter::StopAccepted);
    }

    /// The user dismissed the prompt (arms per-app cooldown).
    pub fn dismiss(&self, app: &AppHandle) {
        let now = self.now_ms();
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.handle(DetectionInput::UserDismissed { now_ms: now })
        };
        // Non-empty outputs mean a real prompt was dismissed (the policy
        // no-ops with an empty vec when there was nothing to dismiss).
        if !outputs.is_empty() {
            self.shared.stats.increment(StatCounter::PromptDismissed);
        }
        apply_outputs(app, &self.shared, &outputs);
    }

    /// The user declined a stop suggestion ("继续录制"): the policy suppresses
    /// further suggestions for the remainder of this recording.
    pub fn decline_stop(&self, app: &AppHandle) {
        let now = self.now_ms();
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.handle(DetectionInput::StopDeclined { now_ms: now })
        };
        if !outputs.is_empty() {
            self.shared.stats.increment(StatCounter::StopDeclined);
        }
        apply_outputs(app, &self.shared, &outputs);
    }

    /// Notify the policy that a recording it prompted for has finished, so it
    /// can return to idle. Safe to call regardless of how the recording started
    /// (a manual meeting no-ops). Also clears the tracked meeting id and
    /// retracts a still-visible stop suggestion.
    pub fn recording_finished(&self, app: &AppHandle) {
        let now = self.now_ms();
        if let Ok(mut active) = self.shared.active_meeting.lock() {
            *active = None;
        }
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.handle(DetectionInput::RecordingFinished { now_ms: now })
        };
        apply_outputs(app, &self.shared, &outputs);
    }

    /// The accepted recording failed to start (or died on an error path that
    /// never reached a successful stop). Returns the policy to idle so future
    /// candidates are not rejected as busy forever; the per-app cooldown armed
    /// at accept stays in effect (see the policy for the rationale).
    pub fn recording_failed(&self, app: &AppHandle) {
        let now = self.now_ms();
        if let Ok(mut active) = self.shared.active_meeting.lock() {
            *active = None;
        }
        let outputs = {
            let mut policy = match self.shared.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.handle(DetectionInput::RecordingFailed { now_ms: now })
        };
        apply_outputs(app, &self.shared, &outputs);
    }

    /// Current local counter values (for `get_meeting_detection_stats`).
    pub fn stats_snapshot(&self) -> DetectionStatsCounters {
        self.shared.stats.snapshot()
    }
}

/// Convert one detector signal into policy input(s) and act on the outputs.
#[cfg(target_os = "macos")]
fn handle_signal(
    shared: &Arc<DetectionShared>,
    app: &AppHandle,
    origin: Instant,
    signal: lumen_platform_macos::DetectorSignal,
) {
    use lumen_platform_macos::DetectorSignal as S;
    let now_ms = origin.elapsed().as_millis() as u64;
    let input = match signal {
        S::Added(input) => DetectionInput::CandidateAdded {
            candidate: lumen_core::Candidate {
                app_class: input.app_class,
                bundle_id: input.bundle_id,
                session_key: input.session_key,
            },
            now_ms,
        },
        S::Removed { session_key } => DetectionInput::CandidateRemoved {
            session_key,
            now_ms,
        },
        S::Tick => DetectionInput::Tick { now_ms },
    };
    let outputs = {
        let mut guard = match shared.policy.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        guard.handle(input)
    };
    apply_outputs(app, shared, &outputs);
}

/// Emit UI events / bump counters / log for policy outputs. `StartRecording`
/// is intentionally ignored here: it only originates from the user-accept
/// command path — and there is deliberately no "stop recording" analogue at
/// all; `SuggestStop` only ever *asks*.
fn apply_outputs(app: &AppHandle, shared: &DetectionShared, outputs: &[DetectionOutput]) {
    for out in outputs {
        match out {
            DetectionOutput::ShowPrompt {
                bundle_id,
                app_class,
            } => {
                shared.stats.increment(StatCounter::PromptShown);
                let _ = app.emit(
                    "meeting-detected",
                    MeetingDetectedEvent {
                        bundle_id: bundle_id.clone(),
                        app_class: format!("{app_class:?}").to_ascii_lowercase(),
                    },
                );
            }
            DetectionOutput::CancelPrompt => {
                let _ = app.emit("meeting-detection-cancelled", ());
            }
            DetectionOutput::SuggestStop { bundle_id } => {
                shared.stats.increment(StatCounter::StopSuggested);
                let meeting_id = shared
                    .active_meeting
                    .lock()
                    .ok()
                    .and_then(|active| active.clone());
                let _ = app.emit(
                    "meeting-detection-stop-suggested",
                    MeetingStopSuggestedEvent {
                        meeting_id,
                        bundle_id: bundle_id.clone(),
                    },
                );
            }
            DetectionOutput::CancelStopPrompt => {
                let _ = app.emit("meeting-detection-stop-cancelled", ());
            }
            DetectionOutput::Decision(d) => log_decision(d),
            // Only the accept command starts recording; never autonomously.
            DetectionOutput::StartRecording { .. } => {}
        }
    }
}

fn log_decision(d: &lumen_core::DetectionDecision) {
    tracing::debug!(app = %d.app, evidence = %d.evidence, reason = %d.reason, "meeting detection decision");
}
