//! LLM output verification: LCS recall, digit preservation, refusal
//! detection, and cleanup-count logging (INV-1832).
//!
//! The Cadenza-inspired "verifier" pattern: the corrector's output is
//! not trusted — it is checked. A failed check falls back to the
//! pre-corrector text with a human-readable reason.

/// LCS (Longest Common Subsequence) character recall: the fraction of
/// the original's characters that appear in the output **in order**.
/// A paraphrase has high recall; a rewrite drops characters.
pub fn lcs_recall(original: &str, output: &str) -> f64 {
    let a: Vec<char> = original.chars().filter(|c| c.is_alphanumeric()).collect();
    let b: Vec<char> = output.chars().filter(|c| c.is_alphanumeric()).collect();
    if a.is_empty() {
        return 1.0;
    }
    if b.is_empty() {
        return 0.0;
    }
    // Standard LCS DP, capped at 4096×4096 for safety.
    let n = a.len().min(4096);
    let m = b.len().min(4096);
    let mut prev = vec![0usize; m + 1];
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = 0;
        for j in 1..=m {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(cur[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m] as f64 / a.len() as f64
}

/// Every digit sequence in the original must appear in the output.
/// "三点" → "3点" is fine (the digit 3 is there); "350 个" → "三个" is
/// not (350 disappeared).
pub fn digits_preserved(original: &str, output: &str) -> bool {
    let extract = |s: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() || (c.is_numeric() && !c.is_alphabetic()) {
                cur.push(c);
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };
    let orig_digits = extract(original);
    if orig_digits.is_empty() {
        return true;
    }
    let out_digits = extract(output);
    // Every digit-run in the original must appear somewhere in the output.
    orig_digits
        .iter()
        .all(|d| out_digits.iter().any(|od| od.contains(d.as_str())))
}

/// Detects a refusal, apology, or meta-comment that the model produced
/// instead of a correction.
pub fn is_refusal(output: &str) -> bool {
    let trimmed = output.trim();
    let lowered = trimmed.to_lowercase();
    const PREFIXES: &[&str] = &[
        "i'm sorry",
        "i am sorry",
        "i cannot",
        "i can't",
        "i apologize",
        "as an ai",
        "i'm unable",
        "i am unable",
        "对不起",
        "抱歉",
        "我无法",
        "我不能",
        "作为ai",
        "作为人工智能",
    ];
    PREFIXES.iter().any(|p| lowered.starts_with(p))
}

/// Cleanup statistics for logging: counts only, never the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupCount {
    /// Hesitation fillers found (嗯/呃/um/uh).
    pub hesitations: u32,
    /// Immediate word repeats (我我我想 / I I think).
    pub repeats: u32,
    /// Pure connector words at clause starts (那个，/you know,).
    pub connectors: u32,
}

const CJK_FILLERS: &[&str] = &["嗯", "呃", "啊"];
const LATIN_FILLERS: &[&str] = &["um", "uh", "er", "ah"];
const CONNECTORS: &[&str] = &[
    "那个，",
    "然后，",
    "就是说，",
    "you know,",
    "like,",
    "I mean,",
];

/// Count cleanup statistics in a transcript — for logging only.
pub fn count_cleanup(text: &str) -> CleanupCount {
    let mut count = CleanupCount::default();
    let lowered = text.to_lowercase();

    for filler in CJK_FILLERS {
        count.hesitations += lowered.matches(*filler).count() as u32;
    }
    let latin_words: std::collections::HashSet<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for filler in LATIN_FILLERS {
        if latin_words.contains(filler) {
            count.hesitations += 1;
        }
    }

    // Word repeats: same word twice in a row (我我 / the the).
    let words: Vec<&str> = text.split_whitespace().collect();
    for w in words.windows(2) {
        if w[0] == w[1] && w[0].len() > 1 {
            count.repeats += 1;
        }
    }
    // NOTE: CJK char-level repeats (天天, 人人) are legitimate reduplication
    // and are NOT counted — only word-level repeats (Latin tokens) fire.

    for conn in CONNECTORS {
        count.connectors += lowered.matches(&conn.to_lowercase()).count() as u32;
    }

    count
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// The verification verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyVerdict {
    /// Output passes all checks.
    Pass,
    /// Output fails; use the original instead. Carries a human reason.
    Fail(String),
}

/// Verify a corrector output against the original transcript.
/// `min_recall` is the LCS character recall threshold (default 0.75).
pub fn verify_output(original: &str, output: &str, min_recall: f64) -> VerifyVerdict {
    let output_trimmed = output.trim();
    if output_trimmed.is_empty() {
        return VerifyVerdict::Pass; // empty handled upstream
    }
    if is_refusal(output_trimmed) {
        return VerifyVerdict::Fail("AI 拒绝回答或道歉，不是清理结果".into());
    }
    let recall = lcs_recall(original, output_trimmed);
    if recall < min_recall {
        return VerifyVerdict::Fail(format!(
            "改写过多：原文按序保留 {recall:.0}%，低于 {min_recall:.0}% 阈值"
        ));
    }
    if !digits_preserved(original, output_trimmed) {
        return VerifyVerdict::Fail("数字丢失：原文中的数字在输出中消失了".into());
    }
    VerifyVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcs_recall_identity_is_one() {
        assert!((lcs_recall("你好世界", "你好世界") - 1.0).abs() < 1e-9);
        assert!((lcs_recall("hello world", "hello world") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn lcs_recall_complete_rewrite_is_low() {
        // Completely different text: near-zero recall.
        let r = lcs_recall("今天下午三点开会讨论预算", "让我们来聊聊人生的意义");
        assert!(r < 0.2, "recall = {r}");
    }

    #[test]
    fn lcs_recall_light_cleanup_is_high() {
        // Fillers removed, structure preserved (~30% chars removed).
        let r = lcs_recall("嗯那个我们下周三开会", "我们下周三开会");
        assert!(r > 0.65, "recall = {r}");
    }

    #[test]
    fn lcs_recall_translation_is_moderate() {
        // Same meaning, different language — moderate overlap.
        let r = lcs_recall("hello world", "你好世界");
        // 'o' appears in both, 'l' in both... CJK↔Latin overlap is low.
        assert!(r < 0.5, "recall = {r}");
    }

    #[test]
    fn digits_preserved_exact() {
        assert!(digits_preserved("三点五", "3.5"));
        assert!(digits_preserved("350 个", "350 个"));
        assert!(digits_preserved("version 2.1", "version 2.1"));
    }

    #[test]
    fn digits_preserved_missing() {
        assert!(!digits_preserved("350 个", "三个"));
        assert!(!digits_preserved("GPT-4", "GPT"));
    }

    #[test]
    fn refusal_detected() {
        assert!(is_refusal("I'm sorry, I can't help with that."));
        assert!(is_refusal("对不起，我无法处理这个请求"));
        assert!(is_refusal("As an AI language model..."));
        assert!(!is_refusal("这是清理后的文本"));
        assert!(!is_refusal("Here is the cleaned text."));
    }

    #[test]
    fn verify_passes_good_cleanup() {
        let verdict = verify_output("嗯那个我们下周三开会", "我们下周三开会", 0.65);
        assert_eq!(verdict, VerifyVerdict::Pass);
    }

    #[test]
    fn verify_fails_rewrite() {
        let verdict = verify_output("今天下午三点开会讨论预算", "让我们聊聊人生", 0.75);
        assert!(matches!(verdict, VerifyVerdict::Fail(_)));
    }

    #[test]
    fn verify_fails_digit_loss() {
        let verdict = verify_output("有 350 个错误", "有一些错误", 0.75);
        assert!(matches!(verdict, VerifyVerdict::Fail(_)));
    }

    #[test]
    fn verify_fails_refusal() {
        let verdict = verify_output("嗯你好", "I'm sorry, I can't do that.", 0.75);
        assert!(matches!(verdict, VerifyVerdict::Fail(_)));
    }

    #[test]
    fn cleanup_count_detects_fillers() {
        let c = count_cleanup("嗯那个我们下周三开会，you know, 就这些");
        assert!(c.hesitations >= 1, "hesitations: {c:?}");
        assert!(c.connectors >= 1, "connectors: {c:?}");
    }

    #[test]
    fn cleanup_count_zero_on_clean_text() {
        let c = count_cleanup("下午去公园散步");
        assert_eq!(c, CleanupCount::default());
    }
}
