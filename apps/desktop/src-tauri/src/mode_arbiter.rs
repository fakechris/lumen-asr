//! Capture-mode arbiter — mutual exclusion between dictation and meeting
//! recording.
//!
//! A meeting is a long, **exclusive** capture mode: while one is recording we
//! suspend the dictation global hotkey (see MEETING.md, Stage M3, "模式互斥").
//! Two reasons: (1) cpal takes a single input device — the mic is held
//! exclusively; (2) dictation injects text at the cursor, so a stray hotkey
//! during a meeting would pollute whatever document is focused.
//!
//! The transition logic here is **pure** and unit-tested. It returns a
//! [`HotkeyAction`] describing the side effect the caller must apply against the
//! real `tauri-plugin-global-shortcut` — the arbiter itself never touches the
//! plugin, so the state machine is testable in isolation.

use std::sync::Mutex;

/// Which capture path currently owns the microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Nothing is capturing; dictation hotkeys are live.
    Idle,
    /// A dictation hold-to-talk capture is in flight (driven by the hotkey).
    Dictation,
    /// A meeting is recording; dictation hotkeys are suspended.
    MeetingRecording,
}

/// Side effect the caller must apply to the global-shortcut plugin after a
/// successful transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// No hotkey change needed.
    None,
    /// Unregister the dictation hotkeys (entering meeting recording).
    Suspend,
    /// Re-register the dictation hotkeys (leaving meeting recording).
    Resume,
}

/// Why a requested transition was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbiterError {
    /// Another capture mode is active and must end first.
    Busy(CaptureMode),
    /// Asked to stop a meeting when none is recording.
    NotRecording,
}

impl std::fmt::Display for ArbiterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArbiterError::Busy(mode) => write!(f, "capture busy: {mode:?}"),
            ArbiterError::NotRecording => write!(f, "no meeting is recording"),
        }
    }
}

impl std::error::Error for ArbiterError {}

/// Pure transition: what happens when a meeting wants to start from `mode`.
///
/// Only allowed from [`CaptureMode::Idle`]; any active mode (a dictation
/// capture, or an already-running meeting) rejects with [`ArbiterError::Busy`].
pub fn plan_begin_meeting(mode: CaptureMode) -> Result<HotkeyAction, ArbiterError> {
    match mode {
        CaptureMode::Idle => Ok(HotkeyAction::Suspend),
        other => Err(ArbiterError::Busy(other)),
    }
}

/// Pure transition: what happens when a meeting wants to stop from `mode`.
pub fn plan_end_meeting(mode: CaptureMode) -> Result<HotkeyAction, ArbiterError> {
    match mode {
        CaptureMode::MeetingRecording => Ok(HotkeyAction::Resume),
        _ => Err(ArbiterError::NotRecording),
    }
}

/// Pure transition: what happens when a dictation capture wants to start from
/// `mode`.
///
/// Only allowed from [`CaptureMode::Idle`]. A running meeting rejects with
/// [`ArbiterError::Busy`] — this is the meeting→dictation half of the mutual
/// exclusion (the dictation hotkey is already suspended during a meeting, so
/// this is a defensive backstop). No hotkey side effect: dictation never touches
/// its own global shortcut, so the action is always [`HotkeyAction::None`].
pub fn plan_begin_dictation(mode: CaptureMode) -> Result<HotkeyAction, ArbiterError> {
    match mode {
        CaptureMode::Idle => Ok(HotkeyAction::None),
        other => Err(ArbiterError::Busy(other)),
    }
}

/// Thread-safe holder of the current [`CaptureMode`]. Applies the pure
/// transitions above and only mutates on success.
pub struct CaptureArbiter {
    mode: Mutex<CaptureMode>,
}

impl Default for CaptureArbiter {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureArbiter {
    pub fn new() -> Self {
        Self {
            mode: Mutex::new(CaptureMode::Idle),
        }
    }

    pub fn mode(&self) -> CaptureMode {
        *self.mode.lock().expect("arbiter mutex poisoned")
    }

    pub fn is_meeting_recording(&self) -> bool {
        self.mode() == CaptureMode::MeetingRecording
    }

    /// Enter meeting recording. On success the mode becomes
    /// [`CaptureMode::MeetingRecording`] and the returned [`HotkeyAction`] tells
    /// the caller to suspend the dictation hotkeys.
    pub fn begin_meeting(&self) -> Result<HotkeyAction, ArbiterError> {
        let mut guard = self.mode.lock().expect("arbiter mutex poisoned");
        let action = plan_begin_meeting(*guard)?;
        *guard = CaptureMode::MeetingRecording;
        Ok(action)
    }

    /// Leave meeting recording. On success the mode returns to
    /// [`CaptureMode::Idle`] and the caller should resume dictation hotkeys.
    pub fn end_meeting(&self) -> Result<HotkeyAction, ArbiterError> {
        let mut guard = self.mode.lock().expect("arbiter mutex poisoned");
        let action = plan_end_meeting(*guard)?;
        *guard = CaptureMode::Idle;
        Ok(action)
    }

    /// Enter dictation capture. On success the mode becomes
    /// [`CaptureMode::Dictation`]. Rejects with [`ArbiterError::Busy`] if a
    /// meeting (or another dictation capture) already owns the mic, which makes
    /// "no dictation while a meeting is recording" a real, enforced invariant.
    pub fn begin_dictation(&self) -> Result<(), ArbiterError> {
        let mut guard = self.mode.lock().expect("arbiter mutex poisoned");
        plan_begin_dictation(*guard)?;
        *guard = CaptureMode::Dictation;
        Ok(())
    }

    /// Leave dictation capture, returning to [`CaptureMode::Idle`]. Idempotent
    /// and *only* clears when the current mode is actually
    /// [`CaptureMode::Dictation`] — it never clobbers an in-flight meeting, so a
    /// stray dictation-end can't accidentally release a meeting's exclusive
    /// hold.
    pub fn end_dictation(&self) {
        let mut guard = self.mode.lock().expect("arbiter mutex poisoned");
        if *guard == CaptureMode::Dictation {
            *guard = CaptureMode::Idle;
        }
    }

    /// Force the mode back to [`CaptureMode::Idle`] without emitting a hotkey
    /// action. Used to roll back a half-started meeting (e.g. the recorder
    /// failed to open the device) before any hotkey suspend was applied.
    pub fn force_idle(&self) {
        *self.mode.lock().expect("arbiter mutex poisoned") = CaptureMode::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_begin_only_from_idle() {
        assert_eq!(
            plan_begin_meeting(CaptureMode::Idle),
            Ok(HotkeyAction::Suspend)
        );
        assert_eq!(
            plan_begin_meeting(CaptureMode::Dictation),
            Err(ArbiterError::Busy(CaptureMode::Dictation))
        );
        assert_eq!(
            plan_begin_meeting(CaptureMode::MeetingRecording),
            Err(ArbiterError::Busy(CaptureMode::MeetingRecording))
        );
    }

    #[test]
    fn plan_end_only_from_meeting() {
        assert_eq!(
            plan_end_meeting(CaptureMode::MeetingRecording),
            Ok(HotkeyAction::Resume)
        );
        assert_eq!(
            plan_end_meeting(CaptureMode::Idle),
            Err(ArbiterError::NotRecording)
        );
        assert_eq!(
            plan_end_meeting(CaptureMode::Dictation),
            Err(ArbiterError::NotRecording)
        );
    }

    #[test]
    fn arbiter_round_trips_and_rejects_double_start() {
        let arbiter = CaptureArbiter::new();
        assert_eq!(arbiter.mode(), CaptureMode::Idle);
        assert!(!arbiter.is_meeting_recording());

        assert_eq!(arbiter.begin_meeting(), Ok(HotkeyAction::Suspend));
        assert_eq!(arbiter.mode(), CaptureMode::MeetingRecording);
        assert!(arbiter.is_meeting_recording());

        // Second start is rejected while recording.
        assert_eq!(
            arbiter.begin_meeting(),
            Err(ArbiterError::Busy(CaptureMode::MeetingRecording))
        );

        assert_eq!(arbiter.end_meeting(), Ok(HotkeyAction::Resume));
        assert_eq!(arbiter.mode(), CaptureMode::Idle);

        // Stopping when idle is rejected.
        assert_eq!(arbiter.end_meeting(), Err(ArbiterError::NotRecording));
    }

    #[test]
    fn plan_begin_dictation_only_from_idle() {
        assert_eq!(
            plan_begin_dictation(CaptureMode::Idle),
            Ok(HotkeyAction::None)
        );
        assert_eq!(
            plan_begin_dictation(CaptureMode::MeetingRecording),
            Err(ArbiterError::Busy(CaptureMode::MeetingRecording))
        );
        assert_eq!(
            plan_begin_dictation(CaptureMode::Dictation),
            Err(ArbiterError::Busy(CaptureMode::Dictation))
        );
    }

    #[test]
    fn dictation_round_trips_and_returns_to_idle() {
        let arbiter = CaptureArbiter::new();
        assert_eq!(arbiter.mode(), CaptureMode::Idle);

        assert_eq!(arbiter.begin_dictation(), Ok(()));
        assert_eq!(arbiter.mode(), CaptureMode::Dictation);

        arbiter.end_dictation();
        assert_eq!(arbiter.mode(), CaptureMode::Idle);
    }

    #[test]
    fn meeting_and_dictation_are_mutually_exclusive_both_ways() {
        // A dictation in flight blocks a meeting from starting.
        let arbiter = CaptureArbiter::new();
        assert_eq!(arbiter.begin_dictation(), Ok(()));
        assert_eq!(
            arbiter.begin_meeting(),
            Err(ArbiterError::Busy(CaptureMode::Dictation))
        );
        arbiter.end_dictation();

        // A meeting in flight blocks dictation from starting.
        assert_eq!(arbiter.begin_meeting(), Ok(HotkeyAction::Suspend));
        assert_eq!(
            arbiter.begin_dictation(),
            Err(ArbiterError::Busy(CaptureMode::MeetingRecording))
        );
        assert_eq!(arbiter.mode(), CaptureMode::MeetingRecording);
    }

    #[test]
    fn end_dictation_never_clobbers_a_meeting() {
        let arbiter = CaptureArbiter::new();
        assert_eq!(arbiter.begin_meeting(), Ok(HotkeyAction::Suspend));
        // A stray dictation-end must not release the meeting's exclusive hold.
        arbiter.end_dictation();
        assert_eq!(arbiter.mode(), CaptureMode::MeetingRecording);
        assert_eq!(arbiter.end_meeting(), Ok(HotkeyAction::Resume));
        assert_eq!(arbiter.mode(), CaptureMode::Idle);
    }

    #[test]
    fn end_dictation_is_idempotent_when_idle() {
        let arbiter = CaptureArbiter::new();
        arbiter.end_dictation();
        assert_eq!(arbiter.mode(), CaptureMode::Idle);
    }

    #[test]
    fn force_idle_rolls_back_without_action() {
        let arbiter = CaptureArbiter::new();
        assert_eq!(arbiter.begin_meeting(), Ok(HotkeyAction::Suspend));
        // Simulate recorder-start failure: roll back.
        arbiter.force_idle();
        assert_eq!(arbiter.mode(), CaptureMode::Idle);
        // A subsequent start works again.
        assert_eq!(arbiter.begin_meeting(), Ok(HotkeyAction::Suspend));
    }
}
