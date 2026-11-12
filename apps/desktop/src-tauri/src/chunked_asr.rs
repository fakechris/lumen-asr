//! Background chunked transcription for local batch engines (INV-1807).
//!
//! While a dictation records, this worker consumes the capture live tap,
//! resamples each chunk to 16 kHz, turns the stream into RMS
//! voice-activity frames, and runs the pure [`lumen_asr::PauseSegmenter`].
//! Every time a finished stretch of speech closes, it transcribes that
//! stretch on the dictation engine handle — the same cached instance the
//! stop path uses, so the engine's recognizer lock serialises background
//! segments with the final tail transcription exactly like `asr_prewarm`
//! does. At stop, only the samples after the segment boundary still need
//! a pass.
//!
//! Fallback contract: if any background segment failed (or the worker did
//! not finish within the grace window), [`ChunkedRun::usable`] is false
//! and the stop path transcribes the whole buffer as before.
//! Cancellation (Esc, engine switch, app quit) clears the tap slot and
//! sets the stop flag; the loop drains and exits, and the already
//! transcribed segment texts are logged so they are not silently lost.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_asr::{
    join_transcripts, resample_linear, AsrEngine, AsrRequest, LiveChunk, PauseSegmenter,
    SegmentPlannerConfig, SegmentRange,
};

/// 30 ms analysis frames at 16 kHz.
const FRAME_SAMPLES: usize = 480;
/// Voiced when the frame RMS reaches this level (matches the RMS VAD
/// default in config; deliberately independent of `vad.enabled`, which
/// gates auto-stop, not segmentation).
pub const VOICED_RMS: f32 = 0.02;
/// Grace window for the worker to hand over its outcome at stop: an
/// in-flight engine call is expected well inside this; beyond it the run
/// is reported unusable and the whole-buffer fallback runs instead of
/// blocking the stop.
const FINISH_GRACE: Duration = Duration::from_secs(90);
/// Consecutive engine failures before the worker gives up on background
/// segmentation for this recording.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// What the worker produced by the time recording stopped.
pub struct ChunkedRun {
    /// Transcript per closed segment, in order; `Err` marks a segment the
    /// engine failed on (which makes the whole run unusable → fallback).
    pub segments: Vec<Result<String, String>>,
    /// Sample index (16 kHz, session-aligned from capture start) closing
    /// the last planned segment — the un-transcribed tail starts here.
    pub boundary_samples: usize,
    /// Wall-clock ms each background segment took (diagnostics).
    pub segment_ms: Vec<u64>,
    /// The capture tap dropped packets during this recording; the boundary
    /// cannot be trusted and the stop path falls back.
    pub tap_dropped: bool,
}

impl ChunkedRun {
    pub fn usable(&self) -> bool {
        !self.tap_dropped && !self.segments.is_empty() && self.segments.iter().all(|s| s.is_ok())
    }

    pub fn joined(&self) -> String {
        let parts: Vec<&str> = self
            .segments
            .iter()
            .map(|s| s.as_ref().map(String::as_str).unwrap_or(""))
            .collect();
        join_transcripts(&parts)
    }
}

struct Outcome {
    segments: Vec<Result<String, String>>,
    segment_ms: Vec<u64>,
    boundary: usize,
    /// True when the tap dropped packets (total received audio is far
    /// less than wall-clock would produce); the run falls back.
    tap_dropped: bool,
}

pub struct ChunkedWorker {
    stop: Arc<AtomicBool>,
    outcome_rx: Receiver<Outcome>,
    engine_kind: lumen_asr::EngineKind,
}

/// Spawn the worker for a recording. `rx` is the receiver half of the
/// capture live tap (the caller installed the sender); the worker exits
/// when the channel closes or the stop flag is set, and sends its outcome
/// exactly once.
pub fn spawn_chunked_worker(
    engine_kind: lumen_asr::EngineKind,
    engine: Arc<dyn AsrEngine>,
    rx: Receiver<LiveChunk>,
    planner_cfg: SegmentPlannerConfig,
) -> ChunkedWorker {
    let stop = Arc::new(AtomicBool::new(false));
    let (outcome_tx, outcome_rx) = channel::<Outcome>();
    let worker_stop = Arc::clone(&stop);
    let _ = std::thread::Builder::new()
        .name("lumen-chunked-asr".into())
        .spawn(move || {
            let outcome = run_loop(engine, rx, planner_cfg, worker_stop);
            // Prepare the cancel-path log first: `send` moves the outcome.
            let dropped_text = {
                let parts: Vec<&str> = outcome
                    .segments
                    .iter()
                    .map(|s| s.as_ref().map(String::as_str).unwrap_or(""))
                    .collect();
                join_transcripts(&parts)
            };
            if outcome_tx.send(outcome).is_err() && !dropped_text.is_empty() {
                // Nobody waits (cancel path): log what was transcribed so
                // the text is not silently lost.
                tracing::info!(%dropped_text, "chunked worker dropped with transcribed segments");
            }
        });
    ChunkedWorker {
        stop,
        outcome_rx,
        engine_kind,
    }
}

fn run_loop(
    engine: Arc<dyn AsrEngine>,
    rx: Receiver<LiveChunk>,
    planner_cfg: SegmentPlannerConfig,
    stop: Arc<AtomicBool>,
) -> Outcome {
    let mut buffer: Vec<f32> = Vec::new();
    let mut planner = PauseSegmenter::new(planner_cfg);
    let mut frame: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);
    let mut segments: Vec<Result<String, String>> = Vec::new();
    let mut segment_ms: Vec<u64> = Vec::new();
    let mut consecutive_errors = 0u32;
    let started_at = Instant::now();
    let mut total_samples_received: usize = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            // Drain any chunks still queued from the capture tap before
            // computing the final boundary: audio that arrived while a
            // segment was transcribing belongs to this recording.
            while let Ok(chunk) = rx.try_recv() {
                let chunk_16k = if chunk.sample_rate == 16_000 {
                    chunk.samples
                } else {
                    resample_linear(&chunk.samples, chunk.sample_rate, 16_000)
                };
                buffer.extend(chunk_16k.iter().copied());
                for sample in chunk_16k {
                    frame.push(sample);
                    if frame.len() == FRAME_SAMPLES {
                        let voiced = rms_of(&frame) >= VOICED_RMS;
                        frame.clear();
                        for range in planner.push_frame(voiced, FRAME_SAMPLES) {
                            let _ = transcribe_range(
                                &engine,
                                &buffer,
                                range,
                                &mut segments,
                                &mut segment_ms,
                            );
                        }
                    }
                }
            }
            break;
        }
        let chunk = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let chunk_16k = if chunk.sample_rate == 16_000 {
            chunk.samples
        } else {
            resample_linear(&chunk.samples, chunk.sample_rate, 16_000)
        };
        // Ranges index the cumulative buffer, so the buffer must contain
        // the frame that closes a segment before that segment transcribes.
        buffer.extend(chunk_16k.iter().copied());
        total_samples_received += chunk_16k.len();
        for sample in chunk_16k {
            frame.push(sample);
            if frame.len() == FRAME_SAMPLES {
                let voiced = rms_of(&frame) >= VOICED_RMS;
                frame.clear();
                for range in planner.push_frame(voiced, FRAME_SAMPLES) {
                    match transcribe_range(&engine, &buffer, range, &mut segments, &mut segment_ms)
                    {
                        Ok(()) => consecutive_errors = 0,
                        Err(error) => {
                            tracing::warn!(%error, "chunked segment transcription failed");
                            consecutive_errors += 1;
                            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                tracing::warn!(
                                    "chunked transcription giving up after repeated errors"
                                );
                                stop.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }
    }

    let elapsed_s = started_at.elapsed().as_secs_f64();
    let expected_samples = (elapsed_s * 16_000.0) as usize;
    let tap_dropped = expected_samples > 0 && total_samples_received < expected_samples * 8 / 10;

    Outcome {
        segments,
        segment_ms,
        boundary: planner.boundary(),
        tap_dropped,
    }
}

fn transcribe_range(
    engine: &Arc<dyn AsrEngine>,
    buffer: &[f32],
    range: SegmentRange,
    segments: &mut Vec<Result<String, String>>,
    segment_ms: &mut Vec<u64>,
) -> Result<(), String> {
    let started = Instant::now();
    let samples = buffer.get(range.start..range.end).ok_or_else(|| {
        format!(
            "segment range {}..{} outside buffer of {} samples",
            range.start,
            range.end,
            buffer.len()
        )
    })?;
    let result = tauri::async_runtime::block_on(
        engine.transcribe(AsrRequest::new(samples.to_vec(), 16_000)),
    );
    segment_ms.push(started.elapsed().as_millis() as u64);
    match result {
        Ok(result) => {
            segments.push(Ok(result.text));
            Ok(())
        }
        Err(error) => {
            segments.push(Err(error.to_string()));
            Err(error.to_string())
        }
    }
}

impl ChunkedWorker {
    /// Engine identity this worker was spawned for; the stop path only
    /// consumes the run when it still matches the selected engine.
    pub fn engine_kind(&self) -> lumen_asr::EngineKind {
        self.engine_kind
    }

    /// Signal the loop to stop, wait up to the grace window for the
    /// outcome, and take it. The caller owns the tap slot and clears it
    /// separately. A timed-out or channel-dropped handover yields an
    /// unusable (empty) run → whole-buffer fallback.
    pub fn finish(self) -> ChunkedRun {
        self.stop.store(true, Ordering::Relaxed);
        let outcome = match self.outcome_rx.recv_timeout(FINISH_GRACE) {
            Ok(outcome) => Some(outcome),
            Err(error) => {
                tracing::warn!(%error, "chunked worker outcome not received in time");
                None
            }
        };
        match outcome {
            Some(Outcome {
                segments,
                segment_ms,
                boundary,
                tap_dropped,
            }) => ChunkedRun {
                segments,
                boundary_samples: boundary,
                segment_ms,
                tap_dropped,
            },
            None => ChunkedRun {
                segments: Vec::new(),
                boundary_samples: 0,
                segment_ms: Vec::new(),
                tap_dropped: false,
            },
        }
    }
}

impl Drop for ChunkedWorker {
    fn drop(&mut self) {
        // Deterministic stop even when nobody calls `finish` (cancel,
        // engine switch): the loop exits after any in-flight engine call.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Test seam: push a synthetic 16 kHz chunk stream through the same
/// framing/classification the worker uses and report the closed ranges.
/// Mirrors `run_loop`'s per-frame decisions without an engine.
#[cfg(test)]
pub(crate) fn plan_ranges_for_test(
    chunks: &[Vec<f32>],
    cfg: SegmentPlannerConfig,
    voiced_rms: f32,
) -> (Vec<SegmentRange>, usize) {
    let mut planner = PauseSegmenter::new(cfg);
    let mut ranges = Vec::new();
    let mut frame: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);
    for chunk in chunks {
        for sample in chunk.iter().copied() {
            frame.push(sample);
            if frame.len() == FRAME_SAMPLES {
                let voiced = rms_of(&frame) >= voiced_rms;
                frame.clear();
                ranges.extend(planner.push_frame(voiced, FRAME_SAMPLES));
            }
        }
    }
    (ranges, planner.boundary())
}

/// Bounded sender wrapper used by tests to emulate the capture tap.
#[cfg(test)]
pub(crate) fn test_tap(
    capacity: usize,
) -> (std::sync::mpsc::SyncSender<LiveChunk>, Receiver<LiveChunk>) {
    std::sync::mpsc::sync_channel(capacity)
}

/// True when a short tail (below the 250 ms threshold) still carries
/// voiced audio: the recording stopped shortly after a hard-cap split
/// and the remaining speech must not be discarded.
pub fn tail_has_speech(tail: &[f32]) -> bool {
    if tail.is_empty() {
        return false;
    }
    // Check 30 ms frames; any voiced frame means speech is present.
    for frame in tail.chunks(FRAME_SAMPLES) {
        if rms_of(frame) >= VOICED_RMS {
            return true;
        }
    }
    false
}

fn rms_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// One whole-buffer vs chunked comparison over a fixed 16 kHz buffer
/// (INV-1807 acceptance measurement). `stop_wait_ms` is what the user
/// waits after the last sample: transcribing the tail plus joining —
/// background segments are assumed to have completed during speech, which
/// the reported per-segment durations vs their speech lengths verify
/// (segment RTF < 1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkedBench {
    pub duration_s: f64,
    pub whole_ms: u64,
    pub whole_text: String,
    pub segments: usize,
    pub seg_ms: Vec<u64>,
    pub stop_wait_ms: u64,
    pub chunked_text: String,
}

/// Run the comparison on `samples_16k` with `engine`. Blocking; intended
/// for the env-gated acceptance bench (`chunked_long_bench`) and offline
/// measurement, never the dictation path.
#[allow(dead_code)]
pub fn measure_chunked(
    engine: Arc<dyn AsrEngine>,
    samples_16k: &[f32],
    cfg: SegmentPlannerConfig,
) -> Result<ChunkedBench, String> {
    let duration_s = samples_16k.len() as f64 / 16_000.0;

    let whole_started = Instant::now();
    let whole = tauri::async_runtime::block_on(
        engine.transcribe(AsrRequest::new(samples_16k.to_vec(), 16_000)),
    )
    .map_err(|e| e.to_string())?;
    let whole_ms = whole_started.elapsed().as_millis() as u64;

    // Simulated stream: classify frames exactly like the worker, closing
    // and transcribing segments as they complete during the feed.
    let mut planner = PauseSegmenter::new(cfg);
    let mut texts: Vec<String> = Vec::new();
    let mut seg_ms: Vec<u64> = Vec::new();
    for frame in samples_16k.chunks(FRAME_SAMPLES) {
        let voiced = rms_of(frame) >= VOICED_RMS;
        for range in planner.push_frame(voiced, frame.len()) {
            let segment = &samples_16k[range.start..range.end];
            let started = Instant::now();
            let text = tauri::async_runtime::block_on(
                engine.transcribe(AsrRequest::new(segment.to_vec(), 16_000)),
            )
            .map_err(|e| e.to_string())?
            .text;
            seg_ms.push(started.elapsed().as_millis() as u64);
            texts.push(text);
        }
    }

    let boundary = planner.boundary().min(samples_16k.len());
    let stop_started = Instant::now();
    let tail = &samples_16k[boundary..];
    if tail.len() >= 16_000 / 4 {
        let tail_text = tauri::async_runtime::block_on(
            engine.transcribe(AsrRequest::new(tail.to_vec(), 16_000)),
        )
        .map_err(|e| e.to_string())?
        .text;
        texts.push(tail_text);
    }
    let part_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let chunked_text = join_transcripts(&part_refs);
    let stop_wait_ms = stop_started.elapsed().as_millis() as u64;

    Ok(ChunkedBench {
        duration_s,
        whole_ms,
        whole_text: whole.text,
        segments: seg_ms.len(),
        seg_ms,
        stop_wait_ms,
        chunked_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_asr::StubAsr;

    fn speech_seconds(s: f64) -> Vec<f32> {
        vec![0.2; (s * 16_000.0) as usize]
    }

    fn silence_seconds(s: f64) -> Vec<f32> {
        vec![0.0; (s * 16_000.0) as usize]
    }

    /// One 2 s speech stretch, 0.9 s silence, another 1.5 s stretch.
    fn chunk_stream() -> Vec<Vec<f32>> {
        vec![
            speech_seconds(2.0),
            silence_seconds(0.9),
            speech_seconds(1.5),
            silence_seconds(0.9),
        ]
    }

    #[test]
    fn worker_transcribes_segments_and_reports_boundary() {
        let (tx, rx) = test_tap(64);
        let engine: Arc<dyn AsrEngine> = Arc::new(StubAsr::new("段"));
        let worker = spawn_chunked_worker(
            lumen_asr::EngineKind::SenseVoice,
            engine,
            rx,
            SegmentPlannerConfig::default(),
        );
        for chunk in chunk_stream() {
            tx.send(LiveChunk {
                samples: chunk,
                sample_rate: 16_000,
            })
            .unwrap();
        }
        // Give the loop time to classify frames and run the stub engine.
        std::thread::sleep(Duration::from_millis(600));
        let run = worker.finish();
        assert!(run.usable(), "{:?}", run.segments);
        assert_eq!(run.segments.len(), 2, "two speech stretches");
        assert_eq!(run.joined(), "段段");
        // Boundary closes the second segment; the buffer holds one extra
        // silence flush beyond it.
        // 2.0 speech + 0.9 silence + 1.5 speech + 0.9 silence = 5.3 s of stream.
        let total = (5.3 * 16_000.0) as usize;
        assert!(run.boundary_samples > 0 && run.boundary_samples < total);
        assert!(!run.segment_ms.is_empty());
    }

    #[test]
    fn cancel_before_any_segment_yields_unusable_empty_run() {
        let (tx, rx) = test_tap(64);
        let engine: Arc<dyn AsrEngine> = Arc::new(StubAsr::new("段"));
        let worker = spawn_chunked_worker(
            lumen_asr::EngineKind::SenseVoice,
            engine,
            rx,
            SegmentPlannerConfig::default(),
        );
        // Only silence so far — no segment can close.
        tx.send(LiveChunk {
            samples: silence_seconds(0.5),
            sample_rate: 16_000,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let run = worker.finish();
        assert!(!run.usable());
        assert_eq!(run.boundary_samples, 0);
        drop(tx);
    }

    #[test]
    fn dropping_the_tap_ends_the_loop_without_hanging() {
        let (tx, rx) = test_tap(4);
        let engine: Arc<dyn AsrEngine> = Arc::new(StubAsr::new("段"));
        let worker = spawn_chunked_worker(
            lumen_asr::EngineKind::SenseVoice,
            engine,
            rx,
            SegmentPlannerConfig::default(),
        );
        tx.send(LiveChunk {
            samples: speech_seconds(0.1),
            sample_rate: 16_000,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(tx); // capture slot cleared — the loop must exit on disconnect
        let started = Instant::now();
        let run = worker.finish();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!run.usable());
    }

    /// INV-1807 acceptance measurement. Real engine + real long audio:
    /// ```
    /// LUMEN_CHUNKED_BENCH_WAV=/tmp/lumen-eval-clips/long-mix-zh.wav \
    /// cargo test -p lumen-asr-desktop chunked_long_bench -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "set LUMEN_CHUNKED_BENCH_WAV (and optional LUMEN_CHUNKED_BENCH_ENGINE=qwen|sensevoice)"]
    fn chunked_long_bench() {
        let wav_path = match std::env::var("LUMEN_CHUNKED_BENCH_WAV") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => panic!("LUMEN_CHUNKED_BENCH_WAV not set"),
        };
        let engine_kind =
            std::env::var("LUMEN_CHUNKED_BENCH_ENGINE").unwrap_or_else(|_| "qwen".into());
        let samples = read_bench_wav_16k(std::path::Path::new(&wav_path)).expect("read bench wav");
        let engine: Arc<dyn AsrEngine> = match engine_kind.as_str() {
            "sensevoice" => {
                let dir = lumen_asr::resolve_sensevoice_dir(None).expect("sensevoice model dir");
                Arc::new(lumen_asr::SenseVoiceSherpaAsr::new(dir).with_language("zh"))
            }
            _ => {
                let dir = lumen_asr::resolve_qwen_asr_dir(None).expect("qwen model dir");
                Arc::new(lumen_asr::QwenAsr::new(lumen_asr::QwenAsrConfig::product(
                    dir,
                    None,
                    Duration::from_secs(600),
                )))
            }
        };
        let bench =
            measure_chunked(engine, &samples, SegmentPlannerConfig::default()).expect("bench runs");
        println!(
            "CHUNKED_BENCH {}",
            serde_json::to_string_pretty(&bench).unwrap()
        );
        // Long-buffer contract: the tail-only wait must beat the whole-
        // buffer pass, and per-segment work must fit inside its speech
        // window (RTF < 1) for the background overlap to hold.
        assert!(!bench.chunked_text.trim().is_empty(), "chunked text empty");
        assert!(
            bench.stop_wait_ms < bench.whole_ms,
            "stop_wait {}ms not < whole {}ms",
            bench.stop_wait_ms,
            bench.whole_ms
        );
    }

    /// Minimal 16-bit PCM mono WAV reader for the bench (test-only).
    fn read_bench_wav_16k(path: &std::path::Path) -> Result<Vec<f32>, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err("not RIFF/WAVE".into());
        }
        let mut rate = 0u32;
        let mut data: Option<(usize, usize)> = None;
        let mut pos = 12;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let body = pos + 8;
            if id == b"fmt " && size >= 16 {
                rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            } else if id == b"data" {
                data = Some((body, size.min(bytes.len() - body)));
            }
            pos = body + size + (size & 1);
        }
        let (offset, len) = data.ok_or("no data chunk")?;
        let mut samples = Vec::with_capacity(len / 2);
        for chunk in bytes[offset..offset + len].chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32_768.0);
        }
        Ok(if rate == 16_000 {
            samples
        } else {
            resample_linear(&samples, rate, 16_000)
        })
    }

    #[test]
    fn plan_ranges_match_the_planner_on_synthetic_streams() {
        let (ranges, boundary) =
            plan_ranges_for_test(&chunk_stream(), SegmentPlannerConfig::default(), VOICED_RMS);
        assert_eq!(ranges.len(), 2);
        assert!(boundary >= ranges[1].end);
        // Pre-roll is included in the first segment.
        assert!(ranges[0].start < (0.25 * 16_000.0) as usize);
    }
}
