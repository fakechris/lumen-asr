//! Deterministic local recap for meetings processed without an LLM.
//!
//! With no language model configured the pipeline produces a transcript but no
//! minutes. Rather than leave the minutes column empty, this module scans the
//! transcript for sentences carrying explicit decision or commitment wording
//! and lists them with their speaker and time range. When nothing matches it
//! falls back to an opening and a closing excerpt so the user still gets a
//! glimpse of the meeting.
//!
//! The recap is keyword extraction, not summarization: it never paraphrases,
//! it only quotes. It is stored under [`SummaryKind::LocalRecap`] in the same
//! JSON shape as [`Minutes`], so the detail view and the Markdown export render
//! it with the existing minutes code while labelling it as a local recap.
//!
//! Rules (all deterministic, no I/O):
//! - a sentence ending in `？`/`?` (or a Chinese question particle) is a
//!   question: it is neither a decision nor an action item;
//! - a sentence matching both cue lists is listed once, as a decision;
//! - fragments shorter than [`MIN_SENTENCE_CHARS`] (a bare "同意。") are
//!   skipped — they carry no content on their own;
//! - at most [`MAX_DECISIONS`] decisions and [`MAX_ACTION_ITEMS`] action items
//!   are kept, in timeline order; repeated sentences are listed once.

use lumen_core::{MeetingSummary, Speaker, SummaryKind, TranscriptSegment};
use uuid::Uuid;

use crate::minutes::{ActionItem, Decision, DiscussionPoint, Minutes, SourceRef};

/// Most decisions listed in a local recap.
pub const MAX_DECISIONS: usize = 4;
/// Most action items listed in a local recap.
pub const MAX_ACTION_ITEMS: usize = 8;
/// Target length (chars) of each fallback excerpt.
pub const EXCERPT_CHARS: usize = 240;
/// Sentences shorter than this (excluding punctuation) are never listed.
pub const MIN_SENTENCE_CHARS: usize = 6;
/// Listed sentences longer than this are cut with an ellipsis.
const MAX_ITEM_CHARS: usize = 160;

/// Chinese decision cues (matched as substrings).
const DECISION_CUES_ZH: &[&str] = &["决定", "同意", "确认", "拍板", "定了", "就这么"];
/// English decision cues (matched as whole words, case-insensitive).
const DECISION_CUES_EN: &[&str] = &["agreed", "decided", "approved", "confirmed"];
/// Phrases containing a decision cue that ask for, rather than state, a
/// decision ("需要确认一下" is open work, not a conclusion).
const DECISION_NEGATIVES_ZH: &[&str] = &["确认一下", "待确认", "需要确认", "再确认", "不同意"];

/// Chinese commitment cues (matched as substrings).
const ACTION_CUES_ZH: &[&str] = &["我来", "我会", "负责", "跟进", "下周", "明天", "截止"];
/// English commitment cues (matched as whole words, case-insensitive).
const ACTION_CUES_EN: &[&str] = &[
    "todo",
    "will",
    "i'll",
    "we'll",
    "follow up",
    "follow-up",
    "action item",
];

/// Build the local recap for a transcript. Returns an empty [`Minutes`] when
/// the transcript has no text.
pub fn build_local_recap(segments: &[TranscriptSegment], speakers: &[Speaker]) -> Minutes {
    let mut recap = Minutes::default();
    let mut seen: Vec<String> = Vec::new();
    for seg in segments {
        let speaker = speaker_name(seg.speaker_id, speakers);
        let source = Some(SourceRef {
            start: seg.start_seconds,
            end: seg.end_seconds,
        });
        for sentence in split_sentences(&seg.text) {
            if content_chars(sentence) < MIN_SENTENCE_CHARS || is_question(sentence) {
                continue;
            }
            // Decision wording wins over commitment wording, so a sentence
            // with both is listed once (as a decision), even once the
            // decision list is full.
            let decision = is_decision(sentence);
            let keep = if decision {
                recap.decisions.len() < MAX_DECISIONS
            } else {
                recap.action_items.len() < MAX_ACTION_ITEMS && is_action(sentence)
            };
            if !keep || seen.iter().any(|s| s == sentence) {
                continue;
            }
            seen.push(sentence.to_string());
            let text = format!("{speaker}：{}", truncate(sentence, MAX_ITEM_CHARS));
            if decision {
                recap.decisions.push(Decision { text, source });
            } else {
                recap.action_items.push(ActionItem {
                    text,
                    owner: None,
                    due: None,
                    source,
                });
            }
        }
    }
    if recap.decisions.is_empty() && recap.action_items.is_empty() {
        recap.discussion = fallback_excerpts(segments, speakers);
    }
    recap
}

/// The [`SummaryKind::LocalRecap`] row for a transcript, or `None` when the
/// transcript has no text to recap.
pub fn local_recap_summary(
    meeting_id: Uuid,
    segments: &[TranscriptSegment],
    speakers: &[Speaker],
) -> Option<MeetingSummary> {
    let recap = build_local_recap(segments, speakers);
    if recap.is_empty() {
        return None;
    }
    let content = serde_json::to_string(&recap).ok()?;
    Some(MeetingSummary::new(
        meeting_id,
        SummaryKind::LocalRecap,
        content,
    ))
}

fn speaker_name(id: Option<Uuid>, speakers: &[Speaker]) -> String {
    match id.and_then(|id| speakers.iter().find(|s| s.id == id)) {
        Some(s) => s
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&s.label)
            .to_string(),
        None => "未知说话人".to_string(),
    }
}

/// Split a segment's text into trimmed sentences, keeping each sentence's
/// terminal punctuation (needed to recognise questions). A `.` ends a sentence
/// only before whitespace or the end, so decimals like `3.5` stay intact.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let ends = match c {
            '。' | '！' | '？' | '!' | '?' | '；' | ';' | '\n' => true,
            '.' => chars.peek().is_none_or(|(_, next)| next.is_whitespace()),
            _ => false,
        };
        if ends {
            let end = i + c.len_utf8();
            push_sentence(&mut out, &text[start..end]);
            start = end;
        }
    }
    push_sentence(&mut out, &text[start..]);
    out
}

fn push_sentence<'a>(out: &mut Vec<&'a str>, sentence: &'a str) {
    let sentence = sentence.trim();
    if !sentence.is_empty() {
        out.push(sentence);
    }
}

/// Characters that count towards a sentence's length: letters, digits, CJK.
fn content_chars(sentence: &str) -> usize {
    sentence.chars().filter(|c| c.is_alphanumeric()).count()
}

/// A question asks; it does not decide or commit. Recognised by a trailing
/// `？`/`?`, or — since ASR output often drops the mark — a trailing Chinese
/// question particle.
fn is_question(sentence: &str) -> bool {
    let body = sentence.trim_end_matches(|c: char| {
        matches!(c, '。' | '！' | '!' | '；' | ';' | '.' | '…') || c.is_whitespace()
    });
    body.ends_with('？')
        || body.ends_with('?')
        || body.ends_with('吗')
        || body.ends_with('么')
        || body.ends_with('呢')
}

fn has_decision_cue(sentence: &str) -> bool {
    DECISION_CUES_ZH.iter().any(|cue| sentence.contains(cue))
        || has_passed_cue(sentence)
        || has_english_word(sentence, DECISION_CUES_EN)
}

fn is_decision(sentence: &str) -> bool {
    has_decision_cue(sentence)
        && !DECISION_NEGATIVES_ZH
            .iter()
            .any(|neg| sentence.contains(neg))
}

/// "通过" also means "by way of" (`通过这个接口`); it reads as "approved" only
/// when completed (`通过了`) or closing the sentence (`方案一致通过。`).
fn has_passed_cue(sentence: &str) -> bool {
    let body = sentence.trim_end_matches(|c: char| !c.is_alphanumeric());
    sentence.contains("通过了") || body.ends_with("通过")
}

fn is_action(sentence: &str) -> bool {
    ACTION_CUES_ZH.iter().any(|cue| sentence.contains(cue))
        || has_weekday_deadline(sentence)
        || has_english_word(sentence, ACTION_CUES_EN)
}

/// `周一前` … `周五前`.
fn has_weekday_deadline(sentence: &str) -> bool {
    let chars: Vec<char> = sentence.chars().collect();
    chars
        .windows(3)
        .any(|w| w[0] == '周' && matches!(w[1], '一' | '二' | '三' | '四' | '五') && w[2] == '前')
}

/// Whether any of `cues` occurs in `sentence` as a whole word (or phrase),
/// case-insensitively: "will" matches "we will" but not "willing".
fn has_english_word(sentence: &str, cues: &[&str]) -> bool {
    let lower = sentence.to_lowercase().replace('’', "'");
    cues.iter().any(|cue| {
        lower.match_indices(cue).any(|(i, m)| {
            let before = lower[..i].chars().next_back();
            let after = lower[i + m.len()..].chars().next();
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\''
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Opening and closing excerpts of roughly [`EXCERPT_CHARS`] each. The closing
/// excerpt only uses segments the opening one did not, so a short meeting gets
/// a single excerpt rather than the same text twice.
fn fallback_excerpts(segments: &[TranscriptSegment], speakers: &[Speaker]) -> Vec<DiscussionPoint> {
    let lines: Vec<(&TranscriptSegment, String)> = segments
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| {
            let line = format!(
                "{}：{}",
                speaker_name(s.speaker_id, speakers),
                s.text.trim()
            );
            (s, line)
        })
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut used = 0;
    let mut len = 0;
    while used < lines.len() && len < EXCERPT_CHARS {
        len += lines[used].1.chars().count();
        used += 1;
    }
    let head = &lines[..used];
    let mut out = vec![excerpt("开场", head, false)];

    let rest = &lines[used..];
    if !rest.is_empty() {
        let mut from = rest.len();
        let mut len = 0;
        while from > 0 && len < EXCERPT_CHARS {
            from -= 1;
            len += rest[from].1.chars().count();
        }
        out.push(excerpt("结尾", &rest[from..], true));
    }
    out
}

/// Join `lines` into one excerpt capped at [`EXCERPT_CHARS`]; `keep_tail`
/// keeps the end of the text (for the closing excerpt) instead of the start.
fn excerpt(
    label: &str,
    lines: &[(&TranscriptSegment, String)],
    keep_tail: bool,
) -> DiscussionPoint {
    let joined = lines
        .iter()
        .map(|(_, l)| l.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let count = joined.chars().count();
    let body = if count <= EXCERPT_CHARS {
        joined
    } else if keep_tail {
        let tail: String = joined.chars().skip(count - EXCERPT_CHARS).collect();
        format!("…{tail}")
    } else {
        let head: String = joined.chars().take(EXCERPT_CHARS).collect();
        format!("{head}…")
    };
    let first = lines.first().map(|(s, _)| s.start_seconds).unwrap_or(0.0);
    let last = lines.last().map(|(s, _)| s.end_seconds).unwrap_or(first);
    DiscussionPoint {
        topic: format!("{label}：{body}"),
        source: Some(SourceRef {
            start: first,
            end: last,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        meeting_id: Uuid,
        speakers: Vec<Speaker>,
        segments: Vec<TranscriptSegment>,
    }

    impl Fixture {
        fn new() -> Self {
            let meeting_id = Uuid::new_v4();
            let mut alice = Speaker::new(meeting_id, "S1");
            alice.display_name = Some("李明".into());
            let bob = Speaker::new(meeting_id, "S2");
            Self {
                meeting_id,
                speakers: vec![alice, bob],
                segments: Vec::new(),
            }
        }

        /// Add a 10-second segment spoken by speaker `who` (0 or 1).
        fn say(&mut self, who: usize, text: &str) -> &mut Self {
            let seq = self.segments.len() as u32;
            let start = f64::from(seq) * 10.0;
            let mut seg = TranscriptSegment::new(self.meeting_id, seq, start, start + 10.0, text);
            seg.speaker_id = Some(self.speakers[who].id);
            self.segments.push(seg);
            self
        }

        fn recap(&self) -> Minutes {
            build_local_recap(&self.segments, &self.speakers)
        }
    }

    fn texts_of_decisions(m: &Minutes) -> Vec<&str> {
        m.decisions.iter().map(|d| d.text.as_str()).collect()
    }

    fn texts_of_actions(m: &Minutes) -> Vec<&str> {
        m.action_items.iter().map(|a| a.text.as_str()).collect()
    }

    #[test]
    fn extracts_chinese_decisions_and_actions_with_speaker_and_time() {
        let mut f = Fixture::new();
        f.say(0, "大家好，今天讨论上线计划。")
            .say(1, "我看了一下测试结果。最后决定下个版本先灰度百分之十。")
            .say(0, "好的，我来整理发布清单，周三前发给大家。")
            .say(1, "这个方案大家一致通过。");
        let m = f.recap();

        assert_eq!(
            texts_of_decisions(&m),
            vec![
                "S2：最后决定下个版本先灰度百分之十。",
                "S2：这个方案大家一致通过。"
            ]
        );
        assert_eq!(
            m.decisions[0].source,
            Some(SourceRef {
                start: 10.0,
                end: 20.0
            })
        );
        assert_eq!(
            texts_of_actions(&m),
            vec!["李明：好的，我来整理发布清单，周三前发给大家。"]
        );
        assert_eq!(
            m.action_items[0].source,
            Some(SourceRef {
                start: 20.0,
                end: 30.0
            })
        );
        assert!(m.discussion.is_empty(), "no excerpts when cues matched");
    }

    #[test]
    fn extracts_english_decisions_and_actions() {
        let mut f = Fixture::new();
        f.say(
            0,
            "Thanks for joining. We agreed to ship the beta on Friday.",
        )
        .say(
            1,
            "I'll follow up with the vendor about pricing. The total is 3.5 percent higher.",
        )
        .say(
            0,
            "Sam will send the notes tomorrow. Anyone willing to help is welcome.",
        );
        let m = f.recap();

        assert_eq!(
            texts_of_decisions(&m),
            vec!["李明：We agreed to ship the beta on Friday."]
        );
        assert_eq!(
            texts_of_actions(&m),
            vec![
                "S2：I'll follow up with the vendor about pricing.",
                "李明：Sam will send the notes tomorrow.",
            ]
        );
    }

    #[test]
    fn questions_are_never_decisions_or_actions() {
        let mut f = Fixture::new();
        f.say(0, "这个方案大家都同意吗？")
            .say(1, "Have we decided on the launch date?")
            .say(0, "那我们就这么定了吗")
            .say(1, "你明天能把报告发出来？")
            .say(0, "Will you follow up with legal?");
        let m = f.recap();
        assert!(m.decisions.is_empty(), "{:?}", m.decisions);
        assert!(m.action_items.is_empty(), "{:?}", m.action_items);
    }

    #[test]
    fn ambiguous_cue_uses_are_not_decisions() {
        let mut f = Fixture::new();
        f.say(0, "我们通过这个接口拿数据比较稳定。")
            .say(1, "这个数字需要确认一下再说。")
            .say(0, "同意。");
        let m = f.recap();
        assert!(m.decisions.is_empty(), "{:?}", m.decisions);
    }

    #[test]
    fn a_sentence_with_both_cues_is_listed_once_as_a_decision() {
        let mut f = Fixture::new();
        f.say(0, "我们决定下周一起上线。")
            .say(1, "我们决定下周一起上线。");
        let m = f.recap();
        assert_eq!(texts_of_decisions(&m), vec!["李明：我们决定下周一起上线。"]);
        assert!(m.action_items.is_empty());
    }

    #[test]
    fn caps_decisions_and_actions() {
        let mut f = Fixture::new();
        for i in 0..6 {
            f.say(0, &format!("第{i}项我们最终决定采用方案甲。"));
        }
        for i in 0..10 {
            f.say(1, &format!("第{i}项我会在明天之前完成。"));
        }
        let m = f.recap();
        assert_eq!(m.decisions.len(), MAX_DECISIONS);
        assert_eq!(m.action_items.len(), MAX_ACTION_ITEMS);
        assert!(m.decisions[0].text.contains("第0项"), "timeline order");
    }

    #[test]
    fn falls_back_to_opening_and_closing_excerpts() {
        let mut f = Fixture::new();
        for i in 0..30 {
            f.say(
                i % 2,
                &format!("这是第{i}段普通的讨论内容，没有任何结论性的表述"),
            );
        }
        let m = f.recap();
        assert!(m.decisions.is_empty() && m.action_items.is_empty());
        assert_eq!(m.discussion.len(), 2);

        let opening = &m.discussion[0];
        assert!(
            opening.topic.starts_with("开场：李明：这是第0段"),
            "{}",
            opening.topic
        );
        assert!(opening.topic.ends_with('…'));
        assert_eq!(opening.source.unwrap().start, 0.0);

        let closing = &m.discussion[1];
        assert!(closing.topic.starts_with("结尾：…"), "{}", closing.topic);
        assert!(closing
            .topic
            .ends_with("第29段普通的讨论内容，没有任何结论性的表述"));
        assert_eq!(closing.source.unwrap().end, 300.0);

        for point in &m.discussion {
            let body_chars = point.topic.chars().count() - "开场：".chars().count();
            assert!(body_chars <= EXCERPT_CHARS + 1, "{body_chars}");
        }
    }

    #[test]
    fn a_short_meeting_gets_one_excerpt() {
        let mut f = Fixture::new();
        f.say(0, "简单测试一下录音效果").say(1, "听得很清楚");
        let m = f.recap();
        assert_eq!(m.discussion.len(), 1);
        assert_eq!(
            m.discussion[0].topic,
            "开场：李明：简单测试一下录音效果 S2：听得很清楚"
        );
        assert_eq!(
            m.discussion[0].source,
            Some(SourceRef {
                start: 0.0,
                end: 20.0
            })
        );
    }

    #[test]
    fn empty_transcript_has_no_recap_row() {
        let mut f = Fixture::new();
        f.say(0, "   ");
        assert!(local_recap_summary(f.meeting_id, &f.segments, &f.speakers).is_none());
    }

    #[test]
    fn recap_row_is_minutes_json_under_the_local_recap_kind() {
        let mut f = Fixture::new();
        f.say(0, "我们决定采用第二套方案。");
        let row = local_recap_summary(f.meeting_id, &f.segments, &f.speakers).unwrap();
        assert_eq!(row.kind, SummaryKind::LocalRecap);
        assert_eq!(row.model, None);
        let parsed = crate::minutes::parse_minutes(&row.content).unwrap();
        assert_eq!(parsed, f.recap());
    }

    #[test]
    fn unknown_speaker_is_labelled() {
        let f = Fixture::new();
        let seg = TranscriptSegment::new(f.meeting_id, 1, 10.0, 12.0, "那就这么办吧，明天开工。");
        let m = build_local_recap(&[seg], &f.speakers);
        assert_eq!(
            texts_of_decisions(&m),
            vec!["未知说话人：那就这么办吧，明天开工。"]
        );
    }
}
