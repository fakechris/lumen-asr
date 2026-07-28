//! Orchestration: diarize -> per-turn ASR -> assemble -> persist.
//!
//! The diarization step ([`diarize_wav`]) is the only part that touches
//! `diar-rs`; it is compiled real only under
//! `#[cfg(all(target_os = "macos", feature = "diarize"))]` and is a
//! [`MeetingError::Unsupported`] stub everywhere else. The rest of the pipeline
//! (ASR fan-out over turns, assembly, storage) is cross-platform.

use std::path::{Path, PathBuf};

use lumen_asr_engine::{AsrEngine, AsrError, AsrRequest};
use lumen_core::MeetingStatus;
use lumen_store::Store;
use thiserror::Error;
use uuid::Uuid;

use crate::assemble::{assemble_meeting, new_meeting, turn_sample_range, DiarTurn};

/// Filesystem locations of the three diarization model artifacts.
///
/// Cross-platform on purpose (no `diar-rs` types) so callers on any OS can
/// build one. On the macOS path it is mapped into `diar_rs::ModelPaths`.
#[derive(Debug, Clone)]
pub struct DiarModels {
    pub segmentation: PathBuf,
    pub embedding: PathBuf,
    pub plda_dir: PathBuf,
}

impl DiarModels {
    pub fn new(
        segmentation: impl Into<PathBuf>,
        embedding: impl Into<PathBuf>,
        plda_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            segmentation: segmentation.into(),
            embedding: embedding.into(),
            plda_dir: plda_dir.into(),
        }
    }

    /// Resolve the standard layout under a diar model root:
    /// `<root>/seg.onnx`, `<root>/emb.onnx`, `<root>/plda/`.
    ///
    /// Convention (soft — not yet load-bearing anywhere else): the Lumen models
    /// root has a `diar/` subdir, so callers typically pass
    /// `lumen_models::lumen_models_dir().join("diar")`.
    pub fn under_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            segmentation: root.join("seg.onnx"),
            embedding: root.join("emb.onnx"),
            plda_dir: root.join("plda"),
        }
    }
}

/// Knobs for one offline run. All optional; `Default` is a plain run.
#[derive(Debug, Clone, Default)]
pub struct MeetingOptions {
    /// Title stored on the meeting row.
    pub title: Option<String>,
    /// ISO-639-1 language hint forwarded to the ASR engine per turn.
    pub language_hint: Option<String>,
    /// Hotwords forwarded to the ASR engine per turn.
    pub hotwords: Vec<String>,
    /// Upper bound on speaker count for clustering (maps to diar-rs
    /// `ahc_max_speakers`). `None` keeps the diar-rs default.
    pub max_speakers: Option<usize>,
}

/// Failure modes of [`transcribe_meeting`].
#[derive(Debug, Error)]
pub enum MeetingError {
    /// This build cannot diarize (non-macOS, or macOS without `diarize`).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The diarization step failed.
    #[error("diarization failed: {0}")]
    Diarize(String),
    /// A per-turn ASR call failed.
    #[error("asr failed: {0}")]
    Asr(#[source] AsrError),
    /// A storage operation failed.
    #[error("store: {0}")]
    Store(#[source] anyhow::Error),
}

/// Result of diarization: the decoded 16 kHz mono samples plus the speaker
/// turns, so the caller can slice per-turn audio for ASR without reloading.
struct DiarOutput {
    samples: Vec<f32>,
    sample_rate: u32,
    turns: Vec<DiarTurn>,
}

/// Transcribe a pre-recorded `wav` into a stored, speaker-attributed meeting.
///
/// Returns the new [`Meeting`](lumen_core::Meeting) id. On success the v6 tables
/// hold: one `meetings` row (status `ready`), one `speakers` row per distinct
/// diarization speaker, and one `transcript_segments` row per turn.
///
/// Errors: [`MeetingError::Unsupported`] when this build cannot diarize;
/// otherwise a diarization, ASR, or storage failure.
pub async fn transcribe_meeting(
    wav: &Path,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    store: &Store,
    opts: &MeetingOptions,
) -> Result<Uuid, MeetingError> {
    let diar = diarize_wav(wav, diar_models, opts)?;
    let duration =
        (diar.sample_rate > 0).then(|| diar.samples.len() as f64 / diar.sample_rate as f64);

    // Transcribe each turn's audio slice. Order is preserved so turns and texts
    // zip positionally in `assemble_meeting`.
    let mut texts = Vec::with_capacity(diar.turns.len());
    for turn in &diar.turns {
        texts.push(transcribe_turn(asr_engine, &diar.samples, diar.sample_rate, turn, opts).await?);
    }

    let mut meeting = new_meeting(wav.to_str().map(str::to_string), duration);
    meeting.title = opts.title.clone();
    let meeting_id = meeting.id;

    let assembled = assemble_meeting(
        meeting_id,
        &diar.turns,
        &texts,
        Some(diar.sample_rate),
        duration,
    );

    store
        .create_meeting(&meeting)
        .map_err(MeetingError::Store)?;
    for speaker in &assembled.speakers {
        store.upsert_speaker(speaker).map_err(MeetingError::Store)?;
    }
    store
        .add_segments(&assembled.segments)
        .map_err(MeetingError::Store)?;
    store
        .update_meeting_status(meeting_id, MeetingStatus::Ready)
        .map_err(MeetingError::Store)?;

    Ok(meeting_id)
}

/// Transcribe one turn's audio slice. Empty/out-of-bounds ranges (and engines
/// that reject empty audio) yield an empty string rather than failing the run.
async fn transcribe_turn(
    engine: &dyn AsrEngine,
    samples: &[f32],
    sample_rate: u32,
    turn: &DiarTurn,
    opts: &MeetingOptions,
) -> Result<String, MeetingError> {
    let Some((start, end)) = turn_sample_range(turn.start, turn.end, sample_rate, samples.len())
    else {
        return Ok(String::new());
    };
    let mut request = AsrRequest::new(samples[start..end].to_vec(), sample_rate);
    request.hotwords = opts.hotwords.clone();
    request.language_hint = opts.language_hint.clone();
    match engine.transcribe(request).await {
        Ok(result) => Ok(result.text),
        Err(AsrError::EmptyAudio) => Ok(String::new()),
        Err(error) => Err(MeetingError::Asr(error)),
    }
}

/// Real diarization via `diar-rs`. macOS + `diarize` feature only.
#[cfg(all(target_os = "macos", feature = "diarize"))]
fn diarize_wav(
    wav: &Path,
    models: &DiarModels,
    opts: &MeetingOptions,
) -> Result<DiarOutput, MeetingError> {
    use diar_rs::{audio, diarize, DiarizeConfig, ModelPaths};

    let model_paths = ModelPaths {
        segmentation: models.segmentation.clone(),
        embedding: models.embedding.clone(),
        plda_dir: models.plda_dir.clone(),
    };
    let mut cfg = DiarizeConfig::default();
    if let Some(max) = opts.max_speakers {
        cfg.ahc_max_speakers = max;
    }

    // Decode once for slicing; diarize() reloads internally (offline, cheap).
    let (samples, sample_rate) =
        audio::load_wav_mono16k(wav).map_err(|e| MeetingError::Diarize(e.to_string()))?;
    let result =
        diarize(wav, &model_paths, &cfg).map_err(|e| MeetingError::Diarize(e.to_string()))?;
    let turns = result
        .timeline
        .iter()
        .map(|t| DiarTurn::new(t.start, t.end, t.speaker))
        .collect();

    Ok(DiarOutput {
        samples,
        sample_rate,
        turns,
    })
}

/// Stub for every non-diarizing build (Windows CI, or macOS without the
/// `diarize` feature). Keeps the crate compiling and callable everywhere while
/// never referencing `diar-rs`.
#[cfg(not(all(target_os = "macos", feature = "diarize")))]
fn diarize_wav(
    _wav: &Path,
    _models: &DiarModels,
    _opts: &MeetingOptions,
) -> Result<DiarOutput, MeetingError> {
    Err(MeetingError::Unsupported(
        "offline diarization requires macOS built with the `diarize` feature (diar-rs)".to_string(),
    ))
}
