//! Lumen core: session state machine and shared domain types.
//!
//! No Tauri, no platform FFI, no network — pure orchestration types.

mod session;
mod types;

pub use session::{Session, SessionCommand, SessionEvent, SessionState};
pub use types::{
    CorrectorEngineId, DictEntryKind, DictEntrySource, EditSource, FocusInfo, InsertStrategy,
    SessionRecord, SessionStatus,
};

// ASR engine identity and runtime diagnostics moved to the shared
// `lumen-asr-engine` crate (lumen-suite). Re-exported here so existing
// `lumen_core::…` imports and the persisted serde shapes stay unchanged.
pub use lumen_asr_engine::{
    AsrEngineId, AsrRuntimeDiagnostics, AsrTokenEvidence, QwenDecodeMode, QwenRuntimeMetrics,
    QwenShadowCandidate, QwenShadowDiagnostics, QwenShadowScore, QwenShadowSpan, QwenShadowStatus,
};
