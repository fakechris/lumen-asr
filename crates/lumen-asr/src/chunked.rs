//! Long-dictation pause segmentation (INV-1807).
//!
//! Local batch engines (SenseVoice / Qwen3-ASR sherpa / Whisper) transcribe
//! whatever they are handed in one pass, so a toggle-mode or long hold
//! recording pays a wait that grows with its length — and sherpa Qwen3-ASR
//! degrades outright past ~100 s. This module provides the pure planning
//! half of the fix: feed it voice-activity frames as the recording
//! streams, and it decides where a "finished stretch of speech" ends, so
//! the caller can transcribe each stretch in the background while the user
//! is still talking and only the tail remains at stop.
//!
//! The audio-driving half (live tap → resample → RMS frames → serial
//! per-segment transcription on the dictation engine handle) lives in the
//! desktop app; keeping this crate pure means the planner is unit-testable
//! on every platform.

/// Tuning knobs for [`PauseSegmenter`]. Defaults follow the product brief:
/// flush a segment after 0.7 s of silence, keep 0.24 s of pre-roll before
/// the first voiced frame so onsets are never clipped, drop noise segments
/// shorter than 1.2 s, and never let a segment exceed 30 s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentPlannerConfig {
    /// Sustained silence that closes the open speech segment.
    pub silence_flush_ms: u64,
    /// Segments shorter than this are dropped (coughs, key clicks).
    pub min_segment_ms: u64,
    /// Hard cap: a segment is closed and the next one starts when speech
    /// runs past this, even without a pause.
    pub max_segment_ms: u64,
    /// Audio kept before the first voiced frame of a segment.
    pub pre_roll_ms: u64,
}

impl Default for SegmentPlannerConfig {
    fn default() -> Self {
        Self {
            silence_flush_ms: 700,
            min_segment_ms: 1_200,
            max_segment_ms: 30_000,
            pre_roll_ms: 240,
        }
    }
}

/// A completed speech segment as sample indices into the caller's
/// cumulative 16 kHz buffer: `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRange {
    pub start: usize,
    pub end: usize,
}

/// Frame-fed pause segmenter. `push_frame` is called once per
/// voice-activity frame (any fixed frame size); it returns the segments
/// that just closed.
#[derive(Debug)]
pub struct PauseSegmenter {
    cfg: SegmentPlannerConfig,
    cursor: usize,
    boundary: usize,
    in_speech: bool,
    segment_start: usize,
    last_voiced_end: usize,
    silence_run_ms: u64,
}

impl PauseSegmenter {
    pub fn new(cfg: SegmentPlannerConfig) -> Self {
        Self {
            cfg,
            cursor: 0,
            boundary: 0,
            in_speech: false,
            segment_start: 0,
            last_voiced_end: 0,
            silence_run_ms: 0,
        }
    }

    /// Sample index of the end of the last *closed* segment — everything
    /// after this is the un-transcribed tail at stop time.
    pub fn boundary(&self) -> usize {
        self.boundary
    }

    pub fn push_frame(&mut self, voiced: bool, frame_samples: usize) -> Vec<SegmentRange> {
        let mut closed = Vec::new();
        self.cursor += frame_samples;
        if voiced {
            if !self.in_speech {
                self.in_speech = true;
                self.segment_start = self
                    .cursor
                    .saturating_sub(ms_to_samples(self.cfg.pre_roll_ms));
            }
            self.last_voiced_end = self.cursor;
            self.silence_run_ms = 0;
            // Hard cap: close at the last voiced frame and continue the
            // next segment from there (speech may run on immediately).
            if voiced_ms_between(self.segment_start, self.last_voiced_end)
                >= self.cfg.max_segment_ms
            {
                if let Some(range) = self.close_segment() {
                    closed.push(range);
                }
                // Speech may run straight on: reopen from the boundary so
                // the next segment does not reach back through pre-roll.
                self.segment_start = self.last_voiced_end;
                self.in_speech = true;
            }
        } else if self.in_speech {
            self.silence_run_ms += samples_to_ms(frame_samples);
            if self.silence_run_ms >= self.cfg.silence_flush_ms {
                if let Some(range) = self.close_segment() {
                    closed.push(range);
                }
            }
        }
        closed
    }

    fn close_segment(&mut self) -> Option<SegmentRange> {
        let start = self.segment_start;
        let end = self.last_voiced_end;
        self.in_speech = false;
        self.silence_run_ms = 0;
        if voiced_ms_between(start, end) < self.cfg.min_segment_ms {
            return None;
        }
        self.boundary = self.boundary.max(end);
        Some(SegmentRange { start, end })
    }
}

fn voiced_ms_between(start: usize, end: usize) -> u64 {
    samples_to_ms(end.saturating_sub(start))
}

const SAMPLE_RATE: u64 = 16_000;

fn samples_to_ms(samples: usize) -> u64 {
    (samples as u64 * 1_000) / SAMPLE_RATE
}

fn ms_to_samples(ms: u64) -> usize {
    (ms * SAMPLE_RATE / 1_000) as usize
}

/// Join per-segment transcripts into one dictation transcript: a space is
/// inserted only between two adjacent Latin word characters. CJK needs no
/// separator, and a CJK↔Latin boundary reads fine without one. Empty parts
/// are skipped; leading/trailing whitespace is trimmed from each part.
pub fn join_transcripts(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let needs_space = match (out.chars().last(), part.chars().next()) {
            (Some(prev), Some(next)) => is_latin_word(prev) && is_latin_word(next),
            _ => false,
        };
        if needs_space {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

fn is_latin_word(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 480; // 30 ms at 16 kHz

    fn frames(seconds: f64) -> usize {
        (seconds * SAMPLE_RATE as f64 / FRAME as f64) as usize
    }

    #[test]
    fn silence_flushes_a_segment() {
        let mut p = PauseSegmenter::new(SegmentPlannerConfig::default());
        let mut closed = Vec::new();
        for _ in 0..frames(2.0) {
            closed.extend(p.push_frame(true, FRAME));
        }
        for _ in 0..frames(0.8) {
            closed.extend(p.push_frame(false, FRAME));
        }
        assert_eq!(closed.len(), 1, "2s speech + 0.8s silence closes once");
        let seg = closed[0];
        assert!(seg.start <= ms_to_samples(240) + 1, "pre-roll included");
        // frames(2.0) is 66 whole 30 ms frames = 1980 ms of speech.
        assert_eq!(seg.end, 66 * FRAME);
        assert_eq!(p.boundary(), seg.end);
    }

    #[test]
    fn short_noise_segments_are_dropped() {
        let mut p = PauseSegmenter::new(SegmentPlannerConfig::default());
        let mut closed = Vec::new();
        for _ in 0..frames(0.4) {
            closed.extend(p.push_frame(true, FRAME));
        }
        for _ in 0..frames(1.0) {
            closed.extend(p.push_frame(false, FRAME));
        }
        assert!(closed.is_empty(), "sub-min_segment speech is not a segment");
        assert_eq!(p.boundary(), 0);
    }

    #[test]
    fn max_segment_cap_splits_continuous_speech() {
        let cfg = SegmentPlannerConfig {
            max_segment_ms: 3_000,
            ..Default::default()
        };
        let mut p = PauseSegmenter::new(cfg);
        let mut closed = Vec::new();
        for _ in 0..frames(10.0) {
            closed.extend(p.push_frame(true, FRAME));
        }
        assert_eq!(closed.len(), 3, "10s continuous speech splits at 3/6/9s");
        assert_eq!(closed[0].end, ms_to_samples(3_000));
        assert_eq!(closed[1].start, ms_to_samples(3_000));
        assert!(closed[2].end <= ms_to_samples(10_000));
    }

    #[test]
    fn pre_roll_never_goes_negative() {
        let mut p = PauseSegmenter::new(SegmentPlannerConfig {
            pre_roll_ms: 5_000,
            ..Default::default()
        });
        // First voiced frame in the second frame of the stream.
        let mut closed = p.push_frame(true, FRAME);
        for _ in 0..frames(2.0) {
            closed.extend(p.push_frame(true, FRAME));
        }
        for _ in 0..frames(0.8) {
            closed.extend(p.push_frame(false, FRAME));
        }
        assert_eq!(closed[0].start, 0, "pre-roll clamps at the stream start");
    }

    #[test]
    fn joining_rules() {
        assert_eq!(join_transcripts(&["hello", "world"]), "hello world");
        assert_eq!(join_transcripts(&["你好", "世界"]), "你好世界");
        assert_eq!(join_transcripts(&["部署", "Kubernetes"]), "部署Kubernetes");
        assert_eq!(join_transcripts(&["scale", "out"]), "scale out");
        assert_eq!(
            join_transcripts(&["先说这些。", "", "然后continue"]),
            "先说这些。然后continue"
        );
        assert_eq!(join_transcripts(&["  spaced  ", "out "]), "spaced out");
    }
}
