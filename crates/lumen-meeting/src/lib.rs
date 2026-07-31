//! Offline meeting pipeline (M2b): turn a pre-recorded wav into a stored,
//! speaker-attributed transcript.
//!
//! Flow: **diarize** the wav into speaker turns (diar-rs, macOS only) ->
//! **transcribe** each turn's audio slice with an injected [`AsrEngine`] ->
//! **assemble** the turns + text into a multi-segment `lumen-transcript.v1`
//! document and the v6 storage rows -> **persist** to [`lumen_store::Store`].
//!
//! ## Platform gating
//! The diarization step depends on `diar-rs` (ONNX Runtime + C++ fbank) and is
//! compiled only under `#[cfg(all(target_os = "macos", feature = "diarize"))]`.
//! Every other build (Windows CI, or macOS without the `diarize` feature) gets
//! a stub that returns [`MeetingError::Unsupported`], so the crate always
//! compiles and no non-macOS target ever resolves or builds `diar-rs`.
//!
//! ## What is tested where
//! The pure "turns + text -> transcript / rows" logic ([`assemble`]) and the
//! storage round-trip are unit-tested with stub data — no models, no audio, no
//! network. The real diar-rs + real-ASR path is exercised by an `#[ignore]`d
//! integration test that needs model weights and a wav (see `tests/`).

mod assemble;
mod cleanup;
mod correct;
pub mod export;
mod merge;
pub mod minutes;
mod pipeline;
mod process;

pub use assemble::{
    assemble_meeting, assemble_meeting_with_channels, new_meeting, speaker_label,
    turn_sample_range, AssembledMeeting, DiarTurn,
};
pub use cleanup::{cleanup_transcript, should_cleanup, CleanupStats};
pub use correct::{correct_segment, correct_words, CorrectionDict};
pub use export::{export_meeting, ExportError, ExportOutput, ExportPreset};
pub use merge::{merge_tracks, system_speaker_offset, MergedTake, TrackTake};
pub use pipeline::{transcribe_meeting, DiarModels, MeetingError, MeetingOptions};
pub use process::{process_meeting, MinutesConfig, ProcessError};

/// The engine speaker-count hint passed to diarization by default.
pub const DEFAULT_MAX_SPEAKERS: usize = 6;
