//! Post-ASR dictionary correction — meeting "hotword" strategy A.
//!
//! sherpa-onnx's offline Paraformer does not expose reliable engine-level
//! contextual biasing ("hotwords") for rare names/jargon, so instead of biasing
//! the decode we repair the transcript *after* it is produced: for each segment
//! we scan the decoded text for near-miss mis-recognitions of the user's
//! personal-dictionary entries and rewrite them to the canonical spelling.
//!
//! Two sources drive the pass (both come from
//! [`lumen_dictionary::split_for_injection`] at the app layer):
//! * **exact replacements** (`from -> to`, e.g. `"脱肯" -> "Token"`) — applied as
//!   deterministic substring substitutions. Highest confidence: the user
//!   authored the pair explicitly, so we trust it verbatim.
//! * **canonical terms** (names/jargon, e.g. `"Kubernetes"`, `"李明"`) — matched
//!   *fuzzily* against windows/tokens of the text with a conservative
//!   edit-distance threshold plus a shared prefix/suffix guard, so only
//!   high-confidence near-misses are rewritten and ordinary words are left
//!   alone.
//!
//! ## Design bias: precision over recall
//! The thresholds below are deliberately tight. A correction pass that corrupts
//! a correctly-transcribed word is far worse than one that misses a mis-hear, so
//! every rule errs toward *not* touching the text unless the candidate is a very
//! close, boundary-anchored match.
//!
//! ## Known limitation (beta)
//! Chinese matching uses **character** edit distance, not phonetics. A homophone
//! mis-hear that shares no character with the canonical term (e.g. `立民` for
//! `李明`) is therefore not caught. Because a bare character-distance rule cannot
//! tell a homophone slip from a genuinely different name, CJK *fuzzy* correction
//! is restricted to terms of **3+ characters** and requires the first **and**
//! last character to match the term (only interior characters may differ) — so a
//! two-character name (`李明` vs `李华`, both share one char) is never fuzzily
//! rewritten; those rely on exact `from -> to` replacements instead. We
//! deliberately do not pull in a pinyin dependency yet (build footprint / cost);
//! adding phonetic keys is the obvious follow-up to raise recall for Chinese
//! names.

use lumen_transcript::Word;

/// The user's personal-dictionary view relevant to correction: canonical terms
/// to fuzzy-match and exact `from -> to` replacements to apply verbatim.
///
/// Built at the app layer from `lumen_dictionary::split_for_injection`; empty by
/// default so a run with no dictionary is a no-op.
#[derive(Debug, Clone, Default)]
pub struct CorrectionDict {
    /// Canonical spellings of names/jargon (the `Term` entries).
    pub terms: Vec<String>,
    /// Exact `(from, to)` substitutions (the `Replacement` entries).
    pub replacements: Vec<(String, String)>,
}

impl CorrectionDict {
    pub fn new(terms: Vec<String>, replacements: Vec<(String, String)>) -> Self {
        Self {
            terms,
            replacements,
        }
    }

    /// True when there is nothing to correct with (so callers can skip the pass).
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.replacements.is_empty()
    }
}

/// Correct one segment's text against the dictionary, returning the repaired
/// string. Empty text or an empty dictionary returns the input unchanged.
///
/// Order: exact replacements first (highest confidence), then fuzzy term
/// correction over the result.
pub fn correct_segment(text: &str, dict: &CorrectionDict) -> String {
    if text.is_empty() || dict.is_empty() {
        return text.to_string();
    }

    // 1. Exact, user-authored replacements (deterministic substring swaps).
    let mut out = text.to_string();
    for (from, to) in &dict.replacements {
        if !from.is_empty() && from != to && out.contains(from.as_str()) {
            out = out.replace(from.as_str(), to);
        }
    }

    // 2. Conservative fuzzy correction of canonical terms.
    for term in &dict.terms {
        let term = term.trim();
        if has_cjk(term) {
            out = correct_cjk_term(&out, term);
        } else {
            out = correct_latin_term(&out, term);
        }
    }

    out
}

/// Best-effort word-level correction that **preserves each token's timing**.
///
/// Only the dictionary's exact `from -> to` replacements are applied here, as an
/// in-place substring swap inside each timed token (keeping its `[start, end]`).
/// Fuzzy term matching is intentionally *not* re-run at the word level: a
/// canonical term frequently spans several timed tokens (Chinese word timings are
/// per-character), and re-segmenting timed tokens is out of scope for beta. As a
/// result a token's text may lag the corrected segment text slightly, but
/// click-to-seek stays correct because no timing is moved. See the module note.
pub fn correct_words(words: &[Word], dict: &CorrectionDict) -> Vec<Word> {
    if words.is_empty() || dict.replacements.is_empty() {
        return words.to_vec();
    }
    words
        .iter()
        .map(|w| {
            let mut text = w.word.clone();
            for (from, to) in &dict.replacements {
                if !from.is_empty() && from != to && text.contains(from.as_str()) {
                    text = text.replace(from.as_str(), to);
                }
            }
            Word::new(text, w.start, w.end)
        })
        .collect()
}

/// Fuzzy-correct a CJK canonical `term` (3+ chars) inside `text`.
///
/// Scans equal-length character windows (homophone/near-miss mis-hears keep the
/// term's length) and replaces a window with `term` only when it is a very close,
/// **both-ends-anchored** match:
/// * the first *and* last character equal the term's (only interior chars differ),
/// * non-zero edit distance within `max_dist` (`len/4`, min 1), and
/// * the window is CJK-dominant.
///
/// Terms shorter than 3 chars are left to exact replacements (see the module
/// note): a two-char window sharing one char is too weak a signal without pinyin.
fn correct_cjk_term(text: &str, term: &str) -> String {
    let term_chars: Vec<char> = term.chars().collect();
    let l = term_chars.len();
    if l < 3 {
        return text.to_string();
    }
    let max_dist = (l / 4).max(1);

    let chars: Vec<char> = text.chars().collect();
    if chars.len() < l {
        return text.to_string();
    }

    let mut result: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i + l <= chars.len() {
        let window = &chars[i..i + l];
        if window == term_chars.as_slice() {
            // Already correct — copy through untouched.
            result.extend_from_slice(window);
            i += l;
            continue;
        }
        let anchored = window[0] == term_chars[0] && window[l - 1] == term_chars[l - 1];
        let cjk_dominant = window.iter().filter(|c| is_cjk(**c)).count() * 2 >= l;
        if anchored && cjk_dominant {
            let dist = char_edit_distance(window, &term_chars);
            if dist >= 1 && dist <= max_dist {
                result.extend_from_slice(&term_chars);
                i += l;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    // Trailing characters that never started a full-length window.
    if i < chars.len() {
        result.extend_from_slice(&chars[i..]);
    }
    result.into_iter().collect()
}

/// Fuzzy-correct a Latin/ASCII canonical `term` inside `text`, token by token.
///
/// A "token" is a maximal run of ASCII alphanumerics; separators are preserved
/// verbatim. A token is rewritten to `term` when, case-insensitively, it is a
/// near-length, boundary-anchored match: length within `max_dist`, sharing a
/// first *or* last character, and within edit distance `max_dist`. `max_dist` is
/// 1 for terms under 12 chars and 2 only for longer ones — so genuine
/// neighbours two edits apart (e.g. "performer" vs "Paraformer") are left alone.
/// Terms shorter than 4 chars are skipped (too collision-prone, e.g. app→API).
fn correct_latin_term(text: &str, term: &str) -> String {
    let term_lower: Vec<char> = term.to_lowercase().chars().collect();
    if term_lower.len() < 4 {
        return text.to_string();
    }
    let max_dist = if term_lower.len() >= 12 { 2 } else { 1 };

    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch);
        } else {
            push_corrected_token(&mut out, &token, term, &term_lower, max_dist);
            token.clear();
            out.push(ch);
        }
    }
    push_corrected_token(&mut out, &token, term, &term_lower, max_dist);
    out
}

/// Append `token` to `out`, rewritten to `term` when it is a conservative
/// near-miss of it (see [`correct_latin_term`]); otherwise appended verbatim.
fn push_corrected_token(
    out: &mut String,
    token: &str,
    term: &str,
    term_lower: &[char],
    max_dist: usize,
) {
    if token.is_empty() {
        return;
    }
    let tok_lower: Vec<char> = token.to_lowercase().chars().collect();
    // Already correct (case-insensitively) — keep the text as transcribed rather
    // than forcing the dictionary's casing on an otherwise-fine word.
    if tok_lower == *term_lower {
        out.push_str(token);
        return;
    }
    let len_diff = tok_lower.len().max(term_lower.len()) - tok_lower.len().min(term_lower.len());
    let shares_anchor =
        tok_lower.first() == term_lower.first() || tok_lower.last() == term_lower.last();
    if len_diff <= max_dist && shares_anchor {
        let dist = char_edit_distance(&tok_lower, term_lower);
        if dist >= 1 && dist <= max_dist {
            out.push_str(term);
            return;
        }
    }
    out.push_str(token);
}

/// Levenshtein edit distance over character slices (two-row DP).
fn char_edit_distance(a: &[char], b: &[char]) -> usize {
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// CJK unified ideographs (main + common extensions/compat), matching the ranges
/// `lumen_dictionary` uses so both layers agree on what "a Chinese character" is.
fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{f900}'..='\u{faff}').contains(&c)
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(terms: &[&str], reps: &[(&str, &str)]) -> CorrectionDict {
        CorrectionDict::new(
            terms.iter().map(|s| s.to_string()).collect(),
            reps.iter()
                .map(|(f, t)| (f.to_string(), t.to_string()))
                .collect(),
        )
    }

    #[test]
    fn empty_dictionary_is_noop() {
        let d = CorrectionDict::default();
        assert!(d.is_empty());
        assert_eq!(correct_segment("你好，世界", &d), "你好，世界");
        assert_eq!(correct_segment("hello world", &d), "hello world");
    }

    #[test]
    fn empty_text_is_noop() {
        let d = dict(&["李明"], &[("脱肯", "Token")]);
        assert_eq!(correct_segment("", &d), "");
    }

    #[test]
    fn exact_replacement_is_applied() {
        let d = dict(&[], &[("脱肯", "Token")]);
        assert_eq!(correct_segment("请用脱肯登录系统", &d), "请用Token登录系统");
    }

    #[test]
    fn exact_replacement_all_occurrences() {
        let d = dict(&[], &[("脱肯", "Token")]);
        assert_eq!(correct_segment("脱肯和脱肯", &d), "Token和Token");
    }

    #[test]
    fn cjk_term_interior_near_miss_is_corrected() {
        // 李小明 -> 李晓明: first 李 and last 明 anchored, one interior char differs.
        let d = dict(&["李晓明"], &[]);
        assert_eq!(correct_segment("会议由李小明主持", &d), "会议由李晓明主持");
    }

    #[test]
    fn cjk_two_char_terms_are_not_fuzzily_corrected() {
        // 李华 shares one char with 李明 but is a different person — must NOT be
        // rewritten. Two-char CJK terms are exact-replacement only (module note).
        let d = dict(&["李明"], &[]);
        assert_eq!(correct_segment("李华说话", &d), "李华说话");
    }

    #[test]
    fn cjk_different_name_sharing_one_anchor_is_not_corrected() {
        // 李大华 shares the prefix 李 with 李晓明 but the suffix differs (华≠明),
        // so it is a different name and is left alone (both anchors required).
        let d = dict(&["李晓明"], &[]);
        assert_eq!(correct_segment("李大华来了", &d), "李大华来了");
    }

    #[test]
    fn cjk_unrelated_words_are_not_over_corrected() {
        let d = dict(&["李晓明"], &[]);
        assert_eq!(correct_segment("今天天气很好", &d), "今天天气很好");
    }

    #[test]
    fn cjk_already_correct_is_untouched() {
        let d = dict(&["李晓明"], &[]);
        assert_eq!(correct_segment("李晓明到场", &d), "李晓明到场");
    }

    #[test]
    fn english_term_near_miss_is_corrected() {
        let d = dict(&["Kubernetes"], &[]);
        assert_eq!(
            correct_segment("we deployed kubernetis today", &d),
            "we deployed Kubernetes today"
        );
    }

    #[test]
    fn english_unrelated_word_is_not_corrected() {
        // "performer" is a real, different word — must not become "Paraformer".
        let d = dict(&["Paraformer"], &[]);
        assert_eq!(
            correct_segment("a great performer sang", &d),
            "a great performer sang"
        );
    }

    #[test]
    fn english_already_correct_keeps_original_casing() {
        // Case-insensitive match already present -> leave the token as-is.
        let d = dict(&["Paraformer"], &[]);
        assert_eq!(
            correct_segment("the paraformer model", &d),
            "the paraformer model"
        );
    }

    #[test]
    fn short_terms_are_skipped() {
        // 2-char CJK term and <4-char Latin term are too collision-prone for
        // fuzzy matching.
        let d = dict(&["明白", "api"], &[]);
        assert_eq!(correct_segment("名白说话", &d), "名白说话");
        assert_eq!(correct_segment("the app runs", &d), "the app runs");
    }

    #[test]
    fn term_longer_than_text_is_noop() {
        let d = dict(&["Kubernetes"], &[]);
        assert_eq!(correct_segment("k8s", &d), "k8s");
        let d = dict(&["李晓明"], &[]);
        assert_eq!(correct_segment("李", &d), "李");
    }

    #[test]
    fn words_get_exact_replacements_with_timing_preserved() {
        let d = dict(&[], &[("脱肯", "Token")]);
        let words = vec![Word::new("脱肯", 1.0, 1.4), Word::new("登录", 1.4, 1.8)];
        let out = correct_words(&words, &d);
        assert_eq!(out[0].word, "Token");
        assert_eq!(out[0].start, 1.0);
        assert_eq!(out[0].end, 1.4);
        assert_eq!(out[1].word, "登录");
    }

    #[test]
    fn words_noop_when_no_replacements() {
        // Only fuzzy terms, no exact replacements -> words pass through unchanged
        // (timing preserved), per the beta word-level policy.
        let d = dict(&["李明"], &[]);
        let words = vec![Word::new("李", 0.0, 0.3), Word::new("鸣", 0.3, 0.6)];
        let out = correct_words(&words, &d);
        assert_eq!(out, words);
    }

    #[test]
    fn replacements_run_before_fuzzy_terms() {
        // Replacement produces "Kubernetis", then the term fixes it to "Kubernetes".
        let d = dict(&["Kubernetes"], &[("k8s", "Kubernetis")]);
        assert_eq!(
            correct_segment("deploy k8s now", &d),
            "deploy Kubernetes now"
        );
    }

    #[test]
    fn edit_distance_basics() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitting".chars().collect();
        assert_eq!(char_edit_distance(&a, &b), 3);
        assert_eq!(char_edit_distance(&[], &b), 7);
        assert_eq!(char_edit_distance(&a, &a), 0);
    }
}
