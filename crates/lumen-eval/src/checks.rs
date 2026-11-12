//! Cleanup checks, dictionary term hits, and the diarization
//! speaker-count guard.
//!
//! The cleanup checks compare a *cleaned* hypothesis (what the pipeline
//! would insert) against the clip's expectations. They are deliberately
//! corpus-driven (`must` / `must_not`) instead of a global filler list, so
//! a filler that is genuine content ("那个意思") is never punished.

use crate::corpus::EvalClip;
use crate::metrics::score_tokens;
use crate::normalize::{is_cjk_letter, normalize_for_scoring, normalize_keep_fillers};

/// One failed check on one clip.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckFailure {
    /// A `must_not` phrase (filler or replaced self-correction) survived
    /// cleanup.
    FillerLeft { phrase: String },
    /// A `must` phrase was lost.
    LostPhrase { phrase: String },
    /// More than one token exists in neither the spoken text nor the
    /// expected cleanup — the cleaner invented content. Pure numbers are
    /// exempt.
    InventedTokens { tokens: Vec<String> },
    /// The Chinese-character share moved more than 30 points away from the
    /// spoken text (e.g. an English-leaning model translated a Chinese
    /// dictation).
    LanguageDrift {
        input_zh_share: f64,
        output_zh_share: f64,
    },
}

/// Guard outcome for diarization speaker counts. A collapse (many expected
/// speakers, one found) fails; finding *more* speakers than expected is a
/// split, reported as a warning-level failure because over-segmentation is
/// the cheaper error to repair downstream.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpeakerGuard {
    pub expected: u32,
    pub found: u32,
    pub pass: bool,
}

pub fn speaker_count_guard(expected: u32, found: u32) -> SpeakerGuard {
    SpeakerGuard {
        expected,
        found,
        pass: found == expected,
    }
}

/// Count distinct speaker labels in a segmented transcript
/// `(speaker_label, start_seconds, end_seconds)`. Diarization collapse
/// shows up here as one label covering everything.
pub fn count_speaker_labels(segments: &[(String, f64, f64)]) -> u32 {
    segments
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u32
}

/// Fraction of CJK characters among CJK characters plus Latin letters.
fn zh_share(text: &str) -> f64 {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    for c in text.chars() {
        if is_cjk_letter(c) {
            cjk += 1;
        } else if c.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    if cjk + latin == 0 {
        return 0.0;
    }
    cjk as f64 / (cjk + latin) as f64
}

/// Containment on the filler-preserving normalization. CJK needles match
/// as substrings; Latin needles match on word boundaries so "um" does not
/// hit "Lumen". A phrase that canonicalizes to nothing can never count as
/// present.
fn normalized_contains(hyp_normalized: &str, phrase: &str) -> bool {
    let needle = normalize_keep_fillers(phrase);
    if needle.is_empty() {
        return false;
    }
    if needle.chars().any(is_cjk_letter) {
        return hyp_normalized.contains(&needle);
    }
    // Latin needle: compare against the hypothesis's Latin word runs.
    let needle_words: Vec<&str> = needle.split(' ').collect();
    let hyp_words: Vec<&str> = hyp_normalized
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    if needle_words.len() > hyp_words.len() {
        return false;
    }
    hyp_words
        .windows(needle_words.len())
        .any(|window| window == needle_words.as_slice())
}

/// Run the corpus-driven cleanup checks for one clip.
///
/// `raw_text` is what was spoken (clip reference), `expected_clean` the
/// target cleanup, `hypothesis` the pipeline's cleaned output.
pub fn cleanup_checks(
    raw_text: &str,
    expected_clean: &str,
    hypothesis: &str,
    _clip: &EvalClip,
    resolved_must: &[String],
    resolved_must_not: &[String],
) -> Vec<CheckFailure> {
    let hyp_norm = normalize_keep_fillers(hypothesis);
    let mut failures = Vec::new();

    for phrase in resolved_must_not {
        if normalized_contains(&hyp_norm, phrase) {
            failures.push(CheckFailure::FillerLeft {
                phrase: phrase.clone(),
            });
        }
    }
    for phrase in resolved_must {
        if !normalized_contains(&hyp_norm, phrase) {
            failures.push(CheckFailure::LostPhrase {
                phrase: phrase.clone(),
            });
        }
    }

    // Invention: hypothesis tokens absent from both the spoken text and
    // the expected cleanup. Numbers are exempt; more than one distinct
    // invented token fails the clip. Scored on the filler-stripping
    // normalization — content that survives there is what counts.
    let allowed: std::collections::BTreeSet<String> =
        score_tokens(&normalize_for_scoring(raw_text))
            .into_iter()
            .chain(score_tokens(&normalize_for_scoring(expected_clean)))
            .collect();
    let invented: Vec<String> = score_tokens(&normalize_for_scoring(hypothesis))
        .into_iter()
        .filter(|t| !allowed.contains(t) && !t.chars().all(|c| c.is_ascii_digit()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if invented.len() > 1 {
        failures.push(CheckFailure::InventedTokens { tokens: invented });
    }

    // Language drift: the zh share of the hypothesis versus the spoken
    // text. Only meaningful when the input has any letters at all.
    let input_share = zh_share(raw_text);
    let output_share = zh_share(hypothesis);
    if (input_share - output_share).abs() > 0.30 && input_share > 0.05 {
        failures.push(CheckFailure::LanguageDrift {
            input_zh_share: (input_share * 100.0).round() / 100.0,
            output_zh_share: (output_share * 100.0).round() / 100.0,
        });
    }

    failures
}

/// Dictionary term hits: how many of the clip's terms appear exactly
/// (normalized) in the hypothesis.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TermHits {
    pub hits: usize,
    pub total: usize,
    pub missed: Vec<String>,
}

pub fn term_hits(hypothesis: &str, terms: &[String]) -> TermHits {
    let hyp_norm = normalize_keep_fillers(hypothesis);
    let mut missed = Vec::new();
    let mut hits = 0usize;
    for term in terms {
        if normalized_contains(&hyp_norm, term) {
            hits += 1;
        } else {
            missed.push(term.clone());
        }
    }
    TermHits {
        hits,
        total: terms.len(),
        missed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{Corpus, EvalSet};

    fn clip(id: &str, must: Vec<String>, must_not: Vec<String>) -> EvalClip {
        EvalClip {
            id: id.into(),
            set: EvalSet::ZhFiller,
            lang: "zh".into(),
            text: Some("text".into()),
            clean: Some("clean".into()),
            must,
            must_not,
            terms: vec![],
            speakers: None,
            source_clip_ids: vec![],
            gap_seconds: None,
        }
    }

    #[test]
    fn filler_residue_is_caught() {
        let c = clip(
            "c",
            vec!["下周三".into()],
            vec!["呃".into(), "下周二".into()],
        );
        let raw = "嗯，那个，我们下周二，呃不对，下周三下午三点开个会。";
        let expected = "我们下周三下午三点开个会。";
        // Buggy cleanup kept the filler and the replaced phrase.
        let hyp = "我们下周二，呃，下周三下午三点开个会。";
        let failures = cleanup_checks(raw, expected, hyp, &c, &c.must, &c.must_not);
        assert!(failures.contains(&CheckFailure::FillerLeft {
            phrase: "呃".into()
        }));
        assert!(failures.contains(&CheckFailure::FillerLeft {
            phrase: "下周二".into()
        }));
        assert!(!failures.is_empty());
    }

    #[test]
    fn good_cleanup_passes() {
        let c = clip("c", vec!["下周三".into(), "预算".into()], vec!["嗯".into()]);
        let raw = "嗯，我们下周三讨论预算。";
        let expected = "我们下周三讨论预算。";
        let failures = cleanup_checks(raw, expected, expected, &c, &c.must, &c.must_not);
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn lost_phrase_is_caught() {
        let c = clip("c", vec!["预算".into()], vec![]);
        let raw = "我们下周三讨论预算。";
        let failures = cleanup_checks(raw, raw, "我们下周三讨论。", &c, &c.must, &c.must_not);
        assert_eq!(
            failures,
            vec![CheckFailure::LostPhrase {
                phrase: "预算".into()
            }]
        );
    }

    #[test]
    fn invented_tokens_are_caught_but_numbers_exempt() {
        let c = clip("c", vec![], vec![]);
        let raw = "会议三点开始";
        // One invented token alone is tolerated (noise), two fail.
        let one = cleanup_checks(raw, raw, "会议三点开始通", &c, &c.must, &c.must_not);
        assert!(one.is_empty(), "{one:?}");
        let two = cleanup_checks(raw, raw, "会议三点开始通知", &c, &c.must, &c.must_not);
        assert!(
            two.iter()
                .any(|f| matches!(f, CheckFailure::InventedTokens { .. })),
            "{two:?}"
        );
        // Digits never count as inventions.
        let digits = cleanup_checks(raw, raw, "会议3点开始", &c, &c.must, &c.must_not);
        assert!(digits.is_empty(), "{digits:?}");
    }

    #[test]
    fn language_drift_is_caught() {
        let c = clip("c", vec![], vec![]);
        let raw = "今天天气很好我们去公园散步";
        let hyp = "The weather is nice today lets take a walk in the park";
        let failures = cleanup_checks(raw, raw, hyp, &c, &c.must, &c.must_not);
        assert!(
            failures
                .iter()
                .any(|f| matches!(f, CheckFailure::LanguageDrift { .. })),
            "{failures:?}"
        );
    }

    #[test]
    fn term_hits_count_normalized_appearances() {
        let t = term_hits(
            "用 sherpa-onnx 和 Qwen3-ASR",
            &["sherpa-onnx".into(), "Qwen3-ASR".into(), "Lumen".into()],
        );
        assert_eq!(t.hits, 2);
        assert_eq!(t.total, 3);
        assert_eq!(t.missed, vec!["Lumen".to_string()]);
    }

    #[test]
    fn speaker_guard_catches_collapse() {
        // INV-153 shape: three speakers, diarization collapsed into one.
        let segments = vec![
            ("说话人1".to_string(), 0.0, 12.0),
            ("说话人1".to_string(), 12.0, 25.0),
            ("说话人1".to_string(), 25.0, 40.0),
        ];
        let found = count_speaker_labels(&segments);
        let guard = speaker_count_guard(3, found);
        assert!(!guard.pass);
        assert_eq!(found, 1);
        assert!(speaker_count_guard(3, 3).pass);
    }

    #[test]
    fn corpus_must_not_phrases_are_gone_from_clean() {
        // Every must_not phrase must be absent from the clip's own clean
        // text (normalized), otherwise the corpus is self-contradictory.
        let corpus = Corpus::embedded().unwrap();
        for clip in corpus.clips.iter().filter(|c| c.source_clip_ids.is_empty()) {
            let (_, clean) = corpus.resolved_text(clip).unwrap();
            let clean_norm = normalize_keep_fillers(&clean);
            for phrase in &clip.must_not {
                let needle = normalize_keep_fillers(phrase);
                if needle.is_empty() {
                    continue;
                }
                assert!(
                    !clean_norm.contains(&needle),
                    "clip {}: must_not phrase {:?} appears in its own clean text",
                    clip.id,
                    phrase
                );
            }
            for phrase in &clip.must {
                let needle = normalize_keep_fillers(phrase);
                assert!(
                    !needle.is_empty() && clean_norm.contains(&needle),
                    "clip {}: must phrase {:?} missing from its own clean text",
                    clip.id,
                    phrase
                );
            }
        }
    }
}
