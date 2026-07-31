//! Meeting-mode domain types.
//!
//! A meeting is a long, multi-speaker recording — a parallel pipeline to the
//! dictation `Session`, not an extension of it. These types are the persisted
//! shape of a meeting and its transcript; the segment/speaker shapes are kept
//! aligned with the `lumen-transcript.v1` interchange format so a finished
//! meeting can be exported to that contract without a lossy remap
//! (`start_seconds`/`end_seconds`/`text`/`speaker`/`confidence`/`words`).
//!
//! This is a data skeleton only. The runtime recording state machine
//! (pause/resume, chunked capture) lives in a later stage; here `MeetingStatus`
//! is just the coarse lifecycle a stored meeting can be in.

use chrono::{DateTime, Utc};
use lumen_transcript::Word;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Coarse lifecycle of a stored meeting.
///
/// This is intentionally minimal: the detailed runtime recording state
/// (buffering, pause/resume) is modeled separately in a later stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    /// Audio is being captured.
    Recording,
    /// Recording finished; the offline pipeline is starting up.
    Processing,
    /// Diarization + per-turn ASR in flight.
    Transcribing,
    /// Transcript is ready; the structured minutes LLM pass is in flight.
    Summarizing,
    /// Transcript (and minutes, when requested) are available.
    Ready,
    /// Recording or processing failed terminally.
    Failed,
}

impl MeetingStatus {
    /// Stable lowercase token persisted in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Processing => "processing",
            Self::Transcribing => "transcribing",
            Self::Summarizing => "summarizing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// Parse a persisted token, defaulting to [`MeetingStatus::Recording`] for
    /// unknown values so a forward-written status never fails a read.
    pub fn from_str_or_recording(value: &str) -> Self {
        match value {
            "processing" => Self::Processing,
            "transcribing" => Self::Transcribing,
            "summarizing" => Self::Summarizing,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            _ => Self::Recording,
        }
    }

    /// The next status on the happy path of the offline pipeline:
    /// `recording → processing → transcribing → summarizing → ready`.
    ///
    /// Terminal states ([`Ready`](Self::Ready), [`Failed`](Self::Failed)) are
    /// fixed points. This is the pure state-transition rule; a step that fails
    /// moves straight to [`Failed`](Self::Failed) instead of advancing.
    pub fn advance(self) -> Self {
        match self {
            Self::Recording => Self::Processing,
            Self::Processing => Self::Transcribing,
            Self::Transcribing => Self::Summarizing,
            Self::Summarizing => Self::Ready,
            Self::Ready => Self::Ready,
            Self::Failed => Self::Failed,
        }
    }
}

/// Which capture track a transcript segment came from.
///
/// Dual-track meetings record two synchronized WAVs: the microphone (the
/// user's own voice) and — on capable macOS hosts — the system audio output
/// (the remote participants in a call). Legacy single-track meetings have no
/// channel recorded (`None` on the segment), which reads the same as `Mic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentChannel {
    /// Captured from the microphone (the local user).
    Mic,
    /// Captured from the system audio output (remote participants).
    System,
}

impl SegmentChannel {
    /// Stable lowercase token persisted in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }

    /// Parse a persisted token; unknown values read as [`SegmentChannel::Mic`]
    /// so a forward-written channel never fails a read.
    pub fn from_str_or_mic(value: &str) -> Self {
        match value {
            "system" => Self::System,
            _ => Self::Mic,
        }
    }
}

/// A meeting recording (maps to the `meetings` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    /// User-facing title; absent until named.
    pub title: Option<String>,
    /// Path to the recorded audio on disk; absent while still recording.
    pub audio_path: Option<String>,
    /// Path to the optional second, synchronized system-audio (remote
    /// participants) WAV. Present only for dual-track recordings on capable
    /// macOS hosts; `None` for mic-only meetings (the pre-dual-track behavior).
    #[serde(default)]
    pub system_audio_path: Option<String>,
    /// Total recording duration in seconds, once known.
    pub duration_seconds: Option<f64>,
    pub status: MeetingStatus,
    /// Primary language as a BCP-47 tag, when detected.
    pub language: Option<String>,
    /// Human-readable reason the meeting is [`MeetingStatus::Failed`], when
    /// known (e.g. missing diarization models, or diarization unsupported on
    /// this platform). Absent on every non-failed meeting.
    pub failure_reason: Option<String>,
    /// Free-form notes the user jots down while the meeting is being recorded.
    /// Empty (never absent) until the user writes something. These are fed to
    /// the minutes LLM pass as extra context so the structured summary reflects
    /// what the user themselves flagged as important, alongside the transcript.
    #[serde(default)]
    pub notes: String,
}

impl Meeting {
    /// A fresh meeting with a new id, `created_at = now`, status `Recording`.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            title: None,
            audio_path: None,
            system_audio_path: None,
            duration_seconds: None,
            status: MeetingStatus::Recording,
            language: None,
            failure_reason: None,
            notes: String::new(),
        }
    }
}

impl Default for Meeting {
    fn default() -> Self {
        Self::new()
    }
}

/// One transcript segment within a meeting (maps to `transcript_segments`).
///
/// Field shapes mirror `lumen_transcript::Segment`: `start_seconds`/
/// `end_seconds` are seconds from media start, `words` is the optional
/// word-level timing (same [`Word`] type as the interchange contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: Uuid,
    pub meeting_id: Uuid,
    /// Zero-based order of this segment within the meeting.
    pub seq: u32,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
    /// References [`Speaker::id`]; absent when the speaker is unassigned.
    pub speaker_id: Option<Uuid>,
    /// Engine confidence in `[0, 1]`, when the engine reports one.
    pub confidence: Option<f64>,
    /// Optional word-level timing, aligned with the interchange `Word` shape.
    pub words: Option<Vec<Word>>,
    /// Capture track this segment came from ([`SegmentChannel::Mic`] /
    /// [`SegmentChannel::System`]). `None` for legacy single-track meetings,
    /// which reads the same as mic.
    #[serde(default)]
    pub channel: Option<SegmentChannel>,
}

impl TranscriptSegment {
    /// A segment with the required fields set and everything optional absent.
    pub fn new(
        meeting_id: Uuid,
        seq: u32,
        start_seconds: f64,
        end_seconds: f64,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            meeting_id,
            seq,
            start_seconds,
            end_seconds,
            text: text.into(),
            speaker_id: None,
            confidence: None,
            words: None,
            channel: None,
        }
    }
}

/// A speaker within one meeting (maps to the `speakers` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: Uuid,
    pub meeting_id: Uuid,
    /// Engine-assigned label, e.g. `"S1"`. Stable within the meeting.
    pub label: String,
    /// User-assigned name, e.g. `"Chris"`; absent until labeled.
    pub display_name: Option<String>,
    /// Reference to a stored voiceprint embedding. Reserved for cross-meeting
    /// speaker enrollment (M5); left empty in v1.
    pub embedding_ref: Option<String>,
}

impl Speaker {
    /// A speaker with the given engine label and everything else absent.
    pub fn new(meeting_id: Uuid, label: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            meeting_id,
            label: label.into(),
            display_name: None,
            embedding_ref: None,
        }
    }
}

/// Kind of generated meeting summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    /// Free-form narrative summary.
    Summary,
    /// Extracted action items.
    ActionItems,
    /// Extracted decisions.
    Decisions,
}

impl SummaryKind {
    /// Stable lowercase token persisted in storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::ActionItems => "action_items",
            Self::Decisions => "decisions",
        }
    }

    /// Parse a persisted token, defaulting to [`SummaryKind::Summary`].
    pub fn from_str_or_summary(value: &str) -> Self {
        match value {
            "action_items" => Self::ActionItems,
            "decisions" => Self::Decisions,
            _ => Self::Summary,
        }
    }
}

/// A generated summary for a meeting (maps to `meeting_summaries`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingSummary {
    pub id: Uuid,
    pub meeting_id: Uuid,
    pub kind: SummaryKind,
    pub content: String,
    pub created_at: DateTime<Utc>,
    /// Model that produced the summary, e.g. `"qwen2.5"`; absent if unknown.
    pub model: Option<String>,
}

impl MeetingSummary {
    /// A summary with a new id and `created_at = now`.
    pub fn new(meeting_id: Uuid, kind: SummaryKind, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            meeting_id,
            kind,
            content: content.into(),
            created_at: Utc::now(),
            model: None,
        }
    }
}

/// An aggregate read-model for one meeting: the meeting row plus its speakers,
/// its `seq`-ordered transcript segments, and all stored summaries.
///
/// This is the single shape the detail view (and the export functions) consume,
/// so callers make one round trip instead of four separate queries. It is a
/// pure value object — the store assembles it in
/// [`get_meeting_detail`](crate::Meeting) territory (see `lumen_store`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingDetail {
    pub meeting: Meeting,
    /// Speakers ordered by label (`S1, S2, …`).
    pub speakers: Vec<Speaker>,
    /// Transcript segments in `seq` order.
    pub segments: Vec<TranscriptSegment>,
    /// All stored summaries (any kind), newest first.
    pub summaries: Vec<MeetingSummary>,
}

impl MeetingDetail {
    /// Look up a speaker within this detail by id.
    pub fn speaker(&self, id: Uuid) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.id == id)
    }

    /// The best human-facing name for a speaker id: the user display name when
    /// set, otherwise the engine label (`S1`), otherwise `"未知说话人"`.
    pub fn speaker_name(&self, id: Option<Uuid>) -> String {
        match id.and_then(|id| self.speaker(id)) {
            Some(s) => s.display_name.clone().unwrap_or_else(|| s.label.clone()),
            None => "未知说话人".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_status_advances_along_the_happy_path_and_pins_terminals() {
        assert_eq!(
            MeetingStatus::Recording.advance(),
            MeetingStatus::Processing
        );
        assert_eq!(
            MeetingStatus::Processing.advance(),
            MeetingStatus::Transcribing
        );
        assert_eq!(
            MeetingStatus::Transcribing.advance(),
            MeetingStatus::Summarizing
        );
        assert_eq!(MeetingStatus::Summarizing.advance(), MeetingStatus::Ready);
        assert_eq!(MeetingStatus::Ready.advance(), MeetingStatus::Ready);
        assert_eq!(MeetingStatus::Failed.advance(), MeetingStatus::Failed);
    }

    #[test]
    fn meeting_status_serde_uses_snake_case_tokens() {
        assert_eq!(
            serde_json::to_string(&MeetingStatus::Processing).unwrap(),
            "\"processing\""
        );
        assert_eq!(
            serde_json::from_str::<MeetingStatus>("\"ready\"").unwrap(),
            MeetingStatus::Ready
        );
        for status in [
            MeetingStatus::Recording,
            MeetingStatus::Processing,
            MeetingStatus::Transcribing,
            MeetingStatus::Summarizing,
            MeetingStatus::Ready,
            MeetingStatus::Failed,
        ] {
            assert_eq!(
                MeetingStatus::from_str_or_recording(status.as_str()),
                status
            );
        }
        assert_eq!(
            MeetingStatus::from_str_or_recording("nonsense"),
            MeetingStatus::Recording
        );
    }

    #[test]
    fn segment_channel_serde_uses_snake_case_tokens() {
        assert_eq!(
            serde_json::to_string(&SegmentChannel::System).unwrap(),
            "\"system\""
        );
        for channel in [SegmentChannel::Mic, SegmentChannel::System] {
            assert_eq!(SegmentChannel::from_str_or_mic(channel.as_str()), channel);
        }
        // Unknown / forward-written tokens read as mic instead of failing.
        assert_eq!(
            SegmentChannel::from_str_or_mic("nonsense"),
            SegmentChannel::Mic
        );
        // Legacy JSON without the field deserializes with channel absent.
        let legacy = r#"{"id":"7f4a1f34-2f4f-4bcb-9f43-111111111111",
            "meeting_id":"7f4a1f34-2f4f-4bcb-9f43-222222222222",
            "seq":0,"start_seconds":0.0,"end_seconds":1.0,"text":"hi",
            "speaker_id":null,"confidence":null,"words":null}"#;
        let seg: TranscriptSegment = serde_json::from_str(legacy).unwrap();
        assert_eq!(seg.channel, None);
    }

    #[test]
    fn summary_kind_serde_uses_snake_case_tokens() {
        assert_eq!(
            serde_json::to_string(&SummaryKind::ActionItems).unwrap(),
            "\"action_items\""
        );
        for kind in [
            SummaryKind::Summary,
            SummaryKind::ActionItems,
            SummaryKind::Decisions,
        ] {
            assert_eq!(SummaryKind::from_str_or_summary(kind.as_str()), kind);
        }
    }
}
