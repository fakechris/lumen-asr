//! Dual-track glue: Core Audio system-audio tap → second meeting WAV track.
//!
//! Bridges the platform capture ([`lumen_platform_macos::SystemAudioCapture`],
//! macOS 14.2+ process tap, capability-gated) to the recorder's second track
//! ([`lumen_asr::SystemTrackRecorder`], the same streaming WAV machinery as
//! the mic path). Everything here is **best-effort**: any failure to start,
//! feed, or finalize the system track degrades the meeting to mic-only with a
//! warning — it must never fail or interrupt the microphone recording.
//!
//! Cross-platform by construction: on non-macOS (and on macOS without the
//! capability) `SystemAudioCapture::start` reports `Unsupported`, so
//! [`MeetingSystemAudio::start`] returns `None` and the recording is exactly
//! the pre-dual-track mic-only path.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lumen_asr::{LiveTapSender, RecordingSummary, SystemTrackRecorder, SystemTrackSender};
use lumen_platform_macos::{SystemAudioCapture, SystemAudioSink};

/// One live tap→WAV session for the active meeting recording.
struct Session {
    capture: SystemAudioCapture,
    track: SystemTrackRecorder,
}

/// App-state holder for the (at most one) system-audio track session.
#[derive(Default)]
pub struct MeetingSystemAudio {
    inner: Mutex<Option<Session>>,
}

impl MeetingSystemAudio {
    /// Whether this host can capture system audio at all (macOS 14.2+ with the
    /// process-tap API present). Cheap runtime probe, no side effects.
    pub fn capability_available() -> bool {
        lumen_platform_macos::system_audio_capability_available()
    }

    /// Try to start the system-audio track into `out_path`. Returns the tap
    /// sample rate on success, or `None` when unavailable/failed — the caller
    /// records mic-only in that case (a warning is logged here).
    ///
    /// `live` optionally fans a copy of each tap chunk out to the real-time
    /// preview worker (bounded, non-blocking, timestamped on the meeting's
    /// unified timeline). Only chunks the WAV writer accepted are forwarded,
    /// so the live feed respects pause/finalize exactly like the file does.
    ///
    /// The tap must be started before the WAV sink exists (the tap reports its
    /// native sample rate, which the WAV header needs), so the capture sink
    /// forwards through a late-bound slot; the handful of callbacks that can
    /// fire before the slot is filled are dropped (a few ms at session start).
    pub fn start(&self, out_path: PathBuf, live: Option<LiveTapSender>) -> Option<u32> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!("system audio session lock poisoned; recording mic-only");
                return None;
            }
        };
        if guard.is_some() {
            tracing::warn!("system audio session already running; recording mic-only");
            return None;
        }

        let slot: Arc<Mutex<Option<SystemTrackSender>>> = Arc::new(Mutex::new(None));
        let sink_slot = Arc::clone(&slot);
        let sink: SystemAudioSink = Arc::new(move |samples: &[f32]| {
            if let Ok(sender) = sink_slot.lock() {
                if let Some(sender) = sender.as_ref() {
                    // WAV first (authoritative). Its `push` returns `false`
                    // while paused/finalized, which also gates the live copy —
                    // the preview never hears audio the file did not keep.
                    if sender.push(samples) {
                        if let Some(live) = live.as_ref() {
                            live.push(samples);
                        }
                    }
                }
            }
        });

        let mut capture = SystemAudioCapture::new();
        let sample_rate = match capture.start(sink) {
            Ok(rate) => rate,
            Err(e) => {
                // Capability absent, permission denied, or a HAL failure —
                // all degrade to mic-only. This is the designed fallback, so
                // log-and-continue rather than surfacing an error.
                tracing::warn!(error = %e, "system audio capture unavailable; recording mic-only");
                return None;
            }
        };

        let track = match SystemTrackRecorder::create(&out_path, sample_rate) {
            Ok(track) => track,
            Err(e) => {
                capture.stop();
                tracing::warn!(
                    error = %e,
                    path = %out_path.display(),
                    "could not create system audio wav; recording mic-only"
                );
                return None;
            }
        };
        if let Ok(mut sender) = slot.lock() {
            *sender = Some(track.sender());
        }

        tracing::info!(
            sample_rate,
            path = %out_path.display(),
            "system audio track recording started"
        );
        *guard = Some(Session { capture, track });
        Some(sample_rate)
    }

    /// Pause/resume the system track in lockstep with the mic recorder, so
    /// paused wall-clock time is dropped from both timelines equally.
    pub fn set_paused(&self, paused: bool) {
        if let Ok(guard) = self.inner.lock() {
            if let Some(session) = guard.as_ref() {
                session.track.set_paused(paused);
            }
        }
    }

    /// Stop the tap and finalize the system WAV. Returns the finalized track
    /// summary, or `None` when no session was running or the finalize failed
    /// (logged) — mirroring the best-effort contract of the whole track.
    pub fn stop(&self) -> Option<RecordingSummary> {
        let session = match self.inner.lock() {
            Ok(mut guard) => guard.take()?,
            Err(_) => {
                tracing::warn!("system audio session lock poisoned on stop");
                return None;
            }
        };
        let Session { mut capture, track } = session;
        // Stop the tap first so no callback races the writer finalize.
        capture.stop();
        match track.finalize() {
            Ok(summary) => {
                tracing::info!(
                    duration_seconds = summary.duration_seconds,
                    path = %summary.wav_path.display(),
                    "system audio track finalized"
                );
                Some(summary)
            }
            Err(e) => {
                tracing::warn!(error = %e, "system audio wav finalize failed");
                None
            }
        }
    }
}
