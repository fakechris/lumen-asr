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
use std::path::{Path, PathBuf};

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
use crate::progress::{
    ProcessingPlan, ProcessingProgress, ProcessingStage, ProcessingTrack, ProgressReporter,
};

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
    /// Minutes style template (see [`crate::minutes_template`]), already
    /// resolved by the caller from the configured name. `None` (or the built-in
    /// default template, whose body is empty) keeps the pre-template prompt.
    pub template: Option<crate::minutes_template::MinutesTemplate>,
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
///
/// `progress`, when supplied, is called at every pipeline stage boundary and,
/// within the loop-heavy `transcribe`/`cleanup` stages, at a throttled cadence
/// (see [`ProgressReporter`]) — the desktop app turns each
/// [`ProcessingProgress`] into a `meeting-processing-progress` Tauri event.
/// `None` runs the pipeline exactly as before (no observability overhead).
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
    progress: Option<&dyn Fn(ProcessingProgress)>,
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
        progress,
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
    segments: &mut Vec<lumen_core::TranscriptSegment>,
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
        new_speakers = outcome.new_speakers.len(),
        reassigned_segments = outcome.reassigned_segments.len(),
        split_segments = outcome.split_segments,
        "live speaker annotations reconciled (timeline boundaries, manual-first)"
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
    reporter: Option<&ProgressReporter<'_>>,
    track: ProcessingTrack,
) -> Result<(TrackTake, BTreeMap<u32, Vec<f32>>, u32, Option<f64>), ProcessError> {
    // Diarization (segmentation + per-speaker centroid voiceprints, both inside
    // `diarize_wav`): a single slow stage with no natural sub-progress. Report
    // it entering, then report `voiceprint` once its embeddings are ready — the
    // two ticks give the bar some movement before the per-turn ASR loop begins.
    if let Some(reporter) = reporter {
        reporter.stage_start(ProcessingStage::Diarize, Some(track));
    }
    let diar = diarize_wav(wav, diar_models, opts)?;
    if let Some(reporter) = reporter {
        reporter.stage_start(ProcessingStage::Voiceprint, Some(track));
    }
    let sample_rate = diar.sample_rate;
    let duration = (sample_rate > 0).then(|| diar.samples.len() as f64 / sample_rate as f64);

    let mut take = TrackTake {
        turns: diar.turns.clone(),
        texts: Vec::with_capacity(diar.turns.len()),
        words: Vec::with_capacity(diar.turns.len()),
    };
    // Per-turn ASR — the loop-heavy stage. Report entering it, then tick once
    // per finished turn (throttled) so a long meeting shows "识别 i/N".
    let total_turns = diar.turns.len();
    if let Some(reporter) = reporter {
        reporter.stage_start(ProcessingStage::Transcribe, Some(track));
    }
    for (i, turn) in diar.turns.iter().enumerate() {
        let (text, turn_words) =
            transcribe_turn(asr_engine, &diar.samples, sample_rate, turn, opts).await?;
        take.texts.push(text);
        take.words.push(turn_words);
        if let Some(reporter) = reporter {
            reporter.tick(ProcessingStage::Transcribe, Some(track), i + 1, total_turns);
        }
    }
    Ok((take, diar.speaker_embeddings, sample_rate, duration))
}

/// Decode an Ogg-Opus track to a temporary 16 kHz PCM WAV (see
/// [`crate::pipeline::materialize_wav_track`]). Every downstream stage —
/// diar-rs's path-based decoder, the echo pass's window reads, per-turn ASR
/// slicing — stays WAV-only, while sidecar reads/writes (timeline offsets,
/// echo diagnostics) keep using the *original* path, which sits next to the
/// meeting's metadata.
fn materialize_wav_track(
    path: &Path,
    scratch: &mut Option<tempfile::TempDir>,
    name: &str,
) -> Result<PathBuf, ProcessError> {
    Ok(crate::pipeline::materialize_wav_track(path, scratch, name)?)
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
    progress: Option<&dyn Fn(ProcessingProgress)>,
) -> Result<(), ProcessError> {
    // Fix the progress plan up front so the overall percent has a stable
    // denominator: which optional stages run is known here (dual track from the
    // presence of a system wav; cleanup from the same gate the stage uses below;
    // minutes from an LLM being configured). The reporter is absent when no sink
    // was supplied — the pipeline then runs with zero observability overhead.
    let reporter = progress.map(|sink| {
        let plan = ProcessingPlan {
            dual_track: system_wav.is_some(),
            cleanup: crate::cleanup::should_cleanup(opts.cleanup_transcript, minutes.is_some()),
            minutes: minutes.is_some(),
        };
        ProgressReporter::new(plan, sink)
    });
    let reporter = reporter.as_ref();

    // ── transcribing ────────────────────────────────────────────────
    store
        .update_meeting_status(meeting_id, MeetingStatus::Transcribing)
        .map_err(ProcessError::Store)?;

    // Opus tracks are decoded to temporary WAVs for the audio-consuming stages;
    // `wav`/`system_wav` keep pointing at the originals so metadata sidecars
    // (`<id>.timeline.json`, echo diagnostics) are read/written in place. The
    // scratch dir lives until the end of `run`.
    //
    // The system track's real (non-silence) failure, when it had one — kept so
    // layer 3 below can surface it if the mic track also has nothing to say.
    let mut system_track_error: Option<String> = None;
    let mut opus_scratch: Option<tempfile::TempDir> = None;
    let mic_audio = materialize_wav_track(wav, &mut opus_scratch, "mic.decoded.wav")?;
    // The system track is best-effort end to end: an opus decode failure
    // degrades to mic-only with a warning, exactly like a transcribe failure.
    let system_audio = match system_wav {
        Some(sys) => match materialize_wav_track(sys, &mut opus_scratch, "system.decoded.wav") {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(
                    meeting_id = %meeting_id,
                    system_wav = %sys.display(),
                    error = %error,
                    "could not decode system track; continuing mic-only"
                );
                system_track_error = Some(match &error {
                    ProcessError::Transcribe(inner) => inner.to_string(),
                    other => other.to_string(),
                });
                None
            }
        },
        None => None,
    };

    // Mic track: authoritative — any failure here fails the meeting, exactly
    // as before dual-track existed.
    let (mic_take, mic_embeddings, sample_rate, mut duration) = transcribe_track(
        &mic_audio,
        diar_models,
        asr_engine,
        opts,
        reporter,
        ProcessingTrack::Mic,
    )
    .await?;

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
    // the whole meeting. (`system_track_error` was declared before opus
    // materialization above, so a decode failure is surfaced the same way.)
    let system_take = match system_audio.as_deref() {
        Some(sys) => match transcribe_track(
            sys,
            diar_models,
            asr_engine,
            opts,
            reporter,
            ProcessingTrack::System,
        )
        .await
        {
            Ok((take, sys_embeddings, _sys_rate, sys_duration)) => {
                duration = match (duration, sys_duration) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                if take.turns.is_empty() {
                    // The silence preflight skipped the track (remote audio was
                    // never played / stayed muted): fall back to the mic-only
                    // merge instead of dragging an empty take through echo
                    // suppression and cross-track unification.
                    tracing::info!(
                        meeting_id = %meeting_id,
                        system_wav = %sys.display(),
                        "system track produced no speech segments; continuing mic-only"
                    );
                    None
                } else {
                    Some((take, sys_embeddings))
                }
            }
            Err(err) => {
                tracing::warn!(
                    meeting_id = %meeting_id,
                    system_wav = %sys.display(),
                    error = %err,
                    "system audio track failed to transcribe; continuing mic-only"
                );
                // Remember the *real* error (unwrapped from the ProcessError
                // shell) for layer 3 below: if the mic track turns out to be
                // silent, this — not a generic "no speech" — is the actionable
                // failure reason.
                system_track_error = Some(match &err {
                    ProcessError::Transcribe(inner) => inner.to_string(),
                    other => other.to_string(),
                });
                None
            }
        },
        None => None,
    };

    // Layer 3 — only when *no* track carried any speech is the meeting failed.
    // Plain reason: "no speech detected on any track". But when the system
    // track failed for a *real* reason (not silence) and the mic track was
    // silent, that swallowed error is the actionable one — surface it instead
    // of hiding it behind the generic no-speech reason. (A mic-track real
    // error already fails the meeting directly above, so it is never
    // swallowed.) A silent track alone (either side) merely degrades to the
    // other track's content.
    if mic_take.turns.is_empty() && system_take.is_none() {
        return Err(ProcessError::Transcribe(match system_track_error {
            Some(reason) => MeetingError::SystemTrackFailed(reason),
            None => MeetingError::NoSpeech,
        }));
    }

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
        if let (Some((system, _)), Some(sys_audio)) = (system_take.as_ref(), system_audio.as_ref())
        {
            // The cross-correlation is CPU-bound (hundreds of millions of
            // multiply-adds per candidate pair) and the sidecar write is file
            // IO, so both run on the blocking pool rather than starving the
            // caller's async runtime. Fail-open extends to the offload itself:
            // a panicked/cancelled task keeps every mic segment.
            let mic_clone = mic_take.clone();
            let system_clone = system.clone();
            // Window reads use the (possibly decoded-from-opus) WAV paths;
            // timeline/diagnostics sidecars live next to the *original* mic
            // track, so those keep using `wav`.
            let mic_audio_path = mic_audio.clone();
            let sys_audio_path = sys_audio.clone();
            let mic_meta_path = wav.to_path_buf();
            let outcome = tokio::task::spawn_blocking(move || {
                // Measured system→mic start skew from the recording-time
                // timeline sidecar (0.0 when absent), so the delay/coverage
                // evidence compares both tracks on one timeline instead of
                // assuming a near-common start.
                let system_skew_s = crate::echo::read_timeline_skew(&mic_meta_path);
                let result = crate::echo::suppress_cross_track_echoes(
                    &mic_clone,
                    &system_clone,
                    &mic_audio_path,
                    &sys_audio_path,
                    system_skew_s,
                );
                if let Err(err) =
                    crate::echo::write_diagnostics_sidecar(&result.diagnostics, &mic_meta_path)
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
    if let Some(reporter) = reporter {
        reporter.stage_start(ProcessingStage::Correct, None);
    }
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
            if let Some(reporter) = reporter {
                reporter.stage_start(ProcessingStage::Cleanup, None);
            }
            // Per-chunk progress: the cleanup loop calls this back with
            // `(chunk_done, chunk_total)`, which the reporter throttles like the
            // per-turn ASR ticks.
            let chunk_progress = reporter.map(|reporter| {
                move |done: usize, total: usize| {
                    reporter.tick(ProcessingStage::Cleanup, None, done, total);
                }
            });
            let chunk_progress_ref = chunk_progress.as_ref().map(|f| f as &dyn Fn(usize, usize));
            let stats = crate::cleanup::cleanup_transcript(
                cfg.corrector,
                &mut texts,
                None,
                chunk_progress_ref,
            )
            .await;
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
    if let Some(reporter) = reporter {
        reporter.stage_start(ProcessingStage::Identify, None);
    }
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
    //
    // Snapshot the assembled segments *before* reconciliation: each still
    // carries its diar cluster's `speaker_id`, which the spread pass below uses
    // to map a manual mark's time back to the cluster (and its centroid) it fell
    // in. Cheap and only kept for this pass.
    let pre_reconcile_segments = assembled.segments.clone();
    reconcile_stored_annotations(
        store,
        meeting_id,
        wav,
        opts.identity_dir.as_deref(),
        &mut assembled.speakers,
        &mut assembled.segments,
    )
    .map_err(ProcessError::Store)?;

    // Annotation voiceprint spread (config `meeting.annotation_voiceprint_spread`):
    // the user's manual marks are high-signal seeds — spread each name's
    // voiceprint (its marked cluster's centroid) onto the *unlabelled* diar
    // clusters that sound like the same person, so one voice's unmarked speech
    // joins their name instead of a stray "说话人N". Runs *after* reconciliation
    // (precise marks already placed; those clusters are excluded as candidates)
    // and *before* cross-track unification, giving the final priority order
    // manual > manual_spread > verification > raw diarization. A no-op without
    // embeddings (non-diarizing builds) or without manual seeds.
    //
    // Progress: this is voiceprint attribution — a fast, synchronous
    // continuation of the same identity work — so it runs under the already
    // in-flight `Identify` stage (started above) rather than emitting its own
    // event; no stage/weight rebalancing, and #96's progress plan is untouched.
    if opts.annotation_voiceprint_spread {
        // Map each cluster's centroid/voiced-duration from the engine-id keyed
        // maps onto the assembled `S{n}` speaker row ids the spread pass works
        // with (the `S`-cluster rows survive reconciliation untouched).
        let mut cluster_centroids: BTreeMap<Uuid, Vec<f32>> = BTreeMap::new();
        let mut cluster_voiced: BTreeMap<Uuid, u64> = BTreeMap::new();
        for (engine_id, embedding) in &speaker_embeddings {
            let label = crate::assemble::speaker_label(*engine_id);
            if let Some(row) = assembled.speakers.iter().find(|s| s.label == label) {
                cluster_centroids.insert(row.id, embedding.clone());
                cluster_voiced.insert(row.id, voiced_ms.get(engine_id).copied().unwrap_or(0));
            }
        }
        let spread = crate::spread::spread_annotations(
            &mut assembled.speakers,
            &pre_reconcile_segments,
            &assembled.segments,
            &cluster_centroids,
            &cluster_voiced,
        );
        if !spread.spread_speakers.is_empty() {
            // Counts only — the manual names are PII.
            tracing::info!(
                meeting_id = %meeting_id,
                spread_speakers = spread.spread_speakers.len(),
                "manual speaker annotations spread to unlabelled clusters via voiceprint"
            );
        }
    }
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
        if let Some(reporter) = reporter {
            reporter.stage_start(ProcessingStage::Unify, None);
        }
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
    // Clear any prior transcript for this meeting first, so a reprocess/retry
    // replaces the result instead of appending a second set of speakers and
    // segments (which doubled the data on the failed-meeting retry path).
    store
        .clear_meeting_transcript(meeting_id)
        .map_err(ProcessError::Store)?;
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

    // Label → enroll: add each manually named speaker's voiceprint to the global
    // identity library so future meetings auto-identify the same person. Local,
    // best-effort — a library failure never fails the meeting.
    if opts.auto_enroll_speakers {
        if let Some(identity_dir) = opts.identity_dir.as_deref() {
            let mut centroids: BTreeMap<Uuid, Vec<f32>> = BTreeMap::new();
            let mut voiced: BTreeMap<Uuid, u64> = BTreeMap::new();
            for (engine_id, embedding) in &speaker_embeddings {
                let label = crate::assemble::speaker_label(*engine_id);
                if let Some(row) = assembled.speakers.iter().find(|s| s.label == label) {
                    centroids.insert(row.id, embedding.clone());
                    voiced.insert(row.id, voiced_ms.get(engine_id).copied().unwrap_or(0));
                }
            }
            match lumen_identity::IdentityStore::open(identity_dir) {
                Ok(mut identity_store) => {
                    let out = crate::enroll::auto_enroll_named_speakers(
                        &mut identity_store,
                        &assembled.speakers,
                        &centroids,
                        &voiced,
                        meeting_id,
                    );
                    if !out.enrolled.is_empty() || !out.conflicts.is_empty() {
                        // Counts only — the names are personal data.
                        tracing::info!(
                            meeting_id = %meeting_id,
                            enrolled = out.enrolled.len(),
                            conflicts = out.conflicts.len(),
                            "auto-enrolled manually named speakers into the identity library"
                        );
                    }
                    // Persist the withheld conflicts for the user to resolve in
                    // the voiceprint manager. One transactional replace so a
                    // reprocess swaps the whole set atomically (never a partial
                    // mix); best-effort — a queue write never fails the meeting.
                    let rows: Vec<lumen_store::NewEnrollConflict> = out
                        .conflicts
                        .iter()
                        .map(|c| lumen_store::NewEnrollConflict {
                            speaker_id: c.speaker_id,
                            label_name: c.name.clone(),
                            existing_name: c.existing_name.clone(),
                            score: c.score,
                        })
                        .collect();
                    if let Err(error) = store.replace_enroll_conflicts(meeting_id, &rows) {
                        tracing::warn!(meeting_id = %meeting_id, error = %error, "record enroll conflicts");
                    }
                }
                Err(error) => tracing::warn!(
                    meeting_id = %meeting_id,
                    error = %error,
                    "auto-enroll: could not open identity library"
                ),
            }
        }
    }

    // ── summarizing (optional) ──────────────────────────────────────
    if let Some(cfg) = minutes {
        store
            .update_meeting_status(meeting_id, MeetingStatus::Summarizing)
            .map_err(ProcessError::Store)?;
        if let Some(reporter) = reporter {
            reporter.stage_start(ProcessingStage::Minutes, None);
        }
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
        // Minutes are a best-effort bonus on top of the (already-persisted)
        // transcript: a flaky LLM — a malformed-JSON reply, a timeout — must not
        // fail the whole meeting and throw away a complete transcript. On any
        // minutes error, log and continue to Ready transcript-only; the user can
        // regenerate minutes later.
        let minutes_result = generate_minutes(
            cfg.corrector,
            &transcript,
            Some(notes.as_str()),
            cfg.max_tokens,
            cfg.template.as_ref(),
        )
        .await
        .and_then(|doc| minutes_summaries(meeting_id, &doc, cfg.model.as_deref()));
        match minutes_result {
            Ok(rows) => {
                for row in rows {
                    store.save_summary(&row).map_err(ProcessError::Store)?;
                }
            }
            Err(error) => {
                tracing::warn!(
                    meeting_id = %meeting_id,
                    error = %error,
                    "minutes generation failed; meeting is ready transcript-only"
                );
            }
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

    #[test]
    fn opus_tracks_materialize_to_wav_and_everything_else_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let mut scratch: Option<tempfile::TempDir> = None;

        // A WAV path is returned unchanged and no scratch dir is created.
        let wav = dir.path().join("meeting.wav");
        std::fs::write(&wav, b"RIFF").unwrap();
        assert_eq!(
            materialize_wav_track(&wav, &mut scratch, "mic.decoded.wav").unwrap(),
            wav
        );
        assert!(scratch.is_none());

        // An Opus track decodes to a 16 kHz mono PCM WAV in the scratch dir.
        let opus = dir.path().join("meeting.opus");
        let rate = 16_000u32;
        let samples: Vec<f32> = (0..rate)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin())
            .collect();
        let mut sink = lumen_audio::OpusSink::create(&opus, rate).unwrap();
        sink.write_samples(&samples).unwrap();
        sink.finalize().unwrap();

        let decoded = materialize_wav_track(&opus, &mut scratch, "mic.decoded.wav").unwrap();
        assert_ne!(decoded, opus);
        assert!(decoded.exists());
        let pcm = crate::echo::read_full_wav_mono_16k(&decoded)
            .expect("materialized wav should be readable");
        assert!((pcm.len() as f64 / 16_000.0 - 1.0).abs() < 0.1);

        // A corrupt opus is a transcribe-stage error, not a panic.
        let junk = dir.path().join("junk.opus");
        std::fs::write(&junk, b"not ogg").unwrap();
        assert!(materialize_wav_track(&junk, &mut scratch, "junk.decoded.wav").is_err());
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
            template: None,
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
            None,
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
