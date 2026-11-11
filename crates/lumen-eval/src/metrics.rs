//! Token-level error rate: a CER on Chinese, a WER on English, both on
//! mixed speech. One token per CJK character, one per Latin word (the
//! normalized string already lowercases, folds numerals and drops
//! fillers, so tokenization is a plain split on that canonical form).

use crate::normalize::{is_cjk_letter, normalize_for_scoring};

/// Tokenize a *normalized* string: single CJK characters plus Latin runs.
pub fn score_tokens(normalized: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut latin = String::new();
    for c in normalized.chars() {
        if is_cjk_letter(c) {
            if !latin.is_empty() {
                tokens.push(std::mem::take(&mut latin));
            }
            tokens.push(c.to_string());
        } else if c.is_alphanumeric() || c == '-' {
            latin.push(c);
        } else if !latin.is_empty() {
            tokens.push(std::mem::take(&mut latin));
        }
    }
    if !latin.is_empty() {
        tokens.push(latin);
    }
    tokens
}

/// Plain Levenshtein distance over arbitrary token slices.
pub fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ta) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, tb) in b.iter().enumerate() {
            let cost = usize::from(ta != tb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Edit count and rate of a hypothesis against a reference.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ErrorRate {
    pub edits: usize,
    pub reference_tokens: usize,
    /// edits / reference tokens; an empty reference with a non-empty
    /// hypothesis scores 1.0 (everything is inserted), empty-vs-empty 0.0.
    pub rate: f64,
}

/// Score raw hypothesis text against raw reference text through the shared
/// normalization pipeline.
pub fn error_rate(hypothesis: &str, reference: &str) -> ErrorRate {
    let hyp = score_tokens(&normalize_for_scoring(hypothesis));
    let r#ref = score_tokens(&normalize_for_scoring(reference));
    let edits = edit_distance(&hyp, &r#ref);
    let rate = if r#ref.is_empty() {
        if hyp.is_empty() {
            0.0
        } else {
            1.0
        }
    } else {
        edits as f64 / r#ref.len() as f64
    };
    ErrorRate {
        edits,
        reference_tokens: r#ref.len(),
        rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_scores_zero() {
        let e = error_rate("今天天气很好", "今天天气很好");
        assert_eq!(e.rate, 0.0);
    }

    #[test]
    fn chinese_counts_per_character() {
        // One wrong character of six → 1/6.
        let e = error_rate("今天大气很好", "今天天气很好");
        assert_eq!(e.edits, 1);
        assert_eq!(e.reference_tokens, 6);
        assert!((e.rate - 1.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn english_counts_per_word() {
        // One wrong word of six.
        let e = error_rate(
            "lets ship the release in friday",
            "lets ship the release on friday",
        );
        assert_eq!(e.edits, 1);
        assert_eq!(e.reference_tokens, 6);
    }

    #[test]
    fn punctuation_and_fillers_are_free() {
        let e = error_rate("嗯，那个，我们下周三！", "我们下周三");
        assert_eq!(e.rate, 0.0);
    }

    #[test]
    fn numerals_are_canonicalized() {
        let e = error_rate("内存是32G", "内存是三十二G");
        assert_eq!(e.rate, 0.0);
    }

    #[test]
    fn empty_reference_behaviour() {
        assert_eq!(error_rate("", "").rate, 0.0);
        assert_eq!(error_rate("多了几个字", "").rate, 1.0);
    }
}
