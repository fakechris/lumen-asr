//! Continuous meeting recorder — an **independent** capture path that never
//! touches the dictation `AudioCapture` (hold-to-talk, single in-memory buffer).
//!
//! Meetings are long (30–60 min+), so we must not keep the whole take in RAM.
//! Samples are **streamed incrementally to a WAV file** as they arrive, so
//! memory stays bounded regardless of duration.
//!
//! Threading mirrors `audio.rs`: `cpal::Stream` is `!Send` on macOS, so the
//! stream lives on a dedicated control thread and `MeetingRecorder` only holds
//! Send/Sync control handles. A second, per-session writer thread owns the
//! [`WavSink`] and does the file I/O off the real-time audio callback (the
//! callback only down-mixes to mono and forwards a chunk over a channel).
//!
//! As in `audio.rs`, CoreAudio input callbacks can keep firing briefly after a
//! `cpal::Stream` is dropped; a per-session `epoch` guards against those
//! "zombie" callbacks polluting the next recording.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeetingRecorderError {
    #[error("no input device")]
    NoDevice,
    #[error("already recording")]
    AlreadyRecording,
    #[error("not recording")]
    NotRecording,
    #[error("device error: {0}")]
    Device(String),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("audio thread unavailable")]
    ThreadGone,
}

/// Result of a finished recording.
#[derive(Debug, Clone)]
pub struct RecordingSummary {
    /// Path of the finalized WAV file.
    pub wav_path: PathBuf,
    /// Total recorded audio length in seconds (excludes paused gaps).
    pub duration_seconds: f64,
    /// Native capture sample rate written to the file.
    pub sample_rate: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// WavSink — streaming PCM16 mono WAV writer with length back-fill.
//
// Fully decoupled from cpal so it can be unit-tested by feeding synthetic
// sample chunks and asserting the resulting header/body.
// ─────────────────────────────────────────────────────────────────────────────

const WAV_HEADER_LEN: u64 = 44;

/// Incremental PCM16 mono WAV writer.
///
/// On [`create`](WavSink::create) a 44-byte header is written with placeholder
/// (zero) lengths. Each [`write_samples`](WavSink::write_samples) appends
/// little-endian `i16` PCM. [`finalize`](WavSink::finalize) seeks back and
/// patches the RIFF and `data` chunk sizes, so a take of any length is written
/// without ever holding the whole thing in memory.
pub struct WavSink {
    writer: BufWriter<File>,
    sample_rate: u32,
    samples_written: u64,
}

impl WavSink {
    /// Create the file and write a placeholder header (lengths back-filled on
    /// [`finalize`](WavSink::finalize)). Mono, 16-bit PCM.
    pub fn create(path: impl AsRef<Path>, sample_rate: u32) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        write_placeholder_header(&mut writer, sample_rate)?;
        Ok(Self {
            writer,
            sample_rate,
            samples_written: 0,
        })
    }

    /// Append mono `f32` samples in `[-1, 1]` as little-endian `i16` PCM.
    pub fn write_samples(&mut self, samples: &[f32]) -> io::Result<()> {
        for &s in samples {
            self.writer.write_all(&f32_to_i16(s).to_le_bytes())?;
        }
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    /// Number of mono samples written so far.
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// Flush, patch the RIFF/`data` sizes, and return the total sample count.
    pub fn finalize(mut self) -> io::Result<u64> {
        self.writer.flush()?;
        let data_bytes = self.samples_written.saturating_mul(2);
        let riff_size = (WAV_HEADER_LEN - 8).saturating_add(data_bytes);

        // RIFF chunk size at offset 4.
        self.writer.seek(SeekFrom::Start(4))?;
        self.writer.write_all(&(riff_size as u32).to_le_bytes())?;
        // data chunk size at offset 40.
        self.writer.seek(SeekFrom::Start(40))?;
        self.writer.write_all(&(data_bytes as u32).to_le_bytes())?;
        self.writer.flush()?;
        Ok(self.samples_written)
    }

    /// Sample rate this sink writes into the header.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn write_placeholder_header<W: Write>(w: &mut W, sample_rate: u32) -> io::Result<()> {
    let channels: u16 = 1;
    let bits: u16 = 16;
    let block_align: u16 = channels * (bits / 8);
    let byte_rate: u32 = sample_rate * u32::from(block_align);

    w.write_all(b"RIFF")?;
    w.write_all(&0u32.to_le_bytes())?; // patched on finalize
    w.write_all(b"WAVE")?;

    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;

    w.write_all(b"data")?;
    w.write_all(&0u32.to_le_bytes())?; // patched on finalize
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Writer thread — owns a WavSink, drains sample chunks off the audio callback.
// ─────────────────────────────────────────────────────────────────────────────

enum WriterMsg {
    Chunk(Vec<f32>),
    Finalize(Sender<io::Result<u64>>),
}

fn spawn_writer(mut sink: WavSink) -> (Sender<WriterMsg>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<WriterMsg>();
    let handle = thread::Builder::new()
        .name("lumen-meeting-wav".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    WriterMsg::Chunk(chunk) => {
                        if let Err(e) = sink.write_samples(&chunk) {
                            tracing::warn!(error = %e, "meeting wav chunk write failed");
                        }
                    }
                    WriterMsg::Finalize(reply) => {
                        let _ = reply.send(sink.finalize());
                        return;
                    }
                }
            }
            // Sender dropped without an explicit finalize; best-effort flush.
            let _ = sink.finalize();
        })
        .expect("spawn meeting wav writer thread");
    (tx, handle)
}

// ─────────────────────────────────────────────────────────────────────────────
// MeetingRecorder — cross-platform (no cfg gate); control handles only.
// ─────────────────────────────────────────────────────────────────────────────

/// A subscriber that receives the same mono `f32` sample chunks the WAV writer
/// gets, at the **native capture sample rate**. Used by the real-time meeting
/// layer (streaming Paraformer) to consume audio while it is still being
/// recorded (see `docs/MEETING.md` M6/P3). When no sink is attached the audio
/// callback does zero extra work.
pub type SampleSink = Sender<Vec<f32>>;

enum RecCmd {
    Start {
        device: Option<String>,
        out_path: PathBuf,
        sample_sink: Option<SampleSink>,
        reply: Sender<Result<u32, MeetingRecorderError>>,
    },
    Pause,
    Resume,
    Stop {
        reply: Sender<Result<RecordingSummary, MeetingRecorderError>>,
    },
}

/// Continuous microphone recorder for meetings (Send + Sync).
pub struct MeetingRecorder {
    recording: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    cmd_tx: Mutex<Option<Sender<RecCmd>>>,
    /// Join handle for the control thread. Kept so [`Drop`] can wait for the
    /// thread's teardown (WAV finalize + writer join) to finish on shutdown.
    control_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Default for MeetingRecorder {
    fn default() -> Self {
        Self::new()
    }
}

struct Session {
    stream: cpal::Stream,
    writer_tx: Sender<WriterMsg>,
    writer_handle: JoinHandle<()>,
    out_path: PathBuf,
    sample_rate: u32,
}

impl MeetingRecorder {
    pub fn new() -> Self {
        let recording = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<RecCmd>();

        let rec_flag = Arc::clone(&recording);
        let paused_flag = Arc::clone(&paused);
        let epoch = Arc::new(AtomicU64::new(0));
        let sample_rate_atom = Arc::new(AtomicU32::new(0));

        let control_handle = thread::Builder::new()
            .name("lumen-meeting-rec".into())
            .spawn(move || {
                // Stream (and its per-session writer handle) live on this thread.
                let mut session: Option<Session> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        RecCmd::Start {
                            device,
                            out_path,
                            sample_sink,
                            reply,
                        } => {
                            let res = start_on_thread(
                                device,
                                out_path,
                                sample_sink,
                                &rec_flag,
                                &paused_flag,
                                &epoch,
                                &sample_rate_atom,
                                &mut session,
                            );
                            let _ = reply.send(res);
                        }
                        RecCmd::Pause => {
                            paused_flag.store(true, Ordering::SeqCst);
                        }
                        RecCmd::Resume => {
                            paused_flag.store(false, Ordering::SeqCst);
                        }
                        RecCmd::Stop { reply } => {
                            let res = stop_on_thread(&rec_flag, &paused_flag, &epoch, &mut session);
                            let _ = reply.send(res);
                        }
                    }
                }
                // The command channel closed — the `MeetingRecorder` was dropped
                // (e.g. app shutdown) while a recording may still be live. Finalize
                // it: invalidate zombie CoreAudio callbacks, stop the stream, and
                // finalize+join the writer so the WAV footer is back-filled instead
                // of leaving a truncated, header-only file.
                epoch.fetch_add(1, Ordering::SeqCst);
                teardown_session(&mut session);
                rec_flag.store(false, Ordering::SeqCst);
            })
            .expect("spawn meeting recorder thread");

        Self {
            recording,
            paused,
            cmd_tx: Mutex::new(Some(tx)),
            control_handle: Mutex::new(Some(control_handle)),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Begin a continuous recording into `out_path`. Returns the native sample
    /// rate. Fails if a recording is already in flight.
    pub fn start(
        &self,
        device: Option<String>,
        out_path: PathBuf,
    ) -> Result<u32, MeetingRecorderError> {
        self.start_with_sink(device, out_path, None)
    }

    /// Like [`start`](Self::start), but also fans each captured mono chunk out
    /// to `sample_sink` (at the native capture sample rate) in addition to
    /// writing the WAV. This powers the real-time meeting layer (streaming
    /// Paraformer) without disturbing the WAV write / pause / Drop teardown.
    /// Passing `None` is byte-for-byte equivalent to [`start`](Self::start)
    /// (the audio callback does no extra work).
    pub fn start_with_sink(
        &self,
        device: Option<String>,
        out_path: PathBuf,
        sample_sink: Option<SampleSink>,
    ) -> Result<u32, MeetingRecorderError> {
        if self.recording.load(Ordering::SeqCst) {
            return Err(MeetingRecorderError::AlreadyRecording);
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let tx = self
            .cmd_tx
            .lock()
            .clone()
            .ok_or(MeetingRecorderError::ThreadGone)?;
        tx.send(RecCmd::Start {
            device,
            out_path,
            sample_sink,
            reply: reply_tx,
        })
        .map_err(|_| MeetingRecorderError::ThreadGone)?;
        reply_rx
            .recv()
            .map_err(|_| MeetingRecorderError::ThreadGone)?
    }

    /// Stop and drop samples until [`resume`](Self::resume). Paused gaps are not
    /// written, so the output has no silent padding for the paused interval.
    pub fn pause(&self) -> Result<(), MeetingRecorderError> {
        if !self.recording.load(Ordering::SeqCst) {
            return Err(MeetingRecorderError::NotRecording);
        }
        let tx = self
            .cmd_tx
            .lock()
            .clone()
            .ok_or(MeetingRecorderError::ThreadGone)?;
        tx.send(RecCmd::Pause)
            .map_err(|_| MeetingRecorderError::ThreadGone)
    }

    pub fn resume(&self) -> Result<(), MeetingRecorderError> {
        if !self.recording.load(Ordering::SeqCst) {
            return Err(MeetingRecorderError::NotRecording);
        }
        let tx = self
            .cmd_tx
            .lock()
            .clone()
            .ok_or(MeetingRecorderError::ThreadGone)?;
        tx.send(RecCmd::Resume)
            .map_err(|_| MeetingRecorderError::ThreadGone)
    }

    /// Finalize the WAV and return `(path, duration_seconds, sample_rate)`.
    pub fn stop(&self) -> Result<RecordingSummary, MeetingRecorderError> {
        if !self.recording.load(Ordering::SeqCst) {
            return Err(MeetingRecorderError::NotRecording);
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let tx = self
            .cmd_tx
            .lock()
            .clone()
            .ok_or(MeetingRecorderError::ThreadGone)?;
        tx.send(RecCmd::Stop { reply: reply_tx })
            .map_err(|_| MeetingRecorderError::ThreadGone)?;
        reply_rx
            .recv()
            .map_err(|_| MeetingRecorderError::ThreadGone)?
    }
}

impl Drop for MeetingRecorder {
    /// Graceful shutdown for an in-flight (or idle) recorder.
    ///
    /// If the process drops the recorder mid-recording, we must not leave a
    /// dangling cpal stream (zombie CoreAudio callbacks) or an un-joined writer
    /// thread (WAV footer never back-filled → corrupt file). We:
    /// 1. drop the command sender, which ends the control thread's `recv` loop;
    ///    on exit that loop finalizes any live session (stop-equivalent teardown
    ///    that reuses [`teardown_session`]);
    /// 2. join the control thread so all teardown completes before we return.
    ///
    /// `Option::take` guards both steps so we never double-drop or double-join,
    /// and nothing here can panic.
    fn drop(&mut self) {
        // Step 1: signal the control thread to stop by closing the channel.
        if let Some(tx) = self.cmd_tx.lock().take() {
            drop(tx);
        }
        // Step 2: wait for the control thread's teardown (finalize + writer join).
        if let Some(handle) = self.control_handle.lock().take() {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_on_thread(
    preferred: Option<String>,
    out_path: PathBuf,
    sample_sink: Option<SampleSink>,
    recording: &AtomicBool,
    paused: &Arc<AtomicBool>,
    epoch: &Arc<AtomicU64>,
    sample_rate_atom: &AtomicU32,
    session: &mut Option<Session>,
) -> Result<u32, MeetingRecorderError> {
    if recording.swap(true, Ordering::SeqCst) {
        return Err(MeetingRecorderError::AlreadyRecording);
    }
    paused.store(false, Ordering::SeqCst);

    // Defensive: never leave a previous stream alive across sessions.
    epoch.fetch_add(1, Ordering::SeqCst);
    teardown_session(session);

    let device = match resolve_device(preferred) {
        Ok(d) => d,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            return Err(MeetingRecorderError::Device(e.to_string()));
        }
    };

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    sample_rate_atom.store(sample_rate, Ordering::SeqCst);

    let sink = match WavSink::create(&out_path, sample_rate) {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            return Err(MeetingRecorderError::Io(e.to_string()));
        }
    };
    let (writer_tx, writer_handle) = spawn_writer(sink);

    let session_epoch = epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let stream_config: StreamConfig = config.clone().into();
    let err_fn = |e| tracing::error!(error = %e, "meeting audio stream error");

    let build = |writer_tx: Sender<WriterMsg>,
                 sample_sink: Option<SampleSink>|
     -> Result<cpal::Stream, MeetingRecorderError> {
        let epoch_cb = Arc::clone(epoch);
        let paused_cb = Arc::clone(paused);
        // Each match arm moves `sample_sink`; only one arm runs, so this is a
        // valid single move (not a use-after-move).
        match config.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(
                &device,
                &stream_config,
                channels,
                writer_tx,
                sample_sink,
                epoch_cb,
                paused_cb,
                session_epoch,
                err_fn,
            ),
            SampleFormat::I16 => build_stream::<i16>(
                &device,
                &stream_config,
                channels,
                writer_tx,
                sample_sink,
                epoch_cb,
                paused_cb,
                session_epoch,
                err_fn,
            ),
            SampleFormat::U16 => build_stream::<u16>(
                &device,
                &stream_config,
                channels,
                writer_tx,
                sample_sink,
                epoch_cb,
                paused_cb,
                session_epoch,
                err_fn,
            ),
            other => Err(MeetingRecorderError::Stream(format!(
                "unsupported sample format: {other:?}"
            ))),
        }
    };

    let stream = build(writer_tx.clone(), sample_sink);
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            // Tear down the writer we just spawned.
            drop(writer_tx);
            let _ = writer_handle.join();
            return Err(e);
        }
    };
    if let Err(e) = stream.play() {
        recording.store(false, Ordering::SeqCst);
        drop(stream);
        drop(writer_tx);
        let _ = writer_handle.join();
        return Err(MeetingRecorderError::Stream(e.to_string()));
    }

    *session = Some(Session {
        stream,
        writer_tx,
        writer_handle,
        out_path,
        sample_rate,
    });
    tracing::info!(
        sample_rate,
        channels,
        session_epoch,
        "meeting recording started"
    );
    Ok(sample_rate)
}

fn stop_on_thread(
    recording: &AtomicBool,
    paused: &AtomicBool,
    epoch: &Arc<AtomicU64>,
    session: &mut Option<Session>,
) -> Result<RecordingSummary, MeetingRecorderError> {
    let Some(session) = session.take() else {
        recording.store(false, Ordering::SeqCst);
        return Err(MeetingRecorderError::NotRecording);
    };

    // Invalidate callbacks first so any zombie stream still draining from a
    // prior Drop cannot append.
    epoch.fetch_add(1, Ordering::SeqCst);
    if let Err(e) = session.stream.pause() {
        tracing::warn!(error = %e, "meeting stream pause failed");
    }
    drop(session.stream);
    // Give in-flight CoreAudio callbacks a moment to exit before finalize.
    thread::sleep(std::time::Duration::from_millis(60));

    let (fin_tx, fin_rx) = mpsc::channel();
    let _ = session.writer_tx.send(WriterMsg::Finalize(fin_tx));
    drop(session.writer_tx);
    let samples = match fin_rx.recv() {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            let _ = session.writer_handle.join();
            recording.store(false, Ordering::SeqCst);
            paused.store(false, Ordering::SeqCst);
            return Err(MeetingRecorderError::Io(e.to_string()));
        }
        Err(_) => {
            let _ = session.writer_handle.join();
            recording.store(false, Ordering::SeqCst);
            paused.store(false, Ordering::SeqCst);
            return Err(MeetingRecorderError::ThreadGone);
        }
    };
    let _ = session.writer_handle.join();

    recording.store(false, Ordering::SeqCst);
    paused.store(false, Ordering::SeqCst);

    let sample_rate = session.sample_rate;
    let duration_seconds = if sample_rate > 0 {
        samples as f64 / sample_rate as f64
    } else {
        0.0
    };
    tracing::info!(
        samples,
        sample_rate,
        duration_seconds,
        path = %session.out_path.display(),
        "meeting recording stopped"
    );
    Ok(RecordingSummary {
        wav_path: session.out_path,
        duration_seconds,
        sample_rate,
    })
}

fn resolve_device(preferred: Option<String>) -> Result<Device, MeetingRecorderError> {
    let host = cpal::default_host();
    if let Some(name) = preferred {
        let devices = host
            .input_devices()
            .map_err(|e| MeetingRecorderError::Device(e.to_string()))?;
        for d in devices {
            if d.name().ok().as_deref() == Some(name.as_str()) {
                return Ok(d);
            }
        }
        tracing::warn!(%name, "preferred device not found, using default");
    }
    host.default_input_device()
        .ok_or(MeetingRecorderError::NoDevice)
}

#[allow(clippy::too_many_arguments)]
fn build_stream<T>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    writer_tx: Sender<WriterMsg>,
    sample_sink: Option<SampleSink>,
    epoch: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    session_epoch: u64,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, MeetingRecorderError>
where
    T: Sample + SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                // Stale stream from a previous session — ignore completely.
                if epoch.load(Ordering::SeqCst) != session_epoch {
                    return;
                }
                // Paused: drop samples so the file has no silent gap. Paused
                // audio is likewise withheld from the fan-out subscriber.
                if paused.load(Ordering::SeqCst) {
                    return;
                }
                let mono = downmix_to_mono(data, channels);
                // Fan-out to the real-time subscriber (streaming ASR), if any.
                // A clone keeps the WAV path authoritative and untouched; when
                // no sink is attached this is skipped entirely (zero extra work
                // / no allocation on the default recording path).
                fanout_chunk(&sample_sink, &mono);
                // Writer thread does the file I/O; if it has gone away the
                // recording is being torn down and dropping the chunk is fine.
                let _ = writer_tx.send(WriterMsg::Chunk(mono));
            },
            err_fn,
            None,
        )
        .map_err(|e| MeetingRecorderError::Stream(e.to_string()))
}

/// Down-mix an interleaved multi-channel `T` frame buffer to mono `f32`.
/// Extracted from the audio callback so the (device-free) mixing logic is unit
/// testable.
fn downmix_to_mono<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: Sample,
    f32: FromSample<T>,
{
    let mut mono = Vec::with_capacity(if channels <= 1 {
        data.len()
    } else {
        data.len() / channels
    });
    if channels <= 1 {
        for &s in data {
            mono.push(s.to_sample::<f32>());
        }
    } else {
        for frame in data.chunks(channels) {
            let mut sum = 0.0f32;
            for &s in frame {
                sum += s.to_sample::<f32>();
            }
            mono.push(sum / channels as f32);
        }
    }
    mono
}

/// Forward one already-down-mixed mono chunk to an optional fan-out subscriber.
/// Mirrors the audio-callback branch so the "clone-and-send when present,
/// no-op when absent" contract is unit-testable without a live cpal stream.
/// Returns `true` if a chunk was delivered to a live subscriber.
fn fanout_chunk(sink: &Option<SampleSink>, mono: &[f32]) -> bool {
    match sink {
        Some(tx) => tx.send(mono.to_vec()).is_ok(),
        None => false,
    }
}

fn teardown_session(session: &mut Option<Session>) {
    if let Some(session) = session.take() {
        if let Err(e) = session.stream.pause() {
            tracing::warn!(error = %e, "stale meeting stream pause failed");
        }
        drop(session.stream);
        drop(session.writer_tx);
        let _ = session.writer_handle.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    }

    fn read_u16_le(bytes: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([bytes[off], bytes[off + 1]])
    }

    #[test]
    fn f32_to_i16_clamps_and_scales() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(2.0), 32767); // clamp high
        assert_eq!(f32_to_i16(-2.0), -32767); // clamp low
    }

    #[test]
    fn wav_sink_writes_header_body_and_backfills_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.wav");
        let sample_rate = 48_000u32;

        // Feed synthetic chunks the way the audio callback would.
        let chunk_a = [0.0f32, 1.0, -1.0];
        let chunk_b = [0.5f32, -0.5];
        let total_samples = (chunk_a.len() + chunk_b.len()) as u64;

        let mut sink = WavSink::create(&path, sample_rate).unwrap();
        sink.write_samples(&chunk_a).unwrap();
        sink.write_samples(&chunk_b).unwrap();
        assert_eq!(sink.samples_written(), total_samples);
        let finalized = sink.finalize().unwrap();
        assert_eq!(finalized, total_samples);

        let bytes = std::fs::read(&path).unwrap();
        let data_bytes = total_samples * 2;

        // ── Header sanity ──
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(read_u32_le(&bytes, 4) as u64, 36 + data_bytes);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(read_u32_le(&bytes, 16), 16); // fmt chunk size
        assert_eq!(read_u16_le(&bytes, 20), 1); // PCM
        assert_eq!(read_u16_le(&bytes, 22), 1); // mono
        assert_eq!(read_u32_le(&bytes, 24), sample_rate);
        assert_eq!(read_u32_le(&bytes, 28), sample_rate * 2); // byte rate
        assert_eq!(read_u16_le(&bytes, 32), 2); // block align
        assert_eq!(read_u16_le(&bytes, 34), 16); // bits
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(read_u32_le(&bytes, 40) as u64, data_bytes);

        // ── Body: total length and known sample values ──
        assert_eq!(bytes.len() as u64, WAV_HEADER_LEN + data_bytes);
        // First three samples: 0, +full, -full.
        assert_eq!(read_u16_le(&bytes, 44) as i16, 0);
        assert_eq!(read_u16_le(&bytes, 46) as i16, 32767);
        assert_eq!(read_u16_le(&bytes, 48) as i16, -32767);
    }

    #[test]
    fn downmix_mono_passes_through() {
        let data = [0.0f32, 0.5, -0.5, 1.0];
        assert_eq!(downmix_to_mono(&data, 1), vec![0.0, 0.5, -0.5, 1.0]);
    }

    #[test]
    fn downmix_stereo_averages_frames() {
        // Two stereo frames: (0.0, 1.0) -> 0.5, (-1.0, 1.0) -> 0.0.
        let data = [0.0f32, 1.0, -1.0, 1.0];
        assert_eq!(downmix_to_mono(&data, 2), vec![0.5, 0.0]);
    }

    #[test]
    fn fanout_delivers_clone_when_subscribed() {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let sink: Option<SampleSink> = Some(tx);
        let chunk = [0.1f32, 0.2, 0.3];
        assert!(fanout_chunk(&sink, &chunk));
        assert_eq!(rx.recv().unwrap(), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn fanout_is_noop_when_unsubscribed() {
        let sink: Option<SampleSink> = None;
        // No panic, no delivery, reports "not delivered".
        assert!(!fanout_chunk(&sink, &[0.0f32, 1.0]));
    }

    #[test]
    fn fanout_reports_false_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        drop(rx); // subscriber went away (e.g. streaming task ended)
        let sink: Option<SampleSink> = Some(tx);
        // Send fails but is swallowed — the recorder never breaks on a dead sink.
        assert!(!fanout_chunk(&sink, &[0.5f32]));
    }

    #[test]
    fn wav_sink_empty_take_is_valid_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.wav");
        let sink = WavSink::create(&path, 16_000).unwrap();
        assert_eq!(sink.finalize().unwrap(), 0);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, WAV_HEADER_LEN);
        assert_eq!(read_u32_le(&bytes, 4), 36); // 36 + 0 data bytes
        assert_eq!(read_u32_le(&bytes, 40), 0); // data length
    }
}
