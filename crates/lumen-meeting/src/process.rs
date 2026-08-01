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

use std::collections::BTreeMap;
use std::path::Path;

use lumen_asr_engine::AsrEngine;
use lumen_core::MeetingStatus;
use lumen_corrector::Corrector;
use lumen_store::Store;
use thiserror::Error;
use uuid::Uuid;

use crate::assemble::assemble_meeting_with_channels;
use crate::correct::{correct_segment, correct_words};
use crate::merge::{merge_tracks, system_speaker_offset, TrackTake};
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
///
/// `system_wav` is the optional second, synchronized system-audio track of a
/// dual-track recording (remote participants). When present, both tracks are
/// diarized and transcribed independently, system speaker labels are offset
/// past the mic ones, and the segments are merged chronologically with a
/// per-segment channel ("mic"/"system"). A failure on the **system** track is
/// downgraded to a warning — the meeting still completes from the mic track
/// alone. Passing `None` is byte-for-byte the legacy single-track pipeline.
#[allow(clippy::too_many_arguments)]
pub async fn process_meeting(
    store: &Store,
    meeting_id: Uuid,
    wav: &Path,
    system_wav: Option<&Path>,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    minutes: Option<&MinutesConfig<'_>>,
    opts: &MeetingOptions,
) -> Result<(), ProcessError> {
    let result = run(
        store,
        meeting_id,
        wav,
        system_wav,
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

/// Apply a meeting's stored recording-time live annotations (L2) to its
/// assembled speakers/segments — the exact chain the offline pipeline runs:
/// load the `live_annotations` rows, resolve display names against the
/// identity library, lift each track's WAV-time segments onto the unified
/// timeline with the `<meeting-id>.timeline.json` sidecar offsets (read from
/// next to `mic_wav`), and reconcile (manual always wins). Mutates
/// `speakers`/`segments` in place *before* persistence. Public so an
/// integration test can drive the very same code path the pipeline uses.
pub fn reconcile_stored_annotations(
    store: &Store,
    meeting_id: Uuid,
    mic_wav: &Path,
    identity_dir: Option<&Path>,
    speakers: &mut Vec<lumen_core::Speaker>,
    segments: &mut [lumen_core::TranscriptSegment],
) -> Result<crate::annotate::AnnotationReconciliation, anyhow::Error> {
    let annotations = store.list_live_annotations(meeting_id)?;
    if annotations.is_empty() {
        return Ok(crate::annotate::AnnotationReconciliation::default());
    }
    let (mic_offset, system_offset) = crate::echo::read_timeline_offsets(mic_wav);
    let resolved = crate::annotate::resolve_annotation_names(&annotations, identity_dir);
    let outcome = crate::annotate::reconcile_annotations(
        meeting_id,
        speakers,
        segments,
        &resolved,
        mic_offset,
        system_offset.unwrap_or(0.0),
    );
    // Counts only — the manual names are PII.
    tracing::info!(
        meeting_id = %meeting_id,
        annotations = annotations.len(),
        renamed_clusters = outcome.renamed_speakers.len(),
        new_speakers = outcome.new_speakers.len(),
        reassigned_segments = outcome.reassigned_segments.len(),
        "live speaker annotations reconciled (manual-first)"
    );
    Ok(outcome)
}

/// Diarize + transcribe one WAV into a positional [`TrackTake`], plus this
/// track's per-speaker centroid voiceprints (keyed by the track's own engine
/// speaker ids) and the decoded duration (`None` when the sample rate is
/// unknown).
async fn transcribe_track(
    wav: &Path,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    opts: &MeetingOptions,
) -> Result<(TrackTake, BTreeMap<u32, Vec<f32>>, u32, Option<f64>), ProcessError> {
    let diar = diarize_wav(wav, diar_models, opts)?;
    let sample_rate = diar.sample_rate;
    let duration = (sample_rate > 0).then(|| diar.samples.len() as f64 / sample_rate as f64);

    let mut take = TrackTake {
        turns: diar.turns.clone(),
        texts: Vec::with_capacity(diar.turns.len()),
        words: Vec::with_capacity(diar.turns.len()),
    };
    for turn in &diar.turns {
        let (text, turn_words) =
            transcribe_turn(asr_engine, &diar.samples, sample_rate, turn, opts).await?;
        take.texts.push(text);
        take.words.push(turn_words);
    }
    Ok((take, diar.speaker_embeddings, sample_rate, duration))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    store: &Store,
    meeting_id: Uuid,
    wav: &Path,
    system_wav: Option<&Path>,
    diar_models: &DiarModels,
    asr_engine: &dyn AsrEngine,
    minutes: Option<&MinutesConfig<'_>>,
    opts: &MeetingOptions,
) -> Result<(), ProcessError> {
    // ── transcribing ────────────────────────────────────────────────
    store
        .update_meeting_status(meeting_id, MeetingStatus::Transcribing)
        .map_err(ProcessError::Store)?;

    // Mic track: authoritative — any failure here fails the meeting, exactly
    // as before dual-track existed.
    let (mic_take, mic_embeddings, sample_rate, mut duration) =
        transcribe_track(wav, diar_models, asr_engine, opts).await?;

    // Per-speaker centroid voiceprints across both tracks, keyed by the
    // *merged* engine speaker id space: mic ids stay as-is; system ids are
    // shifted by the same `system_speaker_offset` that `merge_tracks` applies,
    // so every merged `S{n}` label maps to the right centroid. Both tracks are
    // extracted — the system track carries the remote participants, which are
    // exactly the people worth enrolling/auto-identifying — and the extra cost
    // is one more embedding session over ≤30 s per speaker.
    let mut speaker_embeddings = mic_embeddings;

    // System track (optional): best-effort. The remote-participants track is a
    // bonus on top of a working mic recording, so a diarize/ASR failure here
    // degrades to the mic-only transcript with a warning instead of failing
    // the whole meeting.
    let system_take = match system_wav {
        Some(sys) => match transcribe_track(sys, diar_models, asr_engine, opts).await {
            Ok((take, sys_embeddings, _sys_rate, sys_duration)) => {
                duration = match (duration, sys_duration) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                Some((take, sys_embeddings))
            }
            Err(err) => {
                tracing::warn!(
                    meeting_id = %meeting_id,
                    system_wav = %sys.display(),
                    error = %err,
                    "system audio track failed to transcribe; continuing mic-only"
                );
                None
            }
        },
        None => None,
    };

    // Cross-track echo duplicate suppression (config `meeting.echo_suppression`):
    // without headphones the remote voice plays through the loudspeaker and is
    // picked up by the mic again, so the same utterance would appear once per
    // track in the final transcript. A mic segment is hidden only when all four
    // evidences agree (delay window, time coverage, text similarity, audio
    // cross-correlation over the two WAVs — see `echo`); anything missing
    // (unreadable WAV, short text, …) fails open to keeping the segment. Runs
    // only when enabled AND a system track transcribed, so mic-only meetings
    // and the opt-out stay byte-identical to the previous behavior. Must run
    // *before* the system speaker-id offset below is derived, so the merged id
    // space and the embedding keys agree after mic turns are removed. The
    // per-pair evidence is written to a `<stem>.echo_suppression.json` sidecar
    // next to the meeting audio for auditing; a sidecar write failure is only
    // logged.
    let mut mic_take = mic_take;
    // Diagnostics of *this run's* echo pass, kept for the cross-track speaker
    // unification below. Stays `None` when echo suppression is disabled,
    // never ran (mic-only), or the offloaded task failed — the echo evidence
    // is then simply absent.
    let mut echo_diagnostics: Option<crate::echo::EchoDiagnostics> = None;
    if opts.echo_suppression {
        if let (Some((system, _)), Some(sys_wav)) = (system_take.as_ref(), system_wav) {
            // The cross-correlation is CPU-bound (hundreds of millions of
            // multiply-adds per candidate pair) and the sidecar write is file
            // IO, so both run on the blocking pool rather than starving the
            // caller's async runtime. Fail-open extends to the offload itself:
            // a panicked/cancelled task keeps every mic segment.
            let mic_clone = mic_take.clone();
            let system_clone = system.clone();
            let mic_wav = wav.to_path_buf();
            let sys_wav = sys_wav.to_path_buf();
            let outcome = tokio::task::spawn_blocking(move || {
                // Measured system→mic start skew from the recording-time
                // timeline sidecar (0.0 when absent), so the delay/coverage
                // evidence compares both tracks on one timeline instead of
                // assuming a near-common start.
                let system_skew_s = crate::echo::read_timeline_skew(&mic_wav);
                let result = crate::echo::suppress_cross_track_echoes(
                    &mic_clone,
                    &system_clone,
                    &mic_wav,
                    &sys_wav,
                    system_skew_s,
                );
                if let Err(err) =
                    crate::echo::write_diagnostics_sidecar(&result.diagnostics, &mic_wav)
                {
                    tracing::warn!(
                        error = %err,
                        "could not write echo suppression diagnostics sidecar"
                    );
                }
                result
            })
            .await;
            match outcome {
                Ok(result) => {
                    tracing::info!(
                        meeting_id = %meeting_id,
                        candidates = result.diagnostics.candidates,
                        suppressed = result.diagnostics.suppressed,
                        "cross-track echo suppression evaluated"
                    );
                    if result.diagnostics.suppressed > 0 {
                        mic_take = crate::echo::filter_track_take(&mic_take, &result.keep);
                    }
                    echo_diagnostics = Some(result.diagnostics);
                }
                Err(err) => {
                    tracing::warn!(
                        meeting_id = %meeting_id,
                        error = %err,
                        "echo suppression task failed; keeping all mic segments"
                    );
                }
            }
        }
    }

    // Lift the system track's embeddings into the merged speaker-id space: the
    // same offset `merge_tracks` will apply (computed from the possibly
    // echo-filtered mic turns), so every merged `S{n}` label maps to the right
    // centroid. The offset is remembered for the cross-track unification pass
    // below, which maps the echo diagnostics' per-track engine speaker ids
    // back onto merged `S{n}` labels.
    let mut system_speaker_id_offset = None;
    let system_take = system_take.map(|(take, sys_embeddings)| {
        let offset = system_speaker_offset(&mic_take.turns);
        system_speaker_id_offset = Some(offset);
        for (engine_id, embedding) in sys_embeddings {
            speaker_embeddings.insert(engine_id.saturating_add(offset), embedding);
        }
        take
    });

    // Merge into one chronological take. Mic-only meetings skip the merge and
    // keep untagged (legacy) segments — zero behavior change.
    let (turns, mut texts, mut words, channels) = match system_take {
        Some(system) => {
            let merged = merge_tracks(mic_take, system);
            (merged.turns, merged.texts, merged.words, merged.channels)
        }
        None => (mic_take.turns, mic_take.texts, mic_take.words, Vec::new()),
    };

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

    // Batched LLM transcript cleanup (fillers / punctuation / Chinese-English
    // code-switch), applied *after* dictionary correction and *before* assembly
    // so both the stored verbatim transcript AND the minutes summary below see
    // the cleaned text. Gated on the caller opting in (`opts.cleanup_transcript`)
    // AND an LLM corrector being available (carried by `minutes`): with no LLM
    // the transcript stays raw ASR. Best-effort and boundary-preserving — any
    // LLM failure or a marker/count mismatch keeps that chunk's original text, so
    // it can never drop/reorder/misattribute a segment or fail the pipeline. Only
    // segment *text* is cleaned; word-level `words` timings are left untouched
    // (beta trade-off, see `cleanup`), so click-to-seek is unaffected.
    if crate::cleanup::should_cleanup(opts.cleanup_transcript, minutes.is_some()) {
        if let Some(cfg) = minutes {
            let stats = crate::cleanup::cleanup_transcript(cfg.corrector, &mut texts, None).await;
            tracing::info!(
                meeting_id = %meeting_id,
                chunks = stats.chunks,
                cleaned = stats.cleaned,
                kept_original = stats.kept_original,
                skipped_empty = stats.skipped_empty,
                "meeting transcript llm cleanup done"
            );
        }
    }

    let mut assembled = assemble_meeting_with_channels(
        meeting_id,
        &turns,
        &texts,
        &words,
        &channels,
        Some(sample_rate),
        duration,
    );
    // Cross-meeting auto-identification: match each cluster's voiceprint
    // centroid against the enrolled identity library and assign the real name
    // on a confident hit (logged with cluster label + score only — the name is
    // PII). Unmatched speakers keep their engine label ("说话人N"), as do
    // speakers with too little voiced audio (`IDENTIFY_MIN_VOICED_MS`; the
    // merged turns share the merged engine-id space with `speaker_embeddings`).
    // No-op when `opts.identity_dir` is unset or no embeddings were produced.
    let voiced_ms = crate::identify::speaker_voiced_ms(&turns);
    crate::identify::apply_auto_identification(
        &mut assembled.speakers,
        &speaker_embeddings,
        &voiced_ms,
        opts.identity_dir.as_deref(),
    );
    // Manual live annotations (L2): reconcile the recording-time "who is
    // speaking" marks into the assembled speakers/segments. Runs *after*
    // auto-identification so a manual name always overrides the voiceprint
    // guess (manual > verified > offline_diarization), and *before* the
    // persists/minutes below so every downstream consumer sees the manual
    // attribution. No annotations → byte-for-byte no-op.
    reconcile_stored_annotations(
        store,
        meeting_id,
        wav,
        opts.identity_dir.as_deref(),
        &mut assembled.speakers,
        &mut assembled.segments,
    )
    .map_err(ProcessError::Store)?;
    // Cross-track speaker unification (L4b): the very last attribution step —
    // after auto-identification and annotation reconciliation, before any
    // persistence — merges a mic-track and a system-track speaker row when
    // strong evidence says they are one person (same verified identity, same
    // manual attribution, or strong echo-suppression evidence; see `unify`).
    // The dropped row is never upserted and its label no longer resolves in
    // the embedding persist below, so the surviving row's centroid is the one
    // kept — the simple choice. Mic-only meetings skip this entirely.
    //
    // The echo evidence is deliberately taken from *this run's* in-memory
    // diagnostics only (the same data written to the
    // `<stem>.echo_suppression.json` sidecar for auditing) — never read back
    // from disk. That keeps the pass exactly behind the `echo_suppression`
    // switch (disabled ⇒ evidence absent) and immune to stale sidecars left
    // by an earlier processing run of the same recording.
    let echo_evidence = match (echo_diagnostics.as_ref(), system_speaker_id_offset) {
        (Some(diagnostics), Some(offset)) => {
            crate::unify::echo_evidence_from_diagnostics(diagnostics, &assembled.speakers, offset)
        }
        _ => Vec::new(),
    };
    if system_speaker_id_offset.is_some() {
        let unified = crate::unify::unify_cross_track_speakers(
            &mut assembled.speakers,
            &mut assembled.segments,
            &echo_evidence,
        );
        if !unified.is_empty() {
            tracing::info!(
                meeting_id = %meeting_id,
                merges = unified.len(),
                "cross-track speaker unification applied"
            );
        }
    }
    for speaker in &assembled.speakers {
        store.upsert_speaker(speaker).map_err(ProcessError::Store)?;
    }
    // Persist each cluster's centroid so the user can later enroll a confirmed
    // speaker from this meeting.
    crate::identify::persist_speaker_embeddings(store, &assembled.speakers, &speaker_embeddings)
        .map_err(ProcessError::Store)?;
    store
        .add_segments(&assembled.segments)
        .map_err(ProcessError::Store)?;

    // ── summarizing (optional) ──────────────────────────────────────
    if let Some(cfg) = minutes {
        store
            .update_meeting_status(meeting_id, MeetingStatus::Summarizing)
            .map_err(ProcessError::Store)?;
        let transcript = render_transcript_for_minutes(&assembled.segments, &assembled.speakers);
        // Fuse in the notes the user took during the meeting (Granola-style): the
        // minutes LLM sees both the transcript and the user's own highlights, so
        // the structured summary reflects what the user flagged as important. A
        // meeting with no notes falls back to transcript-only summarization.
        let notes = store
            .get_meeting(meeting_id)
            .map_err(ProcessError::Store)?
            .map(|m| m.notes)
            .unwrap_or_default();
        let doc = generate_minutes(
            cfg.corrector,
            &transcript,
            Some(notes.as_str()),
            cfg.max_tokens,
        )
        .await?;
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
            None,
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
