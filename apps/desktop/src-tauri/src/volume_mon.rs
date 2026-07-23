//! Live mic level for onboarding “say a word” (no ASR).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeLevel {
    pub rms: f32,
    pub peak: f32,
    pub device: String,
}

struct MonitorState {
    stop: Arc<AtomicBool>,
}

static MONITOR: Mutex<Option<MonitorState>> = Mutex::new(None);

pub fn stop_volume_monitoring() {
    let mut g = MONITOR.lock();
    if let Some(m) = g.take() {
        m.stop.store(true, Ordering::SeqCst);
    }
}

/// Start a short-lived input stream that emits `volume-level` ~15–20 times/sec.
pub fn start_volume_monitoring(app: AppHandle, device_name: Option<String>) -> Result<(), String> {
    stop_volume_monitoring();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut g = MONITOR.lock();
        *g = Some(MonitorState {
            stop: Arc::clone(&stop),
        });
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    thread::Builder::new()
        .name("lumen-volume-mon".into())
        .spawn(move || {
            if let Err(e) = run_monitor(app, device_name, stop, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(r) => r,
        Err(_) => Err("volume monitor failed to start".into()),
    }
}

fn run_monitor(
    app: AppHandle,
    preferred: Option<String>,
    stop: Arc<AtomicBool>,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = resolve_device(&host, preferred.as_deref())?;
    let device_label = device.name().unwrap_or_else(|_| "default".into());
    let config = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    let channels = config.channels() as usize;
    let stream_config: StreamConfig = config.clone().into();

    let peak_shared = Arc::new(Mutex::new(0.0f32));
    let rms_acc = Arc::new(Mutex::new((0.0f64, 0u64))); // sum_sq, n
    let peak_cb = Arc::clone(&peak_shared);
    let rms_cb = Arc::clone(&rms_acc);

    let err_fn = |e| tracing::warn!(error = %e, "volume monitor stream error");

    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            build_level_stream::<f32>(&device, &stream_config, channels, peak_cb, rms_cb, err_fn)?
        }
        SampleFormat::I16 => {
            build_level_stream::<i16>(&device, &stream_config, channels, peak_cb, rms_cb, err_fn)?
        }
        SampleFormat::U16 => {
            build_level_stream::<u16>(&device, &stream_config, channels, peak_cb, rms_cb, err_fn)?
        }
        other => return Err(format!("unsupported sample format: {other:?}")),
    };

    stream
        .play()
        .map_err(|e| format!("volume monitor play: {e}"))?;
    let _ = ready_tx.send(Ok(()));
    tracing::info!(device = %device_label, "volume monitoring started");

    let mut last_emit = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
        if last_emit.elapsed() < Duration::from_millis(60) {
            continue;
        }
        last_emit = Instant::now();

        let peak = {
            let mut p = peak_shared.lock();
            let v = *p;
            *p = 0.0;
            v
        };
        let (sum_sq, n) = {
            let mut a = rms_acc.lock();
            let v = *a;
            *a = (0.0, 0);
            v
        };
        let rms = if n > 0 {
            (sum_sq / n as f64).sqrt() as f32
        } else {
            0.0
        };

        let _ = app.emit(
            "volume-level",
            VolumeLevel {
                rms,
                peak,
                device: device_label.clone(),
            },
        );
    }

    drop(stream);
    tracing::info!("volume monitoring stopped");
    Ok(())
}

fn resolve_device(host: &cpal::Host, preferred: Option<&str>) -> Result<cpal::Device, String> {
    if let Some(name) = preferred {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.name().ok().as_deref() == Some(name) {
                    return Ok(d);
                }
            }
        }
    }
    host.default_input_device()
        .ok_or_else(|| "no input device".into())
}

fn build_level_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    peak: Arc<Mutex<f32>>,
    rms_acc: Arc<Mutex<(f64, u64)>>,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: Sample + SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = channels.max(1);
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut local_peak = 0.0f32;
                let mut sum_sq = 0.0f64;
                let mut n = 0u64;
                for frame in data.chunks(channels) {
                    let mut mono = 0.0f32;
                    for s in frame {
                        mono += f32::from_sample(*s);
                    }
                    mono /= channels as f32;
                    let a = mono.abs();
                    if a > local_peak {
                        local_peak = a;
                    }
                    sum_sq += (mono as f64) * (mono as f64);
                    n += 1;
                }
                {
                    let mut p = peak.lock();
                    if local_peak > *p {
                        *p = local_peak;
                    }
                }
                {
                    let mut a = rms_acc.lock();
                    a.0 += sum_sq;
                    a.1 += n;
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start_volume_monitoring_cmd(app: AppHandle, device: Option<String>) -> Result<(), String> {
    start_volume_monitoring(app, device)
}

#[tauri::command]
pub fn stop_volume_monitoring_cmd() -> Result<(), String> {
    stop_volume_monitoring();
    Ok(())
}
