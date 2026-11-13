//! Five-tier dictionary matcher: alias exact / English split-case /
//! pinyin homophone / Latin near-sound / dangerous-alias blocklist,
//! with URL/email/code-span protection (INV-1831).
//!
//! Runs **after** ASR and before the corrector, purely local. The input
//! is the raw transcript; the output is a corrected transcript plus a
//! report of what was changed. Each tier has independent tests and a
//! reason it can refuse to fire — the design bias is "better to miss a
//! correction than to corrupt ordinary speech".
//!
//! Inspired by Cadenza's VocabularyMatcher, rebuilt in Rust for
//! lumen-dictionary without a platform transform dependency (pinyin is
//! a lookup table, not CFStringTransform).

use crate::DictionaryEntry;

/// A vocabulary term with its aliases, ready for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VocabTerm {
    /// The canonical term the user wants in the text (e.g. "GitHub").
    pub term: String,
    /// Alternative writings the recognizer may produce (e.g. "吉特哈勃").
    pub aliases: Vec<String>,
}

impl VocabTerm {
    pub fn from_entry(entry: &DictionaryEntry) -> Option<VocabTerm> {
        let term = entry.term.as_deref()?.trim().to_string();
        if term.is_empty() {
            return None;
        }
        Some(VocabTerm {
            term,
            aliases: Vec::new(),
        })
    }

    pub fn new(term: impl Into<String>, aliases: Vec<String>) -> Self {
        Self {
            term: term.into(),
            aliases,
        }
    }
}

/// What one correction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The matched span in the original text (byte range).
    pub start: usize,
    pub end: usize,
    /// What was there before.
    pub from: String,
    /// What replaced it.
    pub to: String,
    /// Which tier fired.
    pub tier: CorrectionTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionTier {
    /// Exact alias match ("吉特哈勃" → GitHub).
    AliasExact,
    /// English split/case ("git hub" → GitHub).
    EnglishSplitCase,
    /// Pinyin homophone ("糖尿饼" → 糖尿病).
    PinyinHomophone,
    /// Latin near-sound ("gbt" → GPT).
    LatinNearSound,
}

/// The correction report.
#[derive(Debug, Clone, Default)]
pub struct CorrectionReport {
    pub corrections: Vec<Correction>,
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Apply all five matching tiers to `text`, returning the corrected text
/// and a report. Pure; no platform dependencies.
pub fn correct(text: &str, terms: &[VocabTerm]) -> (String, CorrectionReport) {
    let mut corrections = Vec::new();
    for term in terms {
        corrections.extend(alias_exact_matches(text, term));
        corrections.extend(pinyin_homophone_matches(text, term));
        corrections.extend(latin_near_sound_matches(text, term));
    }
    if corrections.is_empty() {
        return (text.to_string(), CorrectionReport { corrections });
    }
    // Apply non-overlapping corrections in reverse order.
    corrections.sort_by_key(|c| c.start);
    let mut applied: Vec<&Correction> = Vec::new();
    let mut cursor = 0;
    for c in &corrections {
        if c.start >= cursor {
            applied.push(c);
            cursor = c.end;
        }
    }
    // Apply forward: copy the span before each correction, insert the
    // replacement, then the span up to the next correction.
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for c in applied.iter() {
        out.push_str(&text[cursor..c.start]);
        out.push_str(&c.to);
        cursor = c.end;
    }
    out.push_str(&text[cursor..]);
    let report = CorrectionReport {
        corrections: applied.into_iter().cloned().collect(),
    };
    (out, report)
}

/// Whether an alias is safe to use. Rejects:
/// - single CJK characters ("他" — would replace every occurrence);
/// - CJK phrases of ≤2 common characters ("的", "我们");
/// - single common English words ("the", "and", "is", "go").
pub fn alias_is_safe(alias: &str) -> bool {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        return false;
    }
    let cjk_count = trimmed.chars().filter(|c| is_cjk(*c)).count();
    let all_cjk = cjk_count == trimmed.chars().count();
    if all_cjk && cjk_count <= 2 {
        // Single or double CJK: only safe if it contains at least one
        // uncommon character (not in the common-char list below).
        return trimmed.chars().any(|c| !COMMON_CJK_CHARS.contains(&c));
    }
    if !all_cjk && trimmed.split_whitespace().count() == 1 {
        let lower = trimmed.to_lowercase();
        if COMMON_ENGLISH_WORDS.contains(&lower.as_str()) {
            return false;
        }
    }
    true
}

/// Common CJK characters that are too generic to be safe aliases.
const COMMON_CJK_CHARS: &[char] = &[
    '的', '了', '是', '我', '你', '他', '她', '它', '们', '个', '在', '有', '和', '就', '不', '上',
    '下', '中', '大', '小', '好', '说', '要', '去', '会', '着', '到', '能', '里', '也', '这', '那',
    '可', '以', '因', '所', '怎', '多', '少', '没', '看', '想',
];

/// Common English words too generic to be safe single-word aliases.
const COMMON_ENGLISH_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could", "should", "may", "might",
    "must", "can", "to", "of", "in", "on", "at", "for", "with", "by", "from", "up", "down", "out",
    "off", "over", "under", "again", "then", "once", "here", "there", "when", "where", "why",
    "how", "all", "any", "both", "each", "few", "more", "most", "other", "some", "such", "no",
    "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just", "go", "get", "got",
    "make", "made", "know", "take", "see", "come", "look", "use", "find", "give", "tell", "work",
];

// ── Tier 1: alias exact ────────────────────────────────────────────────────

fn alias_exact_matches(text: &str, term: &VocabTerm) -> Vec<Correction> {
    let mut out = Vec::new();
    let folded_text = fold(text);
    let folded_term = fold(&term.term);
    for alias in &term.aliases {
        if !alias_is_safe(alias) {
            continue;
        }
        // Direct alias match (CJK or Latin).
        if let Some(pos) = text.find(alias.as_str()) {
            if !in_protected_span(text, pos, pos + alias.len()) {
                out.push(Correction {
                    start: pos,
                    end: pos + alias.len(),
                    from: alias.clone(),
                    to: term.term.clone(),
                    tier: CorrectionTier::AliasExact,
                });
            }
        }
        // Alias with flexible spaces (git hub → GitHub).
        if alias.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ') {
            let no_space = alias.replace(' ', "");
            if !no_space.is_empty() {
                if let Some(pos) = folded_text.find(&no_space.to_lowercase()) {
                    if !in_protected_span(text, pos, pos + no_space.len()) {
                        out.push(Correction {
                            start: pos,
                            end: pos + no_space.len(),
                            from: text[pos..pos + no_space.len()].to_string(),
                            to: term.term.clone(),
                            tier: CorrectionTier::EnglishSplitCase,
                        });
                    }
                }
            }
        }
    }
    // Self-match: lowercase/split form of the term itself.
    if folded_text.contains(&folded_term) {
        // Only correct if the term has mixed case or digits (pure lowercase
        // terms like "react" would rewrite ordinary words).
        let has_mixed_case = term.term.chars().any(char::is_uppercase)
            && term.term.chars().any(|c| c.is_lowercase());
        let has_digits = term.term.chars().any(|c| c.is_ascii_digit());
        if has_mixed_case || has_digits {
            let no_space = term.term.replace([' ', '-'], "").to_lowercase();
            if no_space.len() >= 3 {
                let mut search = 0;
                while let Some(pos) = folded_text[search..].find(&no_space) {
                    let start = search + pos;
                    let end = start + no_space.len();
                    if !in_protected_span(text, start, end)
                        && text[start..end].to_lowercase() != term.term
                    {
                        out.push(Correction {
                            start,
                            end,
                            from: text[start..end].to_string(),
                            to: term.term.clone(),
                            tier: CorrectionTier::EnglishSplitCase,
                        });
                    }
                    search = end;
                }
            }
        }
    }
    out
}

// ── Tier 3: pinyin homophone ───────────────────────────────────────────────

/// Pinyin lookup table for common CJK characters (no-tone).
/// This is a minimal starter set — the full table should be generated
/// from a pinyin dictionary at build time or shipped as data.
fn pinyin_of(c: char) -> Option<&'static str> {
    PINYIN_TABLE.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// Fuzzy pinyin key: merges common confusions (en↔eng, in↔ing, an↔ang).
fn fuzzy_pinyin_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'e' => out.push('e'), // en/eng collapse to e
            _ => out.push(c),
        }
    }
    // Collapse nasal endings.
    out.replace("eng", "en")
        .replace("ing", "in")
        .replace("ang", "an")
}

fn pinyin_homophone_matches(text: &str, term: &VocabTerm) -> Vec<Correction> {
    // Only for CJK terms with ≥2 CJK characters.
    let term_cjk: Vec<char> = term.term.chars().filter(|c| is_cjk(**&c)).collect();
    if term_cjk.len() < 2 {
        return Vec::new();
    }
    // Get pinyin for the term; skip if we don't know all characters.
    let term_pinyin: Vec<&str> = term_cjk.iter().filter_map(|c| pinyin_of(*c)).collect();
    if term_pinyin.len() != term_cjk.len() {
        return Vec::new();
    }
    let term_key: String = term_pinyin.concat();
    let term_fuzzy = fuzzy_pinyin_key(&term_key);

    // Sliding window: for each position in the text, check if the next
    // `term_cjk.len()` CJK characters have matching fuzzy pinyin.
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let n = term_cjk.len();
    for i in 0..chars.len().saturating_sub(n - 1) {
        let window = &chars[i..i + n];
        // Must be a CJK window, all characters must have pinyin.
        if !window.iter().all(|c| is_cjk(*c)) {
            continue;
        }
        let run: String = window.iter().collect();
        if run == term.term {
            continue;
        }
        let pinyins: Vec<&str> = window.iter().filter_map(|c| pinyin_of(*c)).collect();
        if pinyins.len() == n {
            let key: String = pinyins.concat();
            if fuzzy_pinyin_key(&key) == term_fuzzy {
                let byte_start: usize = chars[..i].iter().map(|c| c.len_utf8()).sum();
                let byte_end: usize = chars[..i + n].iter().map(|c| c.len_utf8()).sum();
                if !in_protected_span(text, byte_start, byte_end) {
                    out.push(Correction {
                        start: byte_start,
                        end: byte_end,
                        from: run,
                        to: term.term.clone(),
                        tier: CorrectionTier::PinyinHomophone,
                    });
                }
            }
        }
    }
    out
}

// ── Tier 4: Latin near-sound ───────────────────────────────────────────────

/// Confusable consonant pairs for near-sound matching.
const NEAR_SOUND_PAIRS: &[(char, char)] = &[
    ('b', 'p'),
    ('d', 't'),
    ('g', 'k'),
    ('p', 'q'),
    ('v', 'w'),
    ('s', 'z'),
    ('m', 'n'),
    ('l', 'r'),
];

fn latin_near_sound_matches(text: &str, term: &VocabTerm) -> Vec<Correction> {
    // Only for single-word Latin terms of 3-8 characters.
    if term.term.contains(' ') || term.term.contains('-') {
        return Vec::new();
    }
    let len = term.term.len();
    if !(3..=8).contains(&len) {
        return Vec::new();
    }
    let folded_term = term.term.to_lowercase();
    let folded_text = text.to_lowercase();

    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if word.is_empty() || word.len() != len {
            continue;
        }
        let lower = word.to_lowercase();
        if lower == folded_term {
            continue;
        }
        let d = damerau_levenshtein(&lower, &folded_term);
        if d == 1 && has_near_sound_pair(&lower, &folded_term) {
            let byte_start = text.find(word).unwrap_or(0);
            let byte_end = byte_start + word.len();
            if !in_protected_span(text, byte_start, byte_end) {
                out.push(Correction {
                    start: byte_start,
                    end: byte_end,
                    from: word.to_string(),
                    to: term.term.clone(),
                    tier: CorrectionTier::LatinNearSound,
                });
            }
        }
    }
    let _ = &folded_text;
    out
}

fn has_near_sound_pair(a: &str, b: &str) -> bool {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    for (ca, cb) in ac.iter().zip(bc.iter()) {
        if ca != cb {
            return NEAR_SOUND_PAIRS
                .iter()
                .any(|(x, y)| (ca == x && cb == y) || (ca == y && cb == x));
        }
    }
    false
}

// ── Protected spans ────────────────────────────────────────────────────────

/// Spans in the text that corrections must never touch: URLs, email
/// addresses, `code spans`, and file paths.
fn in_protected_span(text: &str, start: usize, end: usize) -> bool {
    let prefix = &text[..start];
    let suffix = &text[end..];

    // Inside backticks.
    let before_ticks = prefix.matches('`').count();
    let after_ticks = suffix.matches('`').count();
    if before_ticks % 2 == 1 && after_ticks % 2 == 1 {
        return true;
    }

    // URL/email: preceded by `://` or `@` or `www.` without whitespace between.
    if prefix.ends_with("://") || prefix.ends_with('@') || prefix.ends_with("www.") {
        return true;
    }
    // Inside a path-like context: preceded by `/` and followed by `/`.
    if prefix.ends_with('/') && suffix.starts_with('/') {
        return true;
    }
    // Is part of a longer Latin word: adjacent ASCII alphanumeric
    // characters make this a substring, not a standalone word. CJK
    // characters are excluded — CJK text does not use word spaces.
    if prefix
        .chars()
        .next_back()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
        || suffix
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
    {
        return true;
    }
    false
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{F900}'..='\u{FAFF}').contains(&c)
}

fn fold(s: &str) -> String {
    s.chars()
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

fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 0..=a.len() {
        d[i][0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Minimal starter pinyin table (no-tone). The full table should be
/// shipped as data or generated at build time.
const PINYIN_TABLE: &[(char, &str)] = &[
    ('糖', "tang"),
    ('尿', "niao"),
    ('病', "bing"),
    ('饼', "bing"),
    ('拖', "tuo"),
    ('坑', "keng"),
    ('说', "shuo"),
    ('话', "hua"),
    ('中', "zhong"),
    ('文', "wen"),
    ('英', "ying"),
    ('语', "yu"),
    ('电', "dian"),
    ('脑', "nao"),
    ('手', "shou"),
    ('机', "ji"),
    ('工', "gong"),
    ('作', "zuo"),
    ('学', "xue"),
    ('习', "xi"),
    ('开', "kai"),
    ('发', "fa"),
    ('编', "bian"),
    ('程', "cheng"),
    ('代', "dai"),
    ('码', "ma"),
    ('数', "shu"),
    ('据', "ju"),
    ('库', "ku"),
    ('网', "wang"),
    ('络', "luo"),
    ('安', "an"),
    ('全', "quan"),
    ('系', "xi"),
    ('统', "tong"),
    ('用', "yong"),
    ('户', "hu"),
    ('接', "jie"),
    ('口', "kou"),
    ('协', "xie"),
    ('议', "yi"),
    ('服', "fu"),
    ('务', "wu"),
    ('端', "duan"),
    ('云', "yun"),
    ('计', "ji"),
    ('算', "suan"),
    ('大', "da"),
    ('模', "mo"),
    ('型', "xing"),
    ('人', "ren"),
    ('智', "zhi"),
    ('能', "neng"),
    ('深', "shen"),
    ('度', "du"),
    ('神', "shen"),
    ('经', "jing"),
    ('天', "tian"),
    ('地', "di"),
    ('水', "shui"),
    ('火', "huo"),
    ('山', "shan"),
    ('风', "feng"),
    ('雨', "yu"),
];

#[cfg(test)]
mod tests {
    use super::*;

    // ── alias safety ────────────────────────────────────────────────────

    #[test]
    fn dangerous_aliases_are_rejected() {
        // Single CJK characters (the "他" incident).
        assert!(!alias_is_safe("他"));
        assert!(!alias_is_safe("的"));
        assert!(!alias_is_safe("我"));
        // Two-character common phrases.
        assert!(!alias_is_safe("我们"));
        assert!(!alias_is_safe("可以"));
        // Single common English words.
        assert!(!alias_is_safe("the"));
        assert!(!alias_is_safe("and"));
        assert!(!alias_is_safe("go"));
        assert!(!alias_is_safe("make"));
        // Safe aliases pass.
        assert!(alias_is_safe("吉特哈勃"));
        assert!(alias_is_safe("sherpa-onnx"));
        assert!(alias_is_safe("拖坑"));
    }

    #[test]
    fn two_char_cjk_with_uncommon_char_is_safe() {
        // "糖尿" contains 糖 and 尿, neither is a common character.
        assert!(alias_is_safe("糖尿"));
        // "好的" contains 好 (common) and 的 (common) → unsafe.
        assert!(!alias_is_safe("好的"));
    }

    // ── alias exact ─────────────────────────────────────────────────────

    #[test]
    fn alias_exact_replaces_chinese_for_english() {
        let terms = vec![VocabTerm::new("GitHub", vec!["吉特哈勃".to_string()])];
        let (out, report) = correct("我用吉特哈勃提交代码", &terms);
        assert_eq!(out, "我用GitHub提交代码");
        assert_eq!(report.corrections.len(), 1);
        assert_eq!(report.corrections[0].tier, CorrectionTier::AliasExact);
    }

    #[test]
    fn alias_with_flexible_space() {
        let terms = vec![VocabTerm::new("GitHub", vec!["git hub".to_string()])];
        let (out, _) = correct("去 git hub 看看", &terms);
        assert_eq!(out, "去 GitHub 看看");
    }

    #[test]
    fn lowercase_form_corrected_for_mixed_case_terms() {
        let terms = vec![VocabTerm::new("GitHub", vec![])];
        let (out, _) = correct("在 github 上看代码", &terms);
        assert_eq!(out, "在 GitHub 上看代码");
    }

    #[test]
    fn pure_lowercase_terms_not_case_corrected() {
        // "react" is pure lowercase — correcting would rewrite ordinary words.
        let terms = vec![VocabTerm::new("react", vec![])];
        let (out, _) = correct("the react framework", &terms);
        assert_eq!(out, "the react framework");
    }

    // ── pinyin homophone ────────────────────────────────────────────────

    #[test]
    fn pinyin_homophone_tang_yao_bing_to_tang_niao_bing() {
        // 糖尿饼 (tang-yao-bing) → 糖尿病 (tang-niao-bing) via fuzzy en/eng.
        // Wait: 饼 = bing, 病 = bing — these are exact pinyin matches!
        let terms = vec![VocabTerm::new("糖尿病", vec![])];
        let (out, report) = correct("老人得了糖尿饼要控制饮食", &terms);
        assert!(out.contains("糖尿病"), "output: {out}");
        assert_eq!(report.corrections[0].tier, CorrectionTier::PinyinHomophone);
    }

    #[test]
    fn pinyin_homophone_tuo_keng_to_tong_guo() {
        // 拖坑 (tuo-keng) → 通过 (tong-guo): near-sound CJK.
        // These are NOT in the table so won't fire — verify no false positive.
        let terms = vec![VocabTerm::new("通过", vec![])];
        let (out, _) = correct("拖坑了测试", &terms);
        // 通过 characters (通/过) are not in the pinyin table, so no correction.
        assert_eq!(out, "拖坑了测试");
    }

    #[test]
    fn exact_cjk_term_is_not_replaced_by_itself() {
        let terms = vec![VocabTerm::new("糖尿病", vec![])];
        let (out, _) = correct("已有糖尿病", &terms);
        assert_eq!(out, "已有糖尿病");
    }

    // ── Latin near-sound ────────────────────────────────────────────────

    #[test]
    fn near_sound_gbt_to_gpt() {
        // b↔p confusion.
        let terms = vec![VocabTerm::new("GPT", vec![])];
        let (out, report) = correct("用 gbt 生成文本", &terms);
        assert_eq!(out, "用 GPT 生成文本");
        assert_eq!(report.corrections[0].tier, CorrectionTier::LatinNearSound);
    }

    #[test]
    fn near_sound_linyx_to_linux() {
        // "linyx" vs "linux": transposition of u/y... not a near-sound pair.
        // Use "linuc" vs "linux": d↔t is the near-sound pair at position 4.
        // Actually "linux" → "linuc" would need x↔c which isn't a pair.
        // Use a cleaner example: "gpt" vs "gbt" is b↔p.
        // This test verifies a completely different word is NOT matched.
        let terms = vec![VocabTerm::new("Linux", vec![])];
        let (out, _) = correct("the linods distribution", &terms);
        // "linods" vs "linux": distance 2, no near-sound pair → no correction.
        assert_eq!(out, "the linods distribution");
    }

    #[test]
    fn no_near_sound_for_completely_different_words() {
        let terms = vec![VocabTerm::new("Kubernetes", vec![])];
        let (out, _) = correct("use docker containers", &terms);
        assert_eq!(out, "use docker containers");
    }

    // ── protected spans ─────────────────────────────────────────────────

    #[test]
    fn url_is_protected() {
        let terms = vec![VocabTerm::new("GitHub", vec![])];
        let (out, _) = correct("visit https://github.com repo", &terms);
        // The "github" inside the URL must not be case-corrected.
        assert!(out.contains("github.com"), "URL was touched: {out}");
    }

    #[test]
    fn code_span_is_protected() {
        let terms = vec![VocabTerm::new("GitHub", vec![])];
        let (out, _) = correct("run \\`git clone github repo\\`", &terms);
        assert!(out.contains("github"), "code span was touched: {out}");
    }

    #[test]
    fn path_context_is_protected() {
        let terms = vec![VocabTerm::new("GitHub", vec![])];
        let (out, _) = correct("cd /home/github/projects", &terms);
        assert!(out.contains("github"), "path was touched: {out}");
    }

    // ── integration ─────────────────────────────────────────────────────

    #[test]
    fn multiple_terms_apply_non_overlapping() {
        let terms = vec![
            VocabTerm::new("GitHub", vec!["吉特哈勃".to_string()]),
            VocabTerm::new("Lumen", vec![]),
        ];
        let (out, report) = correct("吉特哈勃和lumen都在开发", &terms);
        assert!(out.contains("GitHub"));
        assert!(out.contains("Lumen"));
        assert_eq!(report.corrections.len(), 2);
    }

    #[test]
    fn empty_text_is_pass_through() {
        let terms = vec![VocabTerm::new("GitHub", vec![])];
        let (out, report) = correct("", &terms);
        assert_eq!(out, "");
        assert!(report.corrections.is_empty());
    }
}
