//! Export a dictation session as a `lumen-transcript.v1` document.
//!
//! Field mapping follows the lumen-suite contract (`contracts/TRANSCRIPT.md`
//! section 2.4, "lumen-asr `SessionRecord`"):
//!
//! - The whole session becomes a single segment spanning `[0, duration]`.
//! - `text` is the corrected text (the version the user actually accepted),
//!   falling back to the raw ASR text when no correction exists.
//! - The raw ASR text is kept in `provenance.extra.asr_raw` for reference.
//! - `pasted` is never exported — it is an artifact of the insert step.
//! - `focus` (app/window) is privacy sensitive and not exported.
//! - Duration comes from probing the session WAV. When the audio file is
//!   missing or unreadable the export degrades: `media.duration_seconds` is
//!   omitted and the segment end is `0.0` (`end` is required by the schema
//!   and must be `>= start`; consumers derive duration from segment ends).

use std::path::Path;

use crate::types::SessionRecord;
use lumen_asr_engine::audio::decode_wav_pcm_s16le;
use lumen_transcript::{Media, Provenance, Segment, TranscriptV1};
use serde_json::{Map, Value};

/// Audio facts probed from a session WAV, used to fill `media` and the
/// single segment's end time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioInfo {
    /// Total duration in seconds.
    pub duration_seconds: f64,
    /// Sample rate in Hz as declared by the WAV header.
    pub sample_rate: u32,
    /// File size in bytes.
    pub bytes: u64,
}

/// Probe a WAV file for duration, sample rate, and size.
///
/// Returns `None` when the file is missing, unreadable, or not a decodable
/// PCM s16le WAV — the export then degrades gracefully (no duration).
pub fn probe_wav_info(path: &Path) -> Option<AudioInfo> {
    let bytes = std::fs::read(path).ok()?;
    let file_len = bytes.len() as u64;
    let decoded = decode_wav_pcm_s16le(&bytes).ok()?;
    Some(AudioInfo {
        duration_seconds: decoded.samples.len() as f64 / f64::from(decoded.sample_rate),
        sample_rate: decoded.sample_rate,
        bytes: file_len,
    })
}

/// Map a [`SessionRecord`] to a `lumen-transcript.v1` document.
///
/// Pure mapping: audio facts are passed in by the caller. Use
/// [`export_session_transcript`] to probe them from `record.audio_path`.
pub fn session_to_transcript(record: &SessionRecord, audio: Option<AudioInfo>) -> TranscriptV1 {
    let text = record
        .corrected
        .clone()
        .or_else(|| record.asr_raw.clone())
        .unwrap_or_default();

    let end = audio.map(|a| a.duration_seconds).unwrap_or(0.0);
    let segment = Segment::new(0.0, end, text).with_id(record.id.to_string());

    let mut provenance = Provenance::new("lumen-asr");
    provenance.app_version = Some(env!("CARGO_PKG_VERSION").to_string());
    provenance.engine = record.asr_engine.clone();
    provenance.created_at = Some(record.created_at.to_rfc3339());

    let mut extra = Map::new();
    if let Some(raw) = &record.asr_raw {
        extra.insert("asr_raw".into(), Value::String(raw.clone()));
    }
    if let Some(corrector) = &record.corrector_engine {
        extra.insert("corrector_engine".into(), Value::String(corrector.clone()));
    }
    if !extra.is_empty() {
        provenance.extra = Some(extra);
    }

    let mut doc = TranscriptV1::new(vec![segment]).with_provenance(provenance);

    if record.audio_path.is_some() || audio.is_some() {
        doc = doc.with_media(Media {
            path: record.audio_path.clone(),
            duration_seconds: audio.map(|a| a.duration_seconds),
            sample_rate: audio.map(|a| a.sample_rate),
            bytes: audio.map(|a| a.bytes),
            ..Media::default()
        });
    }

    doc
}

/// Export a session, probing `record.audio_path` (when set and readable)
/// for the media duration.
pub fn export_session_transcript(record: &SessionRecord) -> TranscriptV1 {
    let audio = record
        .audio_path
        .as_deref()
        .and_then(|p| probe_wav_info(Path::new(p)));
    session_to_transcript(record, audio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_asr_engine::audio::samples_to_wav_mono_i16;

    fn record_with_texts(raw: Option<&str>, corrected: Option<&str>) -> SessionRecord {
        let mut rec = SessionRecord::new();
        rec.asr_raw = raw.map(Into::into);
        rec.corrected = corrected.map(Into::into);
        rec.pasted = Some("pasted-artifact-not-exported".into());
        rec.asr_engine = Some("sensevoice_sherpa".into());
        rec.corrector_engine = Some("ollama".into());
        rec
    }

    #[test]
    fn corrected_text_wins_and_raw_goes_to_extra() {
        let rec = record_with_texts(Some("你好 世界"), Some("你好，世界。"));
        let doc = session_to_transcript(&rec, None);

        assert_eq!(doc.segments.len(), 1);
        assert_eq!(doc.segments[0].text, "你好，世界。");
        assert_eq!(
            doc.segments[0].id.as_deref(),
            Some(rec.id.to_string().as_str())
        );

        let prov = doc.provenance.as_ref().expect("provenance");
        assert_eq!(prov.app, "lumen-asr");
        assert_eq!(prov.engine.as_deref(), Some("sensevoice_sherpa"));
        let extra = prov.extra.as_ref().expect("extra");
        assert_eq!(extra["asr_raw"], "你好 世界");
        assert_eq!(extra["corrector_engine"], "ollama");
    }

    #[test]
    fn falls_back_to_raw_text_without_correction() {
        let rec = record_with_texts(Some("raw only"), None);
        let doc = session_to_transcript(&rec, None);
        assert_eq!(doc.segments[0].text, "raw only");
    }

    #[test]
    fn pasted_and_focus_never_exported() {
        let mut rec = record_with_texts(Some("raw"), Some("corrected"));
        rec.focus.app_name = Some("SecretApp".into());
        rec.focus.window_title = Some("secret window".into());
        let json = session_to_transcript(&rec, None)
            .to_json_string_pretty()
            .unwrap();
        assert!(!json.contains("pasted-artifact-not-exported"));
        assert!(!json.contains("SecretApp"));
        assert!(!json.contains("secret window"));
    }

    #[test]
    fn missing_audio_file_degrades_to_zero_end_and_no_duration() {
        let mut rec = record_with_texts(Some("raw"), Some("corrected"));
        rec.audio_path = Some("no/such/dir/session.wav".into());
        let doc = export_session_transcript(&rec);

        assert_eq!(doc.segments[0].start, 0.0);
        assert_eq!(doc.segments[0].end, 0.0);
        let media = doc.media.as_ref().expect("media (path is recorded)");
        assert_eq!(media.path.as_deref(), Some("no/such/dir/session.wav"));
        assert_eq!(media.duration_seconds, None);
        assert_eq!(media.sample_rate, None);
    }

    #[test]
    fn no_audio_path_means_no_media_block() {
        let doc = session_to_transcript(&record_with_texts(Some("raw"), None), None);
        assert!(doc.media.is_none());
    }

    #[test]
    fn wav_probe_fills_duration_and_segment_end() {
        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("session.wav");
        // 0.5 s of 16 kHz mono audio.
        let samples = vec![0.25f32; 8_000];
        std::fs::write(&wav_path, samples_to_wav_mono_i16(&samples, 16_000)).unwrap();

        let mut rec = record_with_texts(Some("raw"), Some("corrected"));
        rec.audio_path = Some(wav_path.to_string_lossy().into_owned());
        let doc = export_session_transcript(&rec);

        let media = doc.media.as_ref().expect("media");
        assert_eq!(media.sample_rate, Some(16_000));
        assert!((media.duration_seconds.unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(media.bytes, Some(44 + 16_000));
        assert!((doc.segments[0].end - 0.5).abs() < 1e-9);
        assert_eq!(doc.segments[0].start, 0.0);
    }

    #[test]
    fn document_round_trips_as_v1() {
        let rec = record_with_texts(Some("raw"), Some("corrected"));
        let json = session_to_transcript(&rec, None).to_json_string().unwrap();
        assert!(json.contains("\"schema\":\"lumen-transcript.v1\""));
        let parsed = TranscriptV1::from_json_str(&json).unwrap();
        assert_eq!(parsed.segments[0].text, "corrected");
    }
}
