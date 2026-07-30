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
//! - `Decision`    → local `tracing` log only (never uploaded)
//! - `StartRecording` is produced *only* by the user-accept path
//!   ([`Self::accept`]), which then reuses the existing `start_meeting_recording`
//!   command — detection never records on its own.
//!
//! The detector half is macOS + capability gated; on every other platform the
//! service exists (so commands compile) but [`Self::start`] does nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lumen_core::{DetectionConfig, DetectionInput, DetectionOutput, MeetingDetectionPolicy};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Payload of the `meeting-detected` event the front-end listens for.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingDetectedEvent {
    /// Normalized bundle id of the app that looks like a meeting.
    bundle_id: String,
    /// Class token (`native_meeting` today) for labelling.
    app_class: String,
}

/// Holds the detection policy and the (macOS) background detector.
pub struct MeetingDetectionService {
    policy: Arc<Mutex<MeetingDetectionPolicy>>,
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
            policy: Arc::new(Mutex::new(MeetingDetectionPolicy::new(
                DetectionConfig::default(),
            ))),
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
            let policy = self.policy.clone();
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
                        handle_signal(&policy, &app_for_cb, origin, signal);
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
    /// prompt that is currently on screen. Without the reset, turning the
    /// setting off while a prompt is showing would leave the front-end prompt
    /// stranded and the policy stuck in `prompted`.
    pub fn stop_and_reset(&self, app: &AppHandle) {
        self.stop();
        let outputs = {
            let mut policy = match self.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.reset()
        };
        apply_outputs(app, &outputs);
    }

    /// The user accepted the prompt. Advances the policy (arming cooldown and
    /// moving to `recording`) and reports whether a recording should now begin.
    /// The caller performs the actual `start_meeting_recording`.
    pub fn accept(&self) -> bool {
        let now = self.now_ms();
        let outputs = {
            let mut policy = match self.policy.lock() {
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
        should_start
    }

    /// The user dismissed the prompt (arms per-app cooldown).
    pub fn dismiss(&self, app: &AppHandle) {
        let now = self.now_ms();
        let outputs = {
            let mut policy = match self.policy.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            policy.handle(DetectionInput::UserDismissed { now_ms: now })
        };
        apply_outputs(app, &outputs);
    }

    /// Notify the policy that a recording it prompted for has finished, so it
    /// can return to idle. Safe to call regardless of how the recording started.
    pub fn recording_finished(&self) {
        let now = self.now_ms();
        if let Ok(mut policy) = self.policy.lock() {
            let outputs = policy.handle(DetectionInput::RecordingFinished { now_ms: now });
            for out in &outputs {
                if let DetectionOutput::Decision(d) = out {
                    log_decision(d);
                }
            }
        }
    }

    /// The accepted recording failed to start (or died on an error path that
    /// never reached a successful stop). Returns the policy to idle so future
    /// candidates are not rejected as busy forever; the per-app cooldown armed
    /// at accept stays in effect (see the policy for the rationale).
    pub fn recording_failed(&self) {
        let now = self.now_ms();
        if let Ok(mut policy) = self.policy.lock() {
            let outputs = policy.handle(DetectionInput::RecordingFailed { now_ms: now });
            for out in &outputs {
                if let DetectionOutput::Decision(d) = out {
                    log_decision(d);
                }
            }
        }
    }
}

/// Convert one detector signal into policy input(s) and act on the outputs.
#[cfg(target_os = "macos")]
fn handle_signal(
    policy: &Arc<Mutex<MeetingDetectionPolicy>>,
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
        let mut guard = match policy.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        guard.handle(input)
    };
    apply_outputs(app, &outputs);
}

/// Emit UI events / log for policy outputs. `StartRecording` is intentionally
/// ignored here: it only originates from the user-accept command path.
fn apply_outputs(app: &AppHandle, outputs: &[DetectionOutput]) {
    for out in outputs {
        match out {
            DetectionOutput::ShowPrompt {
                bundle_id,
                app_class,
            } => {
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
            DetectionOutput::Decision(d) => log_decision(d),
            // Only the accept command starts recording; never autonomously.
            DetectionOutput::StartRecording { .. } => {}
        }
    }
}

fn log_decision(d: &lumen_core::DetectionDecision) {
    tracing::debug!(app = %d.app, evidence = %d.evidence, reason = %d.reason, "meeting detection decision");
}
