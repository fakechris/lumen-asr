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
    /// Absent for a **session voiceprint** hit (L3.5): the name came from a
    /// manual annotation this meeting, not from the permanent identity
    /// library, so there is no enrolled identity to reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_id: Option<String>,
    /// The identity's current real name.
    display_name: String,
    /// Attribution source; always `"voiceprint"` from this layer.
    source: &'static str,
    /// `true` = suggest tentatively; `false` = auto-verified.
    provisional: bool,
}

/// Payload of the `meeting-live-degraded` event: the live worker detected
/// repeated packet gaps on a track's fan-out (the bounded `LiveTapSender`
/// channel dropped chunks because this consumer fell behind). Advisory only —
/// the WAV recording is on a separate, unbounded path and is never affected;
/// the UI shows a quiet "live preview may be missing words" note.
/// `estimated_lost_seconds` is the approximated preview audio lost so far in
/// the current degraded episode.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingLiveDegraded {
    meeting_id: String,
    track: &'static str,
    estimated_lost_seconds: f64,
}

/// Payload of `meeting-live-degraded-cleared`: gaps stopped, the live preview
/// is keeping up again and the UI retracts the note.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingLiveDegradedCleared {
    meeting_id: String,
    track: &'static str,
}

/// Sliding window (meeting-timeline seconds) over recent packet gaps: the gaps
/// inside it decide whether the preview is degraded.
const LIVE_DROP_WINDOW_SECONDS: f64 = 2.0;

/// Gaps needed inside the window before the preview is called degraded. Two
/// keeps a single pause/resume transition — which produces exactly one
/// timestamp gap and zero dropped packets — from ever warning.
const LIVE_DROP_MIN_GAPS: usize = 2;

/// A warned track recovers after this many seconds without a fresh gap.
const LIVE_DROP_CLEAR_SECONDS: f64 = 5.0;

/// One emission decision from [`LiveDropMonitor::observe`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum LiveDropAction {
    None,
    /// Enter the degraded state; carries the estimated lost preview seconds.
    Warn {
        lost_seconds: f64,
    },
    /// Leave the degraded state (recovery).
    Clear,
}

/// Consumer-side detector for fan-out packet drops. The tap
/// ([`lumen_asr::LiveTapSender`]) counts drops on the producer side, but the
/// count is not reachable through the channel, so this monitor infers drops
/// from the received packets' stamps: consecutive `start_seconds` normally
/// differ by one chunk duration (the stamp is taken at callback-delivery
/// time), so a gap well past that means packets were dropped in between.
///
/// Throttling: one warn per degraded episode (further gaps only extend the
/// lost-seconds estimate), one clear per recovery. Pure and unit-testable.
struct LiveDropMonitor {
    last_start_seconds: Option<f64>,
    /// Meeting-timeline stamps of the gaps still inside the window.
    gaps: VecDeque<f64>,
    /// Accumulated lost-preview estimate for the current episode.
    lost_seconds: f64,
    warned: bool,
}

impl LiveDropMonitor {
    fn new() -> Self {
        Self {
            last_start_seconds: None,
            gaps: VecDeque::new(),
            lost_seconds: 0.0,
            warned: false,
        }
    }

    fn observe(&mut self, start_seconds: f64, chunk_seconds: f64) -> LiveDropAction {
        let gap = self.last_start_seconds.map(|last| start_seconds - last);
        self.last_start_seconds = Some(start_seconds);

        // Normal spacing is ~one chunk; a single dropped packet already
        // doubles it. 1.5× + 20 ms absorbs callback jitter without ever
        // firing on undropped spacing.
        if let Some(gap) = gap.filter(|gap| *gap > chunk_seconds * 1.5 + 0.02) {
            while self
                .gaps
                .front()
                .is_some_and(|t| start_seconds - t > LIVE_DROP_WINDOW_SECONDS)
            {
                self.gaps.pop_front();
            }
            self.gaps.push_back(start_seconds);
            self.lost_seconds += (gap - chunk_seconds).max(0.0);
        }

        if self.warned {
            // Recover only after a quiet stretch with no fresh gap; until then
            // stay silent (the warn is already on screen).
            let quiet_for = self
                .gaps
                .back()
                .map_or(f64::INFINITY, |t| start_seconds - t);
            if quiet_for >= LIVE_DROP_CLEAR_SECONDS {
                self.warned = false;
                self.gaps.clear();
                self.lost_seconds = 0.0;
                return LiveDropAction::Clear;
            }
            return LiveDropAction::None;
        }
        if self.gaps.len() >= LIVE_DROP_MIN_GAPS {
            self.warned = true;
            return LiveDropAction::Warn {
                lost_seconds: self.lost_seconds,
            };
        }
        LiveDropAction::None
    }
}

/// Emit the UI event for one drop-monitor decision. Best-effort like every
/// other live event: a failed emit only costs the note, never the recording.
fn emit_drop_action(
    app: &AppHandle,
    meeting_id: &str,
    track: &'static str,
    action: LiveDropAction,
) {
    match action {
        LiveDropAction::Warn { lost_seconds } => {
            tracing::warn!(
                meeting_id,
                track,
                estimated_lost_seconds = lost_seconds,
                "live preview dropping fan-out packets (recording unaffected)"
            );
            let _ = app.emit(
                "meeting-live-degraded",
                MeetingLiveDegraded {
                    meeting_id: meeting_id.to_string(),
                    track,
                    estimated_lost_seconds: lost_seconds,
                },
            );
        }
        LiveDropAction::Clear => {
            tracing::info!(meeting_id, track, "live preview fan-out recovered");
            let _ = app.emit(
                "meeting-live-degraded-cleared",
                MeetingLiveDegradedCleared {
                    meeting_id: meeting_id.to_string(),
                    track,
                },
            );
        }
        LiveDropAction::None => {}
    }
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

/// A change to the recording meeting's manual speaker annotations (L2 chips),
/// forwarded from the annotate/delete commands to the running live worker so
/// it can maintain the in-memory **session voiceprint** set (L3.5). Purely
/// advisory: when no worker is running (not recording, live layer disabled)
/// the notice is silently dropped.
#[derive(Debug, Clone)]
pub enum AnnotationNotice {
    /// The user marked "who is speaking" on a live caption line.
    Annotated {
        /// Capture track the annotated line came from (`"mic"` / `"system"`).
        channel: String,
        /// Annotated range on the unified meeting timeline (open-ended when
        /// `end_seconds` is `None` — "此句及之后").
        start_seconds: f64,
        end_seconds: Option<f64>,
        /// Enrolled identity, when the user picked one from the library.
        /// Registered people are already covered by permanent-library
        /// verification (L3), so the worker only seeds session voiceprints
        /// for ad-hoc names (`None`).
        identity_id: Option<Uuid>,
        /// The annotated name (session voiceprint group key).
        display_name: String,
    },
    /// The user cleared an annotation: retract that name's session
    /// voiceprint samples (simple rule — the whole group by name).
    Cleared { display_name: String },
    /// The user renamed a mistyped name meeting-wide: relabel its session
    /// voiceprint group so the corrected name (not the old one) keeps matching.
    Renamed { old_name: String, new_name: String },
}

/// Bound of the command → worker annotation-notice queue. Notices are
/// user-click-rate events, so this is generous; when it ever fills, the
/// notice is dropped (only session-voiceprint seeding is lost, never the
/// persisted annotation itself).
const ANNOTATION_NOTICE_CAPACITY: usize = 16;

/// Owns the live-transcript worker for the currently active recording (if any).
/// Held in `AppState`; cross-platform and Send + Sync.
#[derive(Default)]
pub struct MeetingLive {
    inner: Mutex<Option<Worker>>,
}

struct Worker {
    /// Meeting this worker is recording — annotation notices for any other
    /// meeting id are ignored.
    meeting_id: String,
    stop: Arc<AtomicBool>,
    notices: SyncSender<AnnotationNotice>,
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
        let (notices, notices_rx) =
            std::sync::mpsc::sync_channel::<AnnotationNotice>(ANNOTATION_NOTICE_CAPACITY);
        let worker_meeting_id = meeting_id.clone();
        let handle = std::thread::Builder::new()
            .name("lumen-meeting-live".into())
            .spawn(move || {
                run_worker(
                    app,
                    worker_meeting_id,
                    streaming_dir,
                    mic,
                    system,
                    notices_rx,
                    stop_worker,
                );
            })
            .expect("spawn meeting live worker thread");
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(Worker {
                meeting_id,
                stop,
                notices,
                handle,
            });
        }
    }

    /// Forward a manual-annotation change to the running live worker (L3.5
    /// session voiceprints). Silently a no-op when no worker is running, when
    /// the worker records a different meeting, or when the notice queue is
    /// momentarily full — the persisted annotation itself is unaffected.
    pub fn notify_annotation(&self, meeting_id: &str, notice: AnnotationNotice) {
        let Ok(guard) = self.inner.lock() else {
            return;
        };
        let Some(worker) = guard.as_ref() else {
            return;
        };
        if worker.meeting_id != meeting_id {
            return;
        }
        match worker.notices.try_send(notice) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("annotation notice queue full; session voiceprint update skipped");
            }
            // Worker already exiting: seeding is moot, the session set is
            // about to be dropped anyway.
            Err(TrySendError::Disconnected(_)) => {}
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

/// Sample-anchored track clock: converts each fan-out packet's wall-clock
/// arrival stamp into the time its **first sample** holds on the unified
/// meeting timeline.
///
/// Why not use the arrival stamps directly? They diverge from the recorded
/// WAV — which is what the offline pipeline (and therefore annotation
/// reconciliation) measures segments against — in two ways:
///
/// - every arrival stamp trails the audio it carries by that chunk's
///   duration plus callback latency (the chunk is stamped when the capture
///   callback *delivers* it, i.e. after it was recorded);
/// - a paused recording drops samples from the WAV while wall-clock time
///   keeps running, so after a pause the arrival stamps lead the WAV
///   timeline by the whole paused interval, forever.
///
/// Instead, the first packet anchors the track (its arrival minus its own
/// duration ≈ the track's first captured frame on the unified timeline) and
/// every later packet is placed at `anchor + samples_fed_so_far / rate` —
/// glued to the same sample count the WAV holds, so live stamps and offline
/// WAV-time-plus-sidecar-offset segments agree. (Packets dropped by a full
/// fan-out channel are not counted here; they are rare, bounded by the
/// channel capacity, and only occur when the preview is already degraded.)
struct SampleClock {
    /// Unified-timeline time of the track's first captured frame; `None`
    /// until the first packet arrives.
    anchor_seconds: Option<f64>,
    /// Samples fed so far (at the track's native capture rate).
    samples_fed: u64,
    capture_rate: u32,
}

impl SampleClock {
    fn new(capture_rate: u32) -> Self {
        Self {
            anchor_seconds: None,
            samples_fed: 0,
            capture_rate: capture_rate.max(1),
        }
    }

    /// Place one packet on the unified timeline. Returns
    /// `(chunk_start_seconds, chunk_end_seconds)` for its first/last sample.
    fn observe(&mut self, arrival_seconds: f64, samples: usize) -> (f64, f64) {
        let rate = f64::from(self.capture_rate);
        let chunk_seconds = samples as f64 / rate;
        let anchor = *self
            .anchor_seconds
            .get_or_insert((arrival_seconds - chunk_seconds).max(0.0));
        let start = anchor + self.samples_fed as f64 / rate;
        self.samples_fed += samples as u64;
        (start, start + chunk_seconds)
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

/// Hits on the same speaker a track needs before *later* provisional hits for
/// that speaker may display as verified (the streak upgrade). Two agreeing
/// utterances establish the speaker on that track; from the next hit on, the
/// person is evidently the one talking there, so a grey-zone score stops
/// re-adding the "?". Tallies are kept per speaker (see
/// [`VerificationStreak`]), so an established speaker who returns after
/// others talked is re-verified on their first utterance back.
const STREAK_UPGRADE_AFTER: u32 = 2;

/// Rolling window of a track's recent audio (model-rate mono), stamped on the
/// meeting's unified timeline. Chunks are stamped with the track's
/// [`SampleClock`] chunk-start times — the same source the segment events use
/// — so an utterance's `[start, end]` span extracts exactly the audio the
/// recognizer saw for it (arrival stamps would drift by a chunk's duration
/// and by any paused interval). Evicted oldest-first past
/// [`WINDOW_CAPACITY_SECONDS`]; extraction slices every chunk overlapping the
/// requested span (gaps simply contribute nothing). Pure and unit-testable.
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

// ─────────────────────────────────────────────────────────────────────────────
// L3.5: session voiceprints — "标一次就够" for unregistered people. When the
// user manually annotates a live line with an ad-hoc name, that line's audio
// is embedded and kept as a **session voiceprint** for the rest of the
// recording, so later utterances by the same person auto-label with the name.
//
// Privacy boundary (design red line): a single chip click must never enroll
// permanent biometrics. Session voiceprints live only in the embedder
// thread's memory, are never written to the identity library or any disk
// path, and are dropped when the worker exits at stop. Names are never
// logged (PII), only counts and scores.
// ─────────────────────────────────────────────────────────────────────────────

/// Longest audio span seeded from one annotation. Open-ended annotations
/// ("此句及之后") take whatever the ring still holds from the annotated start,
/// capped here — a single utterance's worth is plenty for one sample, and the
/// cap bounds the embedder hand-off.
const SESSION_SEED_MAX_SECONDS: f64 = 10.0;

/// Minimum audio a seed needs to be worth an embedding. Below ~2 s a
/// single-span embedding is too noisy to anchor a whole session's matching
/// (mirrors [`lumen_identity::LIVE_PROVISIONAL_MIN_VOICED_MS`]). Spans whose
/// audio the ring window already evicted also fall below this and are skipped.
const SESSION_SEED_MIN_SECONDS: f64 = 2.0;

/// Samples kept per annotated name; further seeds roll the oldest out.
/// Re-annotating the same person in different moments accumulates voice
/// variety, and the small cap bounds per-utterance matching cost.
const SESSION_MAX_SAMPLES_PER_NAME: usize = 3;

/// Session score floor for a tentative ("名字?") label. Slightly *looser*
/// than the permanent library's provisional floor
/// ([`lumen_identity::CONSENSUS_THRESHOLD`] = 0.60 is the score floor there,
/// but permanent samples are multi-meeting centroids): a session sample is a
/// single-utterance embedding from the *same* microphone, room, and hour, so
/// same-person scores run higher and 0.65 keeps strangers out while catching
/// the annotated person reliably.
const SESSION_PROVISIONAL_THRESHOLD: f32 = 0.65;

/// Session score floor for a non-tentative label, combined with the margin
/// rule below. Deliberately a notch above [`lumen_identity::AUTO_TAG_THRESHOLD`]
/// territory scaled to same-session conditions: same-session same-speaker
/// cosine typically lands 0.75+, so 0.72 plus a clear margin is confident
/// without being unreachable.
const SESSION_VERIFIED_THRESHOLD: f32 = 0.72;

/// Minimum `best − runner_up` for a session auto-verified label; reuses the
/// permanent live rule's margin ([`lumen_identity::LIVE_VERIFIED_MIN_MARGIN`]).
const SESSION_VERIFIED_MIN_MARGIN: f32 = lumen_identity::LIVE_VERIFIED_MIN_MARGIN;

/// Decide whether (and with which audio) an annotation seeds a session
/// voiceprint. `None` — skip — when:
///
/// - `identity_id` is set: the person is **enrolled**, permanent-library
///   verification (L3) already labels them, so the session set never
///   duplicates registered biometrics;
/// - `window` is absent (verifier layer disengaged);
/// - the ring window holds less than [`SESSION_SEED_MIN_SECONDS`] of the
///   annotated span — the audio was already evicted, or the line is too
///   short to seed reliably.
///
/// Otherwise the annotated `[start, end]` span's audio (open-ended → whatever
/// exists from `start`, capped at [`SESSION_SEED_MAX_SECONDS`]).
fn plan_session_seed(
    identity_id: Option<Uuid>,
    window: Option<&AudioWindow>,
    start: f64,
    end: Option<f64>,
) -> Option<Vec<f32>> {
    if identity_id.is_some() {
        return None;
    }
    let window = window?;
    let end = end
        .unwrap_or(f64::INFINITY)
        .min(start + SESSION_SEED_MAX_SECONDS);
    let samples = window.extract(start, end);
    let min = (SESSION_SEED_MIN_SECONDS * f64::from(window.sample_rate)) as usize;
    (samples.len() >= min).then_some(samples)
}

/// Outcome of matching one utterance embedding against the session set.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
struct SessionMatch {
    display_name: String,
    /// `true` → render tentatively ("名字?").
    provisional: bool,
    best_score: f32,
    margin: f32,
}

/// The in-memory session voiceprint set: annotated name → up to
/// [`SESSION_MAX_SAMPLES_PER_NAME`] utterance embeddings, seeded from manual
/// chip annotations and consulted only when the permanent identity library
/// does not label an utterance. Owned by the embedder thread; dropped —
/// embeddings and all — when the worker exits at recording stop. Pure and
/// unit-testable.
#[derive(Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct SessionVoiceprints {
    /// Insertion-ordered `(name, samples)` groups; samples oldest-first.
    by_name: Vec<(String, VecDeque<Vec<f32>>)>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl SessionVoiceprints {
    /// Add one sample for `name` (a repeat annotation appends), rolling the
    /// oldest sample out beyond the per-name cap.
    fn seed(&mut self, name: &str, embedding: Vec<f32>) {
        let index = match self.by_name.iter().position(|(n, _)| n.as_str() == name) {
            Some(index) => index,
            None => {
                self.by_name.push((name.to_string(), VecDeque::new()));
                self.by_name.len() - 1
            }
        };
        let samples = &mut self.by_name[index].1;
        samples.push_back(embedding);
        while samples.len() > SESSION_MAX_SAMPLES_PER_NAME {
            samples.pop_front();
        }
    }

    /// Drop every sample seeded for `name` (annotation cleared). Returns
    /// whether the group existed.
    fn retract(&mut self, name: &str) -> bool {
        let before = self.by_name.len();
        self.by_name.retain(|(n, _)| n.as_str() != name);
        self.by_name.len() != before
    }

    /// Relabel a session group after the user fixed a mistyped name: move
    /// `old`'s samples onto `new` (merging into an existing `new` group), so the
    /// accumulated voiceprint is kept and the renamed speaker keeps matching
    /// under the corrected name instead of the old one resurfacing. Returns
    /// whether an `old` group existed.
    fn rename(&mut self, old: &str, new: &str) -> bool {
        if old == new {
            return false;
        }
        let Some(index) = self.by_name.iter().position(|(n, _)| n.as_str() == old) else {
            return false;
        };
        let (_, samples) = self.by_name.remove(index);
        match self.by_name.iter_mut().find(|(n, _)| n.as_str() == new) {
            Some((_, dest)) => {
                for sample in samples {
                    dest.push_back(sample);
                    while dest.len() > SESSION_MAX_SAMPLES_PER_NAME {
                        dest.pop_front();
                    }
                }
            }
            None => self.by_name.push((new.to_string(), samples)),
        }
        true
    }

    /// Match one finalized utterance against the session set: per name the
    /// **best** cosine over its ≤ [`SESSION_MAX_SAMPLES_PER_NAME`] samples,
    /// highest best wins. Decision tiers (constants above):
    ///
    /// - `voiced_ms ≥ 3000` and `best ≥ 0.72` and `margin ≥ 0.08` →
    ///   non-provisional (matches the permanent live rule's duration floor);
    /// - `voiced_ms ≥ 2000` and `best ≥ 0.65` → provisional ("名字?");
    /// - otherwise `None`.
    fn match_speaker(&self, embedding: &[f32], voiced_ms: u64) -> Option<SessionMatch> {
        let mut scored: Vec<(&str, f32)> = self
            .by_name
            .iter()
            .map(|(name, samples)| {
                let best = samples
                    .iter()
                    .map(|s| lumen_identity::cosine_similarity(embedding, s))
                    .fold(f32::NEG_INFINITY, f32::max);
                (name.as_str(), best)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (name, best) = *scored.first()?;
        // Sole group → the margin is maximally permissive (cosine floor),
        // mirroring `VerificationReport::runner_up_score`.
        let runner_up = scored.get(1).map_or(-1.0, |&(_, s)| s);
        let margin = best - runner_up;
        let provisional = if voiced_ms >= lumen_identity::LIVE_VERIFIED_MIN_VOICED_MS
            && best >= SESSION_VERIFIED_THRESHOLD
            && margin >= SESSION_VERIFIED_MIN_MARGIN
        {
            false
        } else if voiced_ms >= lumen_identity::LIVE_PROVISIONAL_MIN_VOICED_MS
            && best >= SESSION_PROVISIONAL_THRESHOLD
        {
            true
        } else {
            return None;
        };
        Some(SessionMatch {
            display_name: name.to_string(),
            provisional,
            best_score: best,
            margin,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// L4: unknown-speaker session clusters — 说话人1, 说话人2, … for people who
// are neither enrolled (L3) nor manually annotated (L3.5). An utterance that
// neither the identity library nor the session voiceprint set claims joins
// the nearest in-memory cluster, or founds a new one with the next
// session-scoped placeholder label.
//
// Same privacy boundary as L3.5: clusters live only in the embedder thread's
// memory, are never persisted, and are dropped at recording stop. A label is
// stable once founded — clusters are never renamed or merged — and one shared
// counter across both tracks keeps 说话人N unique meeting-wide (the UI keys
// speaker colors by display name). After stop the offline pipeline re-decides
// every speaker from scratch; these labels never leave the live preview.
// ─────────────────────────────────────────────────────────────────────────────

/// Cosine floor for joining an existing cluster. Deliberately a notch *above*
/// the session-voiceprint verified floor ([`SESSION_VERIFIED_THRESHOLD`] =
/// 0.72): joining folds the voice into a running centroid with no human
/// anchor, and the unrecoverable error is merging two people into one label —
/// splitting one person into 说话人1/说话人2 is recoverable (the annotate chip
/// can still name each), so the gate errs high. Same-session same-speaker
/// cosine typically lands ≥ 0.75 (see [`SESSION_VERIFIED_THRESHOLD`]), so the
/// same voice still groups reliably; mic-track echo bleed and merely-similar
/// voices fall through to a fresh cluster.
const CLUSTER_ASSIGN_THRESHOLD: f32 = 0.75;

/// Minimum `best − runner_up` between clusters for joining one. A grey-zone
/// utterance between two clusters founds a new one instead of being merged
/// into either on a coin flip. Reuses the permanent live rule's margin
/// ([`lumen_identity::LIVE_VERIFIED_MIN_MARGIN`]).
const CLUSTER_ASSIGN_MIN_MARGIN: f32 = lumen_identity::LIVE_VERIFIED_MIN_MARGIN;

/// Upper bound on clusters per meeting. Real meetings have a handful of
/// unknown speakers; the cap bounds per-utterance matching cost and stops
/// pathological fragmentation. At the cap, unmatched utterances simply stay
/// unlabeled.
const MAX_SESSION_CLUSTERS: usize = 16;

/// Outcome of assigning one utterance embedding to the cluster set.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
struct ClusterMatch {
    /// Session-scoped placeholder label, e.g. `说话人2`.
    label: String,
    /// Cosine to the nearest cluster's centroid: the (passing) join score,
    /// or the (failing) best score when this utterance founded a new cluster
    /// (-1.0 when the set was empty).
    best_score: f32,
    /// `true` when this utterance founded the cluster.
    created: bool,
}

/// One unknown-speaker cluster: a running centroid over the embeddings
/// assigned to it, plus the stable label given at founding.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct SessionCluster {
    label: String,
    /// Running **sum** of the member embeddings. Cosine is scale-invariant
    /// (`lumen_identity::cosine_similarity` normalizes internally), so the
    /// plain sum scores identically to the mean while keeping every member's
    /// full weight in later updates.
    centroid: Vec<f32>,
    count: u32,
}

/// The in-memory unknown-speaker cluster set, consulted only when neither the
/// permanent identity library nor the session voiceprints label an utterance.
/// Owned by the embedder thread; dropped — centroids and all — when the worker
/// exits at recording stop. Pure and unit-testable.
#[derive(Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct SessionClusters {
    /// Clusters in founding order (labels only ever append).
    clusters: Vec<SessionCluster>,
    /// Next label number; never decremented, so labels are never reused even
    /// though clusters are never removed either.
    next_label: u32,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl SessionClusters {
    /// Assign one utterance embedding to the nearest cluster, founding a new
    /// one (with the next `说话人N` label) when nothing clears
    /// [`CLUSTER_ASSIGN_THRESHOLD`] with a [`CLUSTER_ASSIGN_MIN_MARGIN`] lead —
    /// or when the cluster cap is reached (`None`, the utterance stays
    /// unlabeled). A joined cluster adds the embedding to its running centroid
    /// sum, so the anchor drifts toward the speaker's typical voice over the
    /// meeting. A degenerate (empty/zero) embedding never founds anything.
    fn assign(&mut self, embedding: &[f32]) -> Option<ClusterMatch> {
        if embedding.is_empty() || embedding.iter().all(|v| *v == 0.0) {
            return None;
        }
        let mut scored: Vec<(usize, f32)> = self
            .clusters
            .iter()
            .enumerate()
            .map(|(index, cluster)| {
                (
                    index,
                    lumen_identity::cosine_similarity(embedding, &cluster.centroid),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let nearest = scored.first().map_or(-1.0, |&(_, s)| s);
        if let Some(&(index, best)) = scored.first() {
            // Sole cluster → the margin is maximally permissive (cosine
            // floor), mirroring `SessionVoiceprints::match_speaker`.
            let runner_up = scored.get(1).map_or(-1.0, |&(_, s)| s);
            if best >= CLUSTER_ASSIGN_THRESHOLD && best - runner_up >= CLUSTER_ASSIGN_MIN_MARGIN {
                let cluster = &mut self.clusters[index];
                for (c, e) in cluster.centroid.iter_mut().zip(embedding) {
                    *c += e;
                }
                cluster.count += 1;
                return Some(ClusterMatch {
                    label: cluster.label.clone(),
                    best_score: best,
                    created: false,
                });
            }
        }
        if self.clusters.len() >= MAX_SESSION_CLUSTERS {
            return None;
        }
        self.next_label += 1;
        let label = format!("说话人{}", self.next_label);
        self.clusters.push(SessionCluster {
            label: label.clone(),
            centroid: embedding.to_vec(),
            count: 1,
        });
        Some(ClusterMatch {
            label,
            best_score: nearest,
            created: true,
        })
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

/// Everything the worker loop hands to the embedder thread: utterances to
/// verify (L3) and session-voiceprint maintenance (L3.5). All embedding work
/// stays on the one embedder thread; the transcription loop never blocks.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum VerifierMsg {
    /// Verify a finalized utterance's speaker.
    Verify(EmbedJob),
    /// Seed a session voiceprint sample from a manual annotation's audio
    /// span (already extracted from the track's ring window, model rate).
    Seed {
        display_name: String,
        samples: Vec<f32>,
    },
    /// An annotation was cleared: drop that name's session samples.
    Retract { display_name: String },
    /// A mistyped name was renamed meeting-wide: relabel its session samples.
    Rename { old_name: String, new_name: String },
}

/// Handle to the long-lived embedder thread; dropping `tx` ends its loop and
/// `handle` is joined so trailing attributions flush before the worker exits.
struct LiveVerifier {
    tx: SyncSender<VerifierMsg>,
    handle: JoinHandle<()>,
}

impl LiveVerifier {
    /// Hand a finalized utterance over; drops the job when the queue is full
    /// or the thread is gone — verification never delays transcription.
    fn enqueue_verify(&self, job: EmbedJob) {
        match self.tx.try_send(VerifierMsg::Verify(job)) {
            Ok(()) => {}
            Err(TrySendError::Full(VerifierMsg::Verify(job))) => {
                tracing::debug!(
                    segment = %job.segment_id,
                    "live verifier busy; skipping speaker verification for this utterance"
                );
            }
            Err(_) => {}
        }
    }

    /// Hand a session-voiceprint change over (same drop-when-busy contract;
    /// a lost seed only means the user annotates again).
    fn enqueue_session(&self, msg: VerifierMsg) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("live verifier busy; session voiceprint update skipped");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn finish(self) {
        drop(self.tx);
        let _ = self.handle.join();
    }
}

/// Which speaker a live verification decision pointed at, across the three
/// attribution sources. Streak tallies are keyed by this — not by "whoever
/// spoke last on the track" — so a speaker who returns after someone else
/// talked keeps their own accumulated evidence instead of restarting from
/// zero. The sources are distinct namespaces that never alias each other,
/// even when the display strings coincide (a user could literally annotate
/// someone "说话人1").
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SpeakerKey {
    /// Enrolled identity library hit (L3).
    Identity(Uuid),
    /// Session voiceprint hit (L3.5), keyed by the annotated name.
    Session(String),
    /// Unknown-speaker session cluster hit (L4), keyed by the 说话人N label.
    Cluster(String),
}

/// Per-track hit tallies for the streak upgrade rule: after
/// [`STREAK_UPGRADE_AFTER`] hits (provisional or verified) on the same
/// speaker, *subsequent* provisional hits for that speaker on that track may
/// display as verified. Tallies are kept **per speaker** ([`SpeakerKey`]):
///
/// - an established speaker who returns after other speakers talked is
///   re-verified on their first utterance back (the streak pre-rolls),
///   instead of needing two fresh consecutive hits;
/// - an interleaved different speaker neither inherits nor extends anyone
///   else's tally — their own streak starts from zero, and they break the
///   would-be consecutive run (a speaker whose tally is still 1 after an
///   interleave has no streak yet and stays provisional);
/// - a fully unlabeled utterance (verification ran but no source claimed it)
///   clears the track's tallies entirely, while skipped/short utterances
///   (no report) leave them untouched.
///
/// Pure and unit-testable — kept un-gated (like [`EmbedJob`]) so the rule is
/// tested on every platform; its only non-test caller is the macOS embedder
/// thread, hence the dead-code allowance elsewhere.
#[derive(Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct VerificationStreak {
    by_track: std::collections::HashMap<&'static str, std::collections::HashMap<SpeakerKey, u32>>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl VerificationStreak {
    /// Record a hit for `speaker` on `track`; returns `true` when a
    /// provisional hit may display as verified — i.e. the speaker's tally on
    /// this track had *already* reached [`STREAK_UPGRADE_AFTER`] before this
    /// hit (the streak was established earlier, possibly before other
    /// speakers interleaved).
    fn observe_hit(&mut self, track: &'static str, speaker: SpeakerKey) -> bool {
        let tally = self
            .by_track
            .entry(track)
            .or_default()
            .entry(speaker)
            .or_insert(0);
        let upgraded = *tally >= STREAK_UPGRADE_AFTER;
        *tally += 1;
        upgraded
    }

    /// Verification ran but nothing labeled the utterance: the run of
    /// agreement on this track is broken and all its tallies reset.
    fn observe_miss(&mut self, track: &'static str) {
        self.by_track.remove(track);
    }

    /// Forget every tally for `speaker` on all tracks: the annotation that
    /// name was keyed by was retracted or renamed, so the freed name may be
    /// reused for a *different* person and must not inherit the old group's
    /// established streak.
    fn forget_speaker(&mut self, speaker: &SpeakerKey) {
        for tallies in self.by_track.values_mut() {
            tallies.remove(speaker);
        }
        self.by_track.retain(|_, tallies| !tallies.is_empty());
    }
}

/// Spawn the embedder thread when the layer can run: macOS with the diar
/// embedding model installed. Anywhere else `None` — the worker then behaves
/// exactly like L1/L2 (no ring pushes, no jobs, no speaker events).
///
/// Unlike the original L3 gate this no longer requires a non-empty identity
/// library: session voiceprints (L3.5) must work precisely for *unregistered*
/// people, an annotation can arrive at any moment mid-recording, and unknown
/// speakers can always earn a session-cluster label (L4) — so the verify path
/// embeds every queued utterance regardless of library state.
#[cfg(target_os = "macos")]
fn spawn_live_verifier(app: AppHandle, meeting_id: String) -> Option<LiveVerifier> {
    let emb_model = lumen_asr::lumen_models_dir().join("diar").join("emb.onnx");
    if !emb_model.is_file() {
        return None;
    }
    let identity_dir = lumen_identity::default_identity_dir();
    let (tx, rx) = std::sync::mpsc::sync_channel::<VerifierMsg>(EMBED_QUEUE_CAPACITY);
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

/// Embedder thread body: load the WeSpeaker model once, then process queued
/// messages — session voiceprint seeds/retractions (L3.5) and finalized
/// utterances to verify (L3). Verification checks the **permanent identity
/// library first**; when it does not label the utterance it falls back to
/// the in-memory session set (L3.5, emitted with no `identity_id`), and when
/// that also misses, to the unknown-speaker session clusters (L4, emitted as
/// a stable session-scoped `说话人N` placeholder label). Whichever source
/// labels the utterance feeds the per-speaker streak
/// ([`VerificationStreak`]); an utterance no source claims breaks the track's
/// run. Every failure path skips the message; nothing here can affect the
/// transcription loop. The session set and the clusters — the only places
/// session embeddings ever live — are dropped when this thread exits at
/// recording stop.
#[cfg(target_os = "macos")]
fn run_verifier(
    app: AppHandle,
    meeting_id: String,
    emb_model: PathBuf,
    identity_dir: PathBuf,
    rx: Receiver<VerifierMsg>,
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
    // In-memory only; never persisted, dropped at thread exit (privacy
    // boundary of L3.5 — one click must not permanently enroll biometrics).
    let mut session = SessionVoiceprints::default();
    // Same boundary for L4's unknown-speaker clusters: in-memory session
    // state only, dropped with this thread. Labels are session-scoped
    // placeholders (说话人N), shared across tracks via this one instance.
    let mut clusters = SessionClusters::default();
    while let Ok(msg) = rx.recv() {
        let job = match msg {
            VerifierMsg::Seed {
                display_name,
                samples,
            } => {
                match embedder.embed(&samples, STREAMING_TARGET_RATE) {
                    Ok(Some(embedding)) => {
                        session.seed(&display_name, embedding);
                        // Counts only — the annotated name is PII.
                        tracing::info!(
                            names = session.by_name.len(),
                            "session voiceprint seeded from manual annotation"
                        );
                    }
                    Ok(None) => {
                        tracing::info!("session voiceprint seed skipped: audio too short to embed");
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "session voiceprint seed failed");
                    }
                }
                continue;
            }
            VerifierMsg::Retract { display_name } => {
                if session.retract(&display_name) {
                    tracing::info!(
                        names = session.by_name.len(),
                        "session voiceprint retracted (annotation cleared)"
                    );
                }
                // A cleared name may be reused for a different person: it
                // must not keep the old group's streak tally.
                streak.forget_speaker(&SpeakerKey::Session(display_name));
                continue;
            }
            VerifierMsg::Rename { old_name, new_name } => {
                if session.rename(&old_name, &new_name) {
                    tracing::info!(
                        names = session.by_name.len(),
                        "session voiceprint relabeled (annotation renamed)"
                    );
                }
                // The mistyped name is free for reuse; the renamed speaker
                // re-establishes under the corrected name instead of the old
                // name's tally silently following it.
                streak.forget_speaker(&SpeakerKey::Session(old_name));
                continue;
            }
            VerifierMsg::Verify(job) => job,
        };
        // Reopen per utterance so enrollments made mid-recording are picked up
        // (the library is a handful of small JSON files). An empty/unreadable
        // library is fine: session voiceprints and session clusters (L4) can
        // still label, so the embedding work always pays off.
        let identities = IdentityStore::open(&identity_dir)
            .ok()
            .filter(|store| !store.list().is_empty());
        let embedding = match embedder.embed(&job.samples, STREAMING_TARGET_RATE) {
            Ok(Some(embedding)) => embedding,
            Ok(None) => continue, // too short to embed reliably
            Err(error) => {
                tracing::warn!(error = %error, "live embedding failed; skipping utterance");
                continue;
            }
        };
        let voiced_ms = ((job.end_seconds - job.start_seconds).max(0.0) * 1000.0).round() as u64;
        // 1) Permanent identity library (existing L3 behaviour, streak rule
        //    included).
        let mut speaker: Option<LiveSpeaker> = None;
        if let Some(report) = identities.and_then(|ids| ids.verify_speaker(&embedding)) {
            match live_decision(&report, voiced_ms) {
                // No streak wipe here: the session/cluster fallbacks below
                // may still label the utterance, and only a fully unlabeled
                // utterance breaks the track's run of agreement.
                LiveDecision::NoMatch => {}
                decision => {
                    let key = SpeakerKey::Identity(report.identity_id);
                    let provisional = match decision {
                        LiveDecision::VerifiedAuto => {
                            streak.observe_hit(job.track, key);
                            false
                        }
                        _ => !streak.observe_hit(job.track, key),
                    };
                    // Ids and scores only — the matched name is PII.
                    tracing::info!(
                        segment = %job.segment_id,
                        score = report.best_score,
                        margin = report.margin,
                        provisional,
                        "live speaker verification hit"
                    );
                    speaker = Some(LiveSpeaker {
                        identity_id: Some(report.identity_id.to_string()),
                        display_name: report.display_name,
                        source: "voiceprint",
                        provisional,
                    });
                }
            }
        }
        // 2) Session voiceprints (L3.5): only when the permanent library did
        //    not label the utterance. No identity id — the name exists only
        //    for this meeting.
        if speaker.is_none() {
            if let Some(hit) = session.match_speaker(&embedding, voiced_ms) {
                // Same streak rule as the permanent library, keyed by the
                // session name: once this speaker is established on the
                // track, a grey-zone hit displays verified — including the
                // first utterance of a return after other speakers.
                let upgraded =
                    streak.observe_hit(job.track, SpeakerKey::Session(hit.display_name.clone()));
                let provisional = hit.provisional && !upgraded;
                tracing::info!(
                    segment = %job.segment_id,
                    score = hit.best_score,
                    margin = hit.margin,
                    provisional,
                    "session voiceprint hit"
                );
                speaker = Some(LiveSpeaker {
                    identity_id: None,
                    display_name: hit.display_name,
                    source: "voiceprint",
                    provisional,
                });
            }
        }
        // 3) Unknown-speaker session clusters (L4): when nothing named the
        //    utterance, join the nearest cluster or found a new one, earning
        //    a stable session-scoped 说话人N label. Emitted non-provisional:
        //    the placeholder name already reads as "unknown speaker", so a
        //    permanent "?" on every line would add noise without information.
        //    Counts and scores only — the embedding itself is never logged.
        if speaker.is_none() {
            if let Some(hit) = clusters.assign(&embedding) {
                // Cluster labels always display non-provisional, but the hit
                // still feeds the streak so tallies stay uniform across all
                // three attribution sources.
                streak.observe_hit(job.track, SpeakerKey::Cluster(hit.label.clone()));
                tracing::info!(
                    segment = %job.segment_id,
                    created = hit.created,
                    score = hit.best_score,
                    clusters = clusters.clusters.len(),
                    "session cluster label"
                );
                speaker = Some(LiveSpeaker {
                    identity_id: None,
                    display_name: hit.label,
                    source: "voiceprint",
                    provisional: false,
                });
            }
        }
        let Some(speaker) = speaker else {
            // Verification ran but no source labeled the utterance: the run
            // of agreement on this track is broken (the same wipe the old
            // library-NoMatch path applied).
            streak.observe_miss(job.track);
            continue;
        };
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
            Some(speaker),
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
    /// Sample-anchored unified-timeline clock for this track's packets.
    sample_clock: SampleClock,
    /// Consumer-side fan-out drop detector (preview-degradation warnings).
    drop_monitor: LiveDropMonitor,
    /// End (unified timeline) of the audio fed so far. Used as the finalized
    /// segment's `end_seconds`.
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
        let sample_clock = SampleClock::new(feed.capture_rate);
        Self {
            tracker: SegmentTracker::new(track),
            feed,
            stream,
            sample_clock,
            drop_monitor: LiveDropMonitor::new(),
            clock: 0.0,
            disconnected: false,
            window: keep_window.then(|| AudioWindow::new(STREAMING_TARGET_RATE)),
        }
    }

    /// Drain every pending packet into the recognizer stream (resampling to
    /// the model rate). Returns `true` if any audio arrived. A disconnected
    /// sender (that track's capture stopped) is flagged, never an error.
    /// Drop-monitor decisions are pushed to `drop_actions` for the caller to
    /// emit (this method has no `AppHandle`).
    fn drain(&mut self, drop_actions: &mut Vec<LiveDropAction>) -> bool {
        let mut got_audio = false;
        loop {
            match self.feed.rx.try_recv() {
                Ok(packet) => {
                    got_audio = true;
                    let chunk_seconds =
                        packet.samples.len() as f64 / f64::from(self.feed.capture_rate.max(1));
                    let action = self
                        .drop_monitor
                        .observe(packet.start_seconds, chunk_seconds);
                    if action != LiveDropAction::None {
                        drop_actions.push(action);
                    }
                    let (chunk_start, chunk_end) = self
                        .sample_clock
                        .observe(packet.start_seconds, packet.samples.len());
                    self.tracker.note_audio(chunk_start);
                    self.clock = chunk_end;
                    let samples = if self.feed.capture_rate == STREAMING_TARGET_RATE {
                        packet.samples
                    } else {
                        resample_linear(
                            &packet.samples,
                            self.feed.capture_rate,
                            STREAMING_TARGET_RATE,
                        )
                    };
                    // The ring window must share the segment events' time
                    // source (the sample-anchored clock), or utterance
                    // [start, end] extraction would misalign with the spans
                    // the tracker reports — by a chunk's duration always, and
                    // by the whole paused interval after a pause.
                    if let Some(window) = &mut self.window {
                        window.push(chunk_start, &samples);
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
        verifier.enqueue_verify(EmbedJob {
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

/// Apply one manual-annotation notice inside the worker (L3.5):
///
/// - annotation of an **enrolled** identity → nothing to do, the permanent
///   library already covers that person (L3);
/// - annotation of an ad-hoc name → extract the annotated span from that
///   track's ring window and hand it to the embedder thread as a session
///   voiceprint seed (skipped, with a log, when the ring already evicted the
///   audio or the span holds under [`SESSION_SEED_MIN_SECONDS`] of it);
/// - a cleared annotation → retract that name's session samples.
///
/// With the verifier disengaged (no model) there are no ring windows and no
/// session set, so every notice is a no-op.
fn handle_annotation_notice(
    tracks: &[TrackState],
    verifier: &Option<LiveVerifier>,
    notice: AnnotationNotice,
) {
    let Some(verifier) = verifier else {
        return;
    };
    match notice {
        AnnotationNotice::Annotated {
            channel,
            start_seconds,
            end_seconds,
            identity_id,
            display_name,
        } => {
            let window = tracks
                .iter()
                .find(|t| t.tracker.track == channel)
                .and_then(|t| t.window.as_ref());
            match plan_session_seed(identity_id, window, start_seconds, end_seconds) {
                Some(samples) => verifier.enqueue_session(VerifierMsg::Seed {
                    display_name,
                    samples,
                }),
                None if identity_id.is_none() => tracing::info!(
                    start_seconds,
                    "session voiceprint seed skipped: annotated audio already evicted or too short"
                ),
                // Enrolled identity: intentionally nothing to seed.
                None => {}
            }
        }
        AnnotationNotice::Cleared { display_name } => {
            verifier.enqueue_session(VerifierMsg::Retract { display_name });
        }
        AnnotationNotice::Renamed { old_name, new_name } => {
            verifier.enqueue_session(VerifierMsg::Rename { old_name, new_name });
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
    notices: Receiver<AnnotationNotice>,
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
        // Manual-annotation changes (L3.5): seed/retract session voiceprints.
        // Drained before the audio so a seed sees the ring as close to the
        // annotated moment as possible.
        while let Ok(notice) = notices.try_recv() {
            handle_annotation_notice(&tracks, &verifier, notice);
        }
        let mut got_audio = false;
        for track in &mut tracks {
            let mut drop_actions = Vec::new();
            got_audio |= track.drain(&mut drop_actions);
            for action in drop_actions {
                emit_drop_action(&app, &meeting_id, track.tracker.track, action);
            }
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

    #[test]
    fn sample_clock_anchors_at_the_first_captured_frame() {
        let mut clock = SampleClock::new(16_000);
        // First chunk: 1600 samples (0.1 s) arriving 0.30 s after t0 — its
        // first frame was captured at ≈0.20 s.
        let (start, end) = clock.observe(0.30, 1600);
        assert!((start - 0.20).abs() < 1e-9);
        assert!((end - 0.30).abs() < 1e-9);
    }

    #[test]
    fn sample_clock_glues_to_sample_count_not_arrival_jitter() {
        let mut clock = SampleClock::new(16_000);
        clock.observe(0.30, 1600);
        // The next chunk arrives late (scheduling jitter): its placement
        // still follows the fed sample count, not the arrival stamp.
        let (start, end) = clock.observe(0.55, 1600);
        assert!((start - 0.30).abs() < 1e-9);
        assert!((end - 0.40).abs() < 1e-9);
    }

    #[test]
    fn sample_clock_is_immune_to_pause_gaps() {
        let mut clock = SampleClock::new(16_000);
        clock.observe(0.30, 1600);
        // A 10 s pause: no packets flow (the recorder drops paused audio from
        // both the WAV and the fan-out). The first post-resume chunk must
        // continue the *WAV* timeline (0.30 s in), not jump by wall-clock.
        let (start, _) = clock.observe(10.40, 1600);
        assert!((start - 0.30).abs() < 1e-9);
    }

    #[test]
    fn sample_clock_anchor_never_goes_negative() {
        let mut clock = SampleClock::new(16_000);
        // Degenerate stamp (arrival before its own duration): clamp to t0.
        let (start, _) = clock.observe(0.05, 1600);
        assert!(start >= 0.0);
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
        // Notifying with no worker running is a silent no-op too (the
        // annotate/delete commands call this unconditionally).
        live.notify_annotation(
            "m1",
            AnnotationNotice::Cleared {
                display_name: "客户A".into(),
            },
        );
        assert!(live.inner.lock().unwrap().is_none());
    }

    // ---- Live fan-out drop detection -------------------------------------

    /// Feed `monitor` a run of undropped 0.1 s chunks starting at `from`;
    /// returns the next free timestamp.
    fn feed_clean(monitor: &mut LiveDropMonitor, from: f64, count: usize) -> f64 {
        let mut t = from;
        for _ in 0..count {
            assert_eq!(monitor.observe(t, 0.1), LiveDropAction::None, "t={t}");
            t += 0.1;
        }
        t
    }

    #[test]
    fn clean_packet_spacing_never_warns() {
        let mut monitor = LiveDropMonitor::new();
        feed_clean(&mut monitor, 0.0, 200);
    }

    #[test]
    fn one_gap_alone_does_not_warn_but_two_inside_the_window_do() {
        let mut monitor = LiveDropMonitor::new();
        feed_clean(&mut monitor, 0.0, 10);
        // One dropped 0.1 s packet at t=1.0: the next stamp is 1.1 → gap 0.2.
        // A single gap stays silent (a pause/resume transition looks the same).
        assert_eq!(monitor.observe(1.1, 0.1), LiveDropAction::None);
        // A second gap inside the 2 s window → degraded, once.
        match monitor.observe(1.3, 0.1) {
            LiveDropAction::Warn { lost_seconds } => {
                assert!((lost_seconds - 0.2).abs() < 1e-9, "{lost_seconds}");
            }
            other => panic!("expected warn, got {other:?}"),
        }
    }

    #[test]
    fn warn_is_emitted_once_per_episode_and_further_gaps_only_extend_it() {
        let mut monitor = LiveDropMonitor::new();
        monitor.observe(0.0, 0.1);
        monitor.observe(0.3, 0.1); // gap
        assert!(matches!(
            monitor.observe(0.5, 0.1), // second gap → warn
            LiveDropAction::Warn { .. }
        ));
        // More gaps while warned: no repeated warn events.
        assert_eq!(monitor.observe(0.8, 0.1), LiveDropAction::None);
        assert_eq!(monitor.observe(1.1, 0.1), LiveDropAction::None);
    }

    #[test]
    fn warned_state_clears_after_a_quiet_stretch() {
        let mut monitor = LiveDropMonitor::new();
        monitor.observe(0.0, 0.1);
        monitor.observe(0.3, 0.1);
        assert!(matches!(
            monitor.observe(0.5, 0.1),
            LiveDropAction::Warn { .. }
        ));
        // Clean packets resume; 5 s without a fresh gap → cleared.
        let mut t = 0.6;
        let mut cleared = false;
        while t < 6.0 {
            if monitor.observe(t, 0.1) == LiveDropAction::Clear {
                cleared = true;
                break;
            }
            t += 0.1;
        }
        assert!(
            cleared,
            "monitor must recover after {LIVE_DROP_CLEAR_SECONDS}s quiet"
        );
        // After recovery a new episode can warn again: the big jump to 10.0 is
        // the first gap, the one at 10.3 the second inside the window.
        monitor.observe(10.0, 0.1);
        assert!(matches!(
            monitor.observe(10.3, 0.1),
            LiveDropAction::Warn { .. }
        ));
    }

    #[test]
    fn gaps_outside_the_window_do_not_accumulate() {
        let mut monitor = LiveDropMonitor::new();
        monitor.observe(0.0, 0.1);
        monitor.observe(0.3, 0.1); // gap at 0.3
                                   // Next gap 3 s later: the first one already left the 2 s window.
        feed_clean_from(&mut monitor, 0.4, 3.4);
        assert_eq!(monitor.observe(3.6, 0.1), LiveDropAction::None);
    }

    /// Feed undropped 0.1 s chunks from `from` up to (not including) `until`.
    fn feed_clean_from(monitor: &mut LiveDropMonitor, from: f64, until: f64) {
        let mut t = from;
        while t < until {
            assert_eq!(monitor.observe(t, 0.1), LiveDropAction::None, "t={t}");
            t += 0.1;
        }
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
                identity_id: Some("11111111-2222-3333-4444-555555555555".into()),
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

    #[test]
    fn session_speaker_attribution_omits_identity_id() {
        // A session-voiceprint hit (L3.5) has no enrolled identity: the
        // payload carries only the name, and `identityId` is omitted
        // entirely (not sent as null).
        let ev = LiveTranscriptEvent {
            meeting_id: "m1".into(),
            segment_id: "mic-4".into(),
            revision: 3,
            track: TRACK_MIC,
            start_seconds: 2.0,
            end_seconds: Some(5.0),
            text: "好的".into(),
            is_final: true,
            speaker: Some(LiveSpeaker {
                identity_id: None,
                display_name: "客户A".into(),
                source: "voiceprint",
                provisional: true,
            }),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("identityId"));
        assert!(json.contains("\"displayName\":\"客户A\""));
        assert!(json.contains("\"source\":\"voiceprint\""));
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

    fn identity_key(id: Uuid) -> SpeakerKey {
        SpeakerKey::Identity(id)
    }

    #[test]
    fn streak_upgrades_provisional_only_after_two_hits() {
        let a = Uuid::new_v4();
        let mut streak = VerificationStreak::default();
        // Hits 1 and 2 establish the streak but do not upgrade themselves.
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        // From the 3rd hit on, provisional may display verified.
        assert!(streak.observe_hit(TRACK_MIC, identity_key(a)));
        assert!(streak.observe_hit(TRACK_MIC, identity_key(a)));
    }

    #[test]
    fn streak_is_per_track_and_a_miss_clears_it() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut streak = VerificationStreak::default();
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        // The system track has its own independent tallies.
        assert!(!streak.observe_hit(TRACK_SYSTEM, identity_key(a)));
        assert!(streak.observe_hit(TRACK_MIC, identity_key(a)));
        // A different speaker starts their own fresh tally.
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        // A fully unlabeled utterance wipes the track's tallies.
        streak.observe_miss(TRACK_MIC);
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
    }

    #[test]
    fn streak_preroll_verifies_a_returning_established_speaker() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut streak = VerificationStreak::default();
        // a establishes the streak (2 hits), then b talks for a while.
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        assert!(streak.observe_hit(TRACK_MIC, identity_key(b)));
        // a returns: the first utterance back already counts as continuing
        // a's established streak — no two fresh consecutive hits needed.
        assert!(streak.observe_hit(TRACK_MIC, identity_key(a)));
    }

    #[test]
    fn interleaved_speaker_breaks_an_unestablished_run() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut streak = VerificationStreak::default();
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        // b in between: a's tally stays at 1 (the provisional-only previous
        // result), so a's return does NOT pre-roll to verified…
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(b)));
        assert!(!streak.observe_hit(TRACK_MIC, identity_key(a)));
        // …only now does a's tally reach 2, so the next hit upgrades —
        // the interleave broke the would-be consecutive run.
        assert!(streak.observe_hit(TRACK_MIC, identity_key(a)));
    }

    #[test]
    fn streak_keys_are_independent_per_attribution_source() {
        let mut streak = VerificationStreak::default();
        let session = SpeakerKey::Session("说话人1".into());
        let cluster = SpeakerKey::Cluster("说话人1".into());
        // The same display string in different sources never aliases.
        assert!(!streak.observe_hit(TRACK_MIC, session.clone()));
        assert!(!streak.observe_hit(TRACK_MIC, session.clone()));
        assert!(streak.observe_hit(TRACK_MIC, session));
        assert!(
            !streak.observe_hit(TRACK_MIC, cluster),
            "a cluster label keeps its own tally"
        );
    }

    #[test]
    fn cluster_labels_follow_the_same_streak_rule() {
        let c1 = SpeakerKey::Cluster("说话人1".into());
        let c2 = SpeakerKey::Cluster("说话人2".into());
        let mut streak = VerificationStreak::default();
        assert!(!streak.observe_hit(TRACK_MIC, c1.clone()));
        assert!(!streak.observe_hit(TRACK_MIC, c1.clone()));
        // Another cluster interleaves; 说话人1's established tally survives.
        assert!(!streak.observe_hit(TRACK_MIC, c2));
        assert!(streak.observe_hit(TRACK_MIC, c1));
    }

    #[test]
    fn retracted_or_renamed_session_name_loses_its_streak() {
        let mut streak = VerificationStreak::default();
        let name = SpeakerKey::Session("客户A".into());
        // Establish the name on both tracks.
        assert!(!streak.observe_hit(TRACK_MIC, name.clone()));
        assert!(!streak.observe_hit(TRACK_MIC, name.clone()));
        assert!(streak.observe_hit(TRACK_MIC, name.clone()));
        assert!(!streak.observe_hit(TRACK_SYSTEM, name.clone()));
        assert!(!streak.observe_hit(TRACK_SYSTEM, name.clone()));
        // The annotation is cleared: the freed name may belong to a different
        // person next time, so its tally is forgotten on every track.
        streak.forget_speaker(&name);
        assert!(!streak.observe_hit(TRACK_MIC, name.clone()));
        assert!(!streak.observe_hit(TRACK_SYSTEM, name.clone()));
        // Forgetting one speaker never touches another's tally.
        let b = identity_key(Uuid::new_v4());
        assert!(!streak.observe_hit(TRACK_MIC, b.clone()));
        assert!(!streak.observe_hit(TRACK_MIC, b.clone()));
        streak.forget_speaker(&name);
        assert!(streak.observe_hit(TRACK_MIC, b));
    }

    // ---- L3.5: session voiceprints ---------------------------------------

    /// A unit vector whose cosine similarity with [`probe`] is exactly
    /// `cosine` (2-d is enough — the session set is dimension-agnostic).
    fn toward(cosine: f32) -> Vec<f32> {
        vec![cosine, (1.0 - cosine * cosine).sqrt()]
    }

    fn probe() -> Vec<f32> {
        vec![1.0, 0.0]
    }

    /// Comfortably above both live duration floors (≥ 3 s).
    const VOICED_LONG: u64 = 5000;

    #[test]
    fn session_seed_then_match_uses_best_of_samples() {
        let mut session = SessionVoiceprints::default();
        assert!(session.by_name.is_empty());
        assert_eq!(session.match_speaker(&probe(), VOICED_LONG), None);

        session.seed("客户A", toward(0.30));
        session.seed("客户A", toward(0.90));
        let hit = session
            .match_speaker(&probe(), VOICED_LONG)
            .expect("best sample 0.90 matches");
        assert_eq!(hit.display_name, "客户A");
        assert!((hit.best_score - 0.90).abs() < 1e-3, "{hit:?}");
        // Sole group: margin uses the cosine floor (-1.0), so it verifies.
        assert!(!hit.provisional);
        assert!((hit.margin - 1.90).abs() < 1e-3, "{hit:?}");
    }

    #[test]
    fn session_thresholds_provisional_verified_and_reject() {
        let mut session = SessionVoiceprints::default();
        // 0.66: past the provisional floor (0.65), short of verified (0.72).
        session.seed("客户A", toward(0.66));
        let hit = session.match_speaker(&probe(), VOICED_LONG).unwrap();
        assert!(hit.provisional, "grey-zone score renders as 名字?");

        // 0.64: below the provisional floor → no label at all.
        let mut weak = SessionVoiceprints::default();
        weak.seed("客户A", toward(0.64));
        assert_eq!(weak.match_speaker(&probe(), VOICED_LONG), None);

        // 0.80 with a clear field → verified at ≥ 3 s.
        let mut strong = SessionVoiceprints::default();
        strong.seed("客户A", toward(0.80));
        assert!(
            !strong
                .match_speaker(&probe(), VOICED_LONG)
                .unwrap()
                .provisional
        );
    }

    #[test]
    fn session_narrow_margin_between_names_stays_provisional() {
        let mut session = SessionVoiceprints::default();
        session.seed("客户A", toward(0.80));
        session.seed("客户B", toward(0.75)); // margin 0.05 < 0.08
        let hit = session.match_speaker(&probe(), VOICED_LONG).unwrap();
        assert_eq!(hit.display_name, "客户A");
        assert!(hit.provisional, "ambiguous between two names → tentative");
        assert!((hit.margin - 0.05).abs() < 1e-3, "{hit:?}");

        // A distant runner-up restores the verified tier.
        let mut clear = SessionVoiceprints::default();
        clear.seed("客户A", toward(0.80));
        clear.seed("客户B", toward(0.20));
        assert!(
            !clear
                .match_speaker(&probe(), VOICED_LONG)
                .unwrap()
                .provisional
        );
    }

    #[test]
    fn session_rename_relabels_and_merges_voiceprints() {
        // Relabel: the accumulated voiceprint matches under the corrected name,
        // and the old name is gone (not resurfacing in future live chips).
        let mut session = SessionVoiceprints::default();
        session.seed("客户A", toward(0.90));
        assert!(session.rename("客户A", "客户甲"));
        assert_eq!(
            session
                .match_speaker(&probe(), VOICED_LONG)
                .unwrap()
                .display_name,
            "客户甲"
        );
        assert!(!session.retract("客户A"), "old name should be gone");
        assert!(
            !session.rename("不存在", "客户乙"),
            "unknown source is a no-op"
        );

        // Merge into an existing group: the target keeps matching.
        let mut merge = SessionVoiceprints::default();
        merge.seed("客户A", toward(0.90));
        merge.seed("客户B", toward(0.20));
        assert!(merge.rename("客户A", "客户B"));
        assert_eq!(
            merge
                .match_speaker(&probe(), VOICED_LONG)
                .unwrap()
                .display_name,
            "客户B"
        );
    }

    #[test]
    fn session_duration_floors_gate_the_tiers() {
        let mut session = SessionVoiceprints::default();
        session.seed("客户A", toward(0.90));
        // Under 2 s: no label however strong the score.
        assert_eq!(session.match_speaker(&probe(), 1999), None);
        // 2–3 s: provisional at most, even at verified-grade scores.
        assert!(session.match_speaker(&probe(), 2500).unwrap().provisional);
        // ≥ 3 s: verified.
        assert!(!session.match_speaker(&probe(), 3000).unwrap().provisional);
    }

    #[test]
    fn session_samples_cap_at_three_rolling_oldest_out() {
        let mut session = SessionVoiceprints::default();
        for cosine in [0.95, 0.30, 0.40, 0.50] {
            session.seed("客户A", toward(cosine));
        }
        let (_, samples) = &session.by_name[0];
        assert_eq!(samples.len(), SESSION_MAX_SAMPLES_PER_NAME);
        // The 0.95 sample (oldest) rolled out: best is now 0.50.
        let hit = session.match_speaker(&probe(), VOICED_LONG);
        assert_eq!(hit, None, "0.50 is below the provisional floor");
    }

    #[test]
    fn session_retract_removes_the_whole_name_group() {
        let mut session = SessionVoiceprints::default();
        session.seed("客户A", toward(0.90));
        session.seed("客户A", toward(0.85));
        assert!(session.retract("客户A"));
        assert!(session.by_name.is_empty());
        assert_eq!(session.match_speaker(&probe(), VOICED_LONG), None);
        // Retracting an unknown name is a no-op.
        assert!(!session.retract("客户B"));
    }

    #[test]
    fn seed_plan_skips_enrolled_identities() {
        // An enrolled person is already covered by the permanent library —
        // never duplicated into the session set, even with audio available.
        let mut window = AudioWindow::new(10);
        window.push(0.0, &[0.5; 100]); // 10 s of audio
        assert_eq!(
            plan_session_seed(Some(Uuid::new_v4()), Some(&window), 0.0, Some(5.0)),
            None
        );
        // The same span with no identity seeds normally.
        assert!(plan_session_seed(None, Some(&window), 0.0, Some(5.0)).is_some());
        // And with the verifier layer disengaged (no window), nothing seeds.
        assert_eq!(plan_session_seed(None, None, 0.0, Some(5.0)), None);
    }

    #[test]
    fn seed_plan_skips_spans_evicted_from_the_ring() {
        let rate = 100u32;
        let mut window = AudioWindow::new(rate);
        // Fill 40 s at 100 Hz — the 30 s ring keeps roughly [10, 40).
        for second in 0..40 {
            window.push(f64::from(second), &vec![0.5; rate as usize]);
        }
        // The annotated line at [2, 6] was evicted long ago → skip.
        assert_eq!(plan_session_seed(None, Some(&window), 2.0, Some(6.0)), None);
        // A recent line still in the ring seeds fine.
        let seeded = plan_session_seed(None, Some(&window), 35.0, Some(39.0)).unwrap();
        assert_eq!(seeded.len(), 4 * rate as usize);
    }

    #[test]
    fn seed_plan_enforces_minimum_and_caps_open_ended_spans() {
        let rate = 10u32;
        let mut window = AudioWindow::new(rate);
        window.push(0.0, &vec![0.5; 20 * rate as usize]); // 20 s

        // A 1 s annotated line is under the 2 s floor → skip.
        assert_eq!(plan_session_seed(None, Some(&window), 0.0, Some(1.0)), None);
        // An open-ended annotation ("此句及之后") takes at most 10 s.
        let open = plan_session_seed(None, Some(&window), 2.0, None).unwrap();
        assert_eq!(
            open.len(),
            (SESSION_SEED_MAX_SECONDS * f64::from(rate)) as usize
        );
        // A closed span is taken as-is (within the cap).
        let closed = plan_session_seed(None, Some(&window), 2.0, Some(5.0)).unwrap();
        assert_eq!(closed.len(), 3 * rate as usize);
    }

    // ---- L4: unknown-speaker session clusters ----------------------------

    #[test]
    fn cluster_same_speaker_keeps_one_stable_label() {
        let mut clusters = SessionClusters::default();
        let first = clusters.assign(&toward(0.98)).unwrap();
        assert_eq!(first.label, "说话人1");
        assert!(first.created);
        // Same voice, slightly noisy reads around the same direction: every
        // one rejoins the cluster and the label never changes.
        for cosine in [0.96, 0.90, 0.99, 0.93, 0.95] {
            let hit = clusters.assign(&toward(cosine)).unwrap();
            assert_eq!(hit.label, "说话人1", "same voice must keep its label");
            assert!(!hit.created);
        }
        assert_eq!(clusters.clusters.len(), 1);
        assert_eq!(clusters.clusters[0].count, 6);
        // The running centroid sum keeps pointing at the speaker (cosine is
        // scale-invariant, so the raw sum scores like the mean).
        let score = lumen_identity::cosine_similarity(&clusters.clusters[0].centroid, &probe());
        assert!(score > 0.9, "centroid drifted off the speaker: {score}");
    }

    #[test]
    fn two_separated_speakers_get_two_labels() {
        let mut clusters = SessionClusters::default();
        assert_eq!(clusters.assign(&[1.0, 0.0]).unwrap().label, "说话人1");
        // Orthogonal voice: cosine 0, far below the join gate.
        let second = clusters.assign(&[0.0, 1.0]).unwrap();
        assert_eq!(second.label, "说话人2");
        assert!(second.created);
        // And each voice keeps its own label afterwards.
        assert_eq!(clusters.assign(&[0.98, 0.2]).unwrap().label, "说话人1");
        assert_eq!(clusters.assign(&[0.2, 0.98]).unwrap().label, "说话人2");
        assert_eq!(clusters.clusters.len(), 2);
    }

    #[test]
    fn below_threshold_voice_founds_a_new_cluster_instead_of_merging() {
        let mut clusters = SessionClusters::default();
        clusters.assign(&probe()).unwrap(); // 说话人1 at [1, 0]
                                            // 0.70: same hemisphere but under the 0.75 join gate — treated as a
                                            // different voice, not a noisy read of the first one.
        let hit = clusters.assign(&toward(0.70)).unwrap();
        assert_eq!(hit.label, "说话人2");
        assert!(hit.created);
        assert_eq!(clusters.clusters.len(), 2);
    }

    #[test]
    fn grey_zone_between_two_clusters_founds_a_third() {
        let mut clusters = SessionClusters::default();
        clusters.assign(&probe()).unwrap(); // 说话人1 at angle 0°
        clusters.assign(&toward(0.70)).unwrap(); // 说话人2 at ≈45.6°
                                                 // toward(0.92) sits ≈23° off each — cosine ≈ 0.92 to both, so both
                                                 // clear the join threshold but with a margin far under 0.08. Joining
                                                 // either on a coin flip risks merging two people; it founds a new
                                                 // cluster instead.
        let hit = clusters.assign(&toward(0.92)).unwrap();
        assert_eq!(hit.label, "说话人3");
        assert!(hit.created);
    }

    #[test]
    fn label_numbers_are_never_reused() {
        let mut clusters = SessionClusters::default();
        let a = clusters.assign(&[1.0, 0.0]).unwrap();
        let b = clusters.assign(&[0.0, 1.0]).unwrap();
        let c = clusters.assign(&[-1.0, 0.0]).unwrap();
        assert_eq!(a.label, "说话人1");
        assert_eq!(b.label, "说话人2");
        assert_eq!(c.label, "说话人3");
        // Rejoining an existing cluster does not burn a number...
        assert_eq!(clusters.assign(&[0.99, 0.1]).unwrap().label, "说话人1");
        // ...and the next new voice continues the sequence.
        assert_eq!(clusters.assign(&[0.0, -1.0]).unwrap().label, "说话人4");
    }

    #[test]
    fn cluster_cap_stops_new_labels_but_not_joins() {
        let mut clusters = SessionClusters::default();
        // 64-d axis vectors are mutually orthogonal: each founds a cluster.
        for axis in 0..MAX_SESSION_CLUSTERS {
            let mut v = vec![0.0; 64];
            v[axis] = 1.0;
            assert!(clusters.assign(&v).unwrap().created);
        }
        // At the cap a new voice stays unlabeled rather than fragmenting on.
        let mut extra = vec![0.0; 64];
        extra[MAX_SESSION_CLUSTERS] = 1.0;
        assert_eq!(clusters.assign(&extra), None);
        // Joining an existing cluster still works at the cap.
        let mut near_first = vec![0.0; 64];
        near_first[0] = 0.99;
        near_first[1] = 0.05;
        assert_eq!(clusters.assign(&near_first).unwrap().label, "说话人1");
        assert_eq!(clusters.clusters.len(), MAX_SESSION_CLUSTERS);
    }

    #[test]
    fn degenerate_embeddings_never_found_a_cluster() {
        let mut clusters = SessionClusters::default();
        assert_eq!(clusters.assign(&[]), None);
        assert_eq!(clusters.assign(&[0.0, 0.0]), None);
        assert!(clusters.clusters.is_empty());
        assert_eq!(clusters.next_label, 0);
    }
}
