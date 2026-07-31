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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_asr::{
    default_paraformer_streaming_dir, paraformer_streaming_ready, resample_linear, LiveAudioPacket,
    StreamingRecognizer, StreamingStream,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

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
    /// Speaker attribution placeholder — never set by this layer. Kept in the
    /// contract so the payload shape stays stable if attribution is added.
    #[serde(skip_serializing_if = "Option::is_none")]
    speaker: Option<String>,
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

/// One live track inside the worker: its fan-out receiver, its decoding stream
/// on the shared recognizer, and its segment bookkeeping.
struct TrackState {
    tracker: SegmentTracker,
    feed: LiveTrackFeed,
    stream: StreamingStream,
    /// Sample-anchored unified-timeline clock for this track's packets.
    sample_clock: SampleClock,
    /// End (unified timeline) of the audio fed so far. Used as the finalized
    /// segment's `end_seconds`.
    clock: f64,
    disconnected: bool,
}

impl TrackState {
    fn new(track: &'static str, feed: LiveTrackFeed, stream: StreamingStream) -> Self {
        let sample_clock = SampleClock::new(feed.capture_rate);
        Self {
            tracker: SegmentTracker::new(track),
            feed,
            stream,
            sample_clock,
            clock: 0.0,
            disconnected: false,
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

    let mut tracks = vec![TrackState::new(TRACK_MIC, mic, recognizer.new_stream())];
    if let Some(system) = system {
        tracks.push(TrackState::new(
            TRACK_SYSTEM,
            system,
            recognizer.new_stream(),
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
                emit(&app, &meeting_id, track.tracker.track, update);
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
            emit(&app, &meeting_id, track.tracker.track, update);
        }
    }
    tracing::info!("live meeting transcript stopped");
}

fn emit(app: &AppHandle, meeting_id: &str, track: &'static str, update: SegmentUpdate) {
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
            speaker: None,
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
    }
}
