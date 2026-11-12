//! One policy for every dictionary injection site (INV-1803).
//!
//! Two decisions every injection site used to make on its own:
//!
//! - **ASR-side bias lists** (streaming-cloud hot words, meeting hot
//!   words): [`select_hotword_terms`] caps the list and orders it
//!   most-recently-modified first — a streaming recognizer gets slower
//!   and less accurate the longer the injected list, so a capped list
//!   should carry what the user just curated, not the oldest entries.
//! - **Corrector-side "did the user mention this term"**:
//!   [`MentionMatcher`] answers with folded (ASCII case + fullwidth
//!   width) containment plus a bounded Latin edit distance, so "Lumen"
//!   still matches "lumen" / "Ｌｕｍｅｎ" and a one-character ASR slip
//!   ("Lumen" heard as "Lurnen") still counts as a mention.
//!
//! The corrector's character-budget ranking (apps/desktop
//! `dictionary_budget`) layers on top of the matcher for its relevance
//! tiers; the hotword profiles replace the ad-hoc sort-and-truncate that
//! each site did before.

use crate::DictionaryEntry;

/// Cap + ordering for one ASR-side injection site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectionProfile {
    pub max_terms: usize,
}

/// Streaming dictation hot words. 30 by most-recently-modified: longer
/// lists measurably slow a streaming recognizer down and lower its
/// accuracy, and the personal dictionary's newest entries are the ones
/// the user is actively trying to make stick.
pub const REALTIME_HOTWORDS: InjectionProfile = InjectionProfile { max_terms: 30 };

/// Meeting transcription hot words: keeps today's cap, gains the same
/// deterministic recency ordering.
pub const MEETING_HOTWORDS: InjectionProfile = InjectionProfile { max_terms: 128 };

/// Terms to bias one ASR request with: trimmed non-empty terms, folded
/// deduplication keeping the most recently updated duplicate, ordered by
/// (updated_at descending, term ascending), truncated to the profile cap.
pub fn select_hotword_terms(entries: &[DictionaryEntry], profile: InjectionProfile) -> Vec<String> {
    let mut picked: Vec<(&str, chrono::DateTime<chrono::Utc>)> = entries
        .iter()
        .filter_map(|entry| {
            let term = entry.term.as_deref()?.trim();
            if term.is_empty() {
                None
            } else {
                Some((term, entry.updated_at))
            }
        })
        .collect();
    // Folded dedup: the newest copy of a visually duplicate term wins.
    picked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(profile.max_terms);
    for (term, _) in picked {
        if seen.insert(fold(term)) {
            out.push(term.to_string());
            if out.len() == profile.max_terms {
                break;
            }
        }
    }
    out
}

/// "Did the user mention this term?" — the corrector-side filter that
/// decides which dictionary entries ride with this request.
#[derive(Debug, Clone, Copy)]
pub struct MentionMatcher {
    /// 0 disables fuzzy matching (folded containment only).
    pub max_fuzzy_distance: usize,
}

impl Default for MentionMatcher {
    fn default() -> Self {
        Self {
            max_fuzzy_distance: 2,
        }
    }
}

impl MentionMatcher {
    pub fn mentions(&self, text: &str, term: &str) -> bool {
        let term = term.trim();
        if term.is_empty() {
            return false;
        }
        let folded_text = fold(text);
        let folded_term = fold(term);
        if folded_text.contains(&folded_term) {
            return true;
        }
        if self.max_fuzzy_distance == 0 || !is_latin_word(&folded_term) {
            // CJK fuzzy matching is not attempted: near-homophone CJK
            // pairs are exactly what a dictionary term is there to
            // disambiguate, and a loose char-overlap match would fire on
            // ordinary sentences.
            return false;
        }
        // Latin terms (single word or phrase): compare against the text's
        // word runs, allowing a small edit distance scaled by length.
        let allowed = allowed_distance(&folded_term).min(self.max_fuzzy_distance);
        if allowed == 0 {
            return false;
        }
        // Hyphens stay in tokens during fuzzy matching: "sherpa-onnx" is
        // one term, not ["sherpa", "onnx"].
        for token in folded_text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-') {
            if token.is_empty() {
                continue;
            }
            if edit_distance_at_most(token, &folded_term, allowed) {
                return true;
            }
        }
        false
    }
}

/// Containment distance budget: exact short words stay tight, long words
/// tolerate the configured maximum.
fn allowed_distance(term: &str) -> usize {
    let len = term.chars().count();
    if len <= 4 {
        1
    } else {
        term.len().clamp(1, 2)
    }
}

fn is_latin_word(term: &str) -> bool {
    term.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
        && term.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Fullwidth → ASCII, then lowercase.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| {
            let cp = c as u32;
            if (0xFF01..=0xFF5E).contains(&cp) {
                char::from_u32(cp - 0xFEE0).unwrap_or(c)
            } else {
                c
            }
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Levenshtein with an early-out once the bound is exceeded.
fn edit_distance_at_most(a: &str, b: &str, at_most: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > at_most {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > at_most {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()] <= at_most
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DictEntryKind;

    fn entry(term: &str, updated: &str) -> DictionaryEntry {
        DictionaryEntry {
            id: uuid::Uuid::new_v4(),
            kind: DictEntryKind::Term,
            term: Some(term.to_string()),
            from_text: None,
            to_text: None,
            source: crate::DictEntrySource::Manual,
            hit_count: 0,
            confirmed: false,
            updated_at: updated.parse::<chrono::DateTime<chrono::Utc>>().unwrap(),
        }
    }

    #[test]
    fn hotword_selection_orders_by_recency_and_caps() {
        let entries = vec![
            entry("oldest", "2026-01-01T00:00:00Z"),
            entry("newest", "2026-11-01T00:00:00Z"),
            entry("middle", "2026-06-01T00:00:00Z"),
            // Visual duplicate of `newest`; the newer updated_at wins.
            entry("ＮＥＷＥＳＴ", "2026-10-01T00:00:00Z"),
        ];
        let terms = select_hotword_terms(&entries, InjectionProfile { max_terms: 2 });
        assert_eq!(terms, vec!["newest".to_string(), "middle".to_string()]);
    }

    #[test]
    fn hotword_selection_keeps_terms_only() {
        let mut replacement = entry("ignored", "2026-11-01T00:00:00Z");
        replacement.term = None;
        replacement.from_text = Some("x".into());
        let terms = select_hotword_terms(&[replacement], REALTIME_HOTWORDS);
        assert!(terms.is_empty());
    }

    #[test]
    fn realtime_profile_is_thirty_meeting_is_one_twenty_eight() {
        assert_eq!(REALTIME_HOTWORDS.max_terms, 30);
        assert_eq!(MEETING_HOTWORDS.max_terms, 128);
    }

    #[test]
    fn mention_matches_case_width_and_exact() {
        let m = MentionMatcher::default();
        assert!(m.mentions("我用 Ｌｕｍｅｎ 记录", "Lumen"));
        assert!(m.mentions("deploy sherpa-onnx first", "sherpa-onnx"));
        assert!(m.mentions("问了欧阳明远", "欧阳明远"));
    }

    #[test]
    fn mention_tolerates_small_latin_slips() {
        let m = MentionMatcher::default();
        assert!(
            m.mentions("把 Lurnen 打开", "Lumen"),
            "one-char slip matches"
        );
        assert!(
            m.mentions("kubernets 占用太高", "kubernetes"),
            "missing char matches"
        );
        assert!(!m.mentions("completely different words", "kubernetes"));
    }

    #[test]
    fn mention_never_fuzzy_matches_cjk() {
        let m = MentionMatcher::default();
        // 卢门 is a near-homophone of Lumen-shaped CJK but as a CJK term it
        // must not fuzzy-match unrelated CJK text.
        assert!(!m.mentions("今天天气很好", "卢门"));
        assert!(m.mentions("他叫卢门", "卢门"));
    }

    #[test]
    fn mention_fuzzy_can_be_disabled() {
        let m = MentionMatcher {
            max_fuzzy_distance: 0,
        };
        assert!(m.mentions("lumen", "Lumen"));
        assert!(!m.mentions("Lurnen", "Lumen"));
    }

    #[test]
    fn empty_terms_never_match() {
        let m = MentionMatcher::default();
        assert!(!m.mentions("anything", ""));
        assert!(!m.mentions("anything", "  "));
    }
}
