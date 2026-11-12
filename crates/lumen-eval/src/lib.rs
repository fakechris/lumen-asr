//! Scoring metrics and gate harness for ASR / cleanup quality regression.
//!
//! The crate is deliberately **pure**: no audio, no models, no network. It
//! turns (reference, hypothesis) text pairs into comparable numbers and a
//! pass/fail gate, so it can run in `cargo test` on any platform while the
//! audio-dependent side (`eval synth` + `eval asr` in the desktop binary)
//! stays a local, macOS-first workflow.
//!
//! Layers:
//! - [`normalize`]: canonicalize text for scoring — fullwidth → ASCII,
//!   lowercase, Chinese numerals → digits, English number words → digits,
//!   punctuation and standalone fillers dropped (from both sides, so a
//!   filler-heavy transcript is not penalized on raw-ASR scoring).
//! - [`metrics`]: token-level error rate — a CER on Chinese, a WER on
//!   English, both on mixed speech (one token per CJK char, one per Latin
//!   word).
//! - [`corpus`]: the checked-in clip set (`data/asr-corpus.json`) with
//!   per-clip expectations (`clean`, `must`, `must_not`, `terms`) and
//!   long clips assembled from named sources.
//! - [`checks`]: cleanup checks (filler residue, lost phrases, invented
//!   tokens, language drift), dictionary term hits, and the speaker-count
//!   guard for diarization collapse.
//! - [`report`]: per-clip results, per-set aggregates, JSON report and the
//!   baseline comparison that turns a report into a gate verdict.

pub mod checks;
pub mod corpus;
pub mod metrics;
pub mod normalize;
pub mod report;
pub mod selftest;

pub use checks::{
    cleanup_checks, count_speaker_labels, speaker_count_guard, CheckFailure, SpeakerGuard,
};
pub use corpus::{Corpus, EvalClip, EvalSet, BASELINES_JSON, BASE_CORPUS_JSON};
pub use metrics::{edit_distance, error_rate, ErrorRate};
pub use normalize::normalize_for_scoring;
pub use report::{BaselineEntry, Baselines, ClipResult, EngineBaseline, EvalReport, SetSummary};
