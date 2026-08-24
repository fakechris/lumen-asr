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
//! fails because AEC could not engage. The production meeting path currently
//! safety-gates this backend off: VPIO's bundled noise suppression can
//! attenuate quiet far-field speakers, and a mid-session callback stall cannot
//! transfer the same authoritative WAV safely to cpal.
//!
//! Dictation is untouched: this type is only ever driven by the meeting
//! commands.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumen_asr::{LiveTapSender, RecordingSummary, SystemTrackRecorder, SystemTrackSender};
use lumen_platform_macos::{VoiceInputSink, VoiceProcessingError, VoiceProcessingInput};

const STARTUP_AUDIO_TIMEOUT: Duration = Duration::from_millis(1_500);
const READY_AUDIO_MILLIS: u32 = 100;
const READY_CALLBACKS: u32 = 3;
const STARTUP_PROBING: u8 = 0;
const STARTUP_ACCEPTED: u8 = 1;
const STARTUP_REJECTED: u8 = 2;

/// Narrow system-boundary interface for the Core Audio capture lifecycle.
trait VoiceCaptureBackend: Send {
    fn start(
        &mut self,
        preferred_device: Option<&str>,
        sink: VoiceInputSink,
    ) -> Result<u32, VoiceProcessingError>;
    fn stop(&mut self);
}

impl VoiceCaptureBackend for VoiceProcessingInput {
    fn start(
        &mut self,
        preferred_device: Option<&str>,
        sink: VoiceInputSink,
    ) -> Result<u32, VoiceProcessingError> {
        VoiceProcessingInput::start(self, preferred_device, sink)
    }

    fn stop(&mut self) {
        VoiceProcessingInput::stop(self);
    }
}

/// One live VPIO→WAV session for the active meeting's mic track.
struct Session {
    capture: Box<dyn VoiceCaptureBackend>,
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
        self.start_with_capture(
            device,
            out_path,
            live,
            Box::new(VoiceProcessingInput::new()),
            STARTUP_AUDIO_TIMEOUT,
        )
    }

    fn start_with_capture(
        &self,
        device: Option<String>,
        out_path: PathBuf,
        live: Option<LiveTapSender>,
        mut capture: Box<dyn VoiceCaptureBackend>,
        first_audio_timeout: Duration,
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
        let startup_state = Arc::new(std::sync::atomic::AtomicU8::new(STARTUP_PROBING));
        let sink_startup_state = Arc::clone(&startup_state);
        let accepted_samples = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink_accepted_samples = Arc::clone(&accepted_samples);
        let accepted_callbacks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let sink_accepted_callbacks = Arc::clone(&accepted_callbacks);
        let (readiness_tx, readiness_rx) = std::sync::mpsc::sync_channel(1);
        let sink: VoiceInputSink = Arc::new(move |samples: &[f32]| {
            if let Ok(sender) = sink_slot.lock() {
                if let Some(sender) = sender.as_ref() {
                    // WAV first (authoritative). Its `push` returns `false`
                    // while paused/finalized, which also gates the live copy.
                    if sender.push(samples) {
                        // Probe only until startup is proven healthy. Requiring
                        // several callbacks plus a minimum amount of audio
                        // rejects a unit that emits one startup buffer and then
                        // stalls, while keeping the steady-state callback free
                        // of readiness-channel work.
                        let state = sink_startup_state.load(std::sync::atomic::Ordering::Acquire);
                        if state == STARTUP_PROBING {
                            sink_accepted_samples.fetch_add(
                                samples.len() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            sink_accepted_callbacks
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let _ = readiness_tx.try_send(());
                        }
                        // Probe audio belongs to a backend that may still be
                        // rejected. Do not let it enter the live transcript
                        // until AEC is committed as the authoritative mic path.
                        if state == STARTUP_ACCEPTED {
                            if let Some(live) = live.as_ref() {
                                live.push(samples);
                            }
                        }
                    }
                }
            }
        });

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

        let ready_samples =
            u64::from(sample_rate).saturating_mul(u64::from(READY_AUDIO_MILLIS)) / 1_000;
        let deadline = Instant::now() + first_audio_timeout;
        let ready = loop {
            let samples = accepted_samples.load(std::sync::atomic::Ordering::Relaxed);
            let callbacks = accepted_callbacks.load(std::sync::atomic::Ordering::Relaxed);
            if samples >= ready_samples && callbacks >= READY_CALLBACKS {
                break true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break false;
            };
            if readiness_rx.recv_timeout(remaining).is_err() {
                break false;
            }
        };
        if !ready {
            startup_state.store(STARTUP_REJECTED, std::sync::atomic::Ordering::Release);
            // Tear both resources down before the caller opens the same path
            // through cpal. This covers the observed field failure where
            // AudioOutputUnitStart succeeded but no sustained callback stream
            // ever arrived.
            capture.stop();
            if let Err(error) = track.finalize() {
                tracing::warn!(%error, "could not finalize silent AEC probe track");
            }
            if let Err(error) = std::fs::remove_file(&out_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, path = %out_path.display(), "could not remove rejected AEC probe track");
                }
            }
            tracing::warn!(
                timeout_ms = first_audio_timeout.as_millis(),
                callbacks = accepted_callbacks.load(std::sync::atomic::Ordering::Relaxed),
                samples = accepted_samples.load(std::sync::atomic::Ordering::Relaxed),
                "voice-processing mic started without sustained audio callbacks; falling back to cpal"
            );
            return None;
        }

        startup_state.store(STARTUP_ACCEPTED, std::sync::atomic::Ordering::Release);

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

    /// Seconds since the AEC microphone path last carried physical audio above
    /// the room-noise threshold. `None` means this backend is not active.
    pub fn silence_seconds(&self) -> Option<f64> {
        self.inner.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|session| session.track.silence_seconds())
        })
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
    use lumen_asr::live_tap_channel;
    use lumen_platform_macos::VoiceProcessingError;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct SilentCapture {
        stopped: Arc<AtomicBool>,
        sink: Option<VoiceInputSink>,
    }

    #[derive(Default)]
    struct ActiveCapture {
        stopped: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    struct OneShotCapture {
        stopped: Arc<AtomicBool>,
        sink: Option<VoiceInputSink>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    struct FailingCapture;

    struct StartTrackingCapture {
        started: Arc<AtomicBool>,
    }

    impl VoiceCaptureBackend for SilentCapture {
        fn start(
            &mut self,
            _preferred_device: Option<&str>,
            sink: VoiceInputSink,
        ) -> Result<u32, VoiceProcessingError> {
            // Retain the callback without invoking it. This models the field
            // failure: Core Audio reported a successful start and kept the
            // unit alive, but no input callback ever arrived.
            self.sink = Some(sink);
            Ok(44_100)
        }

        fn stop(&mut self) {
            self.sink = None;
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    impl VoiceCaptureBackend for ActiveCapture {
        fn start(
            &mut self,
            _preferred_device: Option<&str>,
            sink: VoiceInputSink,
        ) -> Result<u32, VoiceProcessingError> {
            let stopped = Arc::clone(&self.stopped);
            self.worker = Some(std::thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    sink(&vec![0.25; 441]);
                    std::thread::sleep(Duration::from_millis(2));
                }
            }));
            Ok(44_100)
        }

        fn stop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    impl VoiceCaptureBackend for OneShotCapture {
        fn start(
            &mut self,
            _preferred_device: Option<&str>,
            sink: VoiceInputSink,
        ) -> Result<u32, VoiceProcessingError> {
            self.sink = Some(Arc::clone(&sink));
            self.worker = Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2));
                sink(&vec![0.25; 441]);
            }));
            Ok(44_100)
        }

        fn stop(&mut self) {
            self.sink = None;
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    impl VoiceCaptureBackend for FailingCapture {
        fn start(
            &mut self,
            _preferred_device: Option<&str>,
            _sink: VoiceInputSink,
        ) -> Result<u32, VoiceProcessingError> {
            Err(VoiceProcessingError::Unsupported)
        }

        fn stop(&mut self) {
            panic!("a backend that never started must not be stopped");
        }
    }

    impl VoiceCaptureBackend for StartTrackingCapture {
        fn start(
            &mut self,
            _preferred_device: Option<&str>,
            _sink: VoiceInputSink,
        ) -> Result<u32, VoiceProcessingError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(44_100)
        }

        fn stop(&mut self) {}
    }

    fn temp_wav(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("lumen-{name}-{nonce}.wav"))
    }

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

    #[test]
    fn start_fails_when_voice_processor_delivers_no_audio() {
        let path = temp_wav("silent-aec");
        let stopped = Arc::new(AtomicBool::new(false));
        let aec = MeetingMicAec::default();

        let result = aec.start_with_capture(
            None,
            path.clone(),
            None,
            Box::new(SilentCapture {
                stopped: Arc::clone(&stopped),
                sink: None,
            }),
            Duration::from_millis(25),
        );

        assert_eq!(result, None);
        assert!(stopped.load(Ordering::SeqCst));
        assert!(aec.stop().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn start_succeeds_after_audio_reaches_the_writer() {
        let path = temp_wav("active-aec");
        let aec = MeetingMicAec::default();
        let (live, live_rx) = live_tap_channel("mic", Instant::now(), 2);

        let result = aec.start_with_capture(
            None,
            path.clone(),
            Some(live),
            Box::new(ActiveCapture::default()),
            Duration::from_millis(200),
        );

        assert_eq!(result, Some(44_100));
        let packet = live_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(packet.samples, vec![0.25; 441]);
        let summary = aec.stop().unwrap().unwrap();
        assert_eq!(summary.sample_rate, 44_100);
        assert!(summary.duration_seconds > 0.0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn one_startup_buffer_then_stall_falls_back() {
        let path = temp_wav("one-shot-aec");
        let stopped = Arc::new(AtomicBool::new(false));
        let aec = MeetingMicAec::default();
        let (live, live_rx) = live_tap_channel("mic", Instant::now(), 2);

        let result = aec.start_with_capture(
            None,
            path.clone(),
            Some(live),
            Box::new(OneShotCapture {
                stopped: Arc::clone(&stopped),
                sink: None,
                worker: None,
            }),
            Duration::from_millis(25),
        );

        assert_eq!(result, None);
        assert!(stopped.load(Ordering::SeqCst));
        assert!(aec.stop().is_none());
        assert!(live_rx.recv_timeout(Duration::from_millis(10)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn start_error_falls_back_without_creating_a_session() {
        let path = temp_wav("failed-aec-start");
        let aec = MeetingMicAec::default();

        let result = aec.start_with_capture(
            None,
            path.clone(),
            None,
            Box::new(FailingCapture),
            Duration::from_millis(10),
        );

        assert_eq!(result, None);
        assert!(!path.exists());
        assert!(aec.stop().is_none());
    }

    #[test]
    fn wav_creation_error_stops_capture_and_falls_back() {
        let path = temp_wav("missing-parent").join("mic.wav");
        let stopped = Arc::new(AtomicBool::new(false));
        let aec = MeetingMicAec::default();

        let result = aec.start_with_capture(
            None,
            path,
            None,
            Box::new(SilentCapture {
                stopped: Arc::clone(&stopped),
                sink: None,
            }),
            Duration::from_millis(10),
        );

        assert_eq!(result, None);
        assert!(stopped.load(Ordering::SeqCst));
        assert!(aec.stop().is_none());
    }

    #[test]
    fn second_start_is_rejected_before_touching_its_backend() {
        let path = temp_wav("active-aec-owner");
        let aec = MeetingMicAec::default();
        assert_eq!(
            aec.start_with_capture(
                None,
                path.clone(),
                None,
                Box::new(ActiveCapture::default()),
                Duration::from_millis(200),
            ),
            Some(44_100)
        );

        let second_started = Arc::new(AtomicBool::new(false));
        let second = aec.start_with_capture(
            None,
            temp_wav("rejected-second-aec"),
            None,
            Box::new(StartTrackingCapture {
                started: Arc::clone(&second_started),
            }),
            Duration::from_millis(10),
        );

        assert_eq!(second, None);
        assert!(!second_started.load(Ordering::SeqCst));
        aec.stop().unwrap().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn poisoned_session_lock_falls_back_before_touching_backend() {
        let aec = Arc::new(MeetingMicAec::default());
        let poison_target = Arc::clone(&aec);
        assert!(std::thread::spawn(move || {
            let _guard = poison_target.inner.lock().unwrap();
            panic!("poison AEC session lock for fallback test");
        })
        .join()
        .is_err());

        let started = Arc::new(AtomicBool::new(false));
        let result = aec.start_with_capture(
            None,
            temp_wav("poisoned-aec-lock"),
            None,
            Box::new(StartTrackingCapture {
                started: Arc::clone(&started),
            }),
            Duration::from_millis(10),
        );

        assert_eq!(result, None);
        assert!(!started.load(Ordering::SeqCst));
    }
}
