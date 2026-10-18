//! Bound the personal dictionary injected into the dictation corrector prompt.
//!
//! A small dictionary is injected verbatim, so the prompt is byte-for-byte what
//! it was before this bound existed. Once the rendered block would exceed
//! [`DICTIONARY_PROMPT_BUDGET_CHARS`], entries are ranked by relevance to the
//! ASR text and the most relevant ones are kept within the budget:
//!
//! 1. replacement rules that fire on this text (always kept, even past the
//!    budget, because local preprocessing applies them before the model runs and
//!    dropping one would change the text the user gets);
//! 2. entries whose text occurs verbatim (ASCII case-insensitive) in the text;
//! 3. entries sharing the most CJK characters / ASCII word tokens with the text;
//! 4. then higher `hit_count`, then more recently updated, then store order.
//!
//! Kept entries are emitted in their original store order so replacement
//! chains apply in the same sequence as the unbounded list.
//!
//! The meeting post-ASR correction path does not use this module; it keeps its
//! own term cap (`MAX_MEETING_HOTWORDS`).

use chrono::{DateTime, Utc};
use lumen_core::DictEntryKind;
use lumen_corrector::{preprocess, DictionaryContext};
use lumen_dictionary::{split_for_injection, DictionaryEntry};
use lumen_prompts::{
    DICTIONARY_REPLACEMENTS_LABEL, DICTIONARY_REPLACEMENT_ARROW, DICTIONARY_REPLACEMENT_SEPARATOR,
    DICTIONARY_TERMS_LABEL, DICTIONARY_TERM_SEPARATOR,
};
use std::cmp::Reverse;
use std::collections::HashSet;

/// Maximum characters (Unicode scalar values) of the rendered dictionary block
/// before relevance selection kicks in. Roughly 100-200 typical entries.
pub const DICTIONARY_PROMPT_BUDGET_CHARS: usize = 2_000;

/// The dictionary actually injected for one correction, plus how many
/// confirmed entries were available before the budget was applied.
#[derive(Debug, Clone, Default)]
pub struct DictionarySelection {
    pub context: DictionaryContext,
    pub total_terms: usize,
    pub total_replacements: usize,
}

/// Select the dictionary context for `asr_text` within `budget_chars`.
pub fn select_for_prompt(
    entries: &[DictionaryEntry],
    asr_text: &str,
    budget_chars: usize,
) -> DictionarySelection {
    let (terms, replacements) = split_for_injection(entries);
    let total_terms = terms.len();
    let total_replacements = replacements.len();
    let full = DictionaryContext {
        terms,
        replacements,
    };
    if rendered_chars(&full) <= budget_chars {
        return DictionarySelection {
            context: full,
            total_terms,
            total_replacements,
        };
    }

    let candidates = candidates(entries);
    let text = TextProfile::new(asr_text);
    let forced = firing_replacements(&candidates, asr_text);

    let mut ranked: Vec<(usize, Relevance)> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            (
                index,
                Relevance::of(candidate, &text, forced.contains(&index)),
            )
        })
        .collect();
    ranked.sort_by_key(|(index, relevance)| {
        (
            Reverse(relevance.forced),
            Reverse(relevance.exact),
            Reverse(relevance.overlap_permille),
            Reverse(candidates[*index].hit_count),
            Reverse(candidates[*index].updated_at),
            *index,
        )
    });

    let mut size = BlockSize::default();
    let mut kept = Vec::new();
    for (index, relevance) in ranked {
        let next = size.with(&candidates[index].item);
        if relevance.forced || next.chars() <= budget_chars {
            size = next;
            kept.push(index);
        }
    }
    kept.sort_unstable();

    let mut context = DictionaryContext::default();
    for index in kept {
        match candidates[index].item {
            Item::Term(term) => context.terms.push(term.to_owned()),
            Item::Replacement(from, to) => {
                context.replacements.push((from.to_owned(), to.to_owned()))
            }
        }
    }
    DictionarySelection {
        context,
        total_terms,
        total_replacements,
    }
}

/// Characters of `lumen_prompts::format_dictionary_block` for `context`.
pub fn rendered_chars(context: &DictionaryContext) -> usize {
    let mut size = BlockSize::default();
    for term in &context.terms {
        size = size.with(&Item::Term(term));
    }
    for (from, to) in &context.replacements {
        size = size.with(&Item::Replacement(from, to));
    }
    size.chars()
}

#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    Term(&'a str),
    Replacement(&'a str, &'a str),
}

struct Candidate<'a> {
    item: Item<'a>,
    hit_count: u32,
    updated_at: DateTime<Utc>,
}

/// Same filter and order as `split_for_injection`, keeping ranking metadata.
fn candidates(entries: &[DictionaryEntry]) -> Vec<Candidate<'_>> {
    entries
        .iter()
        .filter(|entry| entry.confirmed)
        .filter_map(|entry| {
            let item = match entry.kind {
                DictEntryKind::Term => {
                    let term = entry.term.as_deref().filter(|t| !t.is_empty())?;
                    Item::Term(term)
                }
                DictEntryKind::Replacement => {
                    let from = entry.from_text.as_deref().filter(|f| !f.is_empty())?;
                    let to = entry.to_text.as_deref().filter(|t| !t.is_empty())?;
                    Item::Replacement(from, to)
                }
            };
            Some(Candidate {
                item,
                hit_count: entry.hit_count,
                updated_at: entry.updated_at,
            })
        })
        .collect()
}

/// Indexes of replacement candidates that must be kept: those whose `from`
/// occurs in the raw text, plus every rule that fires when the full list is
/// applied in order to the preprocessed text (this covers chained rules). Rules
/// outside this set are no-ops on this text, so dropping them leaves local
/// preprocessing output unchanged.
fn firing_replacements(candidates: &[Candidate<'_>], asr_text: &str) -> HashSet<usize> {
    let mut forced = HashSet::new();
    let mut current = preprocess(asr_text);
    for (index, candidate) in candidates.iter().enumerate() {
        let Item::Replacement(from, to) = candidate.item else {
            continue;
        };
        if asr_text.contains(from) {
            forced.insert(index);
        }
        if current.contains(from) {
            forced.insert(index);
            current = current.replace(from, to);
        }
    }
    forced
}

struct TextProfile {
    lowered: String,
    cjk: HashSet<char>,
    tokens: HashSet<String>,
}

impl TextProfile {
    fn new(text: &str) -> Self {
        Self {
            lowered: text.to_lowercase(),
            cjk: text.chars().filter(|c| is_cjk(*c)).collect(),
            tokens: ascii_tokens(text).into_iter().collect(),
        }
    }

    /// Fraction (permille) of the key's CJK characters and ASCII tokens that
    /// also appear in the text.
    fn overlap_permille(&self, key: &str) -> u16 {
        let key_cjk: HashSet<char> = key.chars().filter(|c| is_cjk(*c)).collect();
        let key_tokens: HashSet<String> = ascii_tokens(key).into_iter().collect();
        let total = key_cjk.len() + key_tokens.len();
        if total == 0 {
            return 0;
        }
        let shared = key_cjk.iter().filter(|c| self.cjk.contains(c)).count()
            + key_tokens
                .iter()
                .filter(|token| self.tokens.contains(*token))
                .count();
        ((shared * 1000) / total) as u16
    }

    fn contains(&self, key: &str) -> bool {
        self.lowered.contains(&key.to_lowercase())
    }
}

#[derive(Debug, Clone, Copy)]
struct Relevance {
    forced: bool,
    exact: bool,
    overlap_permille: u16,
}

impl Relevance {
    fn of(candidate: &Candidate<'_>, text: &TextProfile, forced: bool) -> Self {
        let keys = match candidate.item {
            Item::Term(term) => [term, term],
            Item::Replacement(from, to) => [from, to],
        };
        Self {
            forced,
            exact: keys.iter().any(|key| text.contains(key)),
            overlap_permille: keys
                .iter()
                .map(|key| text.overlap_permille(key))
                .max()
                .unwrap_or(0),
        }
    }
}

/// Lowercased ASCII alphanumeric runs (e.g. "Codex CLI" -> ["codex", "cli"]).
fn ascii_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF // Hiragana, Katakana
            | 0x3400..=0x4DBF // CJK Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xAC00..=0xD7AF // Hangul syllables
            | 0xF900..=0xFAFF // CJK Compatibility Ideographs
            | 0x20000..=0x2FA1F // CJK Extensions B-F, compatibility supplement
    )
}

/// Incremental character count of the rendered dictionary block, mirroring
/// `lumen_prompts::format_dictionary_block`.
#[derive(Debug, Clone, Copy, Default)]
struct BlockSize {
    term_count: usize,
    term_chars: usize,
    replacement_count: usize,
    replacement_chars: usize,
}

impl BlockSize {
    fn with(self, item: &Item<'_>) -> Self {
        let mut next = self;
        match item {
            Item::Term(term) => {
                next.term_count += 1;
                next.term_chars += term.chars().count();
            }
            Item::Replacement(from, to) => {
                next.replacement_count += 1;
                next.replacement_chars += from.chars().count()
                    + DICTIONARY_REPLACEMENT_ARROW.chars().count()
                    + to.chars().count();
            }
        }
        next
    }

    fn chars(&self) -> usize {
        let terms = line_chars(
            self.term_count,
            self.term_chars,
            DICTIONARY_TERMS_LABEL,
            DICTIONARY_TERM_SEPARATOR,
        );
        let replacements = line_chars(
            self.replacement_count,
            self.replacement_chars,
            DICTIONARY_REPLACEMENTS_LABEL,
            DICTIONARY_REPLACEMENT_SEPARATOR,
        );
        let newline = usize::from(terms > 0 && replacements > 0);
        terms + replacements + newline
    }
}

fn line_chars(count: usize, item_chars: usize, label: &str, separator: &str) -> usize {
    if count == 0 {
        return 0;
    }
    label.chars().count() + item_chars + (count - 1) * separator.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use lumen_prompts::format_dictionary_block;

    fn term(value: &str) -> DictionaryEntry {
        DictionaryEntry::term(value)
    }

    fn filler_terms(count: usize) -> Vec<DictionaryEntry> {
        (0..count)
            .map(|index| term(&format!("填充术语{index:04}")))
            .collect()
    }

    fn rendered(context: &DictionaryContext) -> String {
        format_dictionary_block(&context.terms, &context.replacements)
    }

    #[test]
    fn rendered_chars_matches_the_prompt_renderer() {
        let cases = [
            DictionaryContext::default(),
            DictionaryContext {
                terms: vec!["Codex".into()],
                replacements: vec![],
            },
            DictionaryContext {
                terms: vec![],
                replacements: vec![("cotex".into(), "Codex".into())],
            },
            DictionaryContext {
                terms: vec!["语音识别".into(), "Tauri".into(), "零".into()],
                replacements: vec![
                    ("配森".into(), "Python".into()),
                    ("rust".into(), "Rust".into()),
                ],
            },
        ];
        for context in cases {
            assert_eq!(
                rendered_chars(&context),
                rendered(&context).chars().count(),
                "{context:?}"
            );
        }
    }

    #[test]
    fn small_dictionary_is_injected_unchanged() {
        let mut entries = vec![
            term("Codex"),
            DictionaryEntry::replacement("cotex", "Codex"),
            term("语音识别"),
        ];
        let mut unconfirmed = term("未确认");
        unconfirmed.confirmed = false;
        entries.push(unconfirmed);

        let selection = select_for_prompt(&entries, "无关文本", DICTIONARY_PROMPT_BUDGET_CHARS);
        let (terms, replacements) = split_for_injection(&entries);

        assert_eq!(selection.context.terms, terms);
        assert_eq!(selection.context.replacements, replacements);
        assert_eq!(selection.total_terms, 2);
        assert_eq!(selection.total_replacements, 1);
    }

    #[test]
    fn candidates_mirror_split_for_injection() {
        let mut entries = filler_terms(3);
        entries.push(DictionaryEntry::replacement("a", "b"));
        entries.push(DictionaryEntry::replacement("", "b"));
        let mut empty_term = term("");
        empty_term.term = Some(String::new());
        entries.push(empty_term);
        let mut unconfirmed = DictionaryEntry::replacement("x", "y");
        unconfirmed.confirmed = false;
        entries.push(unconfirmed);

        let (terms, replacements) = split_for_injection(&entries);
        let mut from_candidates = DictionaryContext::default();
        for candidate in candidates(&entries) {
            match candidate.item {
                Item::Term(t) => from_candidates.terms.push(t.into()),
                Item::Replacement(f, t) => from_candidates.replacements.push((f.into(), t.into())),
            }
        }
        assert_eq!(from_candidates.terms, terms);
        assert_eq!(from_candidates.replacements, replacements);
    }

    #[test]
    fn large_dictionary_stays_within_budget_and_keeps_relevant_entries() {
        let mut entries = filler_terms(400);
        entries.push(term("Kubernetes"));
        entries.push(term("语音识别"));
        entries.push(term("识别率"));
        let text = "今天测试一下 kubernetes 集群上的语音识别服务";

        let selection = select_for_prompt(&entries, text, DICTIONARY_PROMPT_BUDGET_CHARS);
        let block = rendered(&selection.context);

        assert!(block.chars().count() <= DICTIONARY_PROMPT_BUDGET_CHARS);
        assert!(selection.context.terms.len() < 403);
        assert_eq!(selection.total_terms, 403);
        // Case-insensitive exact match and CJK exact match.
        assert!(selection.context.terms.contains(&"Kubernetes".to_owned()));
        assert!(selection.context.terms.contains(&"语音识别".to_owned()));
        // Partial CJK overlap (2 of 3 characters) ranks above the filler,
        // which shares only "语" (1 of 5 units) with the text.
        assert!(selection.context.terms.contains(&"识别率".to_owned()));
    }

    #[test]
    fn firing_replacements_are_always_kept_in_original_order() {
        let mut entries = filler_terms(400);
        entries.insert(0, DictionaryEntry::replacement("配森", "派森"));
        entries.push(DictionaryEntry::replacement("派森", "Python"));
        entries.push(DictionaryEntry::replacement("不相关", "无关"));
        let text = "我在学配森";

        // Even a zero budget keeps the rules local preprocessing will apply,
        // including the chained second rule.
        let selection = select_for_prompt(&entries, text, 0);

        assert!(selection.context.terms.is_empty());
        assert_eq!(
            selection.context.replacements,
            vec![
                ("配森".to_owned(), "派森".to_owned()),
                ("派森".to_owned(), "Python".to_owned()),
            ]
        );
        assert_eq!(selection.total_replacements, 3);
    }

    #[test]
    fn bounded_replacements_do_not_change_local_preprocessing() {
        let mut entries = filler_terms(400);
        entries.push(DictionaryEntry::replacement("cotex", "Codex"));
        entries.push(DictionaryEntry::replacement("Codex", "Codex CLI"));
        entries.push(DictionaryEntry::replacement("never", "seen"));
        let text = "open cotex now";

        let (_, all) = split_for_injection(&entries);
        let selection = select_for_prompt(&entries, text, 100);
        let full = lumen_corrector::preprocess_only(
            text,
            &DictionaryContext {
                terms: vec![],
                replacements: all,
            },
        );
        let bounded = lumen_corrector::preprocess_only(text, &selection.context);

        assert_eq!(bounded.text, full.text);
        assert_eq!(bounded.text, "open Codex CLI now");
        for rule in [("cotex", "Codex"), ("Codex", "Codex CLI")] {
            assert!(selection
                .context
                .replacements
                .contains(&(rule.0.to_owned(), rule.1.to_owned())));
        }
    }

    #[test]
    fn ties_break_by_hit_count_then_recency() {
        let mut entries = filler_terms(400);
        let now = Utc::now();
        for entry in entries.iter_mut() {
            entry.updated_at = now - Duration::days(30);
        }
        let mut frequent = term("高频词条甲乙丙丁");
        frequent.hit_count = 50;
        frequent.updated_at = now - Duration::days(30);
        let mut recent = term("最近词条戊己庚辛");
        recent.updated_at = now;
        entries.push(frequent);
        entries.push(recent);

        // Every term is 8 characters; the budget fits exactly two of them.
        let budget = DICTIONARY_TERMS_LABEL.chars().count() + 8 + 1 + 8;
        let selection = select_for_prompt(&entries, "完全无关的内容", budget);

        assert_eq!(
            selection.context.terms,
            vec!["高频词条甲乙丙丁".to_owned(), "最近词条戊己庚辛".to_owned()]
        );
    }

    #[test]
    fn selection_is_deterministic() {
        let mut entries = filler_terms(400);
        entries.push(term("Lumen"));
        let text = "lumen 的词典";
        let first = select_for_prompt(&entries, text, DICTIONARY_PROMPT_BUDGET_CHARS);
        let second = select_for_prompt(&entries, text, DICTIONARY_PROMPT_BUDGET_CHARS);
        assert_eq!(first.context.terms, second.context.terms);
        assert_eq!(first.context.replacements, second.context.replacements);
    }
}
