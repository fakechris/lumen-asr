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

use crate::minutes_template::MinutesTemplate;
use lumen_core::{MeetingSummary, Speaker, SummaryKind, TranscriptSegment};
use lumen_corrector::{CorrectRequest, Corrector, DictionaryContext};
use lumen_prompts::{
    build_minutes_merge_system_prompt_with_template, build_minutes_system_prompt_with_template,
    minutes_merge_user_message, minutes_retry_user_message, minutes_user_message_with_notes,
    minutes_window_user_message,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Default output-token budget for the final minutes document. Well above the
/// dictation default (1024): a long meeting's structured document can run past
/// 2k tokens, and a cut-off JSON object cannot be parsed at all.
pub const DEFAULT_MINUTES_MAX_TOKENS: u32 = 4096;

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
pub(crate) fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

/// Best-effort repair of the two JSON mistakes models make most often in
/// Chinese minutes: an ASCII `"` used as a quotation mark *inside* a string
/// value (`"讨论"灰度"方案"`), and a trailing comma before `}` / `]`.
///
/// A `"` inside a string is treated as the closing quote only when the next
/// non-whitespace character can follow a string (`,` `}` `]` `:` or the end);
/// otherwise it is escaped as content. Only applied after a strict parse has
/// failed, so valid output is never touched.
pub(crate) fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
            } else if c == '\\' {
                escaped = true;
                out.push(c);
            } else if c == '"' {
                let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
                if matches!(next, None | Some(',' | '}' | ']' | ':')) {
                    in_string = false;
                    out.push(c);
                } else {
                    out.push_str("\\\"");
                }
            } else {
                out.push(c);
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Parse (and tolerate) raw LLM output into [`Minutes`].
///
/// Tries, in order: the first JSON value starting at the first `{` (so trailing
/// prose or a closing fence is ignored); the first-`{`-to-last-`}` slice; and
/// that slice after [`repair_json`]. Missing arrays default to empty; unknown
/// fields are ignored. Returns [`MinutesError::NoJson`] when there is no object
/// at all and [`MinutesError::Parse`] (with the strict parser's error) when no
/// attempt yields a valid object.
pub fn parse_minutes(raw: &str) -> Result<Minutes, MinutesError> {
    let start = raw.find('{').ok_or(MinutesError::NoJson)?;
    let mut stream = serde_json::Deserializer::from_str(&raw[start..]);
    let strict_error = match Minutes::deserialize(&mut stream) {
        Ok(minutes) => return Ok(minutes),
        Err(e) => e.to_string(),
    };
    let json = extract_json_object(raw).ok_or(MinutesError::NoJson)?;
    if let Ok(minutes) = serde_json::from_str(json) {
        return Ok(minutes);
    }
    serde_json::from_str(&repair_json(json)).map_err(|_| MinutesError::Parse(strict_error))
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
///
/// `notes` are the free-form notes the user took during the meeting: when
/// present and non-blank they are fused into the prompt as extra context so the
/// structured minutes reflect what the user flagged as important (Granola-style).
/// Passing `None` (or blank notes) yields the transcript-only behaviour.
///
/// `template` steers the style/emphasis of the minutes (see
/// [`crate::minutes_template`]): its body is interpolated into the system
/// prompt as an advisory section. `None` (or a template with an empty body, as
/// the built-in default has) produces the exact pre-template prompt.
///
/// A transcript longer than [`SINGLE_PASS_MAX_CHARS`] is summarized window by
/// window (about [`WINDOW_CHARS`] each, split on line boundaries) and the
/// partial minutes are then merged — hierarchically when the partials
/// themselves are too long — so a long meeting never depends on one huge
/// prompt fitting the provider's context. Transcript lines carry absolute
/// timestamps, so every window's `source` ranges stay valid in the result.
///
/// Every LLM step is retried once: a failed call is repeated with a doubled
/// output budget (covers truncation as well as transient network errors), and
/// unparseable output is re-asked with a strict JSON-only reminder.
pub async fn generate_minutes(
    corrector: &dyn Corrector,
    transcript: &str,
    notes: Option<&str>,
    max_tokens: Option<u32>,
    template: Option<&MinutesTemplate>,
) -> Result<Minutes, MinutesError> {
    let template_body = template.map(|t| t.body.as_str());
    let final_budget = max_tokens.unwrap_or(DEFAULT_MINUTES_MAX_TOKENS);
    if transcript.chars().count() <= SINGLE_PASS_MAX_CHARS {
        return minutes_step(
            corrector,
            build_minutes_system_prompt_with_template(template_body),
            minutes_user_message_with_notes(transcript, notes),
            final_budget,
        )
        .await;
    }

    let windows = split_lines_into_windows(transcript, WINDOW_CHARS);
    let total = windows.len();
    let mut partials = Vec::with_capacity(total);
    for (index, window) in windows.iter().enumerate() {
        let partial = minutes_step(
            corrector,
            build_minutes_system_prompt_with_template(None),
            minutes_window_user_message(window, index + 1, total, notes),
            WINDOW_MINUTES_MAX_TOKENS,
        )
        .await?;
        if !partial.is_empty() {
            partials.push(partial);
        }
    }
    tracing::info!(
        windows = total,
        partials = partials.len(),
        "long meeting minutes: windows summarized, merging"
    );
    merge_partials(corrector, partials, notes, template_body, final_budget).await
}

/// Transcripts up to this many characters are summarized in one LLM call.
pub const SINGLE_PASS_MAX_CHARS: usize = 16_000;

/// Target size of one transcript window for long meetings.
pub const WINDOW_CHARS: usize = 12_000;

/// Output budget for one window's partial minutes (they cover a fraction of
/// the meeting, so they are smaller than the final document).
const WINDOW_MINUTES_MAX_TOKENS: u32 = 2048;

/// The merge input (serialized partials) is grouped so one merge call never
/// receives more than this many characters.
const MERGE_INPUT_MAX_CHARS: usize = 16_000;

/// Upper bound on hierarchical merge rounds; past it the remaining partials
/// are combined without the LLM.
const MAX_MERGE_ROUNDS: usize = 4;

/// Ceiling for the doubled output budget of a retried call.
const RETRY_MAX_TOKENS_CEILING: u32 = 8192;

/// One minutes LLM step with a single retry (see [`generate_minutes`]).
async fn minutes_step(
    corrector: &dyn Corrector,
    system_prompt: String,
    user_message: String,
    max_tokens: u32,
) -> Result<Minutes, MinutesError> {
    let request = |text: String, max_tokens: u32| CorrectRequest {
        text,
        dictionary: DictionaryContext::default(),
        context_json: None,
        system_prompt: system_prompt.clone(),
        temperature: 0.2,
        max_tokens: Some(max_tokens),
    };
    let raw = match corrector
        .correct(request(user_message.clone(), max_tokens))
        .await
    {
        Ok(result) => result.text,
        Err(first) => {
            let retry_budget = max_tokens.saturating_mul(2).min(RETRY_MAX_TOKENS_CEILING);
            tracing::warn!(error = %first, retry_budget, "minutes llm call failed; retrying once");
            corrector
                .correct(request(user_message.clone(), retry_budget))
                .await
                .map_err(|e| MinutesError::Llm(e.to_string()))?
                .text
        }
    };
    let error = match parse_minutes(&raw) {
        Ok(minutes) => return Ok(minutes),
        Err(error) => error,
    };
    // Lengths only: the output is meeting content and stays out of the log.
    tracing::warn!(error = %error, output_chars = raw.chars().count(), "minutes output did not parse; re-asking once");
    let retry = corrector
        .correct(request(
            minutes_retry_user_message(&user_message, &error.to_string()),
            max_tokens,
        ))
        .await
        .map_err(|e| MinutesError::Llm(e.to_string()))?;
    parse_minutes(&retry.text)
}

/// Merge partial minutes into one document, in rounds of at most
/// [`MERGE_INPUT_MAX_CHARS`] of serialized input per call.
async fn merge_partials(
    corrector: &dyn Corrector,
    mut partials: Vec<Minutes>,
    notes: Option<&str>,
    template_body: Option<&str>,
    final_budget: u32,
) -> Result<Minutes, MinutesError> {
    if partials.is_empty() {
        return Ok(Minutes::default());
    }
    for _ in 0..MAX_MERGE_ROUNDS {
        if partials.len() == 1 {
            return Ok(partials.remove(0));
        }
        let serialized: Vec<String> = partials
            .iter()
            .map(|p| serde_json::to_string(p).map_err(|e| MinutesError::Parse(e.to_string())))
            .collect::<Result<_, _>>()?;
        let groups = group_by_chars(&serialized, MERGE_INPUT_MAX_CHARS);
        if groups.len() == 1 {
            return minutes_step(
                corrector,
                build_minutes_merge_system_prompt_with_template(template_body),
                minutes_merge_user_message(&serialized, notes),
                final_budget,
            )
            .await;
        }
        let mut merged = Vec::with_capacity(groups.len());
        for range in groups {
            if range.len() == 1 {
                merged.push(partials[range.start].clone());
                continue;
            }
            merged.push(
                minutes_step(
                    corrector,
                    build_minutes_merge_system_prompt_with_template(None),
                    minutes_merge_user_message(&serialized[range], notes),
                    WINDOW_MINUTES_MAX_TOKENS,
                )
                .await?,
            );
        }
        partials = merged;
    }
    tracing::warn!(
        partials = partials.len(),
        "minutes merge did not converge; combining partials directly"
    );
    Ok(concat_partials(partials))
}

/// Combine partial minutes without the LLM: arrays are concatenated in time
/// order with exact-duplicate texts dropped, and the one-liners are joined.
fn concat_partials(partials: Vec<Minutes>) -> Minutes {
    let mut out = Minutes::default();
    let one_liners: Vec<String> = partials
        .iter()
        .map(|p| p.one_liner.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.one_liner = one_liners.join("；");
    for p in partials {
        for d in p.decisions {
            if !out.decisions.iter().any(|x| x.text == d.text) {
                out.decisions.push(d);
            }
        }
        for a in p.action_items {
            if !out.action_items.iter().any(|x| x.text == a.text) {
                out.action_items.push(a);
            }
        }
        for t in p.discussion {
            if !out.discussion.iter().any(|x| x.topic == t.topic) {
                out.discussion.push(t);
            }
        }
        for q in p.open_questions {
            if !out.open_questions.iter().any(|x| x.text == q.text) {
                out.open_questions.push(q);
            }
        }
    }
    out
}

/// Split a rendered transcript into windows of about `budget` characters on
/// line boundaries. A single line longer than the budget forms its own window.
fn split_lines_into_windows(transcript: &str, budget: usize) -> Vec<String> {
    let lines: Vec<&str> = transcript.lines().collect();
    let sizes: Vec<usize> = lines.iter().map(|l| l.chars().count() + 1).collect();
    group_sizes(&sizes, budget)
        .into_iter()
        .map(|range| lines[range].join("\n"))
        .collect()
}

/// Group serialized items into consecutive ranges of at most `budget` chars.
fn group_by_chars(items: &[String], budget: usize) -> Vec<std::ops::Range<usize>> {
    let sizes: Vec<usize> = items.iter().map(|s| s.chars().count() + 1).collect();
    group_sizes(&sizes, budget)
}

/// Greedy consecutive grouping: each range's total size stays within `budget`
/// unless a single item alone exceeds it.
fn group_sizes(sizes: &[usize], budget: usize) -> Vec<std::ops::Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (i, &size) in sizes.iter().enumerate() {
        if i > start && used + size > budget {
            groups.push(start..i);
            start = i;
            used = 0;
        }
        used += size;
    }
    if start < sizes.len() {
        groups.push(start..sizes.len());
    }
    groups
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
        let minutes = generate_minutes(&corrector, "[0-2] S1：你好", None, None, None)
            .await
            .unwrap();
        assert_eq!(minutes.decisions.len(), 1);
    }

    #[tokio::test]
    async fn generate_minutes_surfaces_llm_and_parse_failures() {
        let failed = CannedCorrector(Err(()));
        assert!(matches!(
            generate_minutes(&failed, "x", None, None, None).await,
            Err(MinutesError::Llm(_))
        ));

        let garbage = CannedCorrector(Ok("no json here".to_string()));
        assert!(matches!(
            generate_minutes(&garbage, "x", None, None, None).await,
            Err(MinutesError::NoJson)
        ));
    }

    /// Corrector that records the prompt text it was asked to correct, so a test
    /// can assert the user's notes were fused into the minutes request.
    struct CapturingCorrector(std::sync::Mutex<Option<CorrectRequest>>);

    #[async_trait]
    impl Corrector for CapturingCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            *self.0.lock().unwrap() = Some(req);
            Ok(CorrectResult {
                text: SAMPLE_JSON.to_string(),
                engine: CorrectorEngineId::OpenAiCompatible,
                model_applied: true,
                fallback_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn generate_minutes_fuses_user_notes_into_the_prompt() {
        let corrector = CapturingCorrector(std::sync::Mutex::new(None));
        let minutes = generate_minutes(
            &corrector,
            "[0-2] S1：你好",
            Some("跟进预算问题"),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(minutes.decisions.len(), 1);
        let sent = corrector.0.lock().unwrap().clone().unwrap();
        assert!(sent.text.contains("USER_NOTES_BEGIN"));
        assert!(sent.text.contains("跟进预算问题"));

        // Blank notes leave the prompt in its transcript-only shape.
        let corrector = CapturingCorrector(std::sync::Mutex::new(None));
        generate_minutes(&corrector, "[0-2] S1：你好", Some("  "), None, None)
            .await
            .unwrap();
        let sent = corrector.0.lock().unwrap().clone().unwrap();
        assert!(!sent.text.contains("USER_NOTES_BEGIN"));
    }

    #[test]
    fn repairs_ascii_quotes_inside_strings_and_trailing_commas() {
        // The failure class seen on real long meetings: a quotation mark used
        // inside a Chinese string value breaks the JSON mid-line.
        let raw = r#"{"one_liner": "讨论"灰度发布"方案", "decisions": [ {"text": "先发 10%",}, ], "discussion": []}"#;
        assert!(serde_json::from_str::<Minutes>(raw).is_err());
        let m = parse_minutes(raw).unwrap();
        assert_eq!(m.one_liner, "讨论\"灰度发布\"方案");
        assert_eq!(m.decisions[0].text, "先发 10%");
    }

    #[test]
    fn ignores_trailing_prose_that_contains_braces() {
        let raw = format!("{SAMPLE_JSON}\n以上纪要基于 {{转录}} 生成。");
        let m = parse_minutes(&raw).unwrap();
        assert_eq!(m.decisions.len(), 1);
    }

    #[test]
    fn windows_split_on_line_boundaries_within_budget() {
        let transcript = (0..10)
            .map(|i| format!("[{i}.0-{}.0] S1：{}", i + 1, "字".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let windows = split_lines_into_windows(&transcript, 120);
        assert!(windows.len() > 1);
        assert_eq!(windows.join("\n"), transcript);
        for w in &windows {
            assert!(w.chars().count() <= 120 || !w.contains('\n'));
        }
        // An oversized single line is kept whole, in its own window.
        let long = "长".repeat(500);
        assert_eq!(split_lines_into_windows(&long, 120), vec![long.clone()]);
    }

    /// Corrector that replays scripted replies in order and records every
    /// request, so a test can assert the call sequence.
    struct ScriptedCorrector {
        replies: std::sync::Mutex<std::collections::VecDeque<Result<String, ()>>>,
        seen: std::sync::Mutex<Vec<CorrectRequest>>,
    }

    impl ScriptedCorrector {
        fn new(replies: Vec<Result<String, ()>>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into()),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<CorrectRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Corrector for ScriptedCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            self.seen.lock().unwrap().push(req);
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unscripted call")
            {
                Ok(text) => Ok(CorrectResult {
                    text,
                    engine: CorrectorEngineId::OpenAiCompatible,
                    model_applied: true,
                    fallback_reason: None,
                }),
                Err(()) => Err(CorrectorError::Timeout),
            }
        }
    }

    fn partial(one_liner: &str, decision: &str, start: f64) -> String {
        format!(
            r#"{{"one_liner":"{one_liner}","decisions":[{{"text":"{decision}","source":{{"start":{start},"end":{}}}}}]}}"#,
            start + 5.0
        )
    }

    fn long_transcript(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "[{}.0-{}.0] S1：{}",
                    i * 10,
                    i * 10 + 9,
                    "讨论内容".repeat(30)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn short_transcript_stays_single_pass() {
        let corrector = ScriptedCorrector::new(vec![Ok(SAMPLE_JSON.to_string())]);
        generate_minutes(&corrector, "[0-2] S1：你好", None, None, None)
            .await
            .unwrap();
        let seen = corrector.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].max_tokens, Some(DEFAULT_MINUTES_MAX_TOKENS));
        assert!(seen[0].text.contains("TRANSCRIPT_BEGIN"));
        assert!(!seen[0].text.contains("切分后的第"));
    }

    #[tokio::test]
    async fn long_transcript_maps_windows_then_merges() {
        let transcript = long_transcript(150); // ~19k chars → 2 windows
        assert!(transcript.chars().count() > SINGLE_PASS_MAX_CHARS);
        let windows = split_lines_into_windows(&transcript, WINDOW_CHARS).len();
        let mut replies: Vec<Result<String, ()>> = (0..windows)
            .map(|i| {
                Ok(partial(
                    &format!("第{i}段"),
                    &format!("决定{i}"),
                    i as f64 * 100.0,
                ))
            })
            .collect();
        replies.push(Ok(SAMPLE_JSON.to_string()));
        let corrector = ScriptedCorrector::new(replies);

        let minutes = generate_minutes(&corrector, &transcript, Some("关注预算"), None, None)
            .await
            .unwrap();
        assert_eq!(minutes.one_liner, "决定下周发布 beta。");
        let seen = corrector.seen();
        assert_eq!(seen.len(), windows + 1);
        for (i, req) in seen[..windows].iter().enumerate() {
            assert!(req.text.contains(&format!("第 {}/{windows} 段", i + 1)));
            assert!(req.text.chars().count() < WINDOW_CHARS + 1_000);
        }
        let merge = &seen[windows];
        assert!(merge.system_prompt.contains("会议纪要合并器"));
        assert!(merge.text.contains("PARTIALS_BEGIN"));
        assert!(merge.text.contains("决定0") && merge.text.contains("决定1"));
        assert!(merge.text.contains("关注预算"));
        assert_eq!(merge.max_tokens, Some(DEFAULT_MINUTES_MAX_TOKENS));
    }

    #[tokio::test]
    async fn unparseable_output_is_reasked_once_with_a_strict_reminder() {
        let corrector = ScriptedCorrector::new(vec![
            Ok(r#"{"one_liner": "x", "decisions": [ {"text": } ]}"#.to_string()),
            Ok(format!("```json\n{SAMPLE_JSON}\n```")),
        ]);
        let minutes = generate_minutes(&corrector, "[0-2] S1：你好", None, None, None)
            .await
            .unwrap();
        assert_eq!(minutes.decisions.len(), 1);
        let seen = corrector.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].text.contains("无法解析为合法 JSON"));
        assert!(seen[1].text.contains("TRANSCRIPT_BEGIN"));
    }

    #[tokio::test]
    async fn failed_call_is_retried_once_with_a_larger_budget() {
        let corrector = ScriptedCorrector::new(vec![Err(()), Ok(SAMPLE_JSON.to_string())]);
        generate_minutes(&corrector, "[0-2] S1：你好", None, None, None)
            .await
            .unwrap();
        let seen = corrector.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].max_tokens, Some(DEFAULT_MINUTES_MAX_TOKENS * 2));

        // Two failures in a row surface as an LLM error.
        let corrector = ScriptedCorrector::new(vec![Err(()), Err(())]);
        assert!(matches!(
            generate_minutes(&corrector, "x", None, None, None).await,
            Err(MinutesError::Llm(_))
        ));
    }

    #[tokio::test]
    async fn merge_groups_large_partial_sets_hierarchically() {
        // Many windows whose partials together exceed one merge call's input.
        let big = "要点".repeat(2_000);
        let partials: Vec<Minutes> = (0..6)
            .map(|i| {
                parse_minutes(&partial(
                    &format!("{big}{i}"),
                    &format!("决定{i}"),
                    i as f64,
                ))
                .unwrap()
            })
            .collect();
        // Round 1 merges two groups of three; round 2 merges the two results.
        let replies = vec![
            Ok(partial("组1", "决定A", 0.0)),
            Ok(partial("组2", "决定B", 50.0)),
            Ok(SAMPLE_JSON.to_string()),
        ];
        let corrector = ScriptedCorrector::new(replies);
        let minutes = merge_partials(&corrector, partials, None, None, DEFAULT_MINUTES_MAX_TOKENS)
            .await
            .unwrap();
        assert_eq!(minutes.one_liner, "决定下周发布 beta。");
        let seen = corrector.seen();
        assert_eq!(seen.len(), 3);
        assert!(seen[2].text.contains("组1") && seen[2].text.contains("组2"));
        for req in &seen {
            assert!(req.text.chars().count() < MERGE_INPUT_MAX_CHARS + 1_000);
        }
    }

    #[test]
    fn concat_fallback_keeps_items_in_order_without_duplicates() {
        let a = parse_minutes(&partial("上半场", "发布 beta", 1.0)).unwrap();
        let b = parse_minutes(&partial("下半场", "发布 beta", 2.0)).unwrap();
        let c = parse_minutes(&partial("", "定价", 3.0)).unwrap();
        let m = concat_partials(vec![a, b, c]);
        assert_eq!(m.one_liner, "上半场；下半场");
        let texts: Vec<&str> = m.decisions.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts, vec!["发布 beta", "定价"]);
        assert_eq!(m.decisions[0].source.unwrap().start, 1.0);
    }

    #[tokio::test]
    async fn generate_minutes_interpolates_the_template_body_into_the_system_prompt() {
        let template = MinutesTemplate {
            name: "action-items".into(),
            description: String::new(),
            language: None,
            body: "- 以 action_items 为核心".into(),
            builtin: true,
        };
        let corrector = CapturingCorrector(std::sync::Mutex::new(None));
        generate_minutes(&corrector, "[0-2] S1：你好", None, None, Some(&template))
            .await
            .unwrap();
        let sent = corrector.0.lock().unwrap().clone().unwrap();
        assert!(sent.system_prompt.contains("TEMPLATE_BEGIN"));
        assert!(sent.system_prompt.contains("以 action_items 为核心"));
        // The JSON contract is intact ahead of the template section.
        assert!(sent.system_prompt.contains("one_liner"));
        assert!(sent.system_prompt.contains("只输出一个 JSON 对象"));

        // No template → the pre-template constant prompt.
        let corrector = CapturingCorrector(std::sync::Mutex::new(None));
        generate_minutes(&corrector, "[0-2] S1：你好", None, None, None)
            .await
            .unwrap();
        let sent = corrector.0.lock().unwrap().clone().unwrap();
        assert!(!sent.system_prompt.contains("TEMPLATE_BEGIN"));
    }
}
