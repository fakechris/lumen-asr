//! Structured meeting minutes (M4a).
//!
//! The minutes pass takes a whole meeting transcript (timestamped,
//! speaker-attributed) and asks an LLM for a **structured JSON** object — not
//! free-form Markdown — so every decision / action item can carry a `source`
//! time range the UI turns into a click-to-jump link
//! (docs/MEETING_M4_UX.md, "裁决 2").
//!
//! The LLM call goes through `lumen-corrector`'s OpenAI-compatible client (the
//! [`Corrector`] trait), so it is injectable and testable: unit tests feed a
//! fake corrector canned output and assert the JSON parsing / tolerance; the
//! real network call is exercised only by an `#[ignore]`d integration test.
//! This module has **no** platform gating — the LLM path is cross-platform.

use lumen_core::{MeetingSummary, Speaker, SummaryKind, TranscriptSegment};
use lumen_corrector::{CorrectRequest, Corrector, DictionaryContext};
use lumen_prompts::{build_minutes_system_prompt, minutes_user_message};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Default output-token budget for the minutes LLM call. Larger than the
/// dictation default (1024) so a structured document is not truncated.
pub const DEFAULT_MINUTES_MAX_TOKENS: u32 = 2048;

/// A time range (seconds from media start) pointing at the transcript span a
/// minutes item was grounded in. Mirrors a segment's `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub start: f64,
    pub end: f64,
}

/// A decision reached in the meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
}

/// A follow-up action, optionally with an owner and due date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
}

/// A key discussion point (topic-level).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscussionPoint {
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
}

/// An unresolved question left open at the end of the meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
}

/// The structured minutes document produced by the LLM pass.
///
/// All collections default to empty and unknown fields are ignored, so a
/// partial or minimal model response still deserializes (tolerance is the
/// point — a strict schema would reject otherwise-usable output).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Minutes {
    #[serde(default)]
    pub one_liner: String,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    #[serde(default)]
    pub discussion: Vec<DiscussionPoint>,
    #[serde(default)]
    pub open_questions: Vec<OpenQuestion>,
}

impl Minutes {
    /// True when the model returned nothing usable (no summary and no items).
    /// A caller may treat this as a soft failure of the summarization step.
    pub fn is_empty(&self) -> bool {
        self.one_liner.trim().is_empty()
            && self.decisions.is_empty()
            && self.action_items.is_empty()
            && self.discussion.is_empty()
            && self.open_questions.is_empty()
    }
}

/// Failure modes of the minutes pass.
#[derive(Debug, Error)]
pub enum MinutesError {
    /// The LLM call itself failed (network, provider, timeout, empty output).
    #[error("minutes llm call failed: {0}")]
    Llm(String),
    /// The model output contained no `{ … }` JSON object to parse.
    #[error("model output contained no JSON object")]
    NoJson,
    /// The extracted JSON did not parse into [`Minutes`].
    #[error("could not parse minutes JSON: {0}")]
    Parse(String),
}

/// Extract the first top-level JSON object from raw model text.
///
/// Tolerant on purpose: models wrap JSON in prose or ```json fences. Taking the
/// first `{` through the last `}` recovers the object in the common cases
/// without a streaming parser.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

/// Parse (and lightly tolerate) raw LLM output into [`Minutes`].
///
/// Strips surrounding prose / code fences, then deserializes. Missing arrays
/// default to empty; unknown fields are ignored. Returns [`MinutesError::NoJson`]
/// when there is no object at all and [`MinutesError::Parse`] when the object is
/// malformed.
pub fn parse_minutes(raw: &str) -> Result<Minutes, MinutesError> {
    let json = extract_json_object(raw).ok_or(MinutesError::NoJson)?;
    serde_json::from_str(json).map_err(|e| MinutesError::Parse(e.to_string()))
}

/// Render a meeting's segments as the timestamped, speaker-attributed transcript
/// the minutes prompt expects: one line per turn, `[start-end] 说话人：内容`.
pub fn render_transcript_for_minutes(
    segments: &[TranscriptSegment],
    speakers: &[Speaker],
) -> String {
    let name = |id: Option<Uuid>| -> String {
        match id.and_then(|id| speakers.iter().find(|s| s.id == id)) {
            Some(s) => s.display_name.clone().unwrap_or_else(|| s.label.clone()),
            None => "未知说话人".to_string(),
        }
    };
    segments
        .iter()
        .map(|seg| {
            format!(
                "[{:.1}-{:.1}] {}：{}",
                seg.start_seconds,
                seg.end_seconds,
                name(seg.speaker_id),
                seg.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Project [`Minutes`] into `meeting_summaries` rows for storage.
///
/// Three rows are written (see the storage-granularity note in the PR): the
/// full structured document under [`SummaryKind::Summary`] (the canonical
/// record the detail view renders), plus convenience projections of just the
/// decisions and action items under their own kinds. `model` labels every row.
pub fn minutes_summaries(
    meeting_id: Uuid,
    minutes: &Minutes,
    model: Option<&str>,
) -> Result<Vec<MeetingSummary>, MinutesError> {
    let full = serde_json::to_string(minutes).map_err(|e| MinutesError::Parse(e.to_string()))?;
    let decisions = serde_json::to_string(&minutes.decisions)
        .map_err(|e| MinutesError::Parse(e.to_string()))?;
    let actions = serde_json::to_string(&minutes.action_items)
        .map_err(|e| MinutesError::Parse(e.to_string()))?;

    let mut rows = vec![
        MeetingSummary::new(meeting_id, SummaryKind::Summary, full),
        MeetingSummary::new(meeting_id, SummaryKind::Decisions, decisions),
        MeetingSummary::new(meeting_id, SummaryKind::ActionItems, actions),
    ];
    if let Some(model) = model {
        for row in &mut rows {
            row.model = Some(model.to_string());
        }
    }
    Ok(rows)
}

/// Generate structured minutes from a rendered transcript via a [`Corrector`].
///
/// Builds the minutes system prompt + user message, calls the LLM (OpenAI-compat
/// client behind the trait), then parses the JSON. `max_tokens` overrides the
/// output budget ([`DEFAULT_MINUTES_MAX_TOKENS`] when `None`).
pub async fn generate_minutes(
    corrector: &dyn Corrector,
    transcript: &str,
    max_tokens: Option<u32>,
) -> Result<Minutes, MinutesError> {
    let request = CorrectRequest {
        text: minutes_user_message(transcript),
        dictionary: DictionaryContext::default(),
        context_json: None,
        system_prompt: build_minutes_system_prompt(),
        temperature: 0.2,
        max_tokens: Some(max_tokens.unwrap_or(DEFAULT_MINUTES_MAX_TOKENS)),
    };
    let result = corrector
        .correct(request)
        .await
        .map_err(|e| MinutesError::Llm(e.to_string()))?;
    parse_minutes(&result.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lumen_core::CorrectorEngineId;
    use lumen_corrector::{CorrectResult, CorrectorError};

    const SAMPLE_JSON: &str = r#"{
      "one_liner": "决定下周发布 beta。",
      "decisions": [ { "text": "发布 beta", "source": { "start": 12.5, "end": 20.0 } } ],
      "action_items": [
        { "text": "写发布说明", "owner": "李明", "due": "周五", "source": { "start": 30.0, "end": 42.0 } }
      ],
      "discussion": [ { "topic": "定价" } ],
      "open_questions": [ { "text": "是否支持 Windows？" } ]
    }"#;

    #[test]
    fn parses_a_well_formed_minutes_object() {
        let m = parse_minutes(SAMPLE_JSON).unwrap();
        assert_eq!(m.one_liner, "决定下周发布 beta。");
        assert_eq!(m.decisions.len(), 1);
        assert_eq!(m.decisions[0].source.unwrap().start, 12.5);
        assert_eq!(m.action_items[0].owner.as_deref(), Some("李明"));
        assert_eq!(m.action_items[0].due.as_deref(), Some("周五"));
        assert_eq!(m.discussion[0].topic, "定价");
        assert!(m.discussion[0].source.is_none());
        assert_eq!(m.open_questions.len(), 1);
        assert!(!m.is_empty());
    }

    #[test]
    fn extracts_json_from_fenced_or_prose_wrapped_output() {
        let fenced = format!("好的，这是纪要：\n```json\n{SAMPLE_JSON}\n```\n希望有用。");
        let m = parse_minutes(&fenced).unwrap();
        assert_eq!(m.decisions.len(), 1);
        assert_eq!(m.action_items[0].text, "写发布说明");
    }

    #[test]
    fn tolerates_missing_arrays_and_unknown_fields() {
        let m = parse_minutes(r#"{ "one_liner": "简短", "notes": "ignored" }"#).unwrap();
        assert_eq!(m.one_liner, "简短");
        assert!(m.decisions.is_empty());
        assert!(m.action_items.is_empty());
    }

    #[test]
    fn empty_object_is_valid_but_empty() {
        let m = parse_minutes("{}").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn rejects_output_with_no_json_and_malformed_json() {
        assert!(matches!(
            parse_minutes("抱歉我没法生成"),
            Err(MinutesError::NoJson)
        ));
        assert!(matches!(
            parse_minutes(r#"{ "one_liner": "x", "decisions": [ { "text": } ] }"#),
            Err(MinutesError::Parse(_))
        ));
    }

    #[test]
    fn renders_transcript_lines_with_names_and_timestamps() {
        let mid = Uuid::new_v4();
        let mut s1 = Speaker::new(mid, "S1");
        s1.display_name = Some("李明".into());
        let s2 = Speaker::new(mid, "S2");
        let mut seg0 = TranscriptSegment::new(mid, 0, 0.0, 2.0, "大家好");
        seg0.speaker_id = Some(s1.id);
        let mut seg1 = TranscriptSegment::new(mid, 1, 2.0, 5.0, "开始吧");
        seg1.speaker_id = Some(s2.id);
        let unknown = TranscriptSegment::new(mid, 2, 5.0, 6.0, "嗯");

        let rendered = render_transcript_for_minutes(&[seg0, seg1, unknown], &[s1, s2]);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "[0.0-2.0] 李明：大家好");
        assert_eq!(lines[1], "[2.0-5.0] S2：开始吧");
        assert_eq!(lines[2], "[5.0-6.0] 未知说话人：嗯");
    }

    #[test]
    fn projects_minutes_into_three_summary_rows() {
        let mid = Uuid::new_v4();
        let minutes = parse_minutes(SAMPLE_JSON).unwrap();
        let rows = minutes_summaries(mid, &minutes, Some("qwen2.5")).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, SummaryKind::Summary);
        assert_eq!(rows[1].kind, SummaryKind::Decisions);
        assert_eq!(rows[2].kind, SummaryKind::ActionItems);
        for row in &rows {
            assert_eq!(row.model.as_deref(), Some("qwen2.5"));
        }
        // The Summary row round-trips back to the same Minutes.
        assert_eq!(parse_minutes(&rows[0].content).unwrap(), minutes);
        // The Decisions row is a JSON array of the decisions.
        let decisions: Vec<Decision> = serde_json::from_str(&rows[1].content).unwrap();
        assert_eq!(decisions, minutes.decisions);
    }

    // ---- generate_minutes with a fake corrector -------------------------

    struct CannedCorrector(Result<String, ()>);

    #[async_trait]
    impl Corrector for CannedCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, _req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            match &self.0 {
                Ok(text) => Ok(CorrectResult {
                    text: text.clone(),
                    engine: CorrectorEngineId::OpenAiCompatible,
                    model_applied: true,
                    fallback_reason: None,
                }),
                Err(()) => Err(CorrectorError::Timeout),
            }
        }
    }

    #[tokio::test]
    async fn generate_minutes_parses_canned_llm_output() {
        let corrector = CannedCorrector(Ok(SAMPLE_JSON.to_string()));
        let minutes = generate_minutes(&corrector, "[0-2] S1：你好", None)
            .await
            .unwrap();
        assert_eq!(minutes.decisions.len(), 1);
    }

    #[tokio::test]
    async fn generate_minutes_surfaces_llm_and_parse_failures() {
        let failed = CannedCorrector(Err(()));
        assert!(matches!(
            generate_minutes(&failed, "x", None).await,
            Err(MinutesError::Llm(_))
        ));

        let garbage = CannedCorrector(Ok("no json here".to_string()));
        assert!(matches!(
            generate_minutes(&garbage, "x", None).await,
            Err(MinutesError::NoJson)
        ));
    }
}
