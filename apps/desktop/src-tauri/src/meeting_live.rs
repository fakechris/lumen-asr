//! Real-time meeting layer (P3): a rolling live transcript while recording.
//!
//! This is the "record-time" half of the two-layer meeting architecture
//! (`docs/MEETING.md` M6/P3):
//!
//! - **While recording** — a background worker consumes each track's bounded
//!   audio fan-out ([`lumen_asr::LiveTapSender`]), feeds it to a **streaming
//!   Paraformer** recognizer, and emits revisable live-transcript segments to
//!   the UI via the `meeting-live-transcript` Tauri event. This kills the
//!   "black box" feeling: the user sees words appear as they speak.
//! - **After stop** — the existing offline pipeline
//!   ([`lumen_meeting::process_meeting`]) re-transcribes with diarization and
//!   word timestamps and produces the authoritative, speaker-attributed
//!   transcript that *replaces* this live preview. The live text is never
//!   persisted; it is a transient recording-time affordance only.
//!
//! ## Dual-stream: one model, two tracks
//! The ~1 GB streaming Paraformer weights are loaded **once** as a
//! [`StreamingRecognizer`]; each track (mic = 现场, system audio = 远端) gets a
//! cheap [`StreamingStream`] decoding state on top of it. When the system
//! track is unavailable (non-macOS, capability/permission absent, or
//! `meeting.system_live_preview = false`) the worker runs mic-only, exactly
//! like the historical single-stream behaviour.
//!
//! ## Gating & graceful degradation
//! The worker is only spawned on **macOS** *and* when the streaming Paraformer
//! model is installed ([`streaming_dir_if_ready`]). On any other platform, or
//! with no model, nothing is spawned and no events are emitted — the recording,
//! WAV write, and offline pipeline are completely unaffected. The audio fan-out
//! channel itself is cross-platform and harmless.
//!
//! ## Threading contract
//! The sherpa C objects are not internally synchronized: one dedicated
//! `std::thread` owns the [`StreamingRecognizer`] **and all of its streams**,
//! and round-robins accept/decode/endpoint across them (the supported
//! multi-stream pattern — never decode the same recognizer from two threads).
//! The recognizer is created *inside* the worker thread; the worker is a plain
//! synchronous loop, no Tokio runtime needed.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_asr::{
    default_paraformer_streaming_dir, paraformer_streaming_ready, resample_linear, LiveAudioPacket,
    StreamingRecognizer, StreamingStream,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

/// Sample rate the streaming Paraformer model expects. Each track captures at
/// its own native rate, so chunks are resampled to this before decoding.
const STREAMING_TARGET_RATE: u32 = 16_000;

/// How long the worker sleeps when no track had audio pending, before checking
/// the stop flag and polling again. Small enough that stop and captions are
/// responsive, large enough to avoid a busy loop.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// Payload of the `meeting-live-transcript` event.
///
/// Revisable-segment contract: `segment_id` is stable per utterance per track
/// (`"<track>-<utteranceIdx>"`), partial updates re-emit the same `segment_id`
/// with an increasing `revision`, and the endpoint emits `is_final: true` with
/// `end_seconds` set. A final event with empty `text` retracts the segment
/// (the UI drops it). All times are seconds on the meeting's unified timeline
/// (shared `t0` across tracks).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveTranscriptEvent {
    /// Meeting this event belongs to (the UI filters stale listeners).
    meeting_id: String,
    /// Stable segment identity, e.g. `mic-0`, `system-3`.
    segment_id: String,
    /// Monotonic per-segment revision; the UI keeps the highest one.
    revision: u64,
    /// Which capture track produced this: `"mic"` (现场) or `"system"` (远端).
    track: &'static str,
    /// Segment start on the unified meeting timeline, in seconds.
    start_seconds: f64,
    /// Segment end (set on the finalizing event only).
    #[serde(skip_serializing_if = "Option::is_none")]
    end_seconds: Option<f64>,
    /// Rolling text for the segment (partial) or the committed segment text.
    text: String,
    /// `true` once the segment is finalized (endpoint reached).
    is_final: bool,
    /// Live speaker attribution (L3): filled only by the voiceprint
    /// verification revision of a finalized segment; absent on every
    /// transcription-only event, so without an identity library / diar model
    /// the payload is byte-identical to the L1 contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    speaker: Option<LiveSpeaker>,
}

/// Speaker attribution attached to a live segment revision by the voiceprint
/// verifier. `provisional: true` renders tentatively ("李明?"); manual chip
/// annotations (L2) always take display precedence in the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveSpeaker {
    /// Enrolled identity id (the UI translates `self_identity_id` to "我").
    identity_id: String,
    /// The identity's current real name.
    display_name: String,
    /// Attribution source; always `"voiceprint"` from this layer.
    source: &'static str,
    /// `true` = suggest tentatively; `false` = auto-verified.
    provisional: bool,
}

/// Return the streaming Paraformer model directory **iff** the real-time layer
/// should run: macOS and the model is installed. `None` everywhere else, which
/// the caller treats as "record normally, no live transcript".
pub fn streaming_dir_if_ready() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let dir = default_paraformer_streaming_dir();
    paraformer_streaming_ready(&dir).then_some(dir)
}

/// One track's audio feed into the live worker: the bounded fan-out receiver
/// plus the track's native capture rate (resampled to the model rate inside
/// the worker).
pub struct LiveTrackFeed {
    pub rx: Receiver<LiveAudioPacket>,
    pub capture_rate: u32,
}

/// Owns the live-transcript worker for the currently active recording (if any).
/// Held in `AppState`; cross-platform and Send + Sync.
#[derive(Default)]
pub struct MeetingLive {
    inner: Mutex<Option<Worker>>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl MeetingLive {
    /// Spawn the streaming worker for a new recording. `mic` is always present;
    /// `system` is attached only when the system-audio track is live *and* its
    /// live preview is enabled — with `None` the worker behaves exactly like
    /// the historical mic-only path. `streaming_dir` is the (already-validated)
    /// model directory. Any previous worker is stopped first.
    pub fn start(
        &self,
        app: AppHandle,
        meeting_id: String,
        streaming_dir: PathBuf,
        mic: LiveTrackFeed,
        system: Option<LiveTrackFeed>,
    ) {
        // Defensive: never leave a prior worker running.
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("lumen-meeting-live".into())
            .spawn(move || {
                run_worker(app, meeting_id, streaming_dir, mic, system, stop_worker);
            })
            .expect("spawn meeting live worker thread");
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(Worker { stop, handle });
        }
    }

    /// Stop the active worker (if any) and wait for it to drain the last
    /// segment. Called from `stop_meeting_recording` after the recorder has
    /// been stopped (which drops the fan-out senders and naturally ends the
    /// worker loop); the explicit stop flag makes teardown prompt regardless.
    pub fn stop(&self) {
        let worker = self.inner.lock().ok().and_then(|mut g| g.take());
        if let Some(worker) = worker {
            worker.stop.store(true, Ordering::SeqCst);
            let _ = worker.handle.join();
        }
    }
}

/// Track name labels (also the `segment_id` prefixes).
const TRACK_MIC: &str = "mic";
const TRACK_SYSTEM: &str = "system";

/// Pure per-track segment bookkeeping: stable segment ids, monotonically
/// increasing revisions for partial updates, and finalization on endpoints.
/// Extracted from the worker loop so the id/revision contract is unit-testable
/// without a model.
struct SegmentTracker {
    track: &'static str,
    utterance_idx: u64,
    revision: u64,
    /// Start (unified timeline) of the current segment's audio window: the
    /// arrival stamp of the first packet fed after the previous reset.
    start_seconds: Option<f64>,
    last_text: String,
}

/// One event-worth of segment state produced by the tracker.
#[derive(Debug, Clone, PartialEq)]
struct SegmentUpdate {
    segment_id: String,
    revision: u64,
    start_seconds: f64,
    end_seconds: Option<f64>,
    text: String,
    is_final: bool,
}

impl SegmentTracker {
    fn new(track: &'static str) -> Self {
        Self {
            track,
            utterance_idx: 0,
            revision: 0,
            start_seconds: None,
            last_text: String::new(),
        }
    }

    fn segment_id(&self) -> String {
        format!("{}-{}", self.track, self.utterance_idx)
    }

    /// Note the arrival stamp of a packet fed to the recognizer; the first one
    /// after a reset anchors the segment's `start_seconds`.
    fn note_audio(&mut self, packet_start_seconds: f64) {
        if self.start_seconds.is_none() {
            self.start_seconds = Some(packet_start_seconds);
        }
    }

    /// A rolling partial for the current segment. Emits only when the text is
    /// non-empty and actually changed (so the UI is not spammed with identical
    /// frames), bumping `revision` each time.
    fn on_partial(&mut self, text: &str) -> Option<SegmentUpdate> {
        if text.trim().is_empty() || text == self.last_text {
            return None;
        }
        self.last_text = text.to_string();
        self.revision += 1;
        Some(SegmentUpdate {
            segment_id: self.segment_id(),
            revision: self.revision,
            start_seconds: self.start_seconds.unwrap_or(0.0),
            end_seconds: None,
            text: text.to_string(),
            is_final: false,
        })
    }

    /// The endpoint fired (or the input was flushed): finalize the current
    /// segment at `end_seconds` and advance to the next utterance. A segment
    /// that was never announced (no partials) and has no final text produces no
    /// event and keeps its id for the next utterance; a segment that *was*
    /// announced but finalizes empty produces a final empty event so the UI
    /// can retract the stale partial.
    fn on_endpoint(&mut self, text: &str, end_seconds: f64) -> Option<SegmentUpdate> {
        let announced = self.revision > 0;
        let update = if announced || !text.trim().is_empty() {
            self.revision += 1;
            Some(SegmentUpdate {
                segment_id: self.segment_id(),
                revision: self.revision,
                start_seconds: self.start_seconds.unwrap_or(end_seconds),
                end_seconds: Some(end_seconds),
                text: text.to_string(),
                is_final: true,
            })
        } else {
            None
        };
        if update.is_some() {
            self.utterance_idx += 1;
        }
        self.revision = 0;
        self.start_seconds = None;
        self.last_text.clear();
        update
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// L3: live speaker verification — recent-audio ring window, bounded embedder
// hand-off, and the streak upgrade rule. The pure parts (window, streak) are
// cross-platform and unit-tested; only the embedder thread itself is gated on
// macOS + the installed diar embedding model + a non-empty identity library.
// ─────────────────────────────────────────────────────────────────────────────

/// How much recent audio each track retains for utterance extraction. Live
/// utterances are endpointed well under this, so 30 s comfortably covers the
/// finalize-then-verify hand-off while bounding memory (~1.9 MB/track @16k).
const WINDOW_CAPACITY_SECONDS: f64 = 30.0;

/// Only utterances at least this long are worth an embedding attempt — the
/// live policy cannot label anything shorter anyway
/// ([`lumen_identity::LIVE_PROVISIONAL_MIN_VOICED_MS`]).
const VERIFY_MIN_UTTERANCE_SECONDS: f64 =
    lumen_identity::LIVE_PROVISIONAL_MIN_VOICED_MS as f64 / 1000.0;

/// Bound of the worker → embedder job queue. Verification is an enhancement:
/// when the embedder falls behind, jobs are dropped (utterances simply stay
/// unlabeled) rather than ever back-pressuring the transcription loop.
const EMBED_QUEUE_CAPACITY: usize = 4;

/// Consecutive same-identity hits a track needs before *later* provisional
/// hits may display as verified (the streak upgrade). Two agreeing utterances
/// establish the streak; from the next hit on, the person is evidently the
/// one talking on that track, so a grey-zone score stops re-adding the "?".
const STREAK_UPGRADE_AFTER: u32 = 2;

/// Rolling window of a track's recent audio (model-rate mono), stamped on the
/// meeting's unified timeline. Chunks arrive with their fan-out packet stamps
/// and are evicted oldest-first past [`WINDOW_CAPACITY_SECONDS`]; extraction
/// slices every chunk overlapping the requested span (gaps — e.g. pauses —
/// simply contribute nothing). Pure and unit-testable.
struct AudioWindow {
    sample_rate: u32,
    chunks: VecDeque<(f64, Vec<f32>)>,
    total_samples: usize,
}

impl AudioWindow {
    fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            chunks: VecDeque::new(),
            total_samples: 0,
        }
    }

    fn push(&mut self, start_seconds: f64, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        self.total_samples += samples.len();
        self.chunks.push_back((start_seconds, samples.to_vec()));
        let capacity = (WINDOW_CAPACITY_SECONDS * f64::from(self.sample_rate)) as usize;
        while self.total_samples > capacity && self.chunks.len() > 1 {
            if let Some((_, evicted)) = self.chunks.pop_front() {
                self.total_samples -= evicted.len();
            }
        }
    }

    /// Concatenate the audio overlapping `[start, end)` on the unified
    /// timeline. Spans beyond the retained window (or fully in a gap) yield
    /// fewer (or zero) samples — callers treat "too little audio" as skip.
    fn extract(&self, start: f64, end: f64) -> Vec<f32> {
        if !start.is_finite() || !end.is_finite() || end <= start {
            return Vec::new();
        }
        let rate = f64::from(self.sample_rate);
        let mut out = Vec::new();
        for (chunk_start, samples) in &self.chunks {
            let chunk_end = chunk_start + samples.len() as f64 / rate;
            if chunk_end <= start || *chunk_start >= end {
                continue;
            }
            let from = (((start - chunk_start).max(0.0)) * rate) as usize;
            let to = ((((end - chunk_start) * rate) as usize).min(samples.len())).max(from);
            out.extend_from_slice(&samples[from..to]);
        }
        out
    }
}

/// One finalized utterance handed to the embedder thread: everything needed to
/// re-emit the segment with a speaker attribution appended as revision + 1.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct EmbedJob {
    segment_id: String,
    /// Revision of the finalizing event; the attribution event uses `+ 1`.
    revision: u64,
    track: &'static str,
    start_seconds: f64,
    end_seconds: f64,
    text: String,
    /// Utterance audio at the model rate (16 kHz mono).
    samples: Vec<f32>,
}

/// Handle to the long-lived embedder thread; dropping `tx` ends its loop and
/// `handle` is joined so trailing attributions flush before the worker exits.
struct LiveVerifier {
    tx: SyncSender<EmbedJob>,
    handle: JoinHandle<()>,
}

impl LiveVerifier {
    /// Hand a finalized utterance over; drops the job when the queue is full
    /// or the thread is gone — verification never delays transcription.
    fn enqueue(&self, job: EmbedJob) {
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                tracing::debug!(
                    segment = %job.segment_id,
                    "live verifier busy; skipping speaker verification for this utterance"
                );
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn finish(self) {
        drop(self.tx);
        let _ = self.handle.join();
    }
}

/// Per-track consecutive-hit bookkeeping for the streak upgrade rule: after
/// [`STREAK_UPGRADE_AFTER`] consecutive hits (provisional or verified) on the
/// same identity, *subsequent* provisional hits on that track may display as
/// verified. Only actually-matched segments are ever revised; a differing
/// identity resets the streak, while skipped/short utterances (no report)
/// leave it untouched. Pure and unit-testable.
#[derive(Default)]
struct VerificationStreak {
    by_track: std::collections::HashMap<&'static str, (Uuid, u32)>,
}

impl VerificationStreak {
    /// Record a hit for `identity` on `track`; returns `true` when a
    /// provisional hit may display as verified (streak established *before*
    /// this hit).
    fn observe_hit(&mut self, track: &'static str, identity: Uuid) -> bool {
        let entry = self.by_track.entry(track).or_insert((identity, 0));
        if entry.0 != identity {
            *entry = (identity, 0);
        }
        let upgraded = entry.1 >= STREAK_UPGRADE_AFTER;
        entry.1 += 1;
        upgraded
    }

    /// A confident-enough report pointed at a *different* person (or the
    /// policy rejected it): the run of agreement is broken.
    fn observe_miss(&mut self, track: &'static str) {
        self.by_track.remove(track);
    }
}

/// Spawn the embedder thread when the whole layer should run: macOS, the diar
/// embedding model installed, and at least one enrolled identity. Anywhere
/// else `None` — the worker then behaves exactly like L1/L2 (no ring pushes,
/// no jobs, no speaker events).
#[cfg(target_os = "macos")]
fn spawn_live_verifier(app: AppHandle, meeting_id: String) -> Option<LiveVerifier> {
    let emb_model = lumen_asr::lumen_models_dir().join("diar").join("emb.onnx");
    if !emb_model.is_file() {
        return None;
    }
    let identity_dir = lumen_identity::default_identity_dir();
    match lumen_identity::IdentityStore::open(&identity_dir) {
        Ok(store) if !store.list().is_empty() => {}
        _ => return None, // empty/unreadable library → layer stays inert
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<EmbedJob>(EMBED_QUEUE_CAPACITY);
    let handle = std::thread::Builder::new()
        .name("lumen-meeting-live-verify".into())
        .spawn(move || run_verifier(app, meeting_id, emb_model, identity_dir, rx))
        .ok()?;
    Some(LiveVerifier { tx, handle })
}

#[cfg(not(target_os = "macos"))]
fn spawn_live_verifier(_app: AppHandle, _meeting_id: String) -> Option<LiveVerifier> {
    None
}

/// Embedder thread body: load the WeSpeaker model once, then for each queued
/// utterance embed → verify → apply the live decision policy (+ streak
/// upgrade) → append a `revision + 1` event carrying the speaker attribution.
/// Every failure path skips the utterance; nothing here can affect the
/// transcription loop.
#[cfg(target_os = "macos")]
fn run_verifier(
    app: AppHandle,
    meeting_id: String,
    emb_model: PathBuf,
    identity_dir: PathBuf,
    rx: Receiver<EmbedJob>,
) {
    use lumen_identity::{live_decision, IdentityStore, LiveDecision};

    let mut embedder = match lumen_meeting::LiveVoiceprintEmbedder::load(&emb_model) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::warn!(error = %error, "live speaker verification disabled: embedding model failed to load");
            return;
        }
    };
    tracing::info!("live speaker verification started (WeSpeaker embedder thread)");
    let mut streak = VerificationStreak::default();
    while let Ok(job) = rx.recv() {
        // Reopen per utterance so enrollments made mid-recording are picked up
        // (the library is a handful of small JSON files).
        let identities = match IdentityStore::open(&identity_dir) {
            Ok(identities) if !identities.list().is_empty() => identities,
            _ => continue,
        };
        let embedding = match embedder.embed(&job.samples, STREAMING_TARGET_RATE) {
            Ok(Some(embedding)) => embedding,
            Ok(None) => continue, // too short to embed reliably
            Err(error) => {
                tracing::warn!(error = %error, "live embedding failed; skipping utterance");
                continue;
            }
        };
        let Some(report) = identities.verify_speaker(&embedding) else {
            continue;
        };
        let voiced_ms = ((job.end_seconds - job.start_seconds).max(0.0) * 1000.0).round() as u64;
        let provisional = match live_decision(&report, voiced_ms) {
            LiveDecision::NoMatch => {
                streak.observe_miss(job.track);
                continue;
            }
            LiveDecision::VerifiedAuto => {
                streak.observe_hit(job.track, report.identity_id);
                false
            }
            LiveDecision::Provisional => !streak.observe_hit(job.track, report.identity_id),
        };
        // Ids and scores only — the matched name is PII.
        tracing::info!(
            segment = %job.segment_id,
            score = report.best_score,
            margin = report.margin,
            provisional,
            "live speaker verification hit"
        );
        emit(
            &app,
            &meeting_id,
            job.track,
            SegmentUpdate {
                segment_id: job.segment_id,
                revision: job.revision + 1,
                start_seconds: job.start_seconds,
                end_seconds: Some(job.end_seconds),
                text: job.text,
                is_final: true,
            },
            Some(LiveSpeaker {
                identity_id: report.identity_id.to_string(),
                display_name: report.display_name,
                source: "voiceprint",
                provisional,
            }),
        );
    }
    tracing::info!("live speaker verification stopped");
}

/// One live track inside the worker: its fan-out receiver, its decoding stream
/// on the shared recognizer, and its segment bookkeeping.
struct TrackState {
    tracker: SegmentTracker,
    feed: LiveTrackFeed,
    stream: StreamingStream,
    /// End (unified timeline) of the audio fed so far: last packet's arrival
    /// stamp plus its duration. Used as the finalized segment's `end_seconds`.
    clock: f64,
    disconnected: bool,
    /// Recent model-rate audio for utterance extraction (L3). `None` when the
    /// verifier is not engaged — zero extra work or memory then.
    window: Option<AudioWindow>,
}

impl TrackState {
    fn new(
        track: &'static str,
        feed: LiveTrackFeed,
        stream: StreamingStream,
        keep_window: bool,
    ) -> Self {
        Self {
            tracker: SegmentTracker::new(track),
            feed,
            stream,
            clock: 0.0,
            disconnected: false,
            window: keep_window.then(|| AudioWindow::new(STREAMING_TARGET_RATE)),
        }
    }

    /// Drain every pending packet into the recognizer stream (resampling to
    /// the model rate). Returns `true` if any audio arrived. A disconnected
    /// sender (that track's capture stopped) is flagged, never an error.
    fn drain(&mut self) -> bool {
        let mut got_audio = false;
        loop {
            match self.feed.rx.try_recv() {
                Ok(packet) => {
                    got_audio = true;
                    self.tracker.note_audio(packet.start_seconds);
                    self.clock = packet.start_seconds
                        + packet.samples.len() as f64 / f64::from(self.feed.capture_rate.max(1));
                    let samples = if self.feed.capture_rate == STREAMING_TARGET_RATE {
                        packet.samples
                    } else {
                        resample_linear(
                            &packet.samples,
                            self.feed.capture_rate,
                            STREAMING_TARGET_RATE,
                        )
                    };
                    if let Some(window) = &mut self.window {
                        window.push(packet.start_seconds, &samples);
                    }
                    self.stream.accept_waveform(&samples, STREAMING_TARGET_RATE);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        got_audio
    }

    /// After a segment finalized: hand its audio span to the verifier when the
    /// layer is engaged and the utterance is long enough to ever earn a label.
    fn maybe_enqueue_verification(&self, verifier: &Option<LiveVerifier>, update: &SegmentUpdate) {
        let (Some(verifier), Some(window)) = (verifier, &self.window) else {
            return;
        };
        let Some(end_seconds) = update.end_seconds else {
            return;
        };
        if !update.is_final
            || update.text.trim().is_empty()
            || end_seconds - update.start_seconds < VERIFY_MIN_UTTERANCE_SECONDS
        {
            return;
        }
        let samples = window.extract(update.start_seconds, end_seconds);
        // The window may have already evicted part of a very old span; only
        // verify when we still hold (nearly) the whole utterance.
        let wanted =
            ((end_seconds - update.start_seconds) * f64::from(STREAMING_TARGET_RATE)) as usize;
        if samples.len() * 10 < wanted * 9 {
            return;
        }
        verifier.enqueue(EmbedJob {
            segment_id: update.segment_id.clone(),
            revision: update.revision,
            track: self.tracker.track,
            start_seconds: update.start_seconds,
            end_seconds,
            text: update.text.clone(),
            samples,
        });
    }

    /// After a decode pass: finalize on an endpoint (then reset for the next
    /// utterance) or surface a changed rolling partial.
    fn poll_result(&mut self) -> Option<SegmentUpdate> {
        if self.stream.is_endpoint() {
            let text = self.stream.result().text;
            let update = self.tracker.on_endpoint(&text, self.clock);
            self.stream.reset();
            update
        } else {
            self.tracker.on_partial(&self.stream.result().text)
        }
    }
}

/// The worker body: load the shared recognizer once, create one stream per
/// track, then round-robin drain → decode → emit until stopped. Exits when
/// stopped or the mic's sender is dropped (recording ended), flushing any
/// trailing text per track. A system-track failure mid-run degrades to
/// mic-only; it never ends the worker.
fn run_worker(
    app: AppHandle,
    meeting_id: String,
    streaming_dir: PathBuf,
    mic: LiveTrackFeed,
    system: Option<LiveTrackFeed>,
    stop: Arc<AtomicBool>,
) {
    // Building the recognizer can fail (missing/corrupt model, non-`sherpa`
    // build). That must never break the recording: log and return, so the UI
    // simply shows no live text and the offline pipeline still runs at stop.
    let recognizer = match StreamingRecognizer::from_dir(&streaming_dir) {
        Ok(recognizer) => recognizer,
        Err(e) => {
            tracing::warn!(
                dir = %streaming_dir.display(),
                error = %e,
                "live meeting transcript disabled: could not load streaming Paraformer"
            );
            return;
        }
    };

    // L3: live speaker verification (macOS + diar embedding model + non-empty
    // identity library, otherwise `None` and everything below degrades to the
    // exact L1/L2 behaviour — no ring buffers, no jobs, no speaker events).
    let verifier = spawn_live_verifier(app.clone(), meeting_id.clone());
    let keep_window = verifier.is_some();

    let mut tracks = vec![TrackState::new(
        TRACK_MIC,
        mic,
        recognizer.new_stream(),
        keep_window,
    )];
    if let Some(system) = system {
        tracks.push(TrackState::new(
            TRACK_SYSTEM,
            system,
            recognizer.new_stream(),
            keep_window,
        ));
    }

    tracing::info!(
        dir = %streaming_dir.display(),
        tracks = tracks.len(),
        "live meeting transcript started (streaming Paraformer, shared model)"
    );

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut got_audio = false;
        for track in &mut tracks {
            got_audio |= track.drain();
        }
        // Mic sender dropped ⇒ the recording stopped; flush below. A dropped
        // *system* sender only means that track's capture ended (tap failure
        // or stop ordering) — the worker keeps going mic-only.
        if tracks[0].disconnected {
            break;
        }
        if !got_audio {
            std::thread::sleep(IDLE_POLL);
            continue;
        }
        // One batched forward pass across every track that has audio ready
        // (single model, same dedicated thread — see the module docs).
        {
            let mut streams: Vec<&mut StreamingStream> =
                tracks.iter_mut().map(|t| &mut t.stream).collect();
            recognizer.decode_batch(&mut streams);
        }
        for track in &mut tracks {
            if let Some(update) = track.poll_result() {
                track.maybe_enqueue_verification(&verifier, &update);
                emit(&app, &meeting_id, track.tracker.track, update, None);
            }
        }
    }

    // Flush any trailing context so the last segment per track is not lost
    // mid-word.
    for track in &mut tracks {
        track.stream.input_finished();
        track.stream.decode();
        let text = track.stream.result().text;
        let clock = track.clock;
        if let Some(update) = track.tracker.on_endpoint(&text, clock) {
            track.maybe_enqueue_verification(&verifier, &update);
            emit(&app, &meeting_id, track.tracker.track, update, None);
        }
    }
    // Close the job queue and wait for in-flight verifications so a trailing
    // attribution still reaches the UI before the worker reports done.
    if let Some(verifier) = verifier {
        verifier.finish();
    }
    tracing::info!("live meeting transcript stopped");
}

fn emit(
    app: &AppHandle,
    meeting_id: &str,
    track: &'static str,
    update: SegmentUpdate,
    speaker: Option<LiveSpeaker>,
) {
    let _ = app.emit(
        "meeting-live-transcript",
        LiveTranscriptEvent {
            meeting_id: meeting_id.to_string(),
            segment_id: update.segment_id,
            revision: update.revision,
            track,
            start_seconds: update.start_seconds,
            end_seconds: update.end_seconds,
            text: update.text,
            is_final: update.is_final,
            speaker,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_camel_case_and_skips_absent_fields() {
        let ev = LiveTranscriptEvent {
            meeting_id: "m1".into(),
            segment_id: "mic-3".into(),
            revision: 2,
            track: TRACK_MIC,
            start_seconds: 1.5,
            end_seconds: None,
            text: "你好".into(),
            is_final: false,
            speaker: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"meetingId\":\"m1\""));
        assert!(json.contains("\"segmentId\":\"mic-3\""));
        assert!(json.contains("\"revision\":2"));
        assert!(json.contains("\"track\":\"mic\""));
        assert!(json.contains("\"startSeconds\":1.5"));
        assert!(json.contains("\"isFinal\":false"));
        assert!(json.contains("\"text\":\"你好\""));
        // Absent optionals are omitted entirely, not sent as null.
        assert!(!json.contains("endSeconds"));
        assert!(!json.contains("speaker"));
    }

    #[test]
    fn final_event_carries_end_seconds() {
        let ev = LiveTranscriptEvent {
            meeting_id: "m1".into(),
            segment_id: "system-0".into(),
            revision: 4,
            track: TRACK_SYSTEM,
            start_seconds: 0.25,
            end_seconds: Some(3.75),
            text: "好的".into(),
            is_final: true,
            speaker: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"track\":\"system\""));
        assert!(json.contains("\"endSeconds\":3.75"));
        assert!(json.contains("\"isFinal\":true"));
    }

    #[test]
    fn partials_share_segment_id_with_increasing_revisions() {
        let mut tracker = SegmentTracker::new(TRACK_MIC);
        tracker.note_audio(1.0);
        tracker.note_audio(1.2); // later packets never move the anchored start

        let first = tracker.on_partial("你").unwrap();
        assert_eq!(first.segment_id, "mic-0");
        assert_eq!(first.revision, 1);
        assert_eq!(first.start_seconds, 1.0);
        assert!(!first.is_final);
        assert_eq!(first.end_seconds, None);

        // Unchanged text or blank text emit nothing (no revision burn).
        assert_eq!(tracker.on_partial("你"), None);
        assert_eq!(tracker.on_partial("  "), None);

        let second = tracker.on_partial("你好").unwrap();
        assert_eq!(second.segment_id, "mic-0");
        assert_eq!(second.revision, 2);
    }

    #[test]
    fn endpoint_finalizes_with_end_seconds_and_advances_segment_id() {
        let mut tracker = SegmentTracker::new(TRACK_MIC);
        tracker.note_audio(0.5);
        tracker.on_partial("你好").unwrap();

        let fin = tracker.on_endpoint("你好世界", 4.0).unwrap();
        assert_eq!(fin.segment_id, "mic-0");
        assert_eq!(fin.revision, 2); // continues after the partial's revision
        assert!(fin.is_final);
        assert_eq!(fin.start_seconds, 0.5);
        assert_eq!(fin.end_seconds, Some(4.0));

        // The next utterance is a fresh segment with fresh revisions.
        tracker.note_audio(5.0);
        let next = tracker.on_partial("下一句").unwrap();
        assert_eq!(next.segment_id, "mic-1");
        assert_eq!(next.revision, 1);
        assert_eq!(next.start_seconds, 5.0);
    }

    #[test]
    fn silent_endpoint_emits_nothing_and_keeps_segment_id() {
        let mut tracker = SegmentTracker::new(TRACK_SYSTEM);
        tracker.note_audio(1.0);
        // No partial was ever announced and the final text is empty: no event,
        // and the unused id is kept for the next utterance.
        assert_eq!(tracker.on_endpoint("", 2.0), None);
        tracker.note_audio(3.0);
        let next = tracker.on_partial("好").unwrap();
        assert_eq!(next.segment_id, "system-0");
    }

    #[test]
    fn announced_segment_finalizing_empty_is_retracted() {
        let mut tracker = SegmentTracker::new(TRACK_MIC);
        tracker.note_audio(0.0);
        tracker.on_partial("嗯").unwrap();
        // The recognizer dropped the text at the endpoint: the segment was
        // already shown, so a final empty event retracts it in the UI.
        let fin = tracker.on_endpoint("", 1.0).unwrap();
        assert!(fin.is_final);
        assert!(fin.text.is_empty());
        assert_eq!(fin.segment_id, "mic-0");
        // And its id is burned — the next utterance gets a new one.
        tracker.note_audio(2.0);
        assert_eq!(tracker.on_partial("好").unwrap().segment_id, "mic-1");
    }

    // Off macOS the real-time layer is never engaged (no model gating even
    // reached). On macOS without an installed model it is likewise `None`.
    #[test]
    fn streaming_gate_is_none_without_macos_or_model() {
        let gate = streaming_dir_if_ready();
        if !cfg!(target_os = "macos") {
            assert!(gate.is_none(), "must be None on non-macOS");
        }
        // On macOS the result depends on whether a model is installed in the
        // test environment; either way it must not panic.
    }

    #[test]
    fn meeting_live_default_has_no_worker() {
        let live = MeetingLive::default();
        // stop() on an idle instance is a harmless no-op.
        live.stop();
        assert!(live.inner.lock().unwrap().is_none());
    }

    // ---- L3: speaker attribution payload ---------------------------------

    #[test]
    fn speaker_attribution_serializes_camel_case() {
        let ev = LiveTranscriptEvent {
            meeting_id: "m1".into(),
            segment_id: "mic-2".into(),
            revision: 5,
            track: TRACK_MIC,
            start_seconds: 1.0,
            end_seconds: Some(4.5),
            text: "你好".into(),
            is_final: true,
            speaker: Some(LiveSpeaker {
                identity_id: "11111111-2222-3333-4444-555555555555".into(),
                display_name: "李明".into(),
                source: "voiceprint",
                provisional: true,
            }),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(
                "\"speaker\":{\"identityId\":\"11111111-2222-3333-4444-555555555555\",\
             \"displayName\":\"李明\",\"source\":\"voiceprint\",\"provisional\":true}"
            ) || json.contains("\"identityId\":\"11111111-2222-3333-4444-555555555555\"")
        );
        assert!(json.contains("\"displayName\":\"李明\""));
        assert!(json.contains("\"source\":\"voiceprint\""));
        assert!(json.contains("\"provisional\":true"));
    }

    // ---- L3: audio ring window -------------------------------------------

    #[test]
    fn audio_window_extracts_the_requested_span_across_chunks() {
        let mut window = AudioWindow::new(10); // 10 Hz keeps the math readable
        window.push(0.0, &[0.0; 10]); // [0, 1)
        window.push(1.0, &[1.0; 10]); // [1, 2)
        window.push(2.0, &[2.0; 10]); // [2, 3)

        // A span crossing two chunks concatenates the right halves.
        let got = window.extract(0.5, 1.5);
        assert_eq!(got.len(), 10);
        assert_eq!(&got[..5], &[0.0; 5]);
        assert_eq!(&got[5..], &[1.0; 5]);

        // A span fully inside one chunk slices only it.
        assert_eq!(window.extract(2.0, 3.0), vec![2.0; 10]);
        // A span in a gap / before retained audio yields nothing.
        assert!(window.extract(5.0, 6.0).is_empty());
        assert!(window.extract(1.0, 1.0).is_empty(), "empty span");
    }

    #[test]
    fn audio_window_evicts_oldest_beyond_capacity() {
        let rate = 100u32;
        let mut window = AudioWindow::new(rate);
        // Push 40 one-second chunks; capacity is 30 s.
        for second in 0..40 {
            window.push(f64::from(second), &vec![second as f32; rate as usize]);
        }
        assert!(window.total_samples <= (WINDOW_CAPACITY_SECONDS * f64::from(rate)) as usize);
        // The oldest seconds are gone; the newest are intact.
        assert!(window.extract(0.0, 1.0).is_empty());
        assert_eq!(window.extract(39.0, 40.0), vec![39.0f32; rate as usize]);
    }

    #[test]
    fn audio_window_tolerates_gaps_between_chunks() {
        let mut window = AudioWindow::new(10);
        window.push(0.0, &[1.0; 10]); // [0, 1)
        window.push(5.0, &[2.0; 10]); // [5, 6) — a pause in between
        let got = window.extract(0.0, 6.0);
        // Only real audio is returned; the gap contributes nothing.
        assert_eq!(got.len(), 20);
        assert_eq!(&got[..10], &[1.0; 10]);
        assert_eq!(&got[10..], &[2.0; 10]);
    }

    // ---- L3: streak upgrade rule -----------------------------------------

    #[test]
    fn streak_upgrades_provisional_only_after_two_consecutive_hits() {
        let a = Uuid::new_v4();
        let mut streak = VerificationStreak::default();
        // Hits 1 and 2 establish the streak but do not upgrade themselves.
        assert!(!streak.observe_hit(TRACK_MIC, a));
        assert!(!streak.observe_hit(TRACK_MIC, a));
        // From the 3rd consecutive hit on, provisional may display verified.
        assert!(streak.observe_hit(TRACK_MIC, a));
        assert!(streak.observe_hit(TRACK_MIC, a));
    }

    #[test]
    fn streak_is_per_track_and_resets_on_identity_change_or_miss() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut streak = VerificationStreak::default();
        assert!(!streak.observe_hit(TRACK_MIC, a));
        assert!(!streak.observe_hit(TRACK_MIC, a));
        // The system track has its own independent streak.
        assert!(!streak.observe_hit(TRACK_SYSTEM, a));
        assert!(streak.observe_hit(TRACK_MIC, a));
        // A different identity on the same track resets the run.
        assert!(!streak.observe_hit(TRACK_MIC, b));
        assert!(!streak.observe_hit(TRACK_MIC, b));
        // A rejected/differing report breaks it too.
        streak.observe_miss(TRACK_MIC);
        assert!(!streak.observe_hit(TRACK_MIC, b));
    }
}
