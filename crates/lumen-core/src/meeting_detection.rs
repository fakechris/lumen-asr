//! Pure state machine that decides *whether and when* to surface a
//! "looks like a meeting — start recording?" prompt.
//!
//! This module is deliberately platform-free and side-effect-free: it takes
//! discrete inputs (a candidate appeared / disappeared, a clock tick, a user
//! click) plus an explicit monotonic timestamp, and returns a list of outputs
//! (show/cancel the prompt, start recording, or a decision-trail log line). All
//! timing is driven by the caller-supplied `now_ms` so the whole policy is
//! deterministic and unit-testable without a real clock, audio stack, or macOS.
//!
//! ## Why prompt-to-record, never auto-record
//! Detecting audio-input activity is *evidence*, not consent. The state machine
//! never starts recording on its own — the terminal `StartRecording` output is
//! only ever produced in response to an explicit [`DetectionInput::UserAccepted`]
//! (the user clicking "start"). Everything else is advisory.
//!
//! ## End-of-meeting stop suggestion (same contract, in reverse)
//! For a recording that *was* started from a detection prompt, the policy also
//! watches for the triggering candidate's input to disappear. Once it has been
//! continuously gone for [`DetectionConfig::stop_stability_ms`] (debounced so
//! in-meeting mutes/reconnects never fire), it emits
//! [`DetectionOutput::SuggestStop`] — again advisory only: the host asks the
//! user, and the recording is never stopped silently. Declining suppresses
//! further suggestions for the rest of that recording. Manually started
//! meetings have no associated candidate and are never suggested on.
//!
//! ## Classification
//! A raw candidate carries an [`AppClass`] supplied by the host's runtime
//! application catalog. Native meeting apps and configured browsers are
//! promptable after the stability window; browsers receive a stricter UI
//! explanation because process-level capture includes every tab. `Other` apps
//! are never prompted.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Coarse classification of the app holding an audio input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppClass {
    /// A native meeting/conferencing app enabled in the runtime catalog.
    NativeMeeting,
    /// A web browser (or one of its helper/renderer processes).
    Browser,
    /// Anything else holding an input.
    Other,
}

impl AppClass {
    /// Whether input activity from this configured class is strong enough to
    /// prompt. The host only assigns these two classes to catalog entries whose
    /// `detect` flag is enabled; unknown applications arrive as `Other`.
    pub fn promptable_on_input_alone(self) -> bool {
        matches!(self, AppClass::NativeMeeting | AppClass::Browser)
    }
}

/// A normalized audio-input candidate produced by a platform detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// Classification of the owning app (drives whether it is promptable).
    pub app_class: AppClass,
    /// Normalized bundle id (helper/renderer processes folded to the parent).
    /// Used as the cooldown key and shown (indirectly) to the user.
    pub bundle_id: String,
    /// Stable key for *this specific* input session (e.g. bundle id + pid).
    /// A new key restarts the stability timer; jitter therefore cannot prompt.
    pub session_key: String,
}

/// Discrete inputs to the policy. Every variant carries `now_ms`, a monotonic
/// millisecond timestamp supplied by the caller (never read from a wall clock
/// inside the policy) so behaviour is fully deterministic under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionInput {
    /// A new (or re-appearing) input session was observed.
    CandidateAdded { candidate: Candidate, now_ms: u64 },
    /// A previously observed input session ended.
    CandidateRemoved { session_key: String, now_ms: u64 },
    /// Periodic advance (the detector's poll). Promotes a stable candidate to a
    /// prompt once it has been continuously present for the stability window.
    Tick { now_ms: u64 },
    /// The user clicked "start recording" on the prompt.
    UserAccepted { now_ms: u64 },
    /// The user dismissed / ignored the prompt.
    UserDismissed { now_ms: u64 },
    /// An externally-started recording (that we prompted for) has finished.
    RecordingFinished { now_ms: u64 },
    /// The recording the user accepted could not actually start (or aborted on
    /// an error path that never reached a successful stop). Without this exit
    /// the machine would sit in `recording` forever, rejecting every future
    /// candidate as `busy_with_other_candidate` — silently disabling detection
    /// until restart.
    RecordingFailed { now_ms: u64 },
    /// The user declined a stop suggestion ("继续录制"): suppress further stop
    /// suggestions for the remainder of this recording.
    StopDeclined { now_ms: u64 },
}

/// Side-effect requests the host should carry out. The policy itself performs
/// none of these; it only decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionOutput {
    /// Show the prompt for `bundle_id` (class attached for labelling/telemetry).
    ShowPrompt {
        bundle_id: String,
        app_class: AppClass,
    },
    /// Hide a prompt that is no longer warranted (signal vanished / dismissed).
    CancelPrompt,
    /// Begin recording — only ever emitted after an explicit user accept.
    StartRecording { bundle_id: String },
    /// Suggest stopping a detection-started recording: the candidate that
    /// triggered it has been continuously gone for the stop-stability window.
    /// Like `ShowPrompt`, this is advisory — the host asks the user; the
    /// policy never stops a recording on its own.
    SuggestStop { bundle_id: String },
    /// Hide a stop suggestion that is no longer warranted (the meeting app's
    /// input came back, or the recording ended some other way).
    CancelStopPrompt,
    /// A decision-trail line for local logging (never uploaded). Helps triage
    /// false positives/negatives after the fact.
    Decision(DetectionDecision),
}

/// A single auditable decision the policy made about a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionDecision {
    /// App the decision is about (bundle id), or a synthetic marker.
    pub app: String,
    /// Short machine-readable evidence tag (e.g. `native_meeting_input`).
    pub evidence: String,
    /// Short machine-readable reason (e.g. `prompted`, `cooldown`, `ignored_self`).
    pub reason: String,
}

impl DetectionDecision {
    fn new(app: impl Into<String>, evidence: &str, reason: &str) -> Self {
        Self {
            app: app.into(),
            evidence: evidence.to_string(),
            reason: reason.to_string(),
        }
    }
}

/// Tunable thresholds. Defaults are conservative on purpose: the feature ships
/// opt-in and false positives are far more damaging to trust than a missed
/// prompt, so we bias toward *not* prompting.
#[derive(Debug, Clone)]
pub struct DetectionConfig {
    /// How long a single candidate must be *continuously* present before it is
    /// promoted to a prompt. 3s: long enough that a stray/transient input never
    /// prompts, short enough to fire early in a real meeting.
    pub stability_ms: u64,
    /// Per-app quiet period after a prompt is accepted or dismissed. 2 minutes:
    /// avoids nagging on the same app (session churn, re-joins, mute/unmute).
    pub cooldown_ms: u64,
    /// Bundle ids to always ignore (Lumen's own processes). Compared
    /// case-insensitively; a candidate whose bundle contains any of these is
    /// dropped so we never prompt on our own capture.
    pub self_bundle_ids: Vec<String>,
    /// How long the candidate that triggered a detection-started recording must
    /// be *continuously* gone before suggesting a stop. 10s: long enough that
    /// an in-meeting mute, device switch, or reconnect never suggests stopping,
    /// short enough to catch the end of a real meeting promptly.
    pub stop_stability_ms: u64,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            stability_ms: 3_000,
            cooldown_ms: 120_000,
            self_bundle_ids: vec!["com.lumenopen.asr".to_string(), "lumen".to_string()],
            stop_stability_ms: 10_000,
        }
    }
}

/// Internal machine state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Nothing worth acting on.
    Idle,
    /// A promptable candidate is present but not yet stable long enough.
    Candidate { cand: Candidate, since_ms: u64 },
    /// A prompt is currently shown for `cand`.
    Prompted { cand: Candidate },
    /// The user accepted; a recording is in progress for `cand`. Only
    /// detection-started recordings ever enter this state — a manually started
    /// meeting has no associated candidate, so the stop-suggestion fields
    /// below can never fire for it.
    Recording {
        cand: Candidate,
        /// When the tracked candidate's input disappeared (`None` while
        /// present). Cleared whenever the same app's input re-appears, so
        /// mute/reconnect jitter restarts the stop-stability timer.
        gone_since_ms: Option<u64>,
        /// A stop suggestion is currently shown to the user.
        stop_prompted: bool,
        /// The user declined a stop suggestion: never suggest again for this
        /// recording (anti-nag; the user explicitly chose to keep recording).
        stop_suppressed: bool,
    },
}

/// The meeting-detection decision policy. Feed it [`DetectionInput`]s; act on
/// the returned [`DetectionOutput`]s. One instance tracks at most one candidate
/// at a time (the first promptable one wins until it resolves).
#[derive(Debug, Clone)]
pub struct MeetingDetectionPolicy {
    config: DetectionConfig,
    state: State,
    /// bundle_id (lowercased) -> timestamp (ms) until which it is in cooldown.
    cooldown_until: HashMap<String, u64>,
}

impl MeetingDetectionPolicy {
    pub fn new(config: DetectionConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            cooldown_until: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DetectionConfig::default())
    }

    /// Current phase as a stable string, for status/telemetry.
    pub fn phase(&self) -> &'static str {
        match self.state {
            State::Idle => "idle",
            State::Candidate { .. } => "candidate",
            State::Prompted { .. } => "prompted",
            State::Recording { .. } => "recording",
        }
    }

    fn is_self(&self, bundle_id: &str) -> bool {
        let b = bundle_id.to_ascii_lowercase();
        self.config
            .self_bundle_ids
            .iter()
            .any(|s| !s.is_empty() && b.contains(&s.to_ascii_lowercase()))
    }

    fn in_cooldown(&self, bundle_id: &str, now_ms: u64) -> bool {
        self.cooldown_until
            .get(&bundle_id.to_ascii_lowercase())
            .is_some_and(|&until| now_ms < until)
    }

    fn arm_cooldown(&mut self, bundle_id: &str, now_ms: u64) {
        self.cooldown_until.insert(
            bundle_id.to_ascii_lowercase(),
            now_ms.saturating_add(self.config.cooldown_ms),
        );
    }

    /// The session key of whatever candidate the machine is currently tracking.
    fn tracked_key(&self) -> Option<&str> {
        match &self.state {
            State::Idle => None,
            State::Candidate { cand, .. }
            | State::Prompted { cand }
            | State::Recording { cand, .. } => Some(cand.session_key.as_str()),
        }
    }

    /// Drive the machine with one input; returns the side effects to perform.
    pub fn handle(&mut self, input: DetectionInput) -> Vec<DetectionOutput> {
        match input {
            DetectionInput::CandidateAdded { candidate, now_ms } => {
                self.on_candidate_added(candidate, now_ms)
            }
            DetectionInput::CandidateRemoved {
                session_key,
                now_ms,
            } => self.on_candidate_removed(&session_key, now_ms),
            DetectionInput::Tick { now_ms } => self.on_tick(now_ms),
            DetectionInput::UserAccepted { now_ms } => self.on_user_accepted(now_ms),
            DetectionInput::UserDismissed { now_ms } => self.on_user_dismissed(now_ms),
            DetectionInput::RecordingFinished { now_ms } => self.on_recording_finished(now_ms),
            DetectionInput::RecordingFailed { now_ms } => self.on_recording_failed(now_ms),
            DetectionInput::StopDeclined { now_ms } => self.on_stop_declined(now_ms),
        }
    }

    /// Force the machine back to `Idle`, retracting any visible prompt. Used
    /// when the user disables detection while a prompt is showing (otherwise the
    /// prompt stays on screen and the policy stays `Prompted` after the detector
    /// stops). Cooldowns are preserved on purpose: re-enabling shortly after
    /// must not instantly re-nag about an app the user just dealt with.
    pub fn reset(&mut self) -> Vec<DetectionOutput> {
        let outputs = match &self.state {
            State::Idle => Vec::new(),
            State::Prompted { cand } => vec![
                DetectionOutput::CancelPrompt,
                DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id.clone(),
                    "detection_disabled",
                    "prompt_cancelled_reset",
                )),
            ],
            State::Recording {
                cand,
                stop_prompted,
                ..
            } => {
                let mut outs = Vec::new();
                if *stop_prompted {
                    // A stop suggestion is on screen: retract it (the machine
                    // is leaving `recording`, so the question is moot).
                    outs.push(DetectionOutput::CancelStopPrompt);
                }
                outs.push(DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id.clone(),
                    "detection_disabled",
                    "reset_to_idle",
                )));
                outs
            }
            State::Candidate { cand, .. } => {
                vec![DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id.clone(),
                    "detection_disabled",
                    "reset_to_idle",
                ))]
            }
        };
        self.state = State::Idle;
        outputs
    }

    fn on_candidate_added(&mut self, cand: Candidate, now_ms: u64) -> Vec<DetectionOutput> {
        // Never act on our own capture.
        if self.is_self(&cand.bundle_id) {
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id,
                "self_bundle",
                "ignored_self",
            ))];
        }
        // Unknown / disabled apps are never prompted.
        if !cand.app_class.promptable_on_input_alone() {
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id,
                "input_active",
                "class_not_promptable",
            ))];
        }
        // While a detection-started recording is live, a re-appearing input of
        // the *same app* (mute/unmute, device switch, meeting rejoin — usually
        // under a fresh session key) re-attaches to the tracked candidate and
        // withdraws any pending or shown stop suggestion. This must run before
        // the cooldown check below: the recording app is inside the cooldown
        // armed at accept, which would otherwise swallow the re-add and let a
        // stale removal suggest stopping a meeting that is still going.
        if let State::Recording {
            cand: tracked,
            gone_since_ms,
            stop_prompted,
            ..
        } = &mut self.state
        {
            if tracked
                .bundle_id
                .eq_ignore_ascii_case(cand.bundle_id.as_str())
            {
                let was_gone = gone_since_ms.is_some();
                let was_prompted = *stop_prompted;
                tracked.session_key = cand.session_key;
                *gone_since_ms = None;
                *stop_prompted = false;
                if !was_gone && !was_prompted {
                    // Idempotent re-report of a session we already track.
                    return Vec::new();
                }
                let mut outs = Vec::new();
                if was_prompted {
                    outs.push(DetectionOutput::CancelStopPrompt);
                }
                outs.push(DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id,
                    "input_returned",
                    "stop_suggestion_withdrawn",
                )));
                return outs;
            }
        }
        // Respect the per-app quiet period.
        if self.in_cooldown(&cand.bundle_id, now_ms) {
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id,
                "native_meeting_input",
                "cooldown",
            ))];
        }
        // If we are already tracking this exact session, treat as a no-op
        // (idempotent re-report); a *different* session while busy is ignored
        // so a single prompt is never preempted mid-flight.
        if let Some(tracked) = self.tracked_key() {
            if tracked == cand.session_key {
                return Vec::new();
            }
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id,
                "native_meeting_input",
                "busy_with_other_candidate",
            ))];
        }
        // Fresh promptable candidate: start the stability timer.
        let decision = DetectionDecision::new(
            cand.bundle_id.clone(),
            "native_meeting_input",
            "candidate_tracking",
        );
        self.state = State::Candidate {
            cand,
            since_ms: now_ms,
        };
        vec![DetectionOutput::Decision(decision)]
    }

    fn on_candidate_removed(&mut self, session_key: &str, now_ms: u64) -> Vec<DetectionOutput> {
        // Detection-started recording: the candidate that triggered it going
        // away starts the stop-stability timer. Nothing is suggested yet —
        // only a tick after `stop_stability_ms` of continuous absence does
        // that, so a brief drop (reconnect, device switch) never surfaces
        // anything. The recording itself is user-owned and keeps running.
        if let State::Recording {
            cand,
            gone_since_ms,
            ..
        } = &mut self.state
        {
            if cand.session_key == session_key && gone_since_ms.is_none() {
                *gone_since_ms = Some(now_ms);
                return vec![DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id.clone(),
                    "input_ended",
                    "stop_candidate_gone",
                ))];
            }
            // Unrelated key, or already tracked as gone: nothing new.
            return Vec::new();
        }
        match &self.state {
            State::Candidate { cand, .. } if cand.session_key == session_key => {
                // Disappeared before it ever stabilized: silently drop it (there
                // was no prompt to cancel). This is the jitter guard.
                let app = cand.bundle_id.clone();
                self.state = State::Idle;
                vec![DetectionOutput::Decision(DetectionDecision::new(
                    app,
                    "input_ended",
                    "candidate_dropped_before_prompt",
                ))]
            }
            State::Prompted { cand } if cand.session_key == session_key => {
                // Signal vanished while the prompt was up: retract it now.
                let app = cand.bundle_id.clone();
                self.state = State::Idle;
                vec![
                    DetectionOutput::CancelPrompt,
                    DetectionOutput::Decision(DetectionDecision::new(
                        app,
                        "input_ended",
                        "prompt_cancelled_signal_gone",
                    )),
                ]
            }
            // Unrelated key while idle/candidate/prompted: nothing to do.
            // (Recording-state removals are handled above.)
            _ => Vec::new(),
        }
    }

    fn on_tick(&mut self, now_ms: u64) -> Vec<DetectionOutput> {
        // Detection-started recording whose triggering candidate has been gone
        // long enough → suggest stopping, exactly once per absence. Advisory
        // only: the machine stays in `recording` until the user (or the stop
        // command) says otherwise — it never stops a recording on its own.
        if let State::Recording {
            cand,
            gone_since_ms,
            stop_prompted,
            stop_suppressed,
        } = &mut self.state
        {
            let stable = gone_since_ms
                .is_some_and(|gone| now_ms.saturating_sub(gone) >= self.config.stop_stability_ms);
            if stable && !*stop_prompted && !*stop_suppressed {
                *stop_prompted = true;
                let bundle_id = cand.bundle_id.clone();
                return vec![
                    DetectionOutput::SuggestStop {
                        bundle_id: bundle_id.clone(),
                    },
                    DetectionOutput::Decision(DetectionDecision::new(
                        bundle_id,
                        "input_gone_stable",
                        "stop_suggested",
                    )),
                ];
            }
            return Vec::new();
        }
        let State::Candidate { cand, since_ms } = &self.state else {
            return Vec::new();
        };
        // A late-arriving cooldown (armed after this candidate began) still
        // suppresses the prompt.
        if self.in_cooldown(&cand.bundle_id, now_ms) {
            let app = cand.bundle_id.clone();
            self.state = State::Idle;
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                app,
                "native_meeting_input",
                "cooldown",
            ))];
        }
        if now_ms.saturating_sub(*since_ms) < self.config.stability_ms {
            return Vec::new();
        }
        let cand = cand.clone();
        let outputs = vec![
            DetectionOutput::ShowPrompt {
                bundle_id: cand.bundle_id.clone(),
                app_class: cand.app_class,
            },
            DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id.clone(),
                "native_meeting_input_stable",
                "prompted",
            )),
        ];
        self.state = State::Prompted { cand };
        outputs
    }

    fn on_user_accepted(&mut self, now_ms: u64) -> Vec<DetectionOutput> {
        let State::Prompted { cand } = &self.state else {
            return Vec::new();
        };
        let cand = cand.clone();
        // Arm cooldown so stopping the meeting does not immediately re-prompt.
        self.arm_cooldown(&cand.bundle_id, now_ms);
        let outputs = vec![
            DetectionOutput::StartRecording {
                bundle_id: cand.bundle_id.clone(),
            },
            DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id.clone(),
                "user_click",
                "accepted_start_recording",
            )),
        ];
        self.state = State::Recording {
            cand,
            gone_since_ms: None,
            stop_prompted: false,
            stop_suppressed: false,
        };
        outputs
    }

    fn on_user_dismissed(&mut self, now_ms: u64) -> Vec<DetectionOutput> {
        let State::Prompted { cand } = &self.state else {
            return Vec::new();
        };
        let app = cand.bundle_id.clone();
        self.arm_cooldown(&app, now_ms);
        self.state = State::Idle;
        vec![
            DetectionOutput::CancelPrompt,
            DetectionOutput::Decision(DetectionDecision::new(app, "user_click", "dismissed")),
        ]
    }

    fn on_recording_finished(&mut self, _now_ms: u64) -> Vec<DetectionOutput> {
        if let State::Recording {
            cand,
            stop_prompted,
            ..
        } = &self.state
        {
            let app = cand.bundle_id.clone();
            let mut outs = Vec::new();
            if *stop_prompted {
                // The recording ended (user accepted the suggestion, or
                // stopped it manually elsewhere) while the suggestion was
                // still on screen: retract it.
                outs.push(DetectionOutput::CancelStopPrompt);
            }
            self.state = State::Idle;
            outs.push(DetectionOutput::Decision(DetectionDecision::new(
                app,
                "recording_ended",
                "idle",
            )));
            return outs;
        }
        Vec::new()
    }

    /// The user declined a stop suggestion ("继续录制"): hide it and never
    /// suggest again for this recording. The next recording starts fresh.
    fn on_stop_declined(&mut self, _now_ms: u64) -> Vec<DetectionOutput> {
        if let State::Recording {
            cand,
            gone_since_ms,
            stop_prompted,
            stop_suppressed,
        } = &mut self.state
        {
            if *stop_prompted {
                *stop_prompted = false;
                *stop_suppressed = true;
                *gone_since_ms = None;
                return vec![DetectionOutput::Decision(DetectionDecision::new(
                    cand.bundle_id.clone(),
                    "user_click",
                    "stop_declined",
                ))];
            }
        }
        Vec::new()
    }

    /// The accepted recording never (successfully) happened: return to `Idle`
    /// so future candidates are not rejected as busy forever.
    ///
    /// The cooldown armed at accept is deliberately *kept*: the user already
    /// acted on this app's prompt and sees the start error in the UI, and the
    /// cause of a start failure (mic busy, another capture active) usually
    /// persists for a while — an instant re-prompt for the same app would nag
    /// without helping. Other apps are unaffected either way.
    fn on_recording_failed(&mut self, _now_ms: u64) -> Vec<DetectionOutput> {
        if let State::Recording {
            cand,
            stop_prompted,
            ..
        } = &self.state
        {
            let app = cand.bundle_id.clone();
            let mut outs = Vec::new();
            if *stop_prompted {
                // Defensive: a start failure normally precedes any suggestion,
                // but if one is somehow up, leaving `recording` retracts it.
                outs.push(DetectionOutput::CancelStopPrompt);
            }
            self.state = State::Idle;
            outs.push(DetectionOutput::Decision(DetectionDecision::new(
                app,
                "recording_failed",
                "idle",
            )));
            return outs;
        }
        Vec::new()
    }
}

/// Fold a helper/renderer/GPU process bundle id to its parent app bundle id, so
/// e.g. a Chrome renderer and the Chrome browser collapse to one candidate.
///
/// Pure and platform-free (the macOS detector calls it) so it is unit-testable.
pub fn normalize_bundle_id(bundle_id: &str) -> String {
    let trimmed = bundle_id.trim();
    let lower = trimmed.to_ascii_lowercase();
    // Safari / WebKit content & GPU processes belong to Safari.
    if lower.starts_with("com.apple.webkit") {
        return "com.apple.Safari".to_string();
    }
    // Chrome/Edge/Brave "...helper (Renderer)" → strip everything from ".helper".
    if let Some(idx) = lower.find(".helper") {
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(session: &str) -> Candidate {
        Candidate {
            app_class: AppClass::NativeMeeting,
            bundle_id: "us.zoom.xos".to_string(),
            session_key: session.to_string(),
        }
    }

    fn browser(session: &str) -> Candidate {
        Candidate {
            app_class: AppClass::Browser,
            bundle_id: "com.google.chrome".to_string(),
            session_key: session.to_string(),
        }
    }

    fn added(cand: Candidate, now_ms: u64) -> DetectionInput {
        DetectionInput::CandidateAdded {
            candidate: cand,
            now_ms,
        }
    }

    fn removed(session_key: &str, now_ms: u64) -> DetectionInput {
        DetectionInput::CandidateRemoved {
            session_key: session_key.to_string(),
            now_ms,
        }
    }

    fn has_show_prompt(outs: &[DetectionOutput]) -> bool {
        outs.iter()
            .any(|o| matches!(o, DetectionOutput::ShowPrompt { .. }))
    }
    fn has_cancel(outs: &[DetectionOutput]) -> bool {
        outs.iter()
            .any(|o| matches!(o, DetectionOutput::CancelPrompt))
    }
    fn has_start(outs: &[DetectionOutput]) -> bool {
        outs.iter()
            .any(|o| matches!(o, DetectionOutput::StartRecording { .. }))
    }

    // --- Bundle normalization -----------------------------------------------

    #[test]
    fn normalizes_helper_and_webkit_processes_to_parent() {
        assert_eq!(
            normalize_bundle_id("com.google.Chrome.helper.Renderer"),
            "com.google.Chrome"
        );
        assert_eq!(
            normalize_bundle_id("com.google.Chrome.helper"),
            "com.google.Chrome"
        );
        assert_eq!(
            normalize_bundle_id("com.apple.WebKit.WebContent"),
            "com.apple.Safari"
        );
        assert_eq!(normalize_bundle_id("us.zoom.xos"), "us.zoom.xos");
    }

    // --- Core happy path ----------------------------------------------------

    #[test]
    fn stable_native_candidate_prompts_after_window_then_accept_records() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        // Appears at t=0.
        let outs = p.handle(added(native("zoom#1"), 0));
        assert!(!has_show_prompt(&outs), "must not prompt immediately");
        // Ticks before the 3s window: still no prompt.
        assert!(!has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 1_000 })
        ));
        assert!(!has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 2_999 })
        ));
        // Tick at/after the window: prompt.
        let outs = p.handle(DetectionInput::Tick { now_ms: 3_000 });
        assert!(has_show_prompt(&outs));
        assert_eq!(p.phase(), "prompted");
        // User accepts → StartRecording (only here, never autonomously).
        let outs = p.handle(DetectionInput::UserAccepted { now_ms: 3_500 });
        assert!(has_start(&outs));
        assert_eq!(p.phase(), "recording");
    }

    // --- Jitter must not prompt --------------------------------------------

    #[test]
    fn flapping_candidate_never_reaches_the_prompt() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 1_000 });
        // Drops before stabilizing.
        p.handle(removed("zoom#1", 1_500));
        assert_eq!(p.phase(), "idle");
        // Re-appears: timer restarts from t=2000, so a tick at 3000 (only 1s in)
        // must not prompt.
        p.handle(added(native("zoom#2"), 2_000));
        let outs = p.handle(DetectionInput::Tick { now_ms: 3_000 });
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "candidate");
    }

    // --- Configured browser prompt -----------------------------------------

    #[test]
    fn configured_browser_prompts_after_stability_but_never_auto_records() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(!has_show_prompt(&p.handle(added(browser("chrome#1"), 0))));
        assert!(!has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 2_999 })
        ));
        let outs = p.handle(DetectionInput::Tick { now_ms: 3_000 });
        assert!(has_show_prompt(&outs));
        assert!(
            !has_start(&outs),
            "a browser must never start recording on its own"
        );
        assert_eq!(p.phase(), "prompted");
        assert!(has_start(
            &p.handle(DetectionInput::UserAccepted { now_ms: 3_500 })
        ));
    }

    // --- Signal disappearance cancels an unshown/shown prompt ---------------

    #[test]
    fn removal_before_prompt_leaves_nothing_to_cancel() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        let outs = p.handle(removed("zoom#1", 500));
        assert!(!has_cancel(&outs), "no prompt was shown yet");
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn removal_after_prompt_cancels_it() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        assert!(has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 3_000 })
        ));
        let outs = p.handle(removed("zoom#1", 4_000));
        assert!(has_cancel(&outs));
        assert_eq!(p.phase(), "idle");
    }

    // --- Cooldown -----------------------------------------------------------

    #[test]
    fn cooldown_suppresses_reprompt_after_dismiss() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        // Dismiss arms a 2-minute cooldown.
        assert!(has_cancel(
            &p.handle(DetectionInput::UserDismissed { now_ms: 3_100 })
        ));
        // A brand-new session for the same app inside the window is suppressed.
        let outs = p.handle(added(native("zoom#2"), 10_000));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
        assert!(outs.iter().any(|o| matches!(
            o,
            DetectionOutput::Decision(d) if d.reason == "cooldown"
        )));
        // After the cooldown elapses, it can prompt again.
        let outs = p.handle(added(native("zoom#3"), 3_100 + 120_000 + 1));
        assert!(matches!(outs.first(), Some(DetectionOutput::Decision(_))));
        assert_eq!(p.phase(), "candidate");
    }

    #[test]
    fn cooldown_after_accept_prevents_immediate_reprompt_on_stop() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        p.handle(DetectionInput::UserAccepted { now_ms: 3_500 });
        // Recording ends; the same app's input lingers and re-reports.
        p.handle(DetectionInput::RecordingFinished { now_ms: 60_000 });
        assert_eq!(p.phase(), "idle");
        let outs = p.handle(added(native("zoom#2"), 60_100));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn late_cooldown_cancels_a_pending_candidate_on_tick() {
        // Arm a cooldown via a dismiss on session #1, then a new session #2 of a
        // *different* app becomes a candidate; the first app re-appearing while
        // still in cooldown must not prompt even if a tick fires mid-window.
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        p.handle(DetectionInput::UserDismissed { now_ms: 3_100 });
        // zoom re-appears (still in cooldown) — becomes no candidate.
        let outs = p.handle(added(native("zoom#2"), 5_000));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
    }

    // --- Ignore self --------------------------------------------------------

    #[test]
    fn ignores_lumens_own_capture() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        let cand = Candidate {
            app_class: AppClass::NativeMeeting, // even if misclassified, self wins
            bundle_id: "com.lumenopen.asr".to_string(),
            session_key: "self#1".to_string(),
        };
        let outs = p.handle(added(cand, 0));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
        assert!(outs.iter().any(|o| matches!(
            o,
            DetectionOutput::Decision(d) if d.reason == "ignored_self"
        )));
    }

    // --- One-at-a-time / idempotency ---------------------------------------

    #[test]
    fn duplicate_add_of_same_session_is_idempotent() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        let outs = p.handle(added(native("zoom#1"), 500));
        assert!(outs.is_empty(), "re-report of same session is a no-op");
        assert_eq!(p.phase(), "candidate");
    }

    #[test]
    fn second_candidate_does_not_preempt_the_first() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        let other = Candidate {
            app_class: AppClass::NativeMeeting,
            bundle_id: "com.tinyspeck.slackmacgap".to_string(),
            session_key: "slack#1".to_string(),
        };
        p.handle(added(other, 500));
        // First candidate still owns the machine and prompts on schedule.
        assert!(has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 3_000 })
        ));
        match p.state.clone() {
            State::Prompted { cand } => assert_eq!(cand.bundle_id, "us.zoom.xos"),
            other => panic!("expected prompted zoom, got {other:?}"),
        }
    }

    // --- Recording-failure exit (no stuck Recording state) ------------------

    #[test]
    fn failed_start_returns_to_idle_and_detection_keeps_working() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        assert!(has_start(
            &p.handle(DetectionInput::UserAccepted { now_ms: 3_500 })
        ));
        assert_eq!(p.phase(), "recording");
        // start_meeting_recording failed → policy must not stay stuck.
        p.handle(DetectionInput::RecordingFailed { now_ms: 4_000 });
        assert_eq!(p.phase(), "idle");
        // A *different* app can become a candidate and prompt again — the
        // machine is not permanently "busy_with_other_candidate".
        let slack = Candidate {
            app_class: AppClass::NativeMeeting,
            bundle_id: "com.tinyspeck.slackmacgap".to_string(),
            session_key: "slack#1".to_string(),
        };
        p.handle(DetectionInput::CandidateAdded {
            candidate: slack,
            now_ms: 5_000,
        });
        assert_eq!(p.phase(), "candidate");
        assert!(has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 8_000 })
        ));
    }

    #[test]
    fn failed_start_keeps_the_accept_cooldown_for_the_same_app() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        p.handle(DetectionInput::UserAccepted { now_ms: 3_500 });
        p.handle(DetectionInput::RecordingFailed { now_ms: 4_000 });
        // Same app inside the cooldown window: suppressed (no nagging while the
        // start-failure cause likely persists) …
        let outs = p.handle(added(native("zoom#2"), 10_000));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
        // … but after the cooldown elapses it can be tracked again.
        p.handle(added(native("zoom#3"), 3_500 + 120_000 + 1));
        assert_eq!(p.phase(), "candidate");
    }

    #[test]
    fn recording_failed_outside_recording_is_a_noop() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(p
            .handle(DetectionInput::RecordingFailed { now_ms: 0 })
            .is_empty());
        assert_eq!(p.phase(), "idle");
        p.handle(added(native("zoom#1"), 0));
        assert!(p
            .handle(DetectionInput::RecordingFailed { now_ms: 100 })
            .is_empty());
        assert_eq!(p.phase(), "candidate");
    }

    // --- Reset (disable while active) ---------------------------------------

    #[test]
    fn reset_while_prompted_cancels_the_prompt() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        assert!(has_show_prompt(
            &p.handle(DetectionInput::Tick { now_ms: 3_000 })
        ));
        let outs = p.reset();
        assert!(has_cancel(&outs), "visible prompt must be retracted");
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn reset_while_idle_or_candidate_emits_no_cancel() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(p.reset().is_empty());
        p.handle(added(native("zoom#1"), 0));
        let outs = p.reset();
        assert!(!has_cancel(&outs), "no prompt was shown");
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn reset_preserves_cooldowns() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        p.handle(DetectionInput::UserDismissed { now_ms: 3_100 });
        p.reset();
        // Re-enabled shortly after: the dismissed app is still in cooldown.
        let outs = p.handle(added(native("zoom#2"), 10_000));
        assert!(!has_show_prompt(&outs));
        assert_eq!(p.phase(), "idle");
        assert!(outs.iter().any(|o| matches!(
            o,
            DetectionOutput::Decision(d) if d.reason == "cooldown"
        )));
    }

    // --- User actions in wrong states are inert -----------------------------

    #[test]
    fn accept_without_prompt_is_a_noop() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(p
            .handle(DetectionInput::UserAccepted { now_ms: 0 })
            .is_empty());
        assert_eq!(p.phase(), "idle");
    }

    // --- End-of-meeting stop suggestion --------------------------------------

    fn has_suggest_stop(outs: &[DetectionOutput]) -> bool {
        outs.iter()
            .any(|o| matches!(o, DetectionOutput::SuggestStop { .. }))
    }
    fn has_cancel_stop(outs: &[DetectionOutput]) -> bool {
        outs.iter()
            .any(|o| matches!(o, DetectionOutput::CancelStopPrompt))
    }

    /// Drive a policy through detect → prompt → accept so it is `recording`
    /// the way a detection-started meeting is. The tracked session is "zoom#1".
    fn recording_policy() -> MeetingDetectionPolicy {
        let mut p = MeetingDetectionPolicy::with_defaults();
        p.handle(added(native("zoom#1"), 0));
        p.handle(DetectionInput::Tick { now_ms: 3_000 });
        p.handle(DetectionInput::UserAccepted { now_ms: 3_500 });
        assert_eq!(p.phase(), "recording");
        p
    }

    #[test]
    fn candidate_gone_stable_suggests_stop_once() {
        let mut p = recording_policy();
        // Input disappears at t=10s.
        let outs = p.handle(removed("zoom#1", 10_000));
        assert!(!has_suggest_stop(&outs), "removal alone must not suggest");
        // Before the 10s stop window: nothing.
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 15_000 })
        ));
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 19_999 })
        ));
        // At/after the window: exactly one suggestion; still recording.
        let outs = p.handle(DetectionInput::Tick { now_ms: 20_000 });
        assert!(has_suggest_stop(&outs));
        assert_eq!(p.phase(), "recording", "suggestion never stops on its own");
        // Later ticks do not re-suggest while the prompt is up.
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 30_000 })
        ));
    }

    #[test]
    fn input_jitter_during_recording_never_suggests_stop() {
        let mut p = recording_policy();
        // Mute/reconnect: input drops and comes back (new session key) within
        // the window — the absence timer must restart.
        p.handle(removed("zoom#1", 10_000));
        p.handle(added(native("zoom#2"), 15_000));
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 25_000 })
        ));
        // The *new* session going away is what counts now: gone at 30s, so a
        // tick at 39.9s is still inside the window …
        p.handle(removed("zoom#2", 30_000));
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 39_999 })
        ));
        // … and only a stable absence finally suggests.
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 40_000 })
        ));
    }

    #[test]
    fn input_returning_after_suggestion_withdraws_it() {
        let mut p = recording_policy();
        p.handle(removed("zoom#1", 10_000));
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 20_000 })
        ));
        // The meeting app's input comes back (rejoin): retract the suggestion.
        let outs = p.handle(added(native("zoom#3"), 21_000));
        assert!(has_cancel_stop(&outs));
        assert_eq!(p.phase(), "recording");
        // And the timer restarted: no immediate re-suggestion.
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 25_000 })
        ));
    }

    #[test]
    fn declining_a_stop_suggestion_suppresses_it_for_this_recording() {
        let mut p = recording_policy();
        p.handle(removed("zoom#1", 10_000));
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 20_000 })
        ));
        // User clicks 继续录制.
        let outs = p.handle(DetectionInput::StopDeclined { now_ms: 21_000 });
        assert!(!has_cancel_stop(&outs), "front-end already hid the prompt");
        assert_eq!(p.phase(), "recording");
        // Even a fresh disappear + long stability never re-suggests now.
        p.handle(added(native("zoom#4"), 30_000));
        p.handle(removed("zoom#4", 40_000));
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 120_000 })
        ));
        assert_eq!(p.phase(), "recording");
    }

    #[test]
    fn suppression_ends_with_the_recording() {
        let mut p = recording_policy();
        p.handle(removed("zoom#1", 10_000));
        p.handle(DetectionInput::Tick { now_ms: 20_000 });
        p.handle(DetectionInput::StopDeclined { now_ms: 21_000 });
        p.handle(DetectionInput::RecordingFinished { now_ms: 60_000 });
        assert_eq!(p.phase(), "idle");
        // Next detection-started recording (after the cooldown) suggests again.
        let t0 = 60_000 + 120_000 + 1;
        p.handle(added(native("zoom#5"), t0));
        p.handle(DetectionInput::Tick { now_ms: t0 + 3_000 });
        p.handle(DetectionInput::UserAccepted { now_ms: t0 + 3_500 });
        p.handle(removed("zoom#5", t0 + 10_000));
        assert!(has_suggest_stop(&p.handle(DetectionInput::Tick {
            now_ms: t0 + 20_000
        })));
    }

    #[test]
    fn recording_finished_while_suggested_retracts_the_stop_prompt() {
        let mut p = recording_policy();
        p.handle(removed("zoom#1", 10_000));
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 20_000 })
        ));
        // The stop command ran (accepted suggestion or a manual stop elsewhere).
        let outs = p.handle(DetectionInput::RecordingFinished { now_ms: 21_000 });
        assert!(has_cancel_stop(&outs));
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn unrelated_removal_during_recording_never_starts_the_stop_timer() {
        let mut p = recording_policy();
        p.handle(removed("slack#9", 10_000));
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 60_000 })
        ));
        assert_eq!(p.phase(), "recording");
    }

    #[test]
    fn manual_meetings_are_never_suggested_on() {
        // A manually started meeting never routes through the policy, so the
        // machine sits idle: removals and ticks must produce nothing.
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(p.handle(removed("zoom#1", 1_000)).is_empty());
        assert!(!has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 60_000 })
        ));
        assert!(p
            .handle(DetectionInput::StopDeclined { now_ms: 61_000 })
            .is_empty());
        assert_eq!(p.phase(), "idle");
    }

    #[test]
    fn stop_declined_without_a_suggestion_is_a_noop() {
        let mut p = recording_policy();
        assert!(p
            .handle(DetectionInput::StopDeclined { now_ms: 10_000 })
            .is_empty());
        // A later real suggestion still works (no accidental suppression).
        p.handle(removed("zoom#1", 20_000));
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 30_000 })
        ));
    }

    #[test]
    fn reset_while_stop_suggested_retracts_the_stop_prompt() {
        let mut p = recording_policy();
        p.handle(removed("zoom#1", 10_000));
        assert!(has_suggest_stop(
            &p.handle(DetectionInput::Tick { now_ms: 20_000 })
        ));
        let outs = p.reset();
        assert!(has_cancel_stop(&outs));
        assert_eq!(p.phase(), "idle");
    }
}
