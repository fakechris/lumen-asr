//! Cheap per-track speech preflight (pure, cross-platform).
//!
//! Before a track is handed to the (expensive, and on silent audio *failing*)
//! diarization pipeline, its decoded samples are scanned with a coarse
//! 1-second RMS window to estimate how much audibly voiced material it holds.
//! Two consumers:
//!
//! 1. **Silence skip** — a track with less than
//!    [`MIN_TRACK_VOICED_SECONDS`] of voiced audio is skipped entirely (zero
//!    turns) instead of being diarized. diar-rs hard-fails on such tracks
//!    ("pipeline: too few x-vectors"), and before this preflight a fully
//!    silent system track (user never played remote audio) failed the whole
//!    meeting even though the mic track was fine.
//! 2. **Fail-open fallback** — when a track passes the preflight but
//!    diarization still errors (borderline short speech), the scan's merged
//!    voiced spans become single-speaker turns so the track degrades to
//!    "one speaker, ASR over the voiced audio" rather than failing the run
//!    (see [`SpeechScan::fallback_turns`]).
//!
//! The RMS gate matches the `rms >= 0.005` window gate diar-rs itself applies
//! before embedding an x-vector, so any track this preflight rejects would
//! have produced (close to) zero x-vectors in diar-rs anyway — normal-volume
//! speech sails through.

use crate::assemble::DiarTurn;

/// RMS scan window length in seconds.
pub(crate) const PREFLIGHT_WINDOW_SECONDS: f64 = 1.0;

/// A window whose RMS reaches this counts as voiced. Same threshold diar-rs
/// uses to gate x-vector windows (`rms >= 0.005`, ≈ −46 dBFS).
pub(crate) const PREFLIGHT_RMS_THRESHOLD: f32 = 0.005;

/// Tracks with less voiced audio than this are skipped (not diarized): below
/// it diar-rs cannot form enough x-vectors to cluster, and there is nothing
/// meaningful to transcribe anyway.
pub(crate) const MIN_TRACK_VOICED_SECONDS: f64 = 3.0;

/// Result of one track's coarse energy scan.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpeechScan {
    /// Total duration of windows whose RMS reached the threshold.
    pub(crate) voiced_seconds: f64,
    /// Merged, chronological `[start, end)` spans (seconds) of consecutive
    /// voiced windows.
    pub(crate) voiced_spans: Vec<(f64, f64)>,
    /// Full decoded duration of the scanned audio, for logging.
    pub(crate) total_seconds: f64,
}

impl SpeechScan {
    /// Whether the track carries enough voiced audio to be worth diarizing.
    pub(crate) fn has_enough_speech(&self) -> bool {
        self.voiced_seconds >= MIN_TRACK_VOICED_SECONDS
    }

    /// Degraded single-speaker turns for the fail-open path: every merged
    /// voiced span becomes one turn attributed to engine speaker `0`, so the
    /// existing per-turn ASR loop transcribes exactly the audible audio.
    /// (Only the macOS `diarize` path can reach the fail-open branch; kept
    /// un-gated so the policy is unit-testable everywhere.)
    #[cfg_attr(not(all(target_os = "macos", feature = "diarize")), allow(dead_code))]
    pub(crate) fn fallback_turns(&self) -> Vec<DiarTurn> {
        self.voiced_spans
            .iter()
            .map(|&(start, end)| DiarTurn::new(start, end, 0))
            .collect()
    }
}

/// Scan `samples` (mono, `sample_rate` Hz) with non-overlapping
/// [`PREFLIGHT_WINDOW_SECONDS`] windows; the trailing partial window is
/// scanned too (weighted by its real duration). Pure — no IO, no models.
pub(crate) fn scan_speech(samples: &[f32], sample_rate: u32) -> SpeechScan {
    if samples.is_empty() || sample_rate == 0 {
        return SpeechScan::default();
    }
    let rate = f64::from(sample_rate);
    let window = ((PREFLIGHT_WINDOW_SECONDS * rate) as usize).max(1);

    let mut scan = SpeechScan {
        total_seconds: samples.len() as f64 / rate,
        ..SpeechScan::default()
    };
    for (index, chunk) in samples.chunks(window).enumerate() {
        let sum_sq: f64 = chunk.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        let rms = (sum_sq / chunk.len() as f64).sqrt() as f32;
        if rms < PREFLIGHT_RMS_THRESHOLD {
            continue;
        }
        let start = (index * window) as f64 / rate;
        let end = (index * window + chunk.len()) as f64 / rate;
        scan.voiced_seconds += end - start;
        match scan.voiced_spans.last_mut() {
            // Consecutive voiced windows merge into one span.
            Some(last) if (last.1 - start).abs() < 1e-9 => last.1 = end,
            _ => scan.voiced_spans.push((start, end)),
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;

    fn seconds(n: f64) -> usize {
        (n * f64::from(SR)) as usize
    }

    /// Deterministic speech-loudness signal (sine mixture, RMS ≈ 0.3).
    fn speech_like(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                (2.0 * std::f32::consts::PI * 180.0 * t).sin() * 0.4
                    + (2.0 * std::f32::consts::PI * 610.0 * t).sin() * 0.2
            })
            .collect()
    }

    #[test]
    fn all_zero_track_has_no_voiced_audio() {
        let scan = scan_speech(&vec![0.0f32; seconds(30.0)], SR);
        assert_eq!(scan.voiced_seconds, 0.0);
        assert!(scan.voiced_spans.is_empty());
        assert!(!scan.has_enough_speech());
        assert!(scan.fallback_turns().is_empty());
        assert!((scan.total_seconds - 30.0).abs() < 1e-6);
    }

    #[test]
    fn faint_noise_stays_below_the_threshold() {
        // Alternating ±0.001 → RMS = 0.001, well under 0.005: recorder hiss /
        // dithering on an otherwise dead track must not count as speech.
        let noise: Vec<f32> = (0..seconds(30.0))
            .map(|i| if i % 2 == 0 { 0.001 } else { -0.001 })
            .collect();
        let scan = scan_speech(&noise, SR);
        assert_eq!(scan.voiced_seconds, 0.0);
        assert!(!scan.has_enough_speech());
    }

    #[test]
    fn normal_speech_passes_and_yields_one_span() {
        let scan = scan_speech(&speech_like(seconds(10.0)), SR);
        assert!((scan.voiced_seconds - 10.0).abs() < 1e-6);
        assert!(scan.has_enough_speech());
        assert_eq!(scan.voiced_spans, vec![(0.0, 10.0)]);
        let turns = scan.fallback_turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker, 0);
        assert!((turns[0].start - 0.0).abs() < 1e-6);
        assert!((turns[0].end - 10.0).abs() < 1e-6);
    }

    #[test]
    fn short_speech_is_below_the_skip_floor() {
        // 2 s of clear speech in 30 s of silence: audible, but under the ~3 s
        // floor — the track is skipped rather than diarized.
        let mut samples = vec![0.0f32; seconds(30.0)];
        let speech = speech_like(seconds(2.0));
        samples[seconds(5.0)..seconds(7.0)].copy_from_slice(&speech);
        let scan = scan_speech(&samples, SR);
        assert!((scan.voiced_seconds - 2.0).abs() < 1e-6);
        assert!(!scan.has_enough_speech());
    }

    #[test]
    fn separated_utterances_become_separate_spans() {
        // Speech at [2,5) and [10,12): 5 s voiced total across two spans →
        // enough speech, and the fallback keeps the silence gap out of ASR.
        let mut samples = vec![0.0f32; seconds(20.0)];
        let a = speech_like(seconds(3.0));
        let b = speech_like(seconds(2.0));
        samples[seconds(2.0)..seconds(5.0)].copy_from_slice(&a);
        samples[seconds(10.0)..seconds(12.0)].copy_from_slice(&b);
        let scan = scan_speech(&samples, SR);
        assert!((scan.voiced_seconds - 5.0).abs() < 1e-6);
        assert!(scan.has_enough_speech());
        assert_eq!(scan.voiced_spans.len(), 2);
        assert_eq!(scan.voiced_spans[0], (2.0, 5.0));
        assert_eq!(scan.voiced_spans[1], (10.0, 12.0));
        // Every fallback turn is the same single speaker.
        assert!(scan.fallback_turns().iter().all(|t| t.speaker == 0));
    }

    #[test]
    fn trailing_partial_window_is_counted() {
        // 3.5 s of speech: the final half-window still contributes 0.5 s.
        let scan = scan_speech(&speech_like(seconds(3.5)), SR);
        assert!((scan.voiced_seconds - 3.5).abs() < 1e-6);
        assert!(scan.has_enough_speech());
    }

    #[test]
    fn empty_input_and_zero_rate_are_harmless() {
        assert!(!scan_speech(&[], SR).has_enough_speech());
        assert!(!scan_speech(&[0.5; 100], 0).has_enough_speech());
    }
}
