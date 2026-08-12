//! Dictionary entries and edit-learning candidates.
//!
//! Product policy:
//! - Always record edit events at the store layer.
//! - Generate *candidates* here; promote only on user confirm (or optional N threshold).

use chrono::{DateTime, Utc};
use lumen_core::{DictEntryKind, DictEntrySource};
use serde::{Deserialize, Serialize};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryEntry {
    pub id: Uuid,
    pub kind: DictEntryKind,
    pub term: Option<String>,
    pub from_text: Option<String>,
    pub to_text: Option<String>,
    pub source: DictEntrySource,
    pub hit_count: u32,
    pub confirmed: bool,
    pub updated_at: DateTime<Utc>,
}

impl DictionaryEntry {
    pub fn term(term: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: DictEntryKind::Term,
            term: Some(term.into()),
            from_text: None,
            to_text: None,
            source: DictEntrySource::Manual,
            hit_count: 0,
            confirmed: true,
            updated_at: Utc::now(),
        }
    }

    pub fn replacement(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: DictEntryKind::Replacement,
            term: None,
            from_text: Some(from.into()),
            to_text: Some(to.into()),
            source: DictEntrySource::Manual,
            hit_count: 0,
            confirmed: true,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearnCandidate {
    pub kind: DictEntryKind,
    pub term: Option<String>,
    pub from_text: Option<String>,
    pub to_text: Option<String>,
    pub reason: String,
}

/// Extract learnable dictionary candidates from a user edit.
///
/// Strategy:
/// 1. Prefer common-prefix/suffix middle span when it yields a shorter pair (phrase edits)
/// 2. Else short whole-string edits (≤32 chars each) → replacement (+ term if stable)
/// 3. Never propose whole-paragraph rewrites
pub fn candidates_from_edit(before: &str, after: &str) -> Vec<LearnCandidate> {
    let before = before.trim();
    let after = after.trim();
    if before == after || before.is_empty() || after.is_empty() {
        return vec![];
    }

    const MAX_PAIR_CHARS: usize = 32;
    const MAX_MIDDLE_CHARS: usize = 24;

    // Prefer affix middle even when both sides are short (e.g. Chinese phrases ≤32 chars).
    if let Some(out) = middle_span_candidates(before, after, MAX_MIDDLE_CHARS) {
        return out;
    }

    if before.chars().count() <= MAX_PAIR_CHARS && after.chars().count() <= MAX_PAIR_CHARS {
        return short_pair_candidates(before, after);
    }

    vec![]
}

/// Strip shared prefix/suffix; if the remaining middles are short and non-empty, propose them.
/// Returns None when affix strip is unhelpful (no shared context, or middles too long).
fn middle_span_candidates(
    before: &str,
    after: &str,
    max_middle: usize,
) -> Option<Vec<LearnCandidate>> {
    let b_graphemes: Vec<&str> = before.graphemes(true).collect();
    let a_graphemes: Vec<&str> = after.graphemes(true).collect();
    let (mut pre_len, mut suf_len) = common_affix_lens(&b_graphemes, &a_graphemes);
    // Need real shared context — pure whole-string swaps have pre=0,suf=0.
    if pre_len == 0 && suf_len == 0 {
        return None;
    }

    // Shared affixes may stop inside a corrected technical term. For example,
    // `wrong‑term` -> `worktree` shares the leading `w`; treating that `w` as
    // context produces the broken candidate `orktree`. Widen Unicode Latin
    // technical-token boundaries while keeping edits in other scripts focused.
    // Grapheme boundaries keep decomposed accents attached to their base letter.
    while pre_len > 0
        && (splits_technical_token(&b_graphemes, pre_len)
            || splits_technical_token(&a_graphemes, pre_len))
    {
        pre_len -= 1;
    }
    while suf_len > 0
        && (splits_technical_token(&b_graphemes, b_graphemes.len() - suf_len)
            || splits_technical_token(&a_graphemes, a_graphemes.len() - suf_len))
    {
        suf_len -= 1;
    }
    if pre_len + suf_len >= b_graphemes.len() || pre_len + suf_len >= a_graphemes.len() {
        return None;
    }

    let from_span = b_graphemes[pre_len..b_graphemes.len() - suf_len].concat();
    let to_span = a_graphemes[pre_len..a_graphemes.len() - suf_len].concat();
    let from = from_span.trim();
    let to = to_span.trim();
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    // Only prefer middle span when it is strictly shorter than the full strings
    // (otherwise short_pair on the full text is equivalent / clearer).
    let from_n = from.chars().count();
    let to_n = to.chars().count();
    if from.graphemes(true).count() >= b_graphemes.len()
        && to.graphemes(true).count() >= a_graphemes.len()
    {
        return None;
    }
    const MAX_TECHNICAL_MIDDLE_CHARS: usize = 64;
    let within_default_limit = from_n <= max_middle && to_n <= max_middle;
    let within_technical_limit = from_n <= MAX_TECHNICAL_MIDDLE_CHARS
        && to_n <= MAX_TECHNICAL_MIDDLE_CHARS
        && is_complete_technical_token(from)
        && is_complete_technical_token(to);
    if !within_default_limit && !within_technical_limit {
        return None;
    }
    // Avoid learning single punctuation-only swaps.
    if from
        .chars()
        .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
        && to
            .chars()
            .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
    {
        return None;
    }

    let mut out = vec![LearnCandidate {
        kind: DictEntryKind::Replacement,
        term: None,
        from_text: Some(from.to_string()),
        to_text: Some(to.to_string()),
        reason: "changed span inside longer text".into(),
    }];
    if !to.contains(char::is_whitespace)
        && to_n <= 24
        && to.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
    {
        out.push(LearnCandidate {
            kind: DictEntryKind::Term,
            term: Some(to.to_string()),
            from_text: None,
            to_text: None,
            reason: "edited span looks like a stable term".into(),
        });
    }
    Some(out)
}

fn short_pair_candidates(before: &str, after: &str) -> Vec<LearnCandidate> {
    let mut out = vec![LearnCandidate {
        kind: DictEntryKind::Replacement,
        term: None,
        from_text: Some(before.to_string()),
        to_text: Some(after.to_string()),
        reason: "user edited short phrase".into(),
    }];
    if !after.contains(char::is_whitespace)
        && after.chars().count() <= 24
        && after.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
    {
        out.push(LearnCandidate {
            kind: DictEntryKind::Term,
            term: Some(after.to_string()),
            from_text: None,
            to_text: None,
            reason: "edited result looks like a stable term".into(),
        });
    }
    out
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{f900}'..='\u{faff}').contains(&c)
}

fn splits_technical_token(graphemes: &[&str], boundary: usize) -> bool {
    boundary > 0
        && boundary < graphemes.len()
        && is_technical_token_grapheme(graphemes, boundary - 1)
        && is_technical_token_grapheme(graphemes, boundary)
}

fn is_technical_token_grapheme(graphemes: &[&str], index: usize) -> bool {
    let grapheme = graphemes[index];
    if grapheme == "_" || is_latin_word_component(grapheme) {
        return true;
    }

    matches!(
        grapheme,
        "-" | "." | "\u{2010}" | "\u{2011}" | "'" | "\u{2019}"
    ) && index > 0
        && index + 1 < graphemes.len()
        && is_latin_word_component(graphemes[index - 1])
        && is_latin_word_component(graphemes[index + 1])
}

fn is_latin_word_component(grapheme: &str) -> bool {
    let mut characters = grapheme.chars();
    let Some(base) = characters.next() else {
        return false;
    };
    (base.is_ascii_digit() || (base.is_alphabetic() && base.script() == Script::Latin))
        && characters.all(|character| character.script() == Script::Inherited)
}

fn is_complete_technical_token(value: &str) -> bool {
    let graphemes: Vec<&str> = value.graphemes(true).collect();
    !graphemes.is_empty()
        && graphemes
            .iter()
            .any(|grapheme| is_latin_word_component(grapheme))
        && (0..graphemes.len()).all(|index| is_technical_token_grapheme(&graphemes, index))
}

/// Item counts of shared prefix and suffix (non-overlapping).
fn common_affix_lens<T: PartialEq>(a: &[T], b: &[T]) -> (usize, usize) {
    let mut pre = 0usize;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < a.len().saturating_sub(pre)
        && suf < b.len().saturating_sub(pre)
        && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    (pre, suf)
}

/// Build prompt/hotword views from confirmed entries.
pub fn split_for_injection(entries: &[DictionaryEntry]) -> (Vec<String>, Vec<(String, String)>) {
    let mut terms = Vec::new();
    let mut replacements = Vec::new();
    for e in entries {
        if !e.confirmed {
            continue;
        }
        match e.kind {
            DictEntryKind::Term => {
                if let Some(t) = &e.term {
                    if !t.is_empty() {
                        terms.push(t.clone());
                    }
                }
            }
            DictEntryKind::Replacement => {
                if let (Some(f), Some(t)) = (&e.from_text, &e.to_text) {
                    if !f.is_empty() && !t.is_empty() {
                        replacements.push((f.clone(), t.clone()));
                    }
                }
            }
        }
    }
    (terms, replacements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_edit_yields_replacement_and_term() {
        let c = candidates_from_edit("脱肯", "Token");
        assert!(c.iter().any(|x| x.kind == DictEntryKind::Replacement));
        assert!(c.iter().any(|x| x.kind == DictEntryKind::Term));
    }

    #[test]
    fn identical_yields_nothing() {
        assert!(candidates_from_edit("abc", "abc").is_empty());
    }

    #[test]
    fn long_rewrite_skipped() {
        let before = "a".repeat(80);
        let after = "b".repeat(80);
        assert!(candidates_from_edit(&before, &after).is_empty());
    }

    #[test]
    fn middle_span_extracted() {
        let c = candidates_from_edit("请用脱肯鉴权登录系统", "请用Token鉴权登录系统");
        let rep = c
            .iter()
            .find(|x| x.kind == DictEntryKind::Replacement)
            .expect("replacement");
        assert_eq!(rep.from_text.as_deref(), Some("脱肯"));
        assert_eq!(rep.to_text.as_deref(), Some("Token"));
    }

    #[test]
    fn middle_span_does_not_drop_a_shared_technical_token_prefix() {
        let before = "Use wrong‑term here and keep wrong‑term later.";
        let after = "Use worktree here and keep wrong‑term later.";

        let candidates = candidates_from_edit(before, after);
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("wrong‑term"));
        assert_eq!(replacement.to_text.as_deref(), Some("worktree"));
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == DictEntryKind::Term && candidate.term.as_deref() == Some("worktree")
        }));
    }

    #[test]
    fn middle_span_does_not_drop_a_shared_technical_token_suffix() {
        let candidates = candidates_from_edit("Use serber here", "Use server here");
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("serber"));
        assert_eq!(replacement.to_text.as_deref(), Some("server"));
    }

    #[test]
    fn middle_span_keeps_non_ascii_character_edits_focused() {
        let candidates = candidates_from_edit("ひらがな", "ひらげな");
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("が"));
        assert_eq!(replacement.to_text.as_deref(), Some("げ"));
    }

    #[test]
    fn middle_span_keeps_non_latin_script_edits_focused() {
        for (before, after, expected_from, expected_to) in [
            ("Use かなかな here", "Use かにかな here", "な", "に"),
            ("Use 가나가나 here", "Use 가다가나 here", "나", "다"),
            ("Use 𠀀𠀁𠀀 here", "Use 𠀀𠀂𠀀 here", "𠀁", "𠀂"),
        ] {
            let candidates = candidates_from_edit(before, after);
            let replacement = candidates
                .iter()
                .find(|candidate| candidate.kind == DictEntryKind::Replacement)
                .expect("replacement");

            assert_eq!(replacement.from_text.as_deref(), Some(expected_from));
            assert_eq!(replacement.to_text.as_deref(), Some(expected_to));
        }
    }

    #[test]
    fn technical_connectors_only_join_latin_token_interiors() {
        for connector in ["-", ".", "\u{2010}", "\u{2011}", "'", "\u{2019}"] {
            let interior_text = format!("a{connector}b");
            let interior: Vec<&str> = interior_text.graphemes(true).collect();
            assert!(is_technical_token_grapheme(&interior, 1));

            let leading_text = format!("{connector}a");
            let leading: Vec<&str> = leading_text.graphemes(true).collect();
            assert!(!is_technical_token_grapheme(&leading, 0));

            let trailing_text = format!("a{connector}");
            let trailing: Vec<&str> = trailing_text.graphemes(true).collect();
            assert!(!is_technical_token_grapheme(&trailing, 1));
        }

        assert!(is_technical_token_grapheme(&["_", "a"], 0));
        assert!(is_technical_token_grapheme(&["a", "_"], 1));
    }

    #[test]
    fn middle_span_preserves_apostrophes_inside_latin_words() {
        let candidates = candidates_from_edit("Use don't here", "Use doesn't here");
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("don't"));
        assert_eq!(replacement.to_text.as_deref(), Some("doesn't"));
    }

    #[test]
    fn middle_span_preserves_accented_latin_words() {
        let candidates = candidates_from_edit("Use résume here", "Use résumé here");
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("résume"));
        assert_eq!(replacement.to_text.as_deref(), Some("résumé"));
    }

    #[test]
    fn middle_span_does_not_split_combining_character_graphemes() {
        let candidates = candidates_from_edit("Use cafe\u{301} here", "Use caff\u{301} here");
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(replacement.from_text.as_deref(), Some("cafe\u{301}"));
        assert_eq!(replacement.to_text.as_deref(), Some("caff\u{301}"));
    }

    #[test]
    fn middle_span_allows_bounded_long_technical_replacements() {
        let before = "Use very_long_technical_identifier_wrong now";
        let after = "Use very_long_technical_identifier_right now";
        let candidates = candidates_from_edit(before, after);
        let replacement = candidates
            .iter()
            .find(|candidate| candidate.kind == DictEntryKind::Replacement)
            .expect("replacement");

        assert_eq!(
            replacement.from_text.as_deref(),
            Some("very_long_technical_identifier_wrong")
        );
        assert_eq!(
            replacement.to_text.as_deref(),
            Some("very_long_technical_identifier_right")
        );
    }
}
