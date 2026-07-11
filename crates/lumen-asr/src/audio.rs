//! Microphone capture (cpal) and resampling helpers.
//!
//! `cpal::Stream` is `!Send` on macOS, so the live stream lives on a dedicated
//! audio thread. AppState only holds Send/Sync control handles.
//!
//! **Important (macOS):** CoreAudio input callbacks can keep firing briefly (or
//! longer) after `cpal::Stream` is dropped. If every session shares one sample
//! buffer, those "zombie" callbacks stack on the next recording and the audio
//! sounds time-stretched by ~N× (N = number of live streams). We isolate each
//! session with its own buffer + an epoch so stale callbacks cannot pollute
//! the active capture.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
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
    #[error("audio thread unavailable")]
    ThreadGone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct CaptureResult {
    /// Mono f32 samples in [-1, 1].
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

enum AudioCmd {
    Start {
        device: Option<String>,
        reply: Sender<Result<(), AudioError>>,
    },
    Stop {
        reply: Sender<Result<CaptureResult, AudioError>>,
    },
}

/// Cross-platform mic capture manager (Send + Sync).
pub struct AudioCapture {
    recording: Arc<AtomicBool>,
    preferred_device: Arc<Mutex<Option<String>>>,
    cmd_tx: Mutex<Option<Sender<AudioCmd>>>,
}

impl Default for AudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCapture {
    pub fn new() -> Self {
        let recording = Arc::new(AtomicBool::new(false));
        let preferred_device = Arc::new(Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AudioCmd>();
        let rec_flag = Arc::clone(&recording);
        // Shared only as a pointer slot; each Start installs a fresh buffer.
        let samples_slot: Arc<Mutex<Option<Arc<Mutex<Vec<f32>>>>>> =
            Arc::new(Mutex::new(None));
        let sample_rate = Arc::new(AtomicU32::new(0));
        let epoch = Arc::new(AtomicU64::new(0));
        let rate_flag = Arc::clone(&sample_rate);
        let samples_for_thread = Arc::clone(&samples_slot);
        let epoch_for_thread = Arc::clone(&epoch);

        thread::Builder::new()
            .name("lumen-audio".into())
            .spawn(move || {
                // Stream must live on this thread.
                let mut stream_slot: Option<cpal::Stream> = None;
                let mut started_at: Option<Instant> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        AudioCmd::Start { device, reply } => {
                            let res = start_on_thread(
                                device,
                                &rec_flag,
                                &rate_flag,
                                &epoch_for_thread,
                                &samples_for_thread,
                                &mut stream_slot,
                            );
                            if res.is_ok() {
                                started_at = Some(Instant::now());
                            } else {
                                started_at = None;
                            }
                            let _ = reply.send(res);
                        }
                        AudioCmd::Stop { reply } => {
                            // Invalidate callbacks first so any zombie stream
                            // still draining from a prior Drop cannot append.
                            epoch_for_thread.fetch_add(1, Ordering::SeqCst);
                            shutdown_stream(&mut stream_slot);
                            // macOS CoreAudio can glitch if we re-open immediately;
                            // also gives in-flight callbacks a moment to exit.
                            thread::sleep(std::time::Duration::from_millis(60));
                            rec_flag.store(false, Ordering::SeqCst);
                            let sample_rate = rate_flag.load(Ordering::SeqCst);
                            let samples = samples_for_thread
                                .lock()
                                .take()
                                .map(|buf| std::mem::take(&mut *buf.lock()))
                                .unwrap_or_default();
                            let wall_ms = started_at
                                .take()
                                .map(|t| t.elapsed().as_millis() as u64)
                                .unwrap_or(0);
                            let audio_ms = if sample_rate > 0 {
                                (samples.len() as u64 * 1000) / sample_rate as u64
                            } else {
                                0
                            };
                            let ratio = if wall_ms > 50 {
                                audio_ms as f64 / wall_ms as f64
                            } else {
                                0.0
                            };
                            if ratio > 1.25 {
                                tracing::warn!(
                                    n = samples.len(),
                                    sample_rate,
                                    wall_ms,
                                    audio_ms,
                                    ratio,
                                    "recording stopped with stretched sample count (zombie stream?)"
                                );
                            } else {
                                tracing::info!(
                                    n = samples.len(),
                                    sample_rate,
                                    wall_ms,
                                    audio_ms,
                                    ratio,
                                    "recording stopped"
                                );
                            }
                            let _ = reply.send(Ok(CaptureResult {
                                samples,
                                sample_rate,
                            }));
                        }
                    }
                }
            })
            .expect("spawn audio thread");

        Self {
            recording,
            preferred_device,
            cmd_tx: Mutex::new(Some(tx)),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn set_device(&self, name: Option<String>) {
        *self.preferred_device.lock() = name;
    }

    pub fn list_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
        let host = cpal::default_host();
        let default_name = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();

        let mut out = Vec::new();
        let devices = host
            .input_devices()
            .map_err(|e| AudioError::Device(e.to_string()))?;
        for d in devices {
            if let Ok(name) = d.name() {
                let is_default = name == default_name;
                out.push(AudioDeviceInfo { name, is_default });
            }
        }
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
        Ok(out)
    }

    pub fn start(&self) -> Result<(), AudioError> {
        if self.recording.load(Ordering::SeqCst) {
            return Err(AudioError::AlreadyRecording);
        }
        let device = self.preferred_device.lock().clone();
        let (reply_tx, reply_rx) = mpsc::channel();
        let tx = self.cmd_tx.lock().clone().ok_or(AudioError::ThreadGone)?;
        tx.send(AudioCmd::Start {
            device,
            reply: reply_tx,
        })
        .map_err(|_| AudioError::ThreadGone)?;
        reply_rx.recv().map_err(|_| AudioError::ThreadGone)?
    }

    pub fn stop(&self) -> Result<CaptureResult, AudioError> {
        if !self.recording.load(Ordering::SeqCst) {
            return Err(AudioError::NotRecording);
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let tx = self.cmd_tx.lock().clone().ok_or(AudioError::ThreadGone)?;
        tx.send(AudioCmd::Stop { reply: reply_tx })
            .map_err(|_| AudioError::ThreadGone)?;
        reply_rx.recv().map_err(|_| AudioError::ThreadGone)?
    }
}

fn shutdown_stream(stream_slot: &mut Option<cpal::Stream>) {
    if let Some(stream) = stream_slot.take() {
        // Prefer an explicit pause so CoreAudio stops invoking the callback
        // before the Stream (and its callback Arc) is released.
        if let Err(e) = stream.pause() {
            tracing::warn!(error = %e, "audio stream pause failed");
        }
        drop(stream);
    }
}

fn start_on_thread(
    preferred: Option<String>,
    recording: &AtomicBool,
    sample_rate_atom: &AtomicU32,
    epoch: &Arc<AtomicU64>,
    samples_slot: &Arc<Mutex<Option<Arc<Mutex<Vec<f32>>>>>>,
    stream_slot: &mut Option<cpal::Stream>,
) -> Result<(), AudioError> {
    if recording.swap(true, Ordering::SeqCst) {
        return Err(AudioError::AlreadyRecording);
    }

    // Defensive: never leave a previous stream alive across sessions.
    epoch.fetch_add(1, Ordering::SeqCst);
    shutdown_stream(stream_slot);
    *samples_slot.lock() = None;

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
            return Err(AudioError::Device(e.to_string()));
        }
    };

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    sample_rate_atom.store(sample_rate, Ordering::SeqCst);

    // Fresh buffer + epoch for this session only.
    let session_epoch = epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let samples_buf = Arc::new(Mutex::new(Vec::with_capacity(
        sample_rate as usize * 30, // ~30s headroom
    )));
    *samples_slot.lock() = Some(Arc::clone(&samples_buf));

    let samples_cb = Arc::clone(&samples_buf);
    let epoch_cb = Arc::clone(epoch);
    let stream_config: StreamConfig = config.clone().into();
    let err_fn = |e| tracing::error!(error = %e, "audio stream error");

    let stream = match config.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(
            &device,
            &stream_config,
            channels,
            samples_cb,
            epoch_cb,
            session_epoch,
            err_fn,
        ),
        SampleFormat::I16 => build_stream::<i16>(
            &device,
            &stream_config,
            channels,
            samples_cb,
            epoch_cb,
            session_epoch,
            err_fn,
        ),
        SampleFormat::U16 => build_stream::<u16>(
            &device,
            &stream_config,
            channels,
            samples_cb,
            epoch_cb,
            session_epoch,
            err_fn,
        ),
        other => {
            recording.store(false, Ordering::SeqCst);
            *samples_slot.lock() = None;
            return Err(AudioError::Stream(format!(
                "unsupported sample format: {other:?}"
            )));
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            *samples_slot.lock() = None;
            return Err(e);
        }
    };

    if let Err(e) = stream.play() {
        recording.store(false, Ordering::SeqCst);
        *samples_slot.lock() = None;
        return Err(AudioError::Stream(e.to_string()));
    }

    *stream_slot = Some(stream);
    tracing::info!(sample_rate, channels, session_epoch, "recording started");
    Ok(())
}

fn resolve_device(preferred: Option<String>) -> Result<Device, AudioError> {
    let host = cpal::default_host();
    if let Some(name) = preferred {
        let devices = host
            .input_devices()
            .map_err(|e| AudioError::Device(e.to_string()))?;
        for d in devices {
            if d.name().ok().as_deref() == Some(name.as_str()) {
                return Ok(d);
            }
        }
        tracing::warn!(%name, "preferred device not found, using default");
    }
    host.default_input_device().ok_or(AudioError::NoDevice)
}

fn build_stream<T>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    samples: Arc<Mutex<Vec<f32>>>,
    epoch: Arc<AtomicU64>,
    session_epoch: u64,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError>
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
                let mut buf = samples.lock();
                if channels <= 1 {
                    for &s in data {
                        buf.push(s.to_sample::<f32>());
                    }
                } else {
                    for frame in data.chunks(channels) {
                        let mut sum = 0.0f32;
                        for &s in frame {
                            sum += s.to_sample::<f32>();
                        }
                        buf.push(sum / channels as f32);
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
}

/// Linear resample mono f32 to `target_hz`.
pub fn resample_linear(samples: &[f32], from_hz: u32, target_hz: u32) -> Vec<f32> {
    if samples.is_empty() || from_hz == 0 {
        return Vec::new();
    }
    if from_hz == target_hz {
        return samples.to_vec();
    }
    let ratio = from_hz as f64 / target_hz as f64;
    let out_len = ((samples.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(samples.len() - 1);
        let t = (src - i0 as f64) as f32;
        let v = samples[i0] * (1.0 - t) + samples[i1] * t;
        out.push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_identity() {
        let s = vec![0.0, 0.5, 1.0];
        assert_eq!(resample_linear(&s, 16000, 16000), s);
    }

    #[test]
    fn resample_down() {
        let s = vec![0.0, 1.0, 0.0, -1.0];
        let out = resample_linear(&s, 32000, 16000);
        assert!(out.len() >= 2);
    }
}
