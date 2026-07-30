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
//! ## Classification & the browser carve-out
//! A raw candidate carries an [`AppClass`]. Native meeting apps (a small bundle
//! id allow-list) are promptable on input activity alone. Browsers are *not*
//! promptable on microphone alone — a page holding the mic is far too weak a
//! signal (voice messages, mic tests, dictation, permission pre-grants all
//! trip it). Browser candidates are tracked-but-never-prompted here, leaving a
//! clean extension point for a future additional signal. `Other` apps are
//! likewise not prompted.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Coarse classification of the app holding an audio input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppClass {
    /// A known native meeting/conferencing app (allow-listed bundle id).
    NativeMeeting,
    /// A web browser (or one of its helper/renderer processes).
    Browser,
    /// Anything else holding an input.
    Other,
}

impl AppClass {
    /// Whether input activity from this class is, on its own, strong enough
    /// evidence to prompt. Only native meeting apps qualify today; browsers and
    /// everything else need an additional signal that this policy does not yet
    /// have, so they are tracked but never prompted.
    pub fn promptable_on_input_alone(self) -> bool {
        matches!(self, AppClass::NativeMeeting)
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
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            stability_ms: 3_000,
            cooldown_ms: 120_000,
            self_bundle_ids: vec!["com.lumenopen.asr".to_string(), "lumen".to_string()],
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
    /// The user accepted; a recording is in progress for `cand`.
    Recording { cand: Candidate },
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
            | State::Recording { cand } => Some(cand.session_key.as_str()),
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
        }
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
        // Browsers / other apps: tracked-but-not-prompted (extension point).
        if !cand.app_class.promptable_on_input_alone() {
            let reason = match cand.app_class {
                AppClass::Browser => "browser_needs_additional_signal",
                _ => "class_not_promptable",
            };
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                cand.bundle_id,
                "input_active",
                reason,
            ))];
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

    fn on_candidate_removed(&mut self, session_key: &str, _now_ms: u64) -> Vec<DetectionOutput> {
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
            // While Recording (or for an unrelated key), removal is ignored:
            // the recording is user-owned and outlives the detection signal.
            _ => Vec::new(),
        }
    }

    fn on_tick(&mut self, now_ms: u64) -> Vec<DetectionOutput> {
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
        self.state = State::Recording { cand };
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
        if let State::Recording { cand } = &self.state {
            let app = cand.bundle_id.clone();
            self.state = State::Idle;
            return vec![DetectionOutput::Decision(DetectionDecision::new(
                app,
                "recording_ended",
                "idle",
            ))];
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

/// Bundle ids (normalized, lowercased) of known native meeting/conferencing
/// apps. Kept intentionally small and specific.
const NATIVE_MEETING_BUNDLE_IDS: &[&str] = &[
    "us.zoom.xos",                     // Zoom
    "com.microsoft.teams",             // Teams (classic)
    "com.microsoft.teams2",            // Teams (new)
    "com.tinyspeck.slackmacgap",       // Slack
    "com.apple.facetime",              // FaceTime
    "com.cisco.webexmeetingsapp",      // Webex
    "com.webex.meetingmanager",        // Webex (legacy)
    "com.hnc.discord",                 // Discord
    "com.skype.skype",                 // Skype
    "com.microsoft.skypeforbusiness",  // Skype for Business
    "com.google.meetings",             // Google Meet (native wrapper)
    "com.readdle.smartemail-mac.meet", // (defensive; harmless if unused)
];

/// Bundle ids (normalized, lowercased) of common web browsers.
const BROWSER_BUNDLE_IDS: &[&str] = &[
    "com.apple.safari",
    "com.google.chrome",
    "com.google.chrome.canary",
    "com.google.chrome.beta",
    "com.microsoft.edgemac",
    "org.mozilla.firefox",
    "com.brave.browser",
    "com.operasoftware.opera",
    "company.thebrowser.browser", // Arc
    "com.vivaldi.vivaldi",
];

/// Classify an *already-normalized* bundle id.
pub fn classify_bundle_id(bundle_id: &str) -> AppClass {
    let b = bundle_id.to_ascii_lowercase();
    if NATIVE_MEETING_BUNDLE_IDS.contains(&b.as_str()) {
        return AppClass::NativeMeeting;
    }
    if BROWSER_BUNDLE_IDS.contains(&b.as_str()) {
        return AppClass::Browser;
    }
    AppClass::Other
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

    // --- Classification & normalization -------------------------------------

    #[test]
    fn classifies_native_browser_and_other() {
        assert_eq!(classify_bundle_id("us.zoom.xos"), AppClass::NativeMeeting);
        assert_eq!(
            classify_bundle_id("com.microsoft.teams2"),
            AppClass::NativeMeeting
        );
        assert_eq!(classify_bundle_id("com.google.chrome"), AppClass::Browser);
        assert_eq!(classify_bundle_id("com.apple.safari"), AppClass::Browser);
        assert_eq!(classify_bundle_id("com.acme.notes"), AppClass::Other);
    }

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

    #[test]
    fn normalized_then_classified_folds_chrome_renderer_to_browser() {
        let norm = normalize_bundle_id("com.google.Chrome.helper (Renderer)");
        assert_eq!(classify_bundle_id(&norm), AppClass::Browser);
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

    // --- Browser carve-out --------------------------------------------------

    #[test]
    fn browser_mic_alone_never_prompts() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        let outs = p.handle(added(browser("chrome#1"), 0));
        assert!(!has_show_prompt(&outs));
        // Even after a long stable period, a browser is never promoted.
        for t in (1_000..=10_000).step_by(1_000) {
            assert!(!has_show_prompt(
                &p.handle(DetectionInput::Tick { now_ms: t })
            ));
        }
        assert_eq!(p.phase(), "idle");
        // The decision trail records *why* it was skipped.
        let decisions: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                DetectionOutput::Decision(d) => Some(d.reason.as_str()),
                _ => None,
            })
            .collect();
        assert!(decisions.contains(&"browser_needs_additional_signal"));
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

    // --- User actions in wrong states are inert -----------------------------

    #[test]
    fn accept_without_prompt_is_a_noop() {
        let mut p = MeetingDetectionPolicy::with_defaults();
        assert!(p
            .handle(DetectionInput::UserAccepted { now_ms: 0 })
            .is_empty());
        assert_eq!(p.phase(), "idle");
    }
}
