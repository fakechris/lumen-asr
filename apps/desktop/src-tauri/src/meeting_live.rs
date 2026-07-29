//! Real-time meeting layer (P3): a rolling live transcript while recording.
//!
//! This is the "record-time" half of the two-layer meeting architecture
//! (`docs/MEETING.md` M6/P3):
//!
//! - **While recording** — a background worker consumes the recorder's audio
//!   fan-out ([`lumen_asr::SampleSink`]), feeds it to a **streaming Paraformer**
//!   recognizer, and emits a rolling partial transcript (no speaker labels) to
//!   the UI via the `meeting-live-transcript` Tauri event. This kills the
//!   "black box" feeling: the user sees words appear as they speak.
//! - **After stop** — the existing offline pipeline
//!   ([`lumen_meeting::process_meeting`]) re-transcribes with diarization and
//!   word timestamps and produces the authoritative, speaker-attributed
//!   transcript that *replaces* this live preview. The live text is never
//!   persisted; it is a transient recording-time affordance only.
//!
//! ## Gating & graceful degradation
//! The worker is only spawned on **macOS** *and* when the streaming Paraformer
//! model is installed ([`streaming_dir_if_ready`]). On any other platform, or
//! with no model, nothing is spawned and no events are emitted — the recording,
//! WAV write, and offline pipeline are completely unaffected. The audio fan-out
//! channel itself is cross-platform and harmless.
//!
//! ## Threading (`!Send` sherpa recognizer)
//! The streaming recognizer wraps a sherpa-onnx `OnlineRecognizer`; like the
//! offline `process_meeting` path (M4a-2) we keep it on a **dedicated
//! `std::thread`** rather than a Tauri async task. The recognizer is created
//! *inside* the worker thread and never crosses a thread boundary, so there is
//! no `Send`/async-executor friction. The worker is a plain synchronous loop —
//! no Tokio runtime needed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_asr::{
    default_paraformer_streaming_dir, paraformer_streaming_ready, resample_linear,
    StreamingAsrEngine, StreamingParaformerAsr,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Sample rate the streaming Paraformer model expects. The recorder captures at
/// the device-native rate, so chunks are resampled to this before decoding.
const STREAMING_TARGET_RATE: u32 = 16_000;

/// How long the worker blocks waiting for the next audio chunk before checking
/// the stop flag. Small enough that stop is responsive, large enough to avoid a
/// busy loop.
const RECV_POLL: Duration = Duration::from_millis(250);

/// Payload of the `meeting-live-transcript` event. `seq` identifies the current
/// utterance/segment: partials share the `seq` of the in-progress segment and a
/// `is_final: true` event closes it, after which `seq` advances.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveTranscriptEvent {
    /// Rolling text for the current segment (partial) or the committed segment.
    text: String,
    /// `true` once the segment is finalized (endpoint reached); the UI fixes it
    /// and starts a fresh line for the next `seq`.
    is_final: bool,
    /// Monotonic segment index within this recording.
    seq: u32,
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
    /// Spawn the streaming worker for a new recording. Consumes the fan-out
    /// receiver; `capture_rate` is the recorder's native sample rate;
    /// `streaming_dir` is the (already-validated) model directory. Any previous
    /// worker is stopped first.
    pub fn start(
        &self,
        app: AppHandle,
        sample_rx: Receiver<Vec<f32>>,
        capture_rate: u32,
        streaming_dir: PathBuf,
    ) {
        // Defensive: never leave a prior worker running.
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("lumen-meeting-live".into())
            .spawn(move || {
                run_worker(app, sample_rx, capture_rate, streaming_dir, stop_worker);
            })
            .expect("spawn meeting live worker thread");
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(Worker { stop, handle });
        }
    }

    /// Stop the active worker (if any) and wait for it to drain the last
    /// segment. Called from `stop_meeting_recording` after the recorder has
    /// been stopped (which drops the fan-out sender and naturally ends the
    /// worker loop); the explicit stop flag makes teardown prompt regardless.
    pub fn stop(&self) {
        let worker = self.inner.lock().ok().and_then(|mut g| g.take());
        if let Some(worker) = worker {
            worker.stop.store(true, Ordering::SeqCst);
            let _ = worker.handle.join();
        }
    }
}

/// The worker body: build the recognizer, then loop over fan-out chunks emitting
/// rolling partials and finalized segments. Exits when stopped or the recorder's
/// sender is dropped (recording ended), flushing any trailing text.
fn run_worker(
    app: AppHandle,
    sample_rx: Receiver<Vec<f32>>,
    capture_rate: u32,
    streaming_dir: PathBuf,
    stop: Arc<AtomicBool>,
) {
    // Building the recognizer can fail (missing/corrupt model, non-`sherpa`
    // build). That must never break the recording: log and return, so the UI
    // simply shows no live text and the offline pipeline still runs at stop.
    let mut asr = match StreamingParaformerAsr::from_dir(&streaming_dir) {
        Ok(asr) => asr,
        Err(e) => {
            tracing::warn!(
                dir = %streaming_dir.display(),
                error = %e,
                "live meeting transcript disabled: could not load streaming Paraformer"
            );
            return;
        }
    };

    tracing::info!(
        dir = %streaming_dir.display(),
        capture_rate,
        "live meeting transcript started (streaming Paraformer)"
    );

    let mut seq: u32 = 0;
    let mut last_partial = String::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match sample_rx.recv_timeout(RECV_POLL) {
            Ok(chunk) => {
                let samples = if capture_rate == STREAMING_TARGET_RATE {
                    chunk
                } else {
                    resample_linear(&chunk, capture_rate, STREAMING_TARGET_RATE)
                };
                asr.accept_waveform(&samples, STREAMING_TARGET_RATE);
                asr.decode();

                if asr.is_endpoint() {
                    // Commit the finished utterance and move to the next segment.
                    let text = asr.result().text;
                    if !text.trim().is_empty() {
                        emit(&app, &text, true, seq);
                        seq = seq.wrapping_add(1);
                    }
                    asr.reset();
                    last_partial.clear();
                } else {
                    // Rolling partial: only emit when the text actually changed,
                    // so the UI is not spammed with identical frames.
                    let partial = asr.partial_text();
                    if partial != last_partial {
                        last_partial = partial.clone();
                        if !partial.trim().is_empty() {
                            emit(&app, &partial, false, seq);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            // Sender dropped: the recorder stopped. Break and flush below.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Flush any trailing context so the last segment is not lost mid-word.
    asr.input_finished();
    asr.decode();
    let tail = asr.result().text;
    if !tail.trim().is_empty() {
        emit(&app, &tail, true, seq);
    }
    tracing::info!("live meeting transcript stopped");
}

fn emit(app: &AppHandle, text: &str, is_final: bool, seq: u32) {
    let _ = app.emit(
        "meeting-live-transcript",
        LiveTranscriptEvent {
            text: text.to_string(),
            is_final,
            seq,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_camel_case() {
        let ev = LiveTranscriptEvent {
            text: "你好".into(),
            is_final: true,
            seq: 3,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"isFinal\":true"));
        assert!(json.contains("\"seq\":3"));
        assert!(json.contains("\"text\":\"你好\""));
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
