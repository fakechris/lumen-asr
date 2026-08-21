//! Voice-activity helpers for dictation: RMS-based silence auto-stop and
//! trailing-silence trimming.
//!
//! The RMS gate reuses the `0.005` (≈ −46 dBFS) baseline that
//! `lumen-meeting`'s preflight shares with diar-rs. The config `mode` key
//! selects the implementation; unknown modes fall back to RMS.

use std::time::{Duration, Instant};

/// Decision returned by [`SilenceAutoStop::update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadAction {
    Continue,
    /// Sustained silence after speech — end the current dictation.
    AutoStop,
}

/// RMS state machine: wait for first speech, then stop after a sustained
/// silent stretch. Hysteresis (separate start/end thresholds) avoids flapping
/// on borderline input.
#[derive(Debug, Clone)]
pub struct SilenceAutoStop {
    /// RMS that marks the onset of speech.
    pub start_threshold: f32,
    /// RMS below which input counts as silence once speaking.
    pub end_threshold: f32,
    /// Sustained silence needed to trigger [`VadAction::AutoStop`].
    pub silence_timeout: Duration,
    speaking: bool,
    silence_since: Option<Instant>,
}

impl SilenceAutoStop {
    pub fn new(start_threshold: f32, end_threshold: f32, silence_timeout: Duration) -> Self {
        Self {
            start_threshold,
            end_threshold,
            silence_timeout,
            speaking: false,
            silence_since: None,
        }
    }

    /// Feed the latest capture-window RMS. `now` is passed in so tests control
    /// the clock.
    pub fn update(&mut self, rms: f32, now: Instant) -> VadAction {
        if !self.speaking {
            if rms >= self.start_threshold {
                self.speaking = true;
                self.silence_since = None;
            }
            return VadAction::Continue;
        }
        if rms < self.end_threshold {
            let since = *self.silence_since.get_or_insert(now);
            if now.duration_since(since) >= self.silence_timeout {
                return VadAction::AutoStop;
            }
        } else {
            self.silence_since = None;
        }
        VadAction::Continue
    }
}

/// Length (in samples) with trailing silence removed, keeping `padding` of the
/// quiet tail so the last syllable is never clipped. Returns the original
/// length when nothing trimmable is found.
pub fn trim_trailing_silence(
    samples: &[f32],
    sample_rate: u32,
    threshold: f32,
    window: Duration,
    padding: Duration,
) -> usize {
    let window_len = ((sample_rate as u64 * window.as_millis() as u64) / 1000).max(1) as usize;
    let padding_len = (sample_rate as u64 * padding.as_millis() as u64 / 1000) as usize;
    if samples.len() < window_len * 2 {
        return samples.len();
    }
    // Find the last voiced window scanning from the end.
    let mut cut = samples.len();
    let mut windows: Vec<usize> = (0..samples.len()).step_by(window_len).collect();
    if windows.last().copied() != Some(samples.len()) {
        windows.push(samples.len());
    }
    for pair in windows.windows(2).rev() {
        let (start, end) = (pair[0], pair[1]);
        let slice = &samples[start..end];
        let sum_sq: f64 = slice.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        let rms = (sum_sq / slice.len() as f64).sqrt() as f32;
        if rms >= threshold {
            cut = end.saturating_add(padding_len).min(samples.len());
            break;
        }
        cut = start;
    }
    if cut >= samples.len() {
        return samples.len();
    }
    // All windows silent → keep the first window so ASR still sees something.
    cut.max(window_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn never_stops_before_first_speech() {
        let mut vad = SilenceAutoStop::new(0.02, 0.005, Duration::from_millis(1500));
        for i in 0..100 {
            assert_eq!(
                vad.update(0.001, t(i * 100)),
                VadAction::Continue,
                "silence before any speech must not auto-stop"
            );
        }
    }

    #[test]
    fn stops_after_sustained_silence_following_speech() {
        let mut vad = SilenceAutoStop::new(0.02, 0.005, Duration::from_millis(1500));
        assert_eq!(vad.update(0.05, t(0)), VadAction::Continue); // speech onset
        assert_eq!(vad.update(0.001, t(100)), VadAction::Continue);
        assert_eq!(vad.update(0.001, t(1500)), VadAction::Continue);
        assert_eq!(vad.update(0.001, t(1601)), VadAction::AutoStop);
    }

    #[test]
    fn speech_burst_resets_the_silence_clock() {
        let mut vad = SilenceAutoStop::new(0.02, 0.005, Duration::from_millis(1500));
        assert_eq!(vad.update(0.05, t(0)), VadAction::Continue);
        assert_eq!(vad.update(0.001, t(1000)), VadAction::Continue);
        assert_eq!(vad.update(0.04, t(1200)), VadAction::Continue); // reset
        assert_eq!(vad.update(0.001, t(1300)), VadAction::Continue); // new silence start
        assert_eq!(vad.update(0.001, t(2700)), VadAction::Continue); // 1400ms < timeout
        assert_eq!(vad.update(0.001, t(2801)), VadAction::AutoStop);
    }

    #[test]
    fn hysteresis_holds_during_borderline_input() {
        let mut vad = SilenceAutoStop::new(0.02, 0.005, Duration::from_millis(500));
        assert_eq!(vad.update(0.05, t(0)), VadAction::Continue);
        // Between start/end thresholds: counts as speech, silence never starts.
        for i in 1..20 {
            assert_eq!(vad.update(0.01, t(i * 100)), VadAction::Continue);
        }
    }

    #[test]
    fn trims_trailing_silence_with_padding() {
        let rate = 16_000u32;
        let mut samples = vec![0.5f32; rate as usize * 2]; // 2s voiced
        samples.extend(std::iter::repeat(0.0).take(rate as usize * 3)); // 3s silence
        let cut = trim_trailing_silence(
            &samples,
            rate,
            0.005,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        let kept_ms = cut as u64 * 1000 / rate as u64;
        assert!(
            (2000..=2400).contains(&kept_ms),
            "expected ~2s + ≤300ms padding, kept {kept_ms}ms"
        );
    }

    #[test]
    fn keeps_first_window_when_all_silent() {
        let rate = 16_000u32;
        let samples = vec![0.0f32; rate as usize];
        let cut = trim_trailing_silence(
            &samples,
            rate,
            0.005,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        assert_eq!(cut, rate as usize / 10);
    }

    #[test]
    fn short_input_is_never_trimmed() {
        let samples = vec![0.3f32; 1000];
        let cut = trim_trailing_silence(
            &samples,
            16_000,
            0.005,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        assert_eq!(cut, samples.len());
    }
}
