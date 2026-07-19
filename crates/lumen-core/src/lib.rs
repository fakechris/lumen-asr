//! Lumen core: session state machine and shared domain types.
//!
//! No Tauri, no platform FFI, no network — pure orchestration types.

mod session;
mod types;

pub use session::{Session, SessionCommand, SessionEvent, SessionState};
pub use types::{
    AsrEngineId, AsrRuntimeDiagnostics, AsrTokenEvidence, CorrectorEngineId, DictEntryKind,
    DictEntrySource, EditSource, FocusInfo, InsertStrategy, QwenDecodeMode, QwenRuntimeMetrics,
    QwenShadowCandidate, QwenShadowDiagnostics, QwenShadowScore, QwenShadowSpan, QwenShadowStatus,
    SessionRecord, SessionStatus,
};
