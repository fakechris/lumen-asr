//! Lumen core: session state machine and shared domain types.
//!
//! No Tauri, no platform FFI, no network — pure orchestration types.

mod export;
mod meeting;
mod meeting_detection;
mod session;
mod types;

pub use export::{export_session_transcript, probe_wav_info, session_to_transcript, AudioInfo};
pub use meeting::{
    attribution_origin, LiveAnnotation, Meeting, MeetingDetail, MeetingStatus, MeetingSummary,
    SegmentChannel, Speaker, SummaryKind, TranscriptSegment,
};
pub use meeting_detection::{
    normalize_bundle_id, AppClass, Candidate, DetectionConfig, DetectionDecision, DetectionInput,
    DetectionOutput, MeetingDetectionPolicy,
};
pub use session::{Session, SessionCommand, SessionEvent, SessionState};
pub use types::{
    CorrectorEngineId, DictEntryKind, DictEntrySource, EditSource, FocusInfo, InsertStrategy,
    SessionRecord, SessionStatus,
};

// ASR engine identity and runtime diagnostics moved to the shared
// `lumen-asr-engine` crate (lumen-suite). Re-exported here so existing
// `lumen_core::…` imports and the persisted serde shapes stay unchanged.
pub use lumen_asr_engine::{AsrEngineId, AsrRuntimeDiagnostics};

// Shared `lumen-transcript.v1` interchange types (lumen-suite), re-exported
// so app-layer callers of the export functions can name the document types
// without a direct dependency on the shared crate.
pub use lumen_transcript as transcript;
