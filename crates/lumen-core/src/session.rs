//! Session state machine for one dictation turn.

use crate::types::{FocusInfo, InsertStrategy, SessionRecord, SessionStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    CheckingPermissions,
    Listening,
    Transcribing,
    Correcting,
    Review,
    Inserting,
    Verifying,
    CapturingEdits,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    Start,
    PermissionsOk,
    PermissionsDenied { copy_only: bool },
    AudioFinished,
    TranscriptReady { text: String },
    Corrected { text: String },
    CorrectFailed,
    /// User confirmed text (possibly edited) for insert.
    Accept { text: String },
    SkipReview,
    InsertDone { strategy: InsertStrategy },
    InsertFailed,
    VerifyDone,
    EditsFlushed,
    Cancel,
    Reset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    StateChanged { from: SessionState, to: SessionState },
    NeedPermissions,
    StartRecording,
    StopRecording,
    RunAsr,
    RunCorrector { text: String },
    ShowReview { text: String },
    Insert { text: String },
    Verify,
    CaptureEdits,
    Completed { record: SessionRecord },
    Failed { message: String },
    Cancelled,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("invalid transition: state={state:?} command={command}")]
    InvalidTransition {
        state: SessionState,
        command: String,
    },
}

/// Pure state machine for a single dictation session.
#[derive(Debug, Clone)]
pub struct Session {
    state: SessionState,
    record: SessionRecord,
    /// Text shown to user / ready to insert (after correct or edit).
    working_text: String,
    copy_only: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            record: SessionRecord::new(),
            working_text: String::new(),
            copy_only: false,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    pub fn working_text(&self) -> &str {
        &self.working_text
    }

    pub fn set_focus(&mut self, focus: FocusInfo) {
        self.record.focus = focus;
    }

    pub fn set_engines(&mut self, asr: impl Into<String>, corrector: impl Into<String>) {
        self.record.asr_engine = Some(asr.into());
        self.record.corrector_engine = Some(corrector.into());
    }

    pub fn handle(&mut self, cmd: SessionCommand) -> Result<Vec<SessionEvent>, SessionError> {
        use SessionCommand as C;
        use SessionState as S;

        let mut events = Vec::new();
        let from = self.state;

        let transition = match (self.state, &cmd) {
            (S::Idle, C::Start) => Some((S::CheckingPermissions, vec![SessionEvent::NeedPermissions])),

            (S::CheckingPermissions, C::PermissionsOk) => Some((
                S::Listening,
                vec![SessionEvent::StartRecording],
            )),
            (S::CheckingPermissions, C::PermissionsDenied { copy_only }) => {
                self.copy_only = *copy_only;
                if *copy_only {
                    Some((S::Listening, vec![SessionEvent::StartRecording]))
                } else {
                    self.record.status = SessionStatus::Failed;
                    Some((
                        S::Error,
                        vec![SessionEvent::Failed {
                            message: "microphone or accessibility permission missing".into(),
                        }],
                    ))
                }
            }

            (S::Listening, C::AudioFinished) => Some((
                S::Transcribing,
                vec![
                    SessionEvent::StopRecording,
                    SessionEvent::RunAsr,
                ],
            )),
            (S::Listening, C::Cancel) => {
                self.record.status = SessionStatus::Cancelled;
                Some((S::Idle, vec![SessionEvent::Cancelled]))
            }

            (S::Transcribing, C::TranscriptReady { text }) => {
                self.record.asr_raw = Some(text.clone());
                self.working_text = text.clone();
                Some((
                    S::Correcting,
                    vec![SessionEvent::RunCorrector { text: text.clone() }],
                ))
            }
            (S::Transcribing, C::Cancel) => {
                self.record.status = SessionStatus::Cancelled;
                Some((S::Idle, vec![SessionEvent::Cancelled]))
            }

            (S::Correcting, C::Corrected { text }) => {
                self.record.corrected = Some(text.clone());
                self.working_text = text.clone();
                Some((
                    S::Review,
                    vec![SessionEvent::ShowReview { text: text.clone() }],
                ))
            }
            (S::Correcting, C::CorrectFailed) => {
                // Fail soft: use ASR (or preprocess) text.
                let text = self.working_text.clone();
                self.record.corrected = Some(text.clone());
                Some((
                    S::Review,
                    vec![SessionEvent::ShowReview { text }],
                ))
            }

            (S::Review, C::Accept { text }) => {
                self.working_text = text.clone();
                let insert_text = text.clone();
                if self.copy_only {
                    self.record.insert_strategy = InsertStrategy::CopyOnly;
                    self.record.pasted = Some(insert_text.clone());
                    Some((
                        S::CapturingEdits,
                        vec![
                            SessionEvent::Insert { text: insert_text },
                            SessionEvent::CaptureEdits,
                        ],
                    ))
                } else {
                    Some((
                        S::Inserting,
                        vec![SessionEvent::Insert { text: insert_text }],
                    ))
                }
            }
            (S::Review, C::SkipReview) => {
                let text = self.working_text.clone();
                Some((S::Inserting, vec![SessionEvent::Insert { text }]))
            }
            (S::Review, C::Cancel) => {
                self.record.status = SessionStatus::Cancelled;
                Some((S::Idle, vec![SessionEvent::Cancelled]))
            }

            (S::Inserting, C::InsertDone { strategy }) => {
                self.record.insert_strategy = *strategy;
                self.record.pasted = Some(self.working_text.clone());
                Some((
                    S::Verifying,
                    vec![SessionEvent::Verify],
                ))
            }
            (S::Inserting, C::InsertFailed) => {
                // Still save text; mark copy-only-ish failure path.
                self.record.pasted = Some(self.working_text.clone());
                self.record.insert_strategy = InsertStrategy::None;
                Some((S::CapturingEdits, vec![SessionEvent::CaptureEdits]))
            }

            (S::Verifying, C::VerifyDone) | (S::Verifying, C::SkipReview) => {
                Some((S::CapturingEdits, vec![SessionEvent::CaptureEdits]))
            }

            (S::CapturingEdits, C::EditsFlushed) => {
                self.record.status = SessionStatus::Completed;
                let record = self.record.clone();
                Some((S::Idle, vec![SessionEvent::Completed { record }]))
            }

            (S::Error, C::Reset) | (_, C::Reset) => {
                *self = Self::new();
                return Ok(vec![SessionEvent::StateChanged {
                    from,
                    to: SessionState::Idle,
                }]);
            }

            _ => None,
        };

        let Some((to, mut produced)) = transition else {
            return Err(SessionError::InvalidTransition {
                state: self.state,
                command: format!("{cmd:?}"),
            });
        };

        if from != to {
            self.state = to;
            events.push(SessionEvent::StateChanged { from, to });
        }
        events.append(&mut produced);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_to_completed() {
        let mut s = Session::new();
        s.handle(SessionCommand::Start).unwrap();
        assert_eq!(s.state(), SessionState::CheckingPermissions);

        s.handle(SessionCommand::PermissionsOk).unwrap();
        assert_eq!(s.state(), SessionState::Listening);

        s.handle(SessionCommand::AudioFinished).unwrap();
        s.handle(SessionCommand::TranscriptReady {
            text: "你好 世界".into(),
        })
        .unwrap();
        assert_eq!(s.state(), SessionState::Correcting);

        s.handle(SessionCommand::Corrected {
            text: "你好，世界".into(),
        })
        .unwrap();
        assert_eq!(s.state(), SessionState::Review);

        s.handle(SessionCommand::Accept {
            text: "你好，世界。".into(),
        })
        .unwrap();
        assert_eq!(s.state(), SessionState::Inserting);

        s.handle(SessionCommand::InsertDone {
            strategy: InsertStrategy::Paste,
        })
        .unwrap();
        s.handle(SessionCommand::VerifyDone).unwrap();
        let ev = s.handle(SessionCommand::EditsFlushed).unwrap();
        assert_eq!(s.state(), SessionState::Idle);
        assert!(ev.iter().any(|e| matches!(e, SessionEvent::Completed { .. })));
        assert_eq!(s.record().asr_raw.as_deref(), Some("你好 世界"));
        assert_eq!(s.record().pasted.as_deref(), Some("你好，世界。"));
    }

    #[test]
    fn corrector_failure_still_reaches_review() {
        let mut s = Session::new();
        s.handle(SessionCommand::Start).unwrap();
        s.handle(SessionCommand::PermissionsOk).unwrap();
        s.handle(SessionCommand::AudioFinished).unwrap();
        s.handle(SessionCommand::TranscriptReady {
            text: "raw".into(),
        })
        .unwrap();
        s.handle(SessionCommand::CorrectFailed).unwrap();
        assert_eq!(s.state(), SessionState::Review);
        assert_eq!(s.working_text(), "raw");
    }

    #[test]
    fn invalid_transition_errors() {
        let mut s = Session::new();
        let err = s
            .handle(SessionCommand::AudioFinished)
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidTransition { .. }));
    }
}
