//! Export a dictation session as a `lumen-transcript.v1` document.
//!
//! Field mapping follows the lumen-suite contract (`contracts/TRANSCRIPT.md`
//! section 2.4, "lumen-asr `SessionRecord`"):
//!
//! - The whole session becomes a single segment spanning `[0, duration]`.
//! - `text` is the user-accepted final text: `pasted` when present (it carries
//!   the Review-stage accept, including manual edits), else the corrected
//!   text, else the raw ASR text. The contract wording says "corrected", but
//!   its stated intent is "the version the user actually accepted" —
//!   preferring `pasted` is a semantic correction toward that intent, because
//!   `Accept { text }` does not rewrite `record.corrected` (the corrector
//!   output stays pristine as the learning baseline for edit-event diffs).
//!   Only the *text* of `pasted` is used; insert/injection metadata (strategy,
//!   target) is never exported.
//! - The raw ASR text is kept in `provenance.extra.asr_raw` for reference.
//! - `focus` (app/window) is privacy sensitive and not exported.
//! - Duration comes from probing the session WAV. When the audio file is
//!   missing or unreadable the whole `media` block is omitted (per contract:
//!   missing/unreadable audio must not export a stale `path`) and the segment
//!   end degrades to `0.0` (`end` is required by the schema and must be
//!   `>= start`; consumers derive duration from segment ends).

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
    // User-accepted final text first: `pasted` holds what the user actually
    // confirmed at Accept (possibly edited in Review); `corrected` alone can
    // be stale corrector output. See the module docs for why this is a
    // semantic correction of the contract's "corrected" wording.
    let text = record
        .pasted
        .clone()
        .or_else(|| record.corrected.clone())
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

    // Only emit `media` when the WAV was actually probed: a `media` block
    // with a stale path and no facts would violate the contract's
    // "missing/unreadable audio ⇒ omit media" rule.
    if let Some(audio) = audio {
        doc = doc.with_media(Media {
            path: record.audio_path.clone(),
            duration_seconds: Some(audio.duration_seconds),
            sample_rate: Some(audio.sample_rate),
            bytes: Some(audio.bytes),
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
    fn accepted_text_wins_over_stale_corrected() {
        // `pasted` carries the user's final accepted text (possibly edited in
        // Review); it must beat the stale corrector output.
        let mut rec = record_with_texts(Some("raw"), Some("corrector output"));
        rec.pasted = Some("user-edited final".into());
        let doc = session_to_transcript(&rec, None);
        assert_eq!(doc.segments[0].text, "user-edited final");
        // Raw ASR text is still preserved in provenance.
        let prov = doc.provenance.as_ref().expect("provenance");
        assert_eq!(prov.extra.as_ref().expect("extra")["asr_raw"], "raw");
    }

    #[test]
    fn review_edit_then_accept_then_export_uses_edited_text() {
        // Full state-machine flow: corrector output edited by the user in
        // Review, then accepted. Export must carry the edit; `corrected`
        // must stay pristine (learning baseline for edit-event diffs).
        use crate::session::{Session, SessionCommand, SessionEvent};
        use crate::types::InsertStrategy;

        let mut s = Session::new();
        s.handle(SessionCommand::Start).unwrap();
        s.handle(SessionCommand::PermissionsOk).unwrap();
        s.handle(SessionCommand::AudioFinished).unwrap();
        s.handle(SessionCommand::TranscriptReady {
            text: "raw asr".into(),
        })
        .unwrap();
        s.handle(SessionCommand::Corrected {
            text: "corrector output".into(),
        })
        .unwrap();
        s.handle(SessionCommand::Accept {
            text: "user edited this in review".into(),
        })
        .unwrap();
        s.handle(SessionCommand::InsertDone {
            strategy: InsertStrategy::Paste,
        })
        .unwrap();
        s.handle(SessionCommand::VerifyDone).unwrap();
        let events = s.handle(SessionCommand::EditsFlushed).unwrap();
        let record = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Completed { record } => Some(record.clone()),
                _ => None,
            })
            .expect("completed record");

        assert_eq!(record.corrected.as_deref(), Some("corrector output"));
        let doc = session_to_transcript(&record, None);
        assert_eq!(doc.segments[0].text, "user edited this in review");
    }

    #[test]
    fn focus_never_exported() {
        let mut rec = record_with_texts(Some("raw"), Some("corrected"));
        rec.focus.app_name = Some("SecretApp".into());
        rec.focus.window_title = Some("secret window".into());
        let json = session_to_transcript(&rec, None)
            .to_json_string_pretty()
            .unwrap();
        assert!(!json.contains("SecretApp"));
        assert!(!json.contains("secret window"));
    }

    #[test]
    fn missing_audio_file_omits_media_and_degrades_to_zero_end() {
        let mut rec = record_with_texts(Some("raw"), Some("corrected"));
        rec.audio_path = Some("no/such/dir/session.wav".into());
        let doc = export_session_transcript(&rec);

        assert_eq!(doc.segments[0].start, 0.0);
        assert_eq!(doc.segments[0].end, 0.0);
        // Contract: missing/unreadable audio ⇒ no media block (a stale path
        // without probed facts must not be exported).
        assert!(doc.media.is_none());
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
