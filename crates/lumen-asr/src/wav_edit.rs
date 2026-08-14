//! Bounded, streaming edits for the PCM WAV files produced by meeting mode.
//!
//! This module deliberately exposes one small interface: copy a source-time
//! range into a new WAV. Callers own the higher-level transaction (prepare all
//! tracks, update storage, then remove old files), while format validation and
//! sample-accurate range math stay local and unit-tested here.

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WavRangeError {
    #[error("invalid WAV range: start={start_seconds}, end={end_seconds}")]
    InvalidRange {
        start_seconds: f64,
        end_seconds: f64,
    },
    #[error("source and destination WAV paths must differ")]
    SamePath,
    #[error("unsupported meeting WAV format: {0}")]
    Unsupported(String),
    #[error("WAV error: {0}")]
    Hound(#[from] hound::Error),
    #[error("WAV I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The exact range written after clamping the requested end to the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavRangeSummary {
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Copy `[start_seconds, end_seconds)` from one meeting WAV into a new file.
///
/// Meeting mode writes PCM16 WAVs; rejecting any other format is intentional so
/// a future format change cannot silently corrupt a user's only recording.
/// The function streams samples and therefore uses constant memory for a
/// multi-hour meeting. A failed copy removes its partial destination.
pub fn copy_pcm16_wav_range(
    source: &Path,
    destination: &Path,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<WavRangeSummary, WavRangeError> {
    if paths_refer_to_same_file(source, destination) {
        return Err(WavRangeError::SamePath);
    }
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || start_seconds < 0.0
        || end_seconds <= start_seconds
    {
        return Err(WavRangeError::InvalidRange {
            start_seconds,
            end_seconds,
        });
    }

    let result = copy_inner(source, destination, start_seconds, end_seconds);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

/// Compare file identity when both paths exist (covering symlinks, hard links,
/// relative aliases, and case-folding filesystems). A new destination normally
/// does not exist yet, so fall back to the literal comparison in that case.
fn paths_refer_to_same_file(source: &Path, destination: &Path) -> bool {
    source == destination || same_file::is_same_file(source, destination).unwrap_or(false)
}

fn copy_inner(
    source: &Path,
    destination: &Path,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<WavRangeSummary, WavRangeError> {
    let mut reader = hound::WavReader::open(source)?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(WavRangeError::Unsupported(format!(
            "expected PCM16, got {:?}/{}-bit",
            spec.sample_format, spec.bits_per_sample
        )));
    }
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(WavRangeError::Unsupported(
            "zero channels or sample rate".to_string(),
        ));
    }

    let available_frames = u64::from(reader.duration());
    let sample_rate = f64::from(spec.sample_rate);
    let start_frame = (start_seconds * sample_rate).floor() as u64;
    let requested_end = (end_seconds * sample_rate).ceil() as u64;
    let end_frame = requested_end.min(available_frames);
    if start_frame >= end_frame || start_frame > u64::from(u32::MAX) {
        return Err(WavRangeError::InvalidRange {
            start_seconds,
            end_seconds,
        });
    }

    reader.seek(start_frame as u32)?;
    let mut writer = hound::WavWriter::create(destination, spec)?;
    let frames = end_frame - start_frame;
    let sample_count = frames.saturating_mul(u64::from(spec.channels));
    let sample_count = usize::try_from(sample_count).map_err(|_| {
        WavRangeError::Unsupported("selected WAV range is too large for this platform".to_string())
    })?;
    let mut written = 0u64;
    for sample in reader.samples::<i16>().take(sample_count) {
        writer.write_sample(sample?)?;
        written += 1;
    }
    writer.finalize()?;

    let written_frames = written / u64::from(spec.channels);
    Ok(WavRangeSummary {
        duration_seconds: written_frames as f64 / sample_rate,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 10,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in 0i16..20 {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn copies_exact_source_range_and_rewrites_header() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        let destination = dir.path().join("kept.wav");
        write_fixture(&source);

        let summary = copy_pcm16_wav_range(&source, &destination, 0.5, 1.2).unwrap();
        assert_eq!(summary.sample_rate, 10);
        assert_eq!(summary.channels, 1);
        assert!((summary.duration_seconds - 0.7).abs() < 1e-9);

        let mut reader = hound::WavReader::open(destination).unwrap();
        assert_eq!(reader.duration(), 7);
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, (5i16..12).collect::<Vec<_>>());
    }

    #[test]
    fn clamps_end_but_rejects_empty_or_unsupported_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        write_fixture(&source);

        let clamped = dir.path().join("clamped.wav");
        let summary = copy_pcm16_wav_range(&source, &clamped, 1.5, 99.0).unwrap();
        assert!((summary.duration_seconds - 0.5).abs() < 1e-9);

        let empty = dir.path().join("empty.wav");
        assert!(copy_pcm16_wav_range(&source, &empty, 2.0, 3.0).is_err());
        assert!(!empty.exists());
        assert!(copy_pcm16_wav_range(&source, &source, 0.0, 1.0).is_err());
    }

    #[test]
    fn rejects_hard_link_and_relative_aliases_without_touching_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        let hard_link = dir.path().join("alias.wav");
        write_fixture(&source);
        std::fs::hard_link(&source, &hard_link).unwrap();

        assert!(matches!(
            copy_pcm16_wav_range(&source, &hard_link, 0.0, 1.0),
            Err(WavRangeError::SamePath)
        ));
        assert_eq!(hound::WavReader::open(&source).unwrap().duration(), 20);

        let relative_alias = dir.path().join(".").join("source.wav");
        assert!(matches!(
            copy_pcm16_wav_range(&source, &relative_alias, 0.0, 1.0),
            Err(WavRangeError::SamePath)
        ));
        assert_eq!(hound::WavReader::open(&source).unwrap().duration(), 20);
    }
}
