//! System-AEC mic glue: VoiceProcessingIO capture → the meeting's mic WAV.
//!
//! Bridges the platform voice-processing capture
//! ([`lumen_platform_macos::VoiceProcessingInput`], the FaceTime-grade system
//! echo canceller) to the same streaming WAV machinery the recorder uses
//! ([`lumen_asr::SystemTrackRecorder`] — despite the name it is simply an
//! externally-fed mono WAV track with pause/finalize, exactly what a mic
//! track needs). Fan-out to the live worker, pause gating, WAV format, and
//! crash-recovery header repair are all identical to the plain path, so
//! everything downstream (live preview, offline pipeline, SampleClock,
//! sidecars) is unaffected by which capture backend produced the samples.
//!
//! **Best-effort by contract**: any failure to start returns `None` and the
//! caller records through the existing cpal path — a meeting recording never
//! fails because AEC could not engage. The trade-off motivating the config
//! opt-out (`meeting.mic_aec`): VPIO's bundled noise suppression may attenuate
//! quiet far-field speakers in a conference room (see the platform module docs).
//!
//! Dictation is untouched: this type is only ever driven by the meeting
//! commands.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lumen_asr::{LiveTapSender, RecordingSummary, SystemTrackRecorder, SystemTrackSender};
use lumen_platform_macos::{VoiceInputSink, VoiceProcessingInput};

/// One live VPIO→WAV session for the active meeting's mic track.
struct Session {
    capture: VoiceProcessingInput,
    track: SystemTrackRecorder,
}

/// App-state holder for the (at most one) AEC mic capture session.
#[derive(Default)]
pub struct MeetingMicAec {
    inner: Mutex<Option<Session>>,
}

impl MeetingMicAec {
    /// Whether this host exposes the VoiceProcessingIO unit at all. Cheap
    /// runtime probe, no side effects; `false` on non-macOS.
    pub fn is_supported() -> bool {
        lumen_platform_macos::voice_processing_supported()
    }

    /// Try to start the AEC mic capture into `out_path`. Returns the capture
    /// sample rate on success, or `None` when unavailable/failed — the caller
    /// then records through the plain cpal path (a warning is logged here).
    ///
    /// `live` optionally fans each mic chunk out to the real-time preview
    /// worker; only chunks the WAV writer accepted are forwarded, so the live
    /// feed respects pause/finalize exactly like the file does (same contract
    /// as the system-audio track).
    ///
    /// The capture must start before the WAV sink exists (it reports the
    /// client sample rate, which the WAV header needs), so the callback sink
    /// forwards through a late-bound slot; the handful of callbacks that can
    /// fire before the slot is filled are dropped (a few ms at session start).
    pub fn start(
        &self,
        device: Option<String>,
        out_path: PathBuf,
        live: Option<LiveTapSender>,
    ) -> Option<u32> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!("mic AEC session lock poisoned; falling back to cpal mic capture");
                return None;
            }
        };
        if guard.is_some() {
            tracing::warn!("mic AEC session already running; falling back to cpal mic capture");
            return None;
        }

        let slot: Arc<Mutex<Option<SystemTrackSender>>> = Arc::new(Mutex::new(None));
        let sink_slot = Arc::clone(&slot);
        let sink: VoiceInputSink = Arc::new(move |samples: &[f32]| {
            if let Ok(sender) = sink_slot.lock() {
                if let Some(sender) = sender.as_ref() {
                    // WAV first (authoritative). Its `push` returns `false`
                    // while paused/finalized, which also gates the live copy.
                    if sender.push(samples) {
                        if let Some(live) = live.as_ref() {
                            live.push(samples);
                        }
                    }
                }
            }
        });

        let mut capture = VoiceProcessingInput::new();
        let sample_rate = match capture.start(device.as_deref(), sink) {
            Ok(rate) => rate,
            Err(e) => {
                // Capability absent, permission problem, or an AudioUnit
                // failure — all fall back to the proven cpal path. This is
                // the designed degrade, so log-and-continue.
                tracing::warn!(error = %e, "voice-processing mic capture unavailable; falling back to cpal");
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
                    "could not create mic wav for AEC path; falling back to cpal"
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
            "meeting mic recording started via VoiceProcessingIO (system AEC)"
        );
        *guard = Some(Session { capture, track });
        Some(sample_rate)
    }

    /// Pause/resume the AEC mic track (paused chunks are dropped, no silent
    /// gap — same as the cpal recorder). Returns `true` iff an AEC session is
    /// active, so the caller knows whether the cpal recorder needs the call
    /// instead.
    pub fn set_paused(&self, paused: bool) -> bool {
        if let Ok(guard) = self.inner.lock() {
            if let Some(session) = guard.as_ref() {
                session.track.set_paused(paused);
                return true;
            }
        }
        false
    }

    /// Stop the capture and finalize the mic WAV.
    ///
    /// - `None`: no AEC session was running (the recording used the cpal
    ///   path; stop that instead).
    /// - `Some(Ok(summary))`: the AEC-captured mic track, finalized.
    /// - `Some(Err(reason))`: an AEC session existed but the finalize failed —
    ///   the caller treats this exactly like a cpal recorder stop failure.
    pub fn stop(&self) -> Option<Result<RecordingSummary, String>> {
        let session = match self.inner.lock() {
            Ok(mut guard) => guard.take()?,
            Err(_) => {
                tracing::warn!("mic AEC session lock poisoned on stop");
                return None;
            }
        };
        let Session { mut capture, track } = session;
        // Stop the unit first so no callback races the writer finalize.
        capture.stop();
        match track.finalize() {
            Ok(summary) => {
                tracing::info!(
                    duration_seconds = summary.duration_seconds,
                    path = %summary.wav_path.display(),
                    "AEC mic track finalized"
                );
                Some(Ok(summary))
            }
            Err(e) => {
                tracing::warn!(error = %e, "AEC mic wav finalize failed");
                Some(Err(format!("mic wav finalize failed: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_supported_probe_never_panics() {
        let _ = MeetingMicAec::is_supported();
    }

    #[test]
    fn stop_and_pause_without_session_are_noops() {
        let aec = MeetingMicAec::default();
        // No session: stop reports "not the active path" (None) and pause
        // reports the cpal recorder should handle it (false).
        assert!(aec.stop().is_none());
        assert!(!aec.set_paused(true));
        assert!(!aec.set_paused(false));
        // Idempotent.
        assert!(aec.stop().is_none());
    }
}
