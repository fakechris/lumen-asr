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
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime};
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
    /// Capture stalls that were padded with silence (system sleep / App Nap
    /// suspended the audio callback). Empty for a normal recording.
    pub gaps: Vec<MeetingGap>,
}

/// A stretch of wall-clock time during which the audio callback delivered
/// nothing — the OS suspended capture (idle system sleep or App Nap). The
/// recorder pads the WAV with this much silence so media time stays aligned to
/// real time instead of silently collapsing, and records the marker so the app
/// can tell the user roughly this much audio was not captured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeetingGap {
    /// Media-time offset (seconds into the WAV) where the silence pad begins.
    pub start_seconds: f64,
    /// Length of the un-captured stretch, in seconds.
    pub duration_seconds: f64,
}

/// Minimum capture stall (seconds) that counts as a gap worth padding. Normal
/// scheduling jitter and buffer cadence stay well under this; only a real
/// suspension trips it.
const GAP_MIN_SECONDS: f64 = 1.5;

/// Max mono i16 samples one WAV can hold before the 32-bit RIFF/`data` size
/// fields ([`WavSink::finalize`] writes them as `u32`) overflow. Silence padding
/// is clamped to the file's remaining room against this so a very long stall
/// cannot produce a malformed, multi-GB file (~12.4 h at 48 kHz mono PCM16).
const WAV_MAX_SAMPLES: u64 = ((u32::MAX as u64) - WAV_HEADER_LEN) / 2;

/// Detects capture stalls by comparing the wall-clock spacing of consecutive
/// chunks against the audio each one represents. The **wall clock**
/// (`SystemTime`) is deliberate: it advances across system sleep, whereas a
/// monotonic timer may pause, and a suspended process is exactly what we must
/// catch. Pure and clock-injectable so the logic is unit-testable.
struct GapDetector {
    sample_rate: u32,
    last: Option<SystemTime>,
}

impl GapDetector {
    fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            last: None,
        }
    }

    /// Feed one captured chunk of `chunk_len` mono samples arriving at wall
    /// clock `now`. Returns the number of silence samples to pad when the stall
    /// since the previous chunk exceeds [`GAP_MIN_SECONDS`].
    fn observe(&mut self, chunk_len: usize, now: SystemTime) -> Option<u64> {
        let sr = self.sample_rate.max(1) as f64;
        let pad = self.last.and_then(|last| {
            // A backwards clock step (`duration_since` err) is treated as no gap.
            let wall = now.duration_since(last).ok()?.as_secs_f64();
            let audio = chunk_len as f64 / sr;
            let deficit = wall - audio;
            (deficit > GAP_MIN_SECONDS).then_some((deficit * sr) as u64)
        });
        self.last = Some(now);
        pad.filter(|n| *n > 0)
    }

    /// Forget the last arrival so the next chunk cannot form a gap. Called when
    /// pausing/resuming: a paused interval drops chunks on purpose and must not
    /// be mistaken for a capture stall.
    fn reset(&mut self) {
        self.last = None;
    }
}

/// Append `n` samples of silence to `sink` in bounded blocks, so a long stall
/// never allocates one huge buffer. Block size is not correctness-critical.
fn write_silence(sink: &mut WavSink, mut n: u64) -> io::Result<()> {
    const BLOCK: usize = 16_000;
    let zeros = [0.0f32; BLOCK];
    while n > 0 {
        let take = n.min(BLOCK as u64) as usize;
        sink.write_samples(&zeros[..take])?;
        n -= take as u64;
    }
    Ok(())
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
// Crash recovery — back-fill a WAV whose length fields were never patched.
//
// [`WavSink`] writes a placeholder header on `create` and only patches the RIFF
// and `data` chunk sizes on `finalize`. If the app is killed mid-recording the
// PCM samples are already on disk but the header still says `0` bytes, so a
// standard reader sees a zero-length (empty) file. On the next launch the crash
// recovery scan re-derives both sizes from the file's **actual** byte length and
// patches them, so the salvaged audio can be transcribed instead of lost.
//
// Pure file I/O (no cpal), so it is unit-testable by writing a placeholder
// header + N bytes of data and asserting the repaired header.
// ─────────────────────────────────────────────────────────────────────────────

/// What a [`repair_wav_header`] call recovered from the salvaged file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepairedWav {
    /// Sample rate read from the (already-written) fmt chunk.
    pub sample_rate: u32,
    /// Channel count read from the fmt chunk (mono for meeting takes).
    pub channels: u16,
    /// Number of PCM data bytes now recorded in the `data` chunk size.
    pub data_bytes: u64,
    /// Recovered audio length in seconds (`0.0` for a header-only, empty take).
    pub duration_seconds: f64,
}

/// Re-derive and back-fill the RIFF and `data` chunk sizes of `path` from the
/// file's actual byte length, salvaging a recording whose header was never
/// patched (see the module comment above). This is idempotent: a file whose
/// sizes are already correct is rewritten with the same values.
///
/// `data_size = file_len - 44` and `RIFF size = file_len - 8` (the WAV header
/// this recorder writes is a fixed 44 bytes: PCM `fmt ` + `data`). Returns the
/// sample rate, channel count, data length, and derived duration.
///
/// Fails with [`io::ErrorKind::InvalidData`] when the file is shorter than the
/// 44-byte header or is missing the `RIFF`/`WAVE`/`data` markers (i.e. not a WAV
/// this recorder produced) — a header-only file (exactly 44 bytes) is *not* an
/// error: it repairs to a valid zero-length take (`data_bytes == 0`), which the
/// caller treats as "no audio captured".
pub fn repair_wav_header(path: impl AsRef<Path>) -> io::Result<RepairedWav> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < WAV_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("wav too small to repair: {file_len} bytes (< {WAV_HEADER_LEN}-byte header)"),
        ));
    }

    // Read the fixed header and validate the markers so we never "repair" an
    // unrelated file into a bogus WAV.
    let mut header = [0u8; WAV_HEADER_LEN as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" || &header[36..40] != b"data" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a PCM WAV file (missing RIFF/WAVE/data markers)",
        ));
    }
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]);

    let data_bytes = file_len - WAV_HEADER_LEN;
    let riff_size = file_len - 8;

    // RIFF chunk size at offset 4, data chunk size at offset 40.
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&(riff_size as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&(data_bytes as u32).to_le_bytes())?;
    file.flush()?;

    let bytes_per_frame = u64::from(channels) * u64::from(bits_per_sample / 8);
    let duration_seconds = if sample_rate > 0 && bytes_per_frame > 0 {
        (data_bytes / bytes_per_frame) as f64 / f64::from(sample_rate)
    } else {
        0.0
    };

    Ok(RepairedWav {
        sample_rate,
        channels,
        data_bytes,
        duration_seconds,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// VoicedTracker — how long one captured audio track has been effectively silent.
//
// The WAV writer thread already sees every captured chunk, so it doubles as the
// silence probe for the unattended-recording watchdog: per chunk it advances a
// running sample counter and, when the chunk's RMS clears a small threshold,
// stamps the counter as the last "voiced" position. `silence_seconds` is then
// `(total - last_voiced) / sample_rate`. Pure arithmetic behind atomics, so it
// is `Send + Sync`, cheap on the writer thread, and unit-testable without cpal.
//
// Both mic and system tracks opt into the same tracker. The desktop watchdog
// combines them so activity on either side of a meeting keeps it alive.
// ─────────────────────────────────────────────────────────────────────────────

/// RMS below which a captured chunk counts as silence. Room tone / fan noise sit well
/// under this; ordinary speech is far above it. Not correctness-critical — it
/// only decides when the silence timer advances.
const SILENCE_RMS_THRESHOLD: f32 = 0.01;

/// Shared, lock-free tracker of one track's silence for the watchdog (see the section
/// comment above). Cloned behind an `Arc`: the writer thread `observe`s chunks
/// while the recorder reads `silence_seconds`.
pub struct VoicedTracker {
    total_samples: AtomicU64,
    last_voiced_sample: AtomicU64,
    sample_rate: AtomicU32,
}

impl Default for VoicedTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl VoicedTracker {
    /// A fresh tracker with no samples seen and an unknown (`0`) sample rate.
    pub fn new() -> Self {
        Self {
            total_samples: AtomicU64::new(0),
            last_voiced_sample: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0),
        }
    }

    /// Arm the tracker for a new recording: zero the counters and record the
    /// capture sample rate so `silence_seconds` can convert samples to seconds.
    fn arm(&self, sample_rate: u32) {
        self.total_samples.store(0, Ordering::SeqCst);
        self.last_voiced_sample.store(0, Ordering::SeqCst);
        self.sample_rate.store(sample_rate, Ordering::SeqCst);
    }

    /// Fold one mono chunk into the tracker: advance the sample counter and, if
    /// the chunk is loud enough (RMS over [`SILENCE_RMS_THRESHOLD`]), reset the
    /// silence timer by marking this position as the last voiced sample.
    fn observe(&self, chunk: &[f32]) {
        if chunk.is_empty() {
            return;
        }
        let total = self
            .total_samples
            .fetch_add(chunk.len() as u64, Ordering::SeqCst)
            + chunk.len() as u64;
        let sum_sq: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let rms = (sum_sq / chunk.len() as f64).sqrt() as f32;
        if rms > SILENCE_RMS_THRESHOLD {
            self.last_voiced_sample.store(total, Ordering::SeqCst);
        }
    }

    /// Seconds of continuous silence on this track since the last voiced chunk, or
    /// `None` when the rate is unknown (never armed / not recording).
    fn silence_seconds(&self) -> Option<f64> {
        let sample_rate = self.sample_rate.load(Ordering::SeqCst);
        if sample_rate == 0 {
            return None;
        }
        let total = self.total_samples.load(Ordering::SeqCst);
        let last = self.last_voiced_sample.load(Ordering::SeqCst);
        Some((total.saturating_sub(last)) as f64 / sample_rate as f64)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Writer thread — owns a WavSink, drains sample chunks off the audio callback.
// ─────────────────────────────────────────────────────────────────────────────

enum WriterMsg {
    Chunk {
        samples: Vec<f32>,
        /// Wall clock at which the capture callback produced this chunk — stamped
        /// on the callback side, *not* when the writer dequeues it, so gap
        /// detection is immune to writer backlog (disk I/O, silence padding).
        arrived_at: SystemTime,
    },
    /// Forget the gap detector's last-arrival timestamp (sent on pause/resume so
    /// an intentional paused interval is not padded as a capture stall).
    ResetGapClock,
    Finalize(Sender<io::Result<(u64, Vec<MeetingGap>)>>),
}

/// Spawn the WAV writer thread. `voiced`, when present, observes only real
/// captured chunks (never synthetic gap padding), so callers can distinguish
/// an active meeting from prolonged physical silence without involving ASR.
fn spawn_writer(
    mut sink: WavSink,
    voiced: Option<Arc<VoicedTracker>>,
) -> (Sender<WriterMsg>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<WriterMsg>();
    let handle = thread::Builder::new()
        .name("lumen-meeting-wav".into())
        .spawn(move || {
            let mut detector = GapDetector::new(sink.sample_rate());
            let mut gaps: Vec<MeetingGap> = Vec::new();
            while let Ok(msg) = rx.recv() {
                match msg {
                    WriterMsg::Chunk { samples, arrived_at } => {
                        // Feed the silence watchdog before any gap padding, so
                        // it measures the real captured audio.
                        if let Some(voiced) = &voiced {
                            voiced.observe(&samples);
                        }
                        // Before writing this chunk, pad any capture stall that
                        // preceded it so media time stays aligned to real time.
                        // Clamp the pad to the WAV's remaining capacity so a very
                        // long stall can never overflow the 32-bit RIFF sizes into
                        // a malformed, multi-GB file.
                        if let Some(pad) = detector.observe(samples.len(), arrived_at) {
                            let remaining = WAV_MAX_SAMPLES.saturating_sub(sink.samples_written());
                            let pad = pad.min(remaining);
                            let sr = sink.sample_rate().max(1) as f64;
                            let start_seconds = sink.samples_written() as f64 / sr;
                            if pad > 0 {
                                match write_silence(&mut sink, pad) {
                                    Ok(()) => {
                                        let duration_seconds = pad as f64 / sr;
                                        tracing::warn!(
                                            start_seconds,
                                            duration_seconds,
                                            "meeting capture stalled (likely system sleep); padded with silence"
                                        );
                                        gaps.push(MeetingGap {
                                            start_seconds,
                                            duration_seconds,
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "meeting silence pad write failed")
                                    }
                                }
                            }
                        }
                        if let Err(e) = sink.write_samples(&samples) {
                            tracing::warn!(error = %e, "meeting wav chunk write failed");
                        }
                    }
                    WriterMsg::ResetGapClock => detector.reset(),
                    WriterMsg::Finalize(reply) => {
                        let _ = reply.send(sink.finalize().map(|n| (n, std::mem::take(&mut gaps))));
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
// SystemTrackRecorder — an optional second, externally-fed WAV track.
//
// Dual-track meetings write the microphone (via `MeetingRecorder`) and the
// system audio output (remote participants, captured by the platform layer's
// Core Audio process tap) as two synchronized WAVs. This type owns the second
// file: it reuses the exact `WavSink` + writer-thread machinery of the mic
// path, but is fed *externally* — the platform capture pushes mono `f32`
// chunks through a [`SystemTrackSender`] — so this crate never depends on the
// macOS tap FFI and the type compiles (and is unit-testable) everywhere.
//
// The mic path is untouched by this type: a meeting without a system track
// never constructs one.
// ─────────────────────────────────────────────────────────────────────────────

/// Writer side of the system-audio track: owns the WAV writer thread.
pub struct SystemTrackRecorder {
    writer_tx: Sender<WriterMsg>,
    writer_handle: JoinHandle<()>,
    paused: Arc<AtomicBool>,
    out_path: PathBuf,
    sample_rate: u32,
    voiced: Arc<VoicedTracker>,
}

/// Cloneable, thread-safe feed handle for a [`SystemTrackRecorder`]. The
/// platform capture callback pushes each mono chunk through this; chunks are
/// dropped while the track is paused (mirroring the mic recorder, so the two
/// timelines stay aligned with no silent padding).
#[derive(Clone)]
pub struct SystemTrackSender {
    tx: Arc<Mutex<Sender<WriterMsg>>>,
    paused: Arc<AtomicBool>,
}

impl SystemTrackSender {
    /// Forward one mono `f32` chunk to the writer thread. Returns `false` (and
    /// drops the chunk) when the track is paused or already finalized — the
    /// caller never needs to handle an error.
    pub fn push(&self, samples: &[f32]) -> bool {
        if samples.is_empty() || self.paused.load(Ordering::SeqCst) {
            return false;
        }
        self.tx
            .lock()
            .send(WriterMsg::Chunk {
                samples: samples.to_vec(),
                arrived_at: SystemTime::now(),
            })
            .is_ok()
    }
}

impl SystemTrackRecorder {
    /// Create the WAV (placeholder header, same as the mic path) and spawn its
    /// writer thread.
    pub fn create(out_path: impl Into<PathBuf>, sample_rate: u32) -> io::Result<Self> {
        let out_path = out_path.into();
        let sink = WavSink::create(&out_path, sample_rate)?;
        let voiced = Arc::new(VoicedTracker::new());
        voiced.arm(sample_rate);
        let (writer_tx, writer_handle) = spawn_writer(sink, Some(Arc::clone(&voiced)));
        Ok(Self {
            writer_tx,
            writer_handle,
            paused: Arc::new(AtomicBool::new(false)),
            out_path,
            sample_rate,
            voiced,
        })
    }

    /// A feed handle for the platform capture callback.
    pub fn sender(&self) -> SystemTrackSender {
        SystemTrackSender {
            tx: Arc::new(Mutex::new(self.writer_tx.clone())),
            paused: Arc::clone(&self.paused),
        }
    }

    /// Pause / resume the track. Paused chunks are dropped (no silent gap),
    /// matching the mic recorder so pausing compresses both timelines by the
    /// same wall-clock interval.
    pub fn set_paused(&self, paused: bool) {
        // Queue the reset *before* clearing the paused flag: on resume this
        // guarantees the reset is enqueued ahead of any chunk the callback
        // delivers once unpaused, so the paused interval (chunks dropped on
        // purpose) is never converted into a padded capture stall.
        let _ = self.writer_tx.send(WriterMsg::ResetGapClock);
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// Path of the WAV being written.
    pub fn out_path(&self) -> &Path {
        &self.out_path
    }

    /// Seconds since this externally-fed track last carried physical audio
    /// above the room-noise threshold. This is sample-clock based, so pausing
    /// the track also pauses the silence clock.
    pub fn silence_seconds(&self) -> Option<f64> {
        self.voiced.silence_seconds()
    }

    /// Finalize the WAV (back-fill the RIFF/`data` sizes) and join the writer.
    pub fn finalize(self) -> io::Result<RecordingSummary> {
        let (fin_tx, fin_rx) = mpsc::channel();
        let _ = self.writer_tx.send(WriterMsg::Finalize(fin_tx));
        drop(self.writer_tx);
        let result = fin_rx
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("system track writer thread gone")));
        let _ = self.writer_handle.join();
        let (samples, gaps) = result?;
        let duration_seconds = if self.sample_rate > 0 {
            samples as f64 / self.sample_rate as f64
        } else {
            0.0
        };
        Ok(RecordingSummary {
            wav_path: self.out_path,
            duration_seconds,
            sample_rate: self.sample_rate,
            gaps,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Live fan-out — bounded, timestamped, never blocks the capture callback.
//
// Every track of a meeting (mic, system audio) can fan a copy of its mono
// chunks out to the real-time preview worker. The fan-out is:
//
// - **timestamped on a unified timeline**: each packet carries `start_seconds`
//   = the *callback arrival time* minus the meeting's shared `t0` `Instant`
//   (one `t0` for all tracks, taken when the recording starts). Arrival time —
//   not per-track frame accumulation — because the system track starts later
//   than the mic and may drop packets, so frame counting would drift.
// - **bounded and non-blocking**: `try_send` on a small sync channel. When the
//   consumer falls behind the packet is dropped and counted; the capture
//   callback never blocks. The WAV write path is a separate, unbounded writer
//   channel and stays authoritative — live drops never lose recorded audio.
// ─────────────────────────────────────────────────────────────────────────────

/// Default bound of a live fan-out channel, in packets. Capture callbacks
/// arrive every ~10–100 ms, so ~64 packets is seconds of headroom while still
/// keeping worst-case buffered audio (and memory) small.
pub const LIVE_TAP_CAPACITY: usize = 64;

/// Log a channel-full drop summary every this many dropped packets.
const LIVE_TAP_DROP_LOG_EVERY: u64 = 512;

/// One timestamped mono chunk fanned out to the live preview worker.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveAudioPacket {
    /// Seconds since the meeting's shared `t0` at which this chunk arrived.
    pub start_seconds: f64,
    /// Mono samples at the track's native capture rate.
    pub samples: Vec<f32>,
}

/// Bounded, timestamping fan-out handle for one track (see the section
/// comment above). Cloneable; safe to call from a real-time audio callback.
#[derive(Clone)]
pub struct LiveTapSender {
    tx: SyncSender<LiveAudioPacket>,
    t0: Instant,
    track: &'static str,
    dropped: Arc<AtomicU64>,
}

impl LiveTapSender {
    /// Timestamp `samples` against the shared `t0` and `try_send` them to the
    /// live worker. Never blocks: a full channel drops the packet (counted and
    /// periodically summarized via `tracing`), a disconnected consumer is
    /// silently ignored. Returns `true` iff the packet was delivered.
    pub fn push(&self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }
        let start_seconds = self.t0.elapsed().as_secs_f64();
        match self.tx.try_send(LiveAudioPacket {
            start_seconds,
            samples: samples.to_vec(),
        }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_multiple_of(LIVE_TAP_DROP_LOG_EVERY) {
                    tracing::warn!(
                        track = self.track,
                        dropped,
                        "live preview fan-out full; dropping packets (WAV write unaffected)"
                    );
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Total packets dropped because the channel was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Create a bounded live fan-out channel for one track. `track` labels drop
/// logs ("mic"/"system"); `t0` is the meeting's shared timeline origin.
pub fn live_tap_channel(
    track: &'static str,
    t0: Instant,
    capacity: usize,
) -> (LiveTapSender, Receiver<LiveAudioPacket>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        LiveTapSender {
            tx,
            t0,
            track,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        rx,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// MeetingRecorder — cross-platform (no cfg gate); control handles only.
// ─────────────────────────────────────────────────────────────────────────────

/// A subscriber that receives the same mono `f32` sample chunks the WAV writer
/// gets, at the **native capture sample rate**, timestamped on the meeting's
/// unified timeline. Used by the real-time meeting layer (streaming Paraformer)
/// to consume audio while it is still being recorded (see `docs/MEETING.md`
/// M6/P3). When no sink is attached the audio callback does zero extra work.
pub type SampleSink = LiveTapSender;

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
    /// Mic silence tracker shared with the writer thread, read by
    /// [`silence_seconds`](Self::silence_seconds) for the watchdog.
    voiced: Arc<VoicedTracker>,
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
        let voiced = Arc::new(VoicedTracker::new());
        let voiced_thread = Arc::clone(&voiced);

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
                                &voiced_thread,
                                &mut session,
                            );
                            let _ = reply.send(res);
                        }
                        RecCmd::Pause => {
                            // Reset the gap clock before flipping the flag (see
                            // SystemTrackRecorder::set_paused for the ordering).
                            if let Some(s) = &session {
                                let _ = s.writer_tx.send(WriterMsg::ResetGapClock);
                            }
                            paused_flag.store(true, Ordering::SeqCst);
                        }
                        RecCmd::Resume => {
                            // Reset before clearing paused so the reset is queued
                            // ahead of the first post-resume chunk (no false gap).
                            if let Some(s) = &session {
                                let _ = s.writer_tx.send(WriterMsg::ResetGapClock);
                            }
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
            voiced,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Seconds of continuous microphone silence, for the unattended-recording
    /// watchdog. `None` when not recording (or the sample rate is unknown),
    /// which the watchdog treats as "can't tell — do not auto-stop". Note the
    /// system-AEC mic path (`meeting_mic_aec`) does not feed this recorder, so
    /// there it is always `None`.
    pub fn silence_seconds(&self) -> Option<f64> {
        if !self.recording.load(Ordering::SeqCst) {
            return None;
        }
        self.voiced.silence_seconds()
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
    voiced: &Arc<VoicedTracker>,
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
    // Arm the mic silence watchdog for this session at the capture rate.
    voiced.arm(sample_rate);

    let sink = match WavSink::create(&out_path, sample_rate) {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            return Err(MeetingRecorderError::Io(e.to_string()));
        }
    };
    let (writer_tx, writer_handle) = spawn_writer(sink, Some(Arc::clone(voiced)));

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
    let (samples, gaps) = match fin_rx.recv() {
        Ok(Ok(out)) => out,
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
        gaps = gaps.len(),
        path = %session.out_path.display(),
        "meeting recording stopped"
    );
    Ok(RecordingSummary {
        wav_path: session.out_path,
        duration_seconds,
        sample_rate,
        gaps,
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
                let _ = writer_tx.send(WriterMsg::Chunk {
                    samples: mono,
                    arrived_at: SystemTime::now(),
                });
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
/// Mirrors the audio-callback branch so the "timestamp-and-try-send when
/// present, no-op when absent" contract is unit-testable without a live cpal
/// stream. Returns `true` if a chunk was delivered to a live subscriber.
fn fanout_chunk(sink: &Option<SampleSink>, mono: &[f32]) -> bool {
    match sink {
        Some(tap) => tap.push(mono),
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
    use std::time::Duration;

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
    }

    #[test]
    fn gap_detector_ignores_normal_cadence() {
        // 16 kHz, ~100 ms chunks arriving ~100 ms apart: no stall, no pad.
        let mut d = GapDetector::new(16_000);
        assert_eq!(d.observe(1_600, at(0.0)), None); // first chunk: no baseline
        assert_eq!(d.observe(1_600, at(0.1)), None);
        assert_eq!(d.observe(1_600, at(0.2)), None);
        // A little jitter (chunk late by <1.5 s) still isn't a gap.
        assert_eq!(d.observe(1_600, at(1.0)), None);
    }

    #[test]
    fn gap_detector_pads_a_long_stall() {
        // Previous chunk at t=1 s, next arrives ~17 min later (process was
        // suspended): the ~17 min minus this chunk's 0.1 s is padded.
        let mut d = GapDetector::new(16_000);
        assert_eq!(d.observe(1_600, at(1.0)), None);
        let stall = 17.0 * 60.0;
        let pad = d.observe(1_600, at(1.0 + stall)).expect("gap detected");
        let expected = ((stall - 0.1) * 16_000.0) as u64;
        assert_eq!(pad, expected);
    }

    #[test]
    fn gap_detector_reset_prevents_a_gap_across_a_pause() {
        // A paused interval drops chunks; reset() on pause/resume means the first
        // chunk after resume has no baseline and cannot be seen as a stall.
        let mut d = GapDetector::new(16_000);
        assert_eq!(d.observe(1_600, at(1.0)), None);
        d.reset();
        assert_eq!(d.observe(1_600, at(600.0)), None);
    }

    #[test]
    fn gap_detector_treats_backwards_clock_as_no_gap() {
        let mut d = GapDetector::new(16_000);
        assert_eq!(d.observe(1_600, at(10.0)), None);
        assert_eq!(d.observe(1_600, at(5.0)), None); // clock stepped back
    }

    #[test]
    fn voiced_tracker_resets_on_loud_chunk_then_silence_grows() {
        let tracker = VoicedTracker::new();
        // Unarmed → cannot tell.
        assert_eq!(tracker.silence_seconds(), None);

        tracker.arm(16_000);
        // Freshly armed, nothing seen yet: zero silence.
        assert_eq!(tracker.silence_seconds(), Some(0.0));

        // A loud chunk (RMS well over threshold) keeps silence at ~0.
        tracker.observe(&[0.5f32; 1_600]); // 0.1 s of audio
        assert_eq!(tracker.silence_seconds(), Some(0.0));

        // Subsequent silent chunks grow the silence timer: 3 × 0.1 s = 0.3 s.
        tracker.observe(&[0.0f32; 1_600]);
        tracker.observe(&[0.0f32; 1_600]);
        tracker.observe(&[0.0f32; 1_600]);
        let silence = tracker.silence_seconds().expect("armed");
        assert!(
            (silence - 0.3).abs() < 1e-6,
            "expected ~0.3 s silence, got {silence}"
        );

        // A loud chunk again snaps silence back to ~0.
        tracker.observe(&[0.4f32; 1_600]);
        assert_eq!(tracker.silence_seconds(), Some(0.0));
    }

    #[test]
    fn write_silence_appends_exactly_n_zero_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silence.wav");
        let mut sink = WavSink::create(&path, 16_000).unwrap();
        // Cross a block boundary to exercise the loop (BLOCK = 16_000).
        write_silence(&mut sink, 40_000).unwrap();
        assert_eq!(sink.samples_written(), 40_000);
        assert_eq!(sink.finalize().unwrap(), 40_000);
    }

    #[test]
    fn normal_system_track_run_reports_no_gaps() {
        // Regression: chunks pushed back-to-back (no real-time stall) must not
        // produce false gap markers.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sys.wav");
        let track = SystemTrackRecorder::create(&path, 16_000).unwrap();
        let sender = track.sender();
        for _ in 0..10 {
            sender.push(&[0.2f32; 1_600]);
        }
        let summary = track.finalize().unwrap();
        assert!(
            summary.gaps.is_empty(),
            "unexpected gaps: {:?}",
            summary.gaps
        );
    }

    #[test]
    fn externally_fed_track_reports_physical_silence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.wav");
        let track = SystemTrackRecorder::create(&path, 16_000).unwrap();
        let sender = track.sender();
        assert!(sender.push(&[0.4f32; 1_600]));
        assert!(sender.push(&[0.0f32; 1_600]));
        assert!(sender.push(&[0.0f32; 1_600]));

        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if track
                .silence_seconds()
                .is_some_and(|seconds| seconds >= 0.2)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "writer did not update activity tracker"
            );
            std::thread::yield_now();
        }
        track.finalize().unwrap();
    }

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
    fn fanout_delivers_timestamped_clone_when_subscribed() {
        let (tap, rx) = live_tap_channel("mic", Instant::now(), LIVE_TAP_CAPACITY);
        let sink: Option<SampleSink> = Some(tap);
        let chunk = [0.1f32, 0.2, 0.3];
        assert!(fanout_chunk(&sink, &chunk));
        let packet = rx.recv().unwrap();
        assert_eq!(packet.samples, vec![0.1, 0.2, 0.3]);
        assert!(packet.start_seconds >= 0.0);
    }

    #[test]
    fn fanout_is_noop_when_unsubscribed() {
        let sink: Option<SampleSink> = None;
        // No panic, no delivery, reports "not delivered".
        assert!(!fanout_chunk(&sink, &[0.0f32, 1.0]));
    }

    #[test]
    fn fanout_reports_false_when_receiver_dropped() {
        let (tap, rx) = live_tap_channel("mic", Instant::now(), LIVE_TAP_CAPACITY);
        drop(rx); // subscriber went away (e.g. streaming task ended)
        let sink: Option<SampleSink> = Some(tap);
        // Send fails but is swallowed — the recorder never breaks on a dead sink.
        assert!(!fanout_chunk(&sink, &[0.5f32]));
    }

    #[test]
    fn live_tap_timestamps_are_monotonic_on_the_shared_timeline() {
        let (tap, rx) = live_tap_channel("mic", Instant::now(), LIVE_TAP_CAPACITY);
        assert!(tap.push(&[0.1]));
        assert!(tap.push(&[0.2]));
        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        // Arrival-time stamping: non-negative and non-decreasing, regardless of
        // per-track frame counts.
        assert!(first.start_seconds >= 0.0);
        assert!(second.start_seconds >= first.start_seconds);
    }

    #[test]
    fn live_tap_drops_and_counts_when_full_without_blocking() {
        let (tap, rx) = live_tap_channel("system", Instant::now(), 2);
        // Fill the bounded channel, then keep pushing: the extra packets are
        // dropped (returning immediately) and counted, never blocking.
        assert!(tap.push(&[0.1]));
        assert!(tap.push(&[0.2]));
        assert!(!tap.push(&[0.3]));
        assert!(!tap.push(&[0.4]));
        assert!(!tap.push(&[0.5]));
        assert_eq!(tap.dropped(), 3);
        // The two accepted packets are intact; the rest never arrive.
        assert_eq!(rx.recv().unwrap().samples, vec![0.1]);
        assert_eq!(rx.recv().unwrap().samples, vec![0.2]);
        assert!(rx.try_recv().is_err());
        // Draining frees capacity again.
        assert!(tap.push(&[0.6]));
        assert_eq!(tap.dropped(), 3);
    }

    #[test]
    fn live_tap_ignores_empty_chunks() {
        let (tap, rx) = live_tap_channel("mic", Instant::now(), 2);
        assert!(!tap.push(&[]));
        assert!(rx.try_recv().is_err());
        assert_eq!(tap.dropped(), 0);
    }

    #[test]
    fn system_track_writes_pushed_chunks_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.system.wav");
        let track = SystemTrackRecorder::create(&path, 48_000).unwrap();
        assert_eq!(track.out_path(), path.as_path());
        let sender = track.sender();

        assert!(sender.push(&[0.0, 0.5, -0.5]));
        // Empty chunks are dropped without touching the writer.
        assert!(!sender.push(&[]));
        assert!(sender.push(&[1.0]));

        let summary = track.finalize().unwrap();
        assert_eq!(summary.wav_path, path);
        assert_eq!(summary.sample_rate, 48_000);
        assert!((summary.duration_seconds - 4.0 / 48_000.0).abs() < 1e-12);

        // The finalized file is a valid mono PCM16 WAV with all four samples.
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 48_000);
        let decoded: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[3], 32767);

        // Pushing after finalize reports false instead of erroring.
        assert!(!sender.push(&[0.1]));
    }

    #[test]
    fn system_track_drops_chunks_while_paused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paused.system.wav");
        let track = SystemTrackRecorder::create(&path, 16_000).unwrap();
        let sender = track.sender();

        assert!(sender.push(&[0.1, 0.2]));
        track.set_paused(true);
        // Paused: dropped, not written — no silent gap in the file.
        assert!(!sender.push(&[0.9; 100]));
        track.set_paused(false);
        assert!(sender.push(&[0.3]));

        let summary = track.finalize().unwrap();
        // Only the 3 unpaused samples made it to disk.
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len(), 3);
        assert!((summary.duration_seconds - 3.0 / 16_000.0).abs() < 1e-12);
    }

    /// Simulate a crash: write a placeholder header + `n_samples` of PCM but
    /// never call `finalize`, so the RIFF/`data` sizes stay `0`.
    fn write_unfinalized_wav(path: &Path, sample_rate: u32, n_samples: u64) {
        let file = File::create(path).unwrap();
        let mut writer = BufWriter::new(file);
        write_placeholder_header(&mut writer, sample_rate).unwrap();
        for i in 0..n_samples {
            // A deterministic non-zero body so the repaired file has real audio.
            let s = ((i % 100) as i16) - 50;
            writer.write_all(&s.to_le_bytes()).unwrap();
        }
        writer.flush().unwrap();
    }

    #[test]
    fn repair_backfills_lengths_of_an_unfinalized_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crashed.wav");
        let sample_rate = 16_000u32;
        let n_samples = 8_000u64; // 0.5s of mono 16-bit PCM
        write_unfinalized_wav(&path, sample_rate, n_samples);

        // Before repair the header lies: both sizes read 0.
        let before = std::fs::read(&path).unwrap();
        assert_eq!(read_u32_le(&before, 4), 0);
        assert_eq!(read_u32_le(&before, 40), 0);

        let repaired = repair_wav_header(&path).unwrap();
        let data_bytes = n_samples * 2;
        assert_eq!(repaired.sample_rate, sample_rate);
        assert_eq!(repaired.channels, 1);
        assert_eq!(repaired.data_bytes, data_bytes);
        assert!((repaired.duration_seconds - 0.5).abs() < 1e-9);

        // Header now carries the correct RIFF + data sizes.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(read_u32_le(&after, 4) as u64, 36 + data_bytes);
        assert_eq!(read_u32_le(&after, 40) as u64, data_bytes);
        assert_eq!(after.len() as u64, WAV_HEADER_LEN + data_bytes);

        // And a standard WAV reader can now open it and read every sample.
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate, sample_rate);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().bits_per_sample, 16);
        assert_eq!(reader.len() as u64, n_samples);
        let decoded: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(decoded.len() as u64, n_samples);
        assert_eq!(decoded[0], -50);
        assert_eq!(decoded[100], -50);
    }

    #[test]
    fn repair_is_idempotent_on_a_finalized_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.wav");
        let mut sink = WavSink::create(&path, 48_000).unwrap();
        sink.write_samples(&[0.0f32, 0.5, -0.5, 1.0]).unwrap();
        sink.finalize().unwrap();

        let good = std::fs::read(&path).unwrap();
        let repaired = repair_wav_header(&path).unwrap();
        assert_eq!(repaired.data_bytes, 8);
        // Re-deriving the sizes of an already-correct file is a no-op.
        assert_eq!(std::fs::read(&path).unwrap(), good);
    }

    #[test]
    fn repair_of_header_only_file_is_valid_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.wav");
        // Header only, no samples, never finalized.
        write_unfinalized_wav(&path, 16_000, 0);

        let repaired = repair_wav_header(&path).unwrap();
        assert_eq!(repaired.data_bytes, 0);
        assert_eq!(repaired.duration_seconds, 0.0);
        // Repairs to a valid empty WAV rather than erroring — the caller treats
        // `data_bytes == 0` as "no audio captured".
        assert!(hound::WavReader::open(&path).unwrap().len() == 0);
    }

    #[test]
    fn repair_rejects_a_truncated_or_non_wav_file() {
        let dir = tempfile::tempdir().unwrap();

        // Too small to even hold the 44-byte header.
        let tiny = dir.path().join("tiny.wav");
        std::fs::write(&tiny, b"RIFF").unwrap();
        let err = repair_wav_header(&tiny).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Full 44 bytes but not a WAV (bad markers).
        let bogus = dir.path().join("bogus.bin");
        std::fs::write(&bogus, vec![0u8; WAV_HEADER_LEN as usize]).unwrap();
        let err = repair_wav_header(&bogus).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
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
