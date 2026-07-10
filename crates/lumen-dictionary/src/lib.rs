//! Dictionary entries and edit-learning candidates.
//!
//! Product policy (MVP):
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

/// Very small phrase-level heuristic: if texts differ and both are short, propose replacement.
/// Longer diffs only propose if a single contiguous change region is found.
pub fn candidates_from_edit(before: &str, after: &str) -> Vec<LearnCandidate> {
    let before = before.trim();
    let after = after.trim();
    if before == after || before.is_empty() || after.is_empty() {
        return vec![];
    }

    // Identical except whole string swap under length cap → one replacement.
    const MAX_PAIR_CHARS: usize = 32;
    if before.chars().count() <= MAX_PAIR_CHARS && after.chars().count() <= MAX_PAIR_CHARS {
        // If `after` looks like a proper term (no spaces, short), also offer term.
        let mut out = vec![LearnCandidate {
            kind: DictEntryKind::Replacement,
            term: None,
            from_text: Some(before.to_string()),
            to_text: Some(after.to_string()),
            reason: "user edited short phrase".into(),
        }];
        if !after.contains(char::is_whitespace) && after.chars().count() <= 24 {
            out.push(LearnCandidate {
                kind: DictEntryKind::Term,
                term: Some(after.to_string()),
                from_text: None,
                to_text: None,
                reason: "edited result looks like a stable term".into(),
            });
        }
        return out;
    }

    // Fallback: do not auto-propose huge rewrites (align with 闪电说 policy).
    vec![]
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
}
