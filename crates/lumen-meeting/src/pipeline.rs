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
use lumen_transcript::Word;
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
pub(crate) struct DiarOutput {
    pub(crate) samples: Vec<f32>,
    pub(crate) sample_rate: u32,
    pub(crate) turns: Vec<DiarTurn>,
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

    // Transcribe each turn's audio slice. Order is preserved so turns, texts,
    // and words zip positionally in `assemble_meeting`.
    let mut texts = Vec::with_capacity(diar.turns.len());
    let mut words = Vec::with_capacity(diar.turns.len());
    for turn in &diar.turns {
        let (text, turn_words) =
            transcribe_turn(asr_engine, &diar.samples, diar.sample_rate, turn, opts).await?;
        texts.push(text);
        words.push(turn_words);
    }

    let mut meeting = new_meeting(wav.to_str().map(str::to_string), duration);
    meeting.title = opts.title.clone();
    let meeting_id = meeting.id;

    let assembled = assemble_meeting(
        meeting_id,
        &diar.turns,
        &texts,
        &words,
        Some(diar.sample_rate),
        duration,
    );

    // Everything above is assembled in memory before any write, so a diarize
    // or ASR failure persists nothing. The remaining writes (create -> speakers
    // -> segments -> Ready) are grouped so that if any one fails, the partial
    // meeting is removed rather than left dangling in `Processing`.
    let persist = (|| -> Result<(), MeetingError> {
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
        Ok(())
    })();
    if let Err(error) = persist {
        // Best-effort rollback; delete cascades to speakers/segments (schema v6).
        let _ = store.delete_meeting(meeting_id);
        return Err(error);
    }

    Ok(meeting_id)
}

/// Transcribe one turn's audio slice, returning the text plus any word-level
/// timings in **absolute** media time.
///
/// Empty/out-of-bounds ranges (and engines that reject empty audio) yield an
/// empty string and no words rather than failing the run.
///
/// The engine sees only this turn's slice, so any [`WordTiming`] it reports is
/// relative to the slice start. We add the slice's offset —
/// `first_sample / sample_rate` seconds — to lift each word into absolute media
/// time so playback/click-to-seek (M4c) lands on the right spot. Engines without
/// alignment (SenseVoice fallback) report no words, and this returns an empty
/// vec, which [`assemble_meeting`](crate::assemble::assemble_meeting) stores as
/// `words: None`.
///
/// [`WordTiming`]: lumen_asr_engine::WordTiming
pub(crate) async fn transcribe_turn(
    engine: &dyn AsrEngine,
    samples: &[f32],
    sample_rate: u32,
    turn: &DiarTurn,
    opts: &MeetingOptions,
) -> Result<(String, Vec<Word>), MeetingError> {
    let Some((start, end)) = turn_sample_range(turn.start, turn.end, sample_rate, samples.len())
    else {
        return Ok((String::new(), Vec::new()));
    };
    // Absolute-time offset of this slice's first sample.
    let offset = start as f64 / sample_rate as f64;
    let mut request = AsrRequest::new(samples[start..end].to_vec(), sample_rate);
    request.hotwords = opts.hotwords.clone();
    request.language_hint = opts.language_hint.clone();
    match engine.transcribe(request).await {
        Ok(result) => {
            let words = result
                .words
                .into_iter()
                .map(|w| Word::new(w.word, w.start + offset, w.end + offset))
                .collect();
            Ok((result.text, words))
        }
        Err(AsrError::EmptyAudio) => Ok((String::new(), Vec::new())),
        Err(error) => Err(MeetingError::Asr(error)),
    }
}

/// Real diarization via `diar-rs`. macOS + `diarize` feature only.
#[cfg(all(target_os = "macos", feature = "diarize"))]
pub(crate) fn diarize_wav(
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
pub(crate) fn diarize_wav(
    _wav: &Path,
    _models: &DiarModels,
    _opts: &MeetingOptions,
) -> Result<DiarOutput, MeetingError> {
    Err(MeetingError::Unsupported(
        "offline diarization requires macOS built with the `diarize` feature (diar-rs)".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lumen_asr_engine::{AsrEngineId, AsrResult, WordTiming};

    /// Engine that echoes fixed slice-relative word timings, so we can assert the
    /// absolute-time offset is applied per turn (no model/audio needed).
    struct WordEchoAsr;

    #[async_trait]
    impl AsrEngine for WordEchoAsr {
        fn id(&self) -> AsrEngineId {
            AsrEngineId::Paraformer
        }

        async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
            if req.samples.is_empty() {
                return Err(AsrError::EmptyAudio);
            }
            let mut result = AsrResult::new("你好", AsrEngineId::Paraformer);
            // Relative to the start of the slice the engine was handed.
            result.words = vec![
                WordTiming {
                    word: "你".into(),
                    start: 0.0,
                    end: 0.3,
                },
                WordTiming {
                    word: "好".into(),
                    start: 0.3,
                    end: 0.6,
                },
            ];
            Ok(result)
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[tokio::test]
    async fn transcribe_turn_offsets_words_into_absolute_time() {
        let engine = WordEchoAsr;
        // 16 kHz buffer long enough to cover the [1.0, 2.0) turn slice.
        let samples = vec![0.1f32; 40_000];
        let turn = DiarTurn::new(1.0, 2.0, 0);
        let (text, words) =
            transcribe_turn(&engine, &samples, 16_000, &turn, &MeetingOptions::default())
                .await
                .unwrap();

        assert_eq!(text, "你好");
        assert_eq!(words.len(), 2);
        // Slice starts at sample 16000 -> +1.0 s offset applied to each word.
        assert!(approx(words[0].start, 1.0), "{words:?}");
        assert!(approx(words[0].end, 1.3));
        assert!(approx(words[1].start, 1.3));
        assert!(approx(words[1].end, 1.6));
    }

    #[tokio::test]
    async fn transcribe_turn_empty_range_yields_no_words() {
        let engine = WordEchoAsr;
        let samples = vec![0.1f32; 16_000];
        // Zero-length turn -> no slice, no ASR call.
        let turn = DiarTurn::new(1.0, 1.0, 0);
        let (text, words) =
            transcribe_turn(&engine, &samples, 16_000, &turn, &MeetingOptions::default())
                .await
                .unwrap();
        assert!(text.is_empty());
        assert!(words.is_empty());
    }
}
