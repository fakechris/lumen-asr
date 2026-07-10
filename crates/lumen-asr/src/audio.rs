//! Microphone capture (cpal) and resampling helpers.
//!
//! `cpal::Stream` is `!Send` on macOS, so the live stream lives on a dedicated
//! audio thread. AppState only holds Send/Sync control handles.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
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
        let sample_rate = Arc::new(AtomicU32::new(0));
        let samples_buf = Arc::new(Mutex::new(Vec::new()));
        let rate_flag = Arc::clone(&sample_rate);
        let samples_for_thread = Arc::clone(&samples_buf);

        thread::Builder::new()
            .name("lumen-audio".into())
            .spawn(move || {
                // Stream must live on this thread.
                let mut stream_slot: Option<cpal::Stream> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        AudioCmd::Start { device, reply } => {
                            let res = start_on_thread(
                                device,
                                &rec_flag,
                                &rate_flag,
                                &samples_for_thread,
                                &mut stream_slot,
                            );
                            let _ = reply.send(res);
                        }
                        AudioCmd::Stop { reply } => {
                            // Drop stream first (waits for callback), then take buffer.
                            stream_slot = None;
                            // macOS CoreAudio can glitch if we re-open immediately.
                            thread::sleep(std::time::Duration::from_millis(40));
                            rec_flag.store(false, Ordering::SeqCst);
                            let sample_rate = rate_flag.load(Ordering::SeqCst);
                            let samples = std::mem::take(&mut *samples_for_thread.lock());
                            tracing::info!(n = samples.len(), sample_rate, "recording stopped");
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

fn start_on_thread(
    preferred: Option<String>,
    recording: &AtomicBool,
    sample_rate_atom: &AtomicU32,
    samples: &Arc<Mutex<Vec<f32>>>,
    stream_slot: &mut Option<cpal::Stream>,
) -> Result<(), AudioError> {
    if recording.swap(true, Ordering::SeqCst) {
        return Err(AudioError::AlreadyRecording);
    }

    let device = resolve_device(preferred)?;
    let config = device
        .default_input_config()
        .map_err(|e| {
            recording.store(false, Ordering::SeqCst);
            AudioError::Device(e.to_string())
        })?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    sample_rate_atom.store(sample_rate, Ordering::SeqCst);
    samples.lock().clear();

    let samples_cb = Arc::clone(samples);
    let stream_config: StreamConfig = config.clone().into();
    let err_fn = |e| tracing::error!(error = %e, "audio stream error");

    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            build_stream::<f32>(&device, &stream_config, channels, samples_cb, err_fn)
        }
        SampleFormat::I16 => {
            build_stream::<i16>(&device, &stream_config, channels, samples_cb, err_fn)
        }
        SampleFormat::U16 => {
            build_stream::<u16>(&device, &stream_config, channels, samples_cb, err_fn)
        }
        other => {
            recording.store(false, Ordering::SeqCst);
            return Err(AudioError::Stream(format!(
                "unsupported sample format: {other:?}"
            )));
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };

    if let Err(e) = stream.play() {
        recording.store(false, Ordering::SeqCst);
        return Err(AudioError::Stream(e.to_string()));
    }

    *stream_slot = Some(stream);
    tracing::info!(sample_rate, channels, "recording started");
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
