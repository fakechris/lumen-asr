//! End-to-end processing of a **recorded** meeting into the ready state (M4a).
//!
//! Unlike [`transcribe_meeting`](crate::transcribe_meeting) — which creates a
//! fresh meeting from a bare wav (the M2b batch case) — [`process_meeting`]
//! fills an **existing** meeting row (the one the recorder created at start and
//! set to `Processing` at stop). It drives the lifecycle:
//!
//! ```text
//! processing → transcribing → summarizing → ready
//!                     └────────── failed ──────────┘  (any step)
//! ```
//!
//! The transcribe leg (diarize + per-turn ASR) is macOS+`diarize`-gated through
//! [`diarize_wav`](crate::pipeline); on every other build it returns
//! [`MeetingError::Unsupported`], so `process_meeting` moves the meeting to
//! `failed` and returns — which is exactly the path the unit test exercises
//! (no models needed). The minutes leg is cross-platform (an LLM call behind the
//! [`Corrector`] trait) and is skipped when no corrector is supplied.

use std::path::Path;

use lumen_asr_engine::AsrEngine;
use lumen_core::MeetingStatus;
use lumen_corrector::Corrector;
use lumen_store::Store;
use thiserror::Error;
use uuid::Uuid;

use crate::assemble::assemble_meeting;
use crate::correct::{correct_segment, correct_words};
use crate::minutes::{
    generate_minutes, minutes_summaries, render_transcript_for_minutes, MinutesError,
};
use crate::pipeline::{diarize_wav, transcribe_turn, DiarModels, MeetingError, MeetingOptions};

/// Failure of [`process_meeting`]. Whichever step fails, the meeting is left in
/// [`MeetingStatus::Failed`].
#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("transcribe: {0}")]
    Transcribe(#[from] MeetingError),
    #[error("minutes: {0}")]
    Minutes(#[from] MinutesError),
    #[error("store: {0}")]
    Store(#[source] anyhow::Error),
}

/// What to do for the summarization step. Kept as a small struct so the (macOS,
/// app-layer) caller can pass its configured LLM client without this crate
/// depending on the concrete corrector type.
pub struct MinutesConfig<'a> {
    /// The LLM client (lumen-corrector's OpenAI-compat client behind the trait).
    pub corrector: &'a dyn Corrector,
    /// Model label recorded on the stored summaries, e.g. `"qwen2.5"`.
    pub model: Option<String>,
    /// Output-token override; `None` uses the minutes default.
    pub max_tokens: Option<u32>,
}

/// Process an already-recorded meeting to `ready`, advancing status at each
/// step and marking `failed` on any error.
///
/// The meeting row must already exist (created at recording start). Segments and
/// speakers are written into it; `minutes` (when `Some`) adds the structured
/// summaries. Passing `minutes: None` runs transcript-only (goes straight from
/// transcribing to ready).
pub async fn process_meeting(
    store: &Store,
    meeting_id: Uuid,
    wav: &Path,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    minutes: Option<&MinutesConfig<'_>>,
    opts: &MeetingOptions,
) -> Result<(), ProcessError> {
    let result = run(
        store,
        meeting_id,
        wav,
        diar_models,
        asr_engine,
        minutes,
        opts,
    )
    .await;
    if let Err(err) = &result {
        // Best-effort: never let a processing failure leave the meeting stuck in
        // an in-flight state. Record *why* it failed so the UI can surface an
        // actionable reason (missing diar models, diarization unsupported here,
        // …) instead of a bare "failed". The original error is what we return.
        let reason = err.to_string();
        if let Err(e) = store.fail_meeting(meeting_id, Some(reason.as_str())) {
            tracing::warn!(meeting_id = %meeting_id, error = %e, "could not mark meeting failed");
        }
    }
    result
}

async fn run(
    store: &Store,
    meeting_id: Uuid,
    wav: &Path,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    minutes: Option<&MinutesConfig<'_>>,
    opts: &MeetingOptions,
) -> Result<(), ProcessError> {
    // ── transcribing ────────────────────────────────────────────────
    store
        .update_meeting_status(meeting_id, MeetingStatus::Transcribing)
        .map_err(ProcessError::Store)?;

    let diar = diarize_wav(wav, diar_models, opts)?;
    let sample_rate = diar.sample_rate;
    let duration = (sample_rate > 0).then(|| diar.samples.len() as f64 / sample_rate as f64);

    let mut texts = Vec::with_capacity(diar.turns.len());
    let mut words = Vec::with_capacity(diar.turns.len());
    for turn in &diar.turns {
        let (text, turn_words) =
            transcribe_turn(asr_engine, &diar.samples, sample_rate, turn, opts).await?;
        texts.push(text);
        words.push(turn_words);
    }

    // Post-ASR dictionary correction (meeting "hotword" strategy A): repair
    // near-miss mis-recognitions of the user's names/jargon before assembly, so
    // the stored transcript *and* the minutes summary below both see the
    // corrected text. Engine-agnostic (runs for Paraformer and SenseVoice) and
    // cross-platform; a no-op when the dictionary is empty. Word-level timings are
    // preserved (see `correct_words`).
    if !opts.correction.is_empty() {
        for text in &mut texts {
            *text = correct_segment(text, &opts.correction);
        }
        for turn_words in &mut words {
            *turn_words = correct_words(turn_words, &opts.correction);
        }
    }

    let assembled = assemble_meeting(
        meeting_id,
        &diar.turns,
        &texts,
        &words,
        Some(sample_rate),
        duration,
    );
    for speaker in &assembled.speakers {
        store.upsert_speaker(speaker).map_err(ProcessError::Store)?;
    }
    store
        .add_segments(&assembled.segments)
        .map_err(ProcessError::Store)?;

    // ── summarizing (optional) ──────────────────────────────────────
    if let Some(cfg) = minutes {
        store
            .update_meeting_status(meeting_id, MeetingStatus::Summarizing)
            .map_err(ProcessError::Store)?;
        let transcript = render_transcript_for_minutes(&assembled.segments, &assembled.speakers);
        let doc = generate_minutes(cfg.corrector, &transcript, cfg.max_tokens).await?;
        for row in minutes_summaries(meeting_id, &doc, cfg.model.as_deref())? {
            store.save_summary(&row).map_err(ProcessError::Store)?;
        }
    }

    // ── ready ───────────────────────────────────────────────────────
    store
        .update_meeting_status(meeting_id, MeetingStatus::Ready)
        .map_err(ProcessError::Store)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_asr_engine::StubAsr;
    use lumen_core::Meeting;
    use lumen_corrector::NullCorrector;
    use std::path::PathBuf;

    fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.sqlite")).unwrap();
        (dir, store)
    }

    // On any build without the macOS `diarize` feature, `diarize_wav` returns
    // `Unsupported` before touching audio or models — so this exercises the
    // failure path of the whole state machine deterministically, no models
    // needed. (Under `--features diarize`, the bad wav path fails in diarize
    // instead; still a `Transcribe` error and still `Failed`.)
    #[tokio::test]
    async fn transcribe_step_failure_marks_meeting_failed() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new(); // status Recording
        store.create_meeting(&meeting).unwrap();
        store
            .update_meeting_status(meeting.id, MeetingStatus::Processing)
            .unwrap();

        let engine = StubAsr::new("unused");
        let corrector = NullCorrector;
        let cfg = MinutesConfig {
            corrector: &corrector,
            model: None,
            max_tokens: None,
        };
        let models = DiarModels::new("seg.onnx", "emb.onnx", PathBuf::from("plda"));

        let result = process_meeting(
            &store,
            meeting.id,
            Path::new("/does/not/exist.wav"),
            &models,
            &engine,
            Some(&cfg),
            &MeetingOptions::default(),
        )
        .await;

        assert!(matches!(result, Err(ProcessError::Transcribe(_))));
        let after = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(after.status, MeetingStatus::Failed);
        // The failure reason is recorded so the UI can surface it.
        assert!(after
            .failure_reason
            .as_deref()
            .is_some_and(|r| r.contains("transcribe")));
        // No transcript rows were written on the failed path.
        assert!(store.list_segments(meeting.id).unwrap().is_empty());
    }
}
