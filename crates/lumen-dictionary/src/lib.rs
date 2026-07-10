//! Dictionary entries and edit-learning candidates.
//!
//! Product policy:
//! - Always record edit events at the store layer.
//! - Generate *candidates* here; promote only on user confirm (or optional N threshold).

use chrono::{DateTime, Utc};
use lumen_core::{DictEntryKind, DictEntrySource};
use serde::{Deserialize, Serialize};
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
    let (pre_len, suf_len) = common_affix_lens(before, after);
    // Need real shared context — pure whole-string swaps have pre=0,suf=0.
    if pre_len == 0 && suf_len == 0 {
        return None;
    }

    let b_chars: Vec<char> = before.chars().collect();
    let a_chars: Vec<char> = after.chars().collect();
    if pre_len + suf_len >= b_chars.len() || pre_len + suf_len >= a_chars.len() {
        return None;
    }

    let from: String = b_chars[pre_len..b_chars.len() - suf_len].iter().collect();
    let to: String = a_chars[pre_len..a_chars.len() - suf_len].iter().collect();
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    // Only prefer middle span when it is strictly shorter than the full strings
    // (otherwise short_pair on the full text is equivalent / clearer).
    let from_n = from.chars().count();
    let to_n = to.chars().count();
    if from_n >= b_chars.len() && to_n >= a_chars.len() {
        return None;
    }
    if from_n > max_middle || to_n > max_middle {
        return None;
    }
    // Avoid learning single punctuation-only swaps.
    if from.chars().all(|c| c.is_ascii_punctuation() || c.is_whitespace())
        && to.chars().all(|c| c.is_ascii_punctuation() || c.is_whitespace())
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

/// Character counts of shared prefix and suffix (non-overlapping).
fn common_affix_lens(a: &str, b: &str) -> (usize, usize) {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut pre = 0usize;
    while pre < ac.len() && pre < bc.len() && ac[pre] == bc[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < ac.len().saturating_sub(pre)
        && suf < bc.len().saturating_sub(pre)
        && ac[ac.len() - 1 - suf] == bc[bc.len() - 1 - suf]
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
}
