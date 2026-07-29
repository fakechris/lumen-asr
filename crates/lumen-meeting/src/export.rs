//! Export a stored meeting into one of the four M4 presets
//! (docs/MEETING_M4_UX.md, "导出（预设优先）").
//!
//! Every renderer is a **pure function** over a [`MeetingDetail`] aggregate —
//! no store, no models, no I/O — so each is unit-tested against a small fixture:
//!
//! - `会议纪要.md`   — structured minutes rendered to Markdown.
//! - `完整逐字稿.md` — the transcript, one block per **speaker turn** (runs of
//!   the same speaker merged, one timestamp each).
//! - `字幕.srt`      — SRT subtitles, one cue per segment.
//! - `会议数据.json` — a `lumen-transcript.v1` document (= the Cut import
//!   format; "导出与送 Cut 精修合一", 裁决 3).

use std::collections::HashMap;
use std::fmt::Write as _;

use lumen_core::{MeetingDetail, SummaryKind, TranscriptSegment};
use lumen_transcript::{Media, Provenance, Segment as TSegment, Speaker as TSpeaker, TranscriptV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::minutes::{parse_minutes, Minutes};

/// One of the four fixed export presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPreset {
    /// `会议纪要.md` — structured minutes as Markdown.
    MinutesMd,
    /// `完整逐字稿.md` — full transcript, one block per speaker turn.
    TranscriptMd,
    /// `字幕.srt` — SRT subtitles.
    SubtitlesSrt,
    /// `会议数据.json` — `lumen-transcript.v1` (Cut import format).
    DataJson,
}

impl ExportPreset {
    /// Parse a stable token (as sent from the UI). Accepts the snake_case ids.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minutes_md" | "minutes" => Some(Self::MinutesMd),
            "transcript_md" | "transcript" => Some(Self::TranscriptMd),
            "subtitles_srt" | "srt" | "subtitles" => Some(Self::SubtitlesSrt),
            "data_json" | "json" | "transcript_json" => Some(Self::DataJson),
            _ => None,
        }
    }

    /// The default download filename for this preset.
    pub fn filename(self) -> &'static str {
        match self {
            Self::MinutesMd => "会议纪要.md",
            Self::TranscriptMd => "完整逐字稿.md",
            Self::SubtitlesSrt => "字幕.srt",
            Self::DataJson => "会议数据.json",
        }
    }
}

/// The rendered export: a suggested filename and text content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportOutput {
    pub filename: String,
    pub content: String,
}

/// Export failures. Only the JSON preset can fail (serialization).
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("failed to serialize transcript json: {0}")]
    Serialize(String),
}

/// Render `detail` into the given `preset`.
pub fn export_meeting(
    detail: &MeetingDetail,
    preset: ExportPreset,
) -> Result<ExportOutput, ExportError> {
    let content = match preset {
        ExportPreset::MinutesMd => render_minutes_md(detail),
        ExportPreset::TranscriptMd => render_transcript_md(detail),
        ExportPreset::SubtitlesSrt => render_srt(detail),
        ExportPreset::DataJson => detail_to_transcript(detail)
            .to_json_string_pretty()
            .map_err(|e| ExportError::Serialize(e.to_string()))?,
    };
    Ok(ExportOutput {
        filename: preset.filename().to_string(),
        content,
    })
}

// ── time formatting ──────────────────────────────────────────────────

/// `mm:ss`, or `h:mm:ss` past one hour. Negatives clamp to zero.
fn fmt_clock(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `HH:MM:SS,mmm` — the SRT timestamp format. Negatives clamp to zero.
fn fmt_srt(seconds: f64) -> String {
    let clamped = seconds.max(0.0);
    let total_ms = (clamped * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total = total_ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

fn range_hint(start: f64, end: f64) -> String {
    format!("{}–{}", fmt_clock(start), fmt_clock(end))
}

// ── 会议纪要.md ───────────────────────────────────────────────────────

fn render_minutes_md(detail: &MeetingDetail) -> String {
    let title = detail.meeting.title.as_deref().unwrap_or("未命名会议");
    let mut out = format!("# {title}\n\n");

    let minutes: Option<Minutes> = detail
        .summaries
        .iter()
        .find(|s| s.kind == SummaryKind::Summary)
        .and_then(|s| parse_minutes(&s.content).ok());

    let Some(minutes) = minutes else {
        out.push_str("> 暂无纪要（尚未生成或生成失败）。\n");
        return out;
    };

    if !minutes.one_liner.trim().is_empty() {
        let _ = writeln!(out, "> {}\n", minutes.one_liner.trim());
    }

    if !minutes.decisions.is_empty() {
        out.push_str("## 决策\n\n");
        for d in &minutes.decisions {
            let hint = d
                .source
                .map(|s| format!("  `{}`", range_hint(s.start, s.end)))
                .unwrap_or_default();
            let _ = writeln!(out, "- {}{}", d.text, hint);
        }
        out.push('\n');
    }

    if !minutes.action_items.is_empty() {
        out.push_str("## 行动项\n\n");
        for a in &minutes.action_items {
            let mut line = format!("- {}", a.text);
            if let Some(owner) = &a.owner {
                let _ = write!(line, " ｜ 负责人：{owner}");
            }
            if let Some(due) = &a.due {
                let _ = write!(line, " ｜ 截止：{due}");
            }
            if let Some(s) = a.source {
                let _ = write!(line, "  `{}`", range_hint(s.start, s.end));
            }
            let _ = writeln!(out, "{line}");
        }
        out.push('\n');
    }

    if !minutes.discussion.is_empty() {
        out.push_str("## 关键讨论\n\n");
        for d in &minutes.discussion {
            let hint = d
                .source
                .map(|s| format!("  `{}`", range_hint(s.start, s.end)))
                .unwrap_or_default();
            let _ = writeln!(out, "- {}{}", d.topic, hint);
        }
        out.push('\n');
    }

    if !minutes.open_questions.is_empty() {
        out.push_str("## 未决问题\n\n");
        for q in &minutes.open_questions {
            let hint = q
                .source
                .map(|s| format!("  `{}`", range_hint(s.start, s.end)))
                .unwrap_or_default();
            let _ = writeln!(out, "- {}{}", q.text, hint);
        }
        out.push('\n');
    }

    out
}

// ── 完整逐字稿.md（按说话轮次分段）─────────────────────────────────────

fn render_transcript_md(detail: &MeetingDetail) -> String {
    let title = detail.meeting.title.as_deref().unwrap_or("未命名会议");
    let mut out = format!("# {title} — 逐字稿\n\n");

    // Merge runs of consecutive segments by the same speaker into one block,
    // one timestamp per block (the first segment's start).
    let mut idx = 0;
    let segments = &detail.segments;
    while idx < segments.len() {
        let speaker_id = segments[idx].speaker_id;
        let start = segments[idx].start_seconds;
        let mut texts = Vec::new();
        while idx < segments.len() && segments[idx].speaker_id == speaker_id {
            let text = segments[idx].text.trim();
            if !text.is_empty() {
                texts.push(text.to_string());
            }
            idx += 1;
        }
        let name = detail.speaker_name(speaker_id);
        let _ = writeln!(out, "**[{}] {}**", fmt_clock(start), name);
        out.push('\n');
        let _ = writeln!(out, "{}", texts.join(" "));
        out.push('\n');
    }

    out
}

// ── 字幕.srt ──────────────────────────────────────────────────────────

fn render_srt(detail: &MeetingDetail) -> String {
    let mut out = String::new();
    for (i, seg) in detail.segments.iter().enumerate() {
        let _ = writeln!(out, "{}", i + 1);
        let _ = writeln!(
            out,
            "{} --> {}",
            fmt_srt(seg.start_seconds),
            fmt_srt(seg.end_seconds)
        );
        let _ = writeln!(out, "{}", seg.text.trim());
        out.push('\n');
    }
    out
}

// ── 会议数据.json（= lumen-transcript.v1 / Cut 导入格式）───────────────

/// Build a `lumen-transcript.v1` document from stored rows. Segment speaker ids
/// are resolved to their stable labels (`S1`) so the document is self-contained
/// and matches the speaker table (the same shape M2b assembles from turns).
fn detail_to_transcript(detail: &MeetingDetail) -> TranscriptV1 {
    let label_of: HashMap<Uuid, &str> = detail
        .speakers
        .iter()
        .map(|s| (s.id, s.label.as_str()))
        .collect();

    let t_segments: Vec<TSegment> = detail
        .segments
        .iter()
        .map(|seg: &TranscriptSegment| {
            let mut ts = TSegment::new(seg.start_seconds, seg.end_seconds, seg.text.clone())
                .with_id(seg.seq.to_string());
            if let Some(label) = seg.speaker_id.and_then(|id| label_of.get(&id)) {
                ts = ts.with_speaker((*label).to_string());
            }
            if let Some(confidence) = seg.confidence {
                ts = ts.with_confidence(confidence);
            }
            if let Some(words) = seg.words.clone() {
                ts = ts.with_words(words);
            }
            ts
        })
        .collect();

    let t_speakers: Vec<TSpeaker> = detail
        .speakers
        .iter()
        .map(|s| {
            let mut ts = TSpeaker::new(s.label.clone());
            if let Some(name) = &s.display_name {
                ts = ts.with_display_name(name.clone());
            }
            ts
        })
        .collect();

    let media = Media {
        path: detail.meeting.audio_path.clone(),
        duration_seconds: detail.meeting.duration_seconds,
        ..Media::default()
    };

    let mut provenance = Provenance::new("lumen-meeting");
    provenance.engine = Some("diar-rs+asr".to_string());
    provenance.language = detail.meeting.language.clone();
    provenance.created_at = Some(detail.meeting.created_at.to_rfc3339());

    TranscriptV1::new(t_segments)
        .with_provenance(provenance)
        .with_media(media)
        .with_speakers(t_speakers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{Meeting, MeetingSummary, Speaker};

    fn fixture() -> MeetingDetail {
        let mut meeting = Meeting::new();
        meeting.title = Some("周会".into());
        meeting.audio_path = Some("/store/m.wav".into());
        meeting.duration_seconds = Some(65.0);
        meeting.language = Some("zh-CN".into());
        let mid = meeting.id;

        let mut s1 = Speaker::new(mid, "S1");
        s1.display_name = Some("李明".into());
        let s2 = Speaker::new(mid, "S2");

        // S1, S1 (mergeable run), then S2.
        let mut seg0 = TranscriptSegment::new(mid, 0, 0.0, 3.0, "大家好");
        seg0.speaker_id = Some(s1.id);
        seg0.confidence = Some(0.9);
        let mut seg1 = TranscriptSegment::new(mid, 1, 3.0, 6.0, "开始开会");
        seg1.speaker_id = Some(s1.id);
        let mut seg2 = TranscriptSegment::new(mid, 2, 6.0, 3671.0, "好的");
        seg2.speaker_id = Some(s2.id);

        let minutes_json = r#"{
          "one_liner": "决定发布 beta。",
          "decisions": [ { "text": "发布 beta", "source": { "start": 3.0, "end": 6.0 } } ],
          "action_items": [ { "text": "写说明", "owner": "李明", "source": { "start": 6.0, "end": 9.0 } } ],
          "discussion": [],
          "open_questions": []
        }"#;

        MeetingDetail {
            meeting,
            speakers: vec![s1, s2],
            segments: vec![seg0, seg1, seg2],
            summaries: vec![MeetingSummary::new(mid, SummaryKind::Summary, minutes_json)],
        }
    }

    #[test]
    fn clock_and_srt_time_formats() {
        assert_eq!(fmt_clock(0.0), "00:00");
        assert_eq!(fmt_clock(65.0), "01:05");
        assert_eq!(fmt_clock(3671.0), "1:01:11");
        assert_eq!(fmt_srt(0.0), "00:00:00,000");
        assert_eq!(fmt_srt(65.5), "00:01:05,500");
        assert_eq!(fmt_srt(3671.25), "01:01:11,250");
    }

    #[test]
    fn minutes_md_renders_sections_with_source_hints() {
        let out = export_meeting(&fixture(), ExportPreset::MinutesMd).unwrap();
        assert_eq!(out.filename, "会议纪要.md");
        assert!(out.content.contains("# 周会"));
        assert!(out.content.contains("> 决定发布 beta。"));
        assert!(out.content.contains("## 决策"));
        assert!(out.content.contains("- 发布 beta  `00:03–00:06`"));
        assert!(out.content.contains("## 行动项"));
        assert!(out.content.contains("负责人：李明"));
        // Empty sections are omitted.
        assert!(!out.content.contains("## 关键讨论"));
        assert!(!out.content.contains("## 未决问题"));
    }

    #[test]
    fn minutes_md_without_summary_notes_absence() {
        let mut detail = fixture();
        detail.summaries.clear();
        let out = export_meeting(&detail, ExportPreset::MinutesMd).unwrap();
        assert!(out.content.contains("暂无纪要"));
    }

    #[test]
    fn transcript_md_merges_consecutive_same_speaker_turns() {
        let out = export_meeting(&fixture(), ExportPreset::TranscriptMd).unwrap();
        // S1's two segments merge into one block with one timestamp + name.
        assert!(out.content.contains("**[00:00] 李明**"));
        assert!(out.content.contains("大家好 开始开会"));
        // S2 (no display name) uses its label.
        assert!(out.content.contains("**[00:06] S2**"));
        // Exactly two speaker blocks.
        assert_eq!(out.content.matches("**[").count(), 2);
    }

    #[test]
    fn srt_has_one_indexed_cue_per_segment() {
        let out = export_meeting(&fixture(), ExportPreset::SubtitlesSrt).unwrap();
        assert_eq!(out.filename, "字幕.srt");
        assert!(out
            .content
            .starts_with("1\n00:00:00,000 --> 00:00:03,000\n大家好\n"));
        assert!(out
            .content
            .contains("3\n00:00:06,000 --> 01:01:11,000\n好的"));
        assert_eq!(out.content.matches(" --> ").count(), 3);
    }

    #[test]
    fn data_json_is_a_valid_lumen_transcript_v1_with_labels() {
        let out = export_meeting(&fixture(), ExportPreset::DataJson).unwrap();
        assert_eq!(out.filename, "会议数据.json");
        assert!(out.content.contains("\"schema\": \"lumen-transcript.v1\""));

        let doc = TranscriptV1::from_json_str(&out.content).unwrap();
        assert_eq!(doc.segments.len(), 3);
        assert_eq!(doc.segments[0].speaker.as_deref(), Some("S1"));
        assert_eq!(doc.segments[2].speaker.as_deref(), Some("S2"));
        assert_eq!(doc.segments[0].confidence, Some(0.9));
        let speakers = doc.speakers.as_ref().unwrap();
        assert_eq!(speakers.len(), 2);
        assert_eq!(speakers[0].id, "S1");
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));
        let media = doc.media.as_ref().unwrap();
        assert_eq!(media.duration_seconds, Some(65.0));
        assert_eq!(media.path.as_deref(), Some("/store/m.wav"));
        assert_eq!(doc.provenance.as_ref().unwrap().app, "lumen-meeting");
    }
}
