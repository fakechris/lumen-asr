//! Orchestration: diarize -> per-turn ASR -> assemble -> persist.
//!
//! The diarization step ([`diarize_wav`]) is the only part that touches
//! `diar-rs`; it is compiled real only under
//! `#[cfg(all(target_os = "macos", feature = "diarize"))]` and is a
//! [`MeetingError::Unsupported`] stub everywhere else. The rest of the pipeline
//! (ASR fan-out over turns, assembly, storage) is cross-platform.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lumen_asr_engine::{AsrEngine, AsrError, AsrRequest};
use lumen_core::MeetingStatus;
use lumen_store::Store;
use lumen_transcript::Word;
use thiserror::Error;
use uuid::Uuid;

use crate::assemble::{assemble_meeting, new_meeting, turn_sample_range, DiarTurn};
use crate::correct::CorrectionDict;

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
    /// Hotwords forwarded to the ASR engine per turn. sherpa-onnx's offline
    /// Paraformer does not reliably act on these, so the authoritative
    /// name/jargon fix is the post-ASR [`correction`](Self::correction) pass
    /// below; this field is kept as a best-effort engine hint.
    pub hotwords: Vec<String>,
    /// Personal-dictionary view driving the post-ASR correction pass (meeting
    /// "hotword" strategy A). Empty = correction is skipped and text is stored
    /// exactly as transcribed.
    pub correction: CorrectionDict,
    /// Upper bound on speaker count for clustering (maps to diar-rs
    /// `ahc_max_speakers`). `None` keeps the diar-rs default.
    pub max_speakers: Option<usize>,
    /// Run the batched LLM transcript-cleanup pass (fillers / punctuation /
    /// code-switch) after dictionary correction. Only effective when an LLM
    /// corrector is also supplied to [`process_meeting`](crate::process_meeting)
    /// (via `MinutesConfig`); with no corrector the pass is skipped. `Default` is
    /// `false` — the app layer sets it from config, where it defaults **on** so a
    /// user with an LLM configured gets a cleaned transcript automatically.
    pub cleanup_transcript: bool,
    /// Suppress mic-track echo duplicates of system-track speech before the
    /// dual-track merge: without headphones the remote voice plays through the
    /// loudspeaker, is picked up by the mic again, and would appear twice in
    /// the final transcript. Multi-evidence (delay window + time coverage +
    /// text similarity + audio cross-correlation, see the private `echo` module) and
    /// fail-open: any missing evidence keeps the segment. Only meaningful for
    /// dual-track meetings; the mic-only pipeline never consults it. `Default`
    /// is `false` — the app layer sets it from config
    /// (`meeting.echo_suppression`), where it defaults **on**.
    pub echo_suppression: bool,
    /// Directory of the local speaker-identity library (enrolled voiceprints).
    /// When set, each diarized speaker's centroid embedding is matched against
    /// the enrolled identities and a confident hit auto-assigns the real name
    /// (see [`auto_identify_speakers`](crate::auto_identify_speakers)). `None`
    /// disables auto-identification.
    pub identity_dir: Option<PathBuf>,
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
    /// No track carried any audible speech (all-silent recording), so there is
    /// nothing to transcribe. Distinct from [`MeetingError::Diarize`] so the UI
    /// can show an actionable "the recording was silent" reason instead of an
    /// internal pipeline error.
    #[error("no speech detected on any track")]
    NoSpeech,
    /// The mic track carried no speech and the system track failed outright
    /// (a real error, not silence). The system track's original error is
    /// surfaced — a bare "no speech" would hide the actionable cause of the
    /// only track that might have had content.
    #[error("system track failed: {0}; mic track: no speech")]
    SystemTrackFailed(String),
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
    /// Per-speaker centroid voiceprint embedding (engine speaker id → 256-d
    /// WeSpeaker x-vector average), when this build can compute them. Empty on
    /// non-diarizing builds and on any best-effort computation failure — the
    /// pipeline persists/matches embeddings only when present ("可得则匹配").
    pub(crate) speaker_embeddings: BTreeMap<u32, Vec<f32>>,
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
    // A silent wav is skipped by the preflight (zero turns): fail explicitly
    // rather than storing an empty "ready" meeting nobody asked for.
    if diar.turns.is_empty() {
        return Err(MeetingError::NoSpeech);
    }
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

    let mut assembled = assemble_meeting(
        meeting_id,
        &diar.turns,
        &texts,
        &words,
        Some(diar.sample_rate),
        duration,
    );

    // Cross-meeting auto-identification: give already-enrolled voices their
    // real names before the speaker rows are written (a hit sets
    // `display_name`, i.e. the speaker persists as confirmed). Speakers with
    // too little voiced audio are skipped (see `IDENTIFY_MIN_VOICED_MS`).
    let voiced_ms = crate::identify::speaker_voiced_ms(&diar.turns);
    crate::identify::apply_auto_identification(
        &mut assembled.speakers,
        &diar.speaker_embeddings,
        &voiced_ms,
        opts.identity_dir.as_deref(),
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
        crate::identify::persist_speaker_embeddings(
            store,
            &assembled.speakers,
            &diar.speaker_embeddings,
        )
        .map_err(MeetingError::Store)?;
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

    // Layer 1 — silence preflight: a track with (almost) no voiced audio is
    // skipped outright instead of being diarized. diar-rs hard-fails on such
    // tracks ("pipeline: too few x-vectors"); before this check a fully silent
    // system track (remote audio never played) failed the *whole* meeting even
    // though the mic track transcribed fine. Zero turns → the caller stores no
    // segments for this track and moves on.
    let scan = crate::preflight::scan_speech(&samples, sample_rate);
    if !scan.has_enough_speech() {
        tracing::info!(
            wav = %wav.display(),
            voiced_seconds = scan.voiced_seconds,
            total_seconds = scan.total_seconds,
            "track skipped: effectively silent"
        );
        return Ok(DiarOutput {
            samples,
            sample_rate,
            turns: Vec::new(),
            speaker_embeddings: BTreeMap::new(),
        });
    }

    // Layer 2 — per-track fail-open: when the preflight found audible speech
    // but diarization still errors (borderline-short speech can yield too few
    // x-vectors to cluster), the track degrades to a single speaker over the
    // preflight's voiced spans instead of failing the run: the per-turn ASR
    // loop then transcribes exactly the audible audio, attributed to one
    // "说话人" cluster. Clustering quality is lost; the content is not.
    let turns: Vec<DiarTurn> = match diarize(wav, &model_paths, &cfg) {
        Ok(result) => result
            .timeline
            .iter()
            .map(|t| DiarTurn::new(t.start, t.end, t.speaker))
            .collect(),
        Err(error) => {
            tracing::warn!(
                wav = %wav.display(),
                error = %error,
                voiced_seconds = scan.voiced_seconds,
                "diarization failed on a voiced track; degrading to a single speaker over voiced spans"
            );
            scan.fallback_turns()
        }
    };

    // Best-effort per-speaker voiceprint centroids for enrollment/matching.
    // A failure here degrades to "no embeddings" (no enrollment for this
    // meeting) rather than failing the transcription.
    let speaker_embeddings = match speaker_centroids(
        &samples,
        sample_rate,
        &turns,
        &model_paths.embedding,
    ) {
        Ok(embeddings) => embeddings,
        Err(error) => {
            tracing::warn!(error = %error, "speaker centroid computation failed; no voiceprints for this meeting");
            BTreeMap::new()
        }
    };

    Ok(DiarOutput {
        samples,
        sample_rate,
        turns,
        speaker_embeddings,
    })
}

/// Per-speaker duration budget for centroid computation. diar-rs's clustering
/// already saw the whole file; for a stable voiceprint an average over the
/// speaker's longest ~30 s of speech is plenty, and capping keeps the extra
/// embedding passes cheap on long meetings.
const CENTROID_MAX_SECONDS_PER_SPEAKER: f64 = 30.0;

/// Skip turns shorter than this when building centroids: sub-second snippets
/// yield noisy x-vectors (and too few fbank frames to embed at all).
const CENTROID_MIN_TURN_SECONDS: f64 = 0.5;

/// Injected embedding forward pass for [`accumulate_centroids`]: pcm slice →
/// `Ok(Some(x-vector))`, `Ok(None)` when the slice is too short to embed
/// reliably, `Err` when the forward itself failed.
type EmbedFn<'a> = &'a mut dyn FnMut(&[f32]) -> Result<Option<Vec<f64>>, String>;

/// Compute one centroid voiceprint per diarized speaker: embed each of the
/// speaker's (longest-first, up to [`CENTROID_MAX_SECONDS_PER_SPEAKER`]) turns
/// with the same WeSpeaker embedding model + kaldi fbank front-end diar-rs
/// used for clustering, then average duration-weighted. `diar_rs::diarize`
/// does not expose its internal window x-vectors, so this recomputes them over
/// the final merged turns via diar-rs's public `fbank`/`onnx_emb` modules —
/// one extra ONNX session plus a handful of forward passes per meeting.
#[cfg(all(target_os = "macos", feature = "diarize"))]
fn speaker_centroids(
    samples: &[f32],
    sample_rate: u32,
    turns: &[DiarTurn],
    embedding_model: &Path,
) -> Result<BTreeMap<u32, Vec<f32>>, String> {
    use diar_rs::fbank::{compute_fbank, FbankOptions};
    use diar_rs::onnx_emb::EmbModel;

    if !turns
        .iter()
        .any(|t| t.end - t.start >= CENTROID_MIN_TURN_SECONDS)
    {
        return Ok(BTreeMap::new());
    }
    let mut model = EmbModel::load(embedding_model, 2).map_err(|e| e.to_string())?;
    let fb_opts = FbankOptions {
        sample_rate,
        subtract_mean: true,
        ..FbankOptions::default()
    };
    let mut embed = |chunk: &[f32]| -> Result<Option<Vec<f64>>, String> {
        let (fb, t_fb) = compute_fbank(chunk, &fb_opts).map_err(|e| e.to_string())?;
        if t_fb < 10 {
            return Ok(None); // too short to embed reliably
        }
        model
            .embed_fbank(&fb, t_fb)
            .map(Some)
            .map_err(|e| e.to_string())
    };
    Ok(accumulate_centroids(
        samples,
        sample_rate,
        turns,
        &mut embed,
    ))
}

/// Pure centroid accumulation over diarized turns, with the embedding forward
/// pass injected (`embed`: slice → `Ok(Some(x-vector))`, `Ok(None)` for
/// "too short to embed", `Err` for a failed forward). Failure tolerance is
/// per-turn: a failing (or wrong-dimension) turn is skipped with a warning and
/// never poisons the speaker's other turns or the other speakers — a speaker
/// ends up without a centroid only when *none* of its turns embed.
///
/// Kept un-gated (the real caller is the macOS `diarize` path) so this policy
/// is unit-testable everywhere with a mock `embed`.
#[cfg_attr(not(all(target_os = "macos", feature = "diarize")), allow(dead_code))]
fn accumulate_centroids(
    samples: &[f32],
    sample_rate: u32,
    turns: &[DiarTurn],
    embed: EmbedFn<'_>,
) -> BTreeMap<u32, Vec<f32>> {
    use lumen_identity::EMBEDDING_DIM;

    // Group turns per speaker, longest first (most reliable audio first, so
    // the duration budget is spent on the best material).
    let mut by_speaker: BTreeMap<u32, Vec<&DiarTurn>> = BTreeMap::new();
    for turn in turns {
        if turn.end - turn.start >= CENTROID_MIN_TURN_SECONDS {
            by_speaker.entry(turn.speaker).or_default().push(turn);
        }
    }

    let mut centroids = BTreeMap::new();
    for (speaker, mut speaker_turns) in by_speaker {
        speaker_turns.sort_by(|a, b| {
            (b.end - b.start)
                .partial_cmp(&(a.end - a.start))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut sum = vec![0.0f64; EMBEDDING_DIM];
        let mut weight = 0.0f64;
        let mut budget = CENTROID_MAX_SECONDS_PER_SPEAKER;
        for turn in speaker_turns {
            if budget <= 0.0 {
                break;
            }
            let Some((start, end)) =
                turn_sample_range(turn.start, turn.end, sample_rate, samples.len())
            else {
                continue;
            };
            let seconds = (end - start) as f64 / sample_rate as f64;
            let xvec = match embed(&samples[start..end]) {
                Ok(Some(xvec)) if xvec.len() == EMBEDDING_DIM => xvec,
                Ok(Some(xvec)) => {
                    tracing::warn!(
                        speaker,
                        dim = xvec.len(),
                        "skipping turn with unexpected embedding dimension"
                    );
                    continue;
                }
                Ok(None) => continue, // too short to embed reliably
                Err(error) => {
                    tracing::warn!(speaker, error = %error, "skipping turn whose embedding failed");
                    continue;
                }
            };
            for (accumulator, value) in sum.iter_mut().zip(xvec.iter()) {
                *accumulator += value * seconds;
            }
            weight += seconds;
            budget -= seconds;
        }
        if weight > 0.0 {
            let centroid: Vec<f32> = sum.iter().map(|v| (v / weight) as f32).collect();
            centroids.insert(speaker, centroid);
        }
    }
    centroids
}

/// Long-lived single-utterance voiceprint embedder for the **real-time**
/// meeting layer (L3): the live worker's dedicated embedder thread loads it
/// once and feeds it each finalized utterance's PCM. Exactly the same forward
/// pass as the offline [`speaker_centroids`] (diar-rs kaldi fbank → WeSpeaker
/// ONNX), so live verification and offline enrollment score in the same
/// embedding space. macOS + `diarize` only, like every diar-rs touchpoint.
#[cfg(all(target_os = "macos", feature = "diarize"))]
pub struct LiveVoiceprintEmbedder {
    model: diar_rs::onnx_emb::EmbModel,
}

#[cfg(all(target_os = "macos", feature = "diarize"))]
impl LiveVoiceprintEmbedder {
    /// Load the WeSpeaker embedding model (`emb.onnx` under the diar model
    /// root). One ONNX session; keep the instance alive across utterances.
    /// Two intra-op threads, matching the offline centroid pass.
    pub fn load(embedding_model: &Path) -> Result<Self, String> {
        let model =
            diar_rs::onnx_emb::EmbModel::load(embedding_model, 2).map_err(|e| e.to_string())?;
        Ok(Self { model })
    }

    /// Embed one utterance's mono PCM into a 256-d x-vector. `Ok(None)` when
    /// the audio is too short to embed reliably (same 10-frame floor as the
    /// offline pass) or the model yields an unexpected dimension; `Err` when
    /// the fbank/forward itself failed.
    pub fn embed(&mut self, samples: &[f32], sample_rate: u32) -> Result<Option<Vec<f32>>, String> {
        use diar_rs::fbank::{compute_fbank, FbankOptions};
        use lumen_identity::EMBEDDING_DIM;

        let fb_opts = FbankOptions {
            sample_rate,
            subtract_mean: true,
            ..FbankOptions::default()
        };
        let (fb, t_fb) = compute_fbank(samples, &fb_opts).map_err(|e| e.to_string())?;
        if t_fb < 10 {
            return Ok(None); // too short to embed reliably
        }
        let xvec = self
            .model
            .embed_fbank(&fb, t_fb)
            .map_err(|e| e.to_string())?;
        if xvec.len() != EMBEDDING_DIM {
            tracing::warn!(dim = xvec.len(), "live embedding has unexpected dimension");
            return Ok(None);
        }
        Ok(Some(xvec.iter().map(|&v| v as f32).collect()))
    }
}

/// Stub for every non-diarizing build (Windows CI, or macOS without the
/// `diarize` feature). Keeps the crate compiling and callable everywhere while
/// never referencing `diar-rs`.
///
/// The silence preflight (layer 1) still runs here: an effectively silent
/// track is "skipped" (zero turns) on **every** build, which keeps the
/// dual-track silent-system behavior — and the "no speech on any track"
/// failure — identical and testable cross-platform. A *voiced* track still
/// needs real diarization and yields [`MeetingError::Unsupported`], exactly as
/// before (an unreadable wav lands there too).
#[cfg(not(all(target_os = "macos", feature = "diarize")))]
pub(crate) fn diarize_wav(
    wav: &Path,
    _models: &DiarModels,
    _opts: &MeetingOptions,
) -> Result<DiarOutput, MeetingError> {
    if let Some(samples) = crate::echo::read_full_wav_mono_16k(wav) {
        let scan = crate::preflight::scan_speech(&samples, 16_000);
        if !scan.has_enough_speech() {
            tracing::info!(
                wav = %wav.display(),
                voiced_seconds = scan.voiced_seconds,
                total_seconds = scan.total_seconds,
                "track skipped: effectively silent"
            );
            return Ok(DiarOutput {
                samples,
                sample_rate: 16_000,
                turns: Vec::new(),
                speaker_embeddings: BTreeMap::new(),
            });
        }
    }
    // Note: `DiarOutput.speaker_embeddings` stays empty on this path by
    // construction, so voiceprint enrollment/matching is naturally unavailable
    // wherever diarization is.
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

    #[test]
    fn accumulate_centroids_skips_failing_turn_without_poisoning_others() {
        use lumen_identity::EMBEDDING_DIM;
        // 16 kHz, 4 s of audio; marker values tell the mock embedder which
        // turn's slice it received.
        let sr = 16_000u32;
        let mut samples = vec![0.0f32; 4 * sr as usize];
        for (second, marker) in [(0, 1.0f32), (1, 2.0), (2, 3.0), (3, 4.0)] {
            let start = second * sr as usize;
            samples[start..start + sr as usize].fill(marker);
        }
        // Speaker 0: turns at [0,1) (marker 1, will FAIL) and [1,2) (marker 2).
        // Speaker 1: turn at [2,3) (marker 3). Speaker 2: only [3,4) (marker 4,
        // will FAIL) → no centroid at all.
        let turns = vec![
            DiarTurn::new(0.0, 1.0, 0),
            DiarTurn::new(1.0, 2.0, 0),
            DiarTurn::new(2.0, 3.0, 1),
            DiarTurn::new(3.0, 4.0, 2),
        ];
        let mut embed = |chunk: &[f32]| -> Result<Option<Vec<f64>>, String> {
            match chunk[0] {
                m if m == 1.0 || m == 4.0 => Err("onnx forward failed".to_string()),
                m => Ok(Some(vec![f64::from(m); EMBEDDING_DIM])),
            }
        };

        let centroids = accumulate_centroids(&samples, sr, &turns, &mut embed);

        // Speaker 0 still gets a centroid from its surviving turn (marker 2).
        let s0 = centroids.get(&0).expect("speaker 0 centroid survives");
        assert!(approx(f64::from(s0[0]), 2.0), "{s0:?}");
        // Speaker 1 is untouched by the other speakers' failures.
        let s1 = centroids.get(&1).expect("speaker 1 centroid");
        assert!(approx(f64::from(s1[0]), 3.0));
        // Speaker 2 had no usable turn → no centroid, and nothing else broke.
        assert!(!centroids.contains_key(&2));
        assert_eq!(centroids.len(), 2);
    }

    #[test]
    fn accumulate_centroids_weights_by_duration_and_skips_short_turns() {
        use lumen_identity::EMBEDDING_DIM;
        let sr = 16_000u32;
        let mut samples = vec![0.0f32; 4 * sr as usize];
        samples[..sr as usize].fill(1.0); // [0,1) marker 1
        samples[sr as usize..3 * sr as usize].fill(4.0); // [1,3) marker 4
                                                         // Speaker 0: 1 s of "1" + 2 s of "4" → weighted mean (1*1 + 4*2)/3 = 3.
                                                         // The 0.2 s turn is below CENTROID_MIN_TURN_SECONDS and never embedded.
        let turns = vec![
            DiarTurn::new(0.0, 1.0, 0),
            DiarTurn::new(1.0, 3.0, 0),
            DiarTurn::new(3.0, 3.2, 0),
        ];
        let mut calls = 0usize;
        let mut embed = |chunk: &[f32]| -> Result<Option<Vec<f64>>, String> {
            calls += 1;
            Ok(Some(vec![f64::from(chunk[0]); EMBEDDING_DIM]))
        };

        let centroids = accumulate_centroids(&samples, sr, &turns, &mut embed);

        assert_eq!(calls, 2, "sub-threshold turn is not embedded");
        let s0 = centroids.get(&0).expect("centroid");
        assert!(approx(f64::from(s0[0]), 3.0), "{:?}", &s0[..2]);
        assert_eq!(s0.len(), EMBEDDING_DIM);
    }

    /// Real-model smoke test for the live embedder (needs the WeSpeaker
    /// `emb.onnx` installed under the shared Lumen models root). Run with:
    /// `cargo test -p lumen-meeting --features diarize -- --ignored`.
    #[cfg(all(target_os = "macos", feature = "diarize"))]
    #[test]
    #[ignore = "requires the installed diar embedding model"]
    fn live_embedder_produces_stable_embeddings_with_the_real_model() {
        let home = std::env::var_os("HOME").expect("HOME set");
        let model = std::path::PathBuf::from(home)
            .join("Library/Application Support/Lumen/models/diar/emb.onnx");
        assert!(model.is_file(), "diar embedding model not installed");
        let mut embedder = LiveVoiceprintEmbedder::load(&model).unwrap();

        // 3 s of a synthetic voiced-ish signal at 16 kHz.
        let sr = 16_000u32;
        let samples: Vec<f32> = (0..3 * sr as usize)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * 180.0 * t).sin() * 0.4
                    + (2.0 * std::f32::consts::PI * 610.0 * t).sin() * 0.2
            })
            .collect();
        let a = embedder.embed(&samples, sr).unwrap().expect("long enough");
        assert_eq!(a.len(), lumen_identity::EMBEDDING_DIM);
        // Deterministic: same audio → (near-)identical embedding.
        let b = embedder.embed(&samples, sr).unwrap().unwrap();
        assert!(lumen_identity::cosine_similarity(&a, &b) > 0.999);
        // Too-short audio declines to embed instead of guessing.
        assert_eq!(embedder.embed(&samples[..800], sr).unwrap(), None);
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
