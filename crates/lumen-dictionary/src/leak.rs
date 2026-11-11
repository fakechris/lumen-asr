//! Dictionary-term echo detection and contrastive removal (INV-1806).
//!
//! When a recognizer is fed the personal dictionary as hot words
//! (streaming cloud ASR) or as prompt context, a no-speech stretch can
//! make the model write the injected list back out verbatim —
//! "…嗯。Lumen、Tauri、sherpa-onnx。" after a trailing pause. The
//! failure has a text signature: a run of **injected terms chained by
//! pure separators at a sentence boundary**, with no content words
//! around them, because a person who actually recites their terms says
//! them inside a sentence ("术语表里有 A、B 和 C" — the "和" is content,
//! the run is not at a boundary, so it is not flagged).
//!
//! [`detect_term_echo`] finds such a run; [`clean_with_contrast`] then
//! removes only the terms a **contrast transcription** of the same audio
//! (no hot words injected) does not contain — a term the user really
//! said, even at the very end, survives; an echoed one disappears.

/// Detection defaults: a chain of at least two distinct injected terms,
/// separated by at most three separator characters, at a sentence
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoConfig {
    pub min_chain_terms: usize,
    pub max_gap_chars: usize,
}

impl Default for EchoConfig {
    fn default() -> Self {
        Self {
            min_chain_terms: 2,
            max_gap_chars: 3,
        }
    }
}

/// A detected echo run: the byte span in the transcript and the distinct
/// injected terms found chained inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoRun {
    pub start: usize,
    pub end: usize,
    pub terms: Vec<String>,
}

/// Result of [`clean_with_contrast`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoCleaned {
    pub text: String,
    /// Echoed terms removed (absent from the contrast transcription).
    pub removed: Vec<String>,
    /// Terms the contrast transcription confirms the user actually said;
    /// kept in place, sentence-final ones included.
    pub kept: Vec<String>,
}

/// One non-overlapping occurrence of an injected term.
#[derive(Debug)]
struct Occurrence {
    start: usize,
    end: usize,
    term: usize,
}

fn is_separator(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '、' | '，' | ',' | ';' | '；' | '·' | '/' | '|' | '・' | '﹑'
    )
}

/// A run may only start where a sentence ends (or the text does): echo
/// runs appear appended after a finished sentence or a hesitation, never
/// woven into one.
fn at_sentence_boundary(text: &str, start: usize) -> bool {
    let prefix = &text[..start];
    match prefix.chars().next_back() {
        None => true,
        Some('。') | Some('！') | Some('？') | Some('!') | Some('?') | Some('\n') | Some('；')
        | Some(';') | Some('.') | Some('嗯') | Some('呃') | Some('啊') | Some('哦') => true,
        _ => false,
    }
}

/// ASCII-case-insensitive and fullwidth-folded containment, used to ask
/// the contrast transcription whether a term was actually spoken.
fn folded_contains(haystack: &str, needle: &str) -> bool {
    let fold = |s: &str| {
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
            .collect::<String>()
    };
    let h = fold(haystack);
    let n = fold(needle);
    !n.is_empty() && h.contains(&n)
}

/// Find the injected-term echo run in `text`, if any. Pure; no audio.
pub fn detect_term_echo(text: &str, injected_terms: &[String]) -> Option<EchoRun> {
    detect_term_echo_with(text, injected_terms, &EchoConfig::default())
}

pub fn detect_term_echo_with(
    text: &str,
    injected_terms: &[String],
    cfg: &EchoConfig,
) -> Option<EchoRun> {
    // Terms worth matching: at least two characters, so single-CJK-char
    // dictionary entries cannot flood the scan with spurious hits.
    let terms: Vec<&str> = injected_terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| t.chars().count() >= 2)
        .collect();
    if terms.is_empty() {
        return None;
    }

    let lowered = text.to_lowercase();
    let mut occurrences: Vec<Occurrence> = Vec::new();
    for (index, term) in terms.iter().enumerate() {
        let needle = term.to_lowercase();
        let mut from = 0;
        while let Some(found) = lowered[from..].find(&needle) {
            let start = from + found;
            occurrences.push(Occurrence {
                start,
                end: start + needle.len(),
                term: index,
            });
            from = start + needle.len().max(1);
        }
    }
    if occurrences.is_empty() {
        return None;
    }
    occurrences.sort_by_key(|o| (o.start, o.end));

    // Maximal chains: consecutive occurrences joined by pure-separator
    // gaps within the budget, no overlaps. A chain qualifies when it has
    // enough DISTINCT terms and starts at a sentence boundary.
    let mut best: Option<EchoRun> = None;
    let mut chain_start = 0;
    for i in 0..occurrences.len() {
        if i == 0 || !gap_is_separator(text, &occurrences[i - 1], &occurrences[i], cfg) {
            chain_start = i;
        }
        let start = occurrences[chain_start].start;
        let end = occurrences[i].end;
        let distinct: std::collections::BTreeSet<usize> =
            (chain_start..=i).map(|j| occurrences[j].term).collect();
        if distinct.len() < cfg.min_chain_terms || !at_sentence_boundary(text, start) {
            continue;
        }
        let chain_terms: Vec<String> = distinct
            .into_iter()
            .filter_map(|t| terms.get(t).map(|x| x.to_string()))
            .collect();
        let candidate = EchoRun {
            start,
            end,
            terms: chain_terms,
        };
        let better = match &best {
            None => true,
            Some(prev) => {
                candidate.terms.len() > prev.terms.len()
                    || (candidate.terms.len() == prev.terms.len()
                        && candidate.end - candidate.start > prev.end - prev.start)
            }
        };
        if better {
            best = Some(candidate);
        }
    }

    best
}

/// `true` when the gap between two occurrences is pure separators within
/// the budget (and they do not overlap).
fn gap_is_separator(text: &str, a: &Occurrence, b: &Occurrence, cfg: &EchoConfig) -> bool {
    if b.start < a.end {
        return false;
    }
    let gap = &text[a.end..b.start];
    let count = gap.chars().count();
    count <= cfg.max_gap_chars && gap.chars().all(is_separator)
}

/// Remove the echoed terms the contrast transcription does not confirm.
/// Terms the contrast contains are spoken and stay exactly where they
/// are (sentence-final included); unspoken ones are cut out together
/// with the separators the echo chained them with.
pub fn clean_with_contrast(text: &str, run: &EchoRun, contrast: &str) -> EchoCleaned {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for term in &run.terms {
        if folded_contains(contrast, term) {
            kept.push(term.clone());
        } else {
            removed.push(term.clone());
        }
    }
    if removed.is_empty() {
        return EchoCleaned {
            text: text.to_string(),
            removed,
            kept,
        };
    }

    // Locate each removed term inside the run span and cut it plus its
    // trailing separator run, so chains do not leave dangling "、、".
    let span = &text[run.start..run.end];
    let lowered_span = span.to_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new(); // span-relative
    for term in &removed {
        let needle = term.to_lowercase();
        let mut from = 0;
        while let Some(found) = lowered_span[from..].find(&needle) {
            let start = from + found;
            let end = start + needle.len();
            let mut cut_end = end;
            while let Some(c) = span[cut_end..].chars().next() {
                if is_separator(c) {
                    cut_end += c.len_utf8();
                } else {
                    break;
                }
            }
            cuts.push((start, cut_end));
            from = end;
        }
    }
    cuts.sort_unstable();

    let mut cleaned_span = String::with_capacity(span.len());
    let mut cursor = 0;
    for (start, end) in cuts {
        if start >= cursor {
            cleaned_span.push_str(&span[cursor..start]);
            cursor = cursor.max(end);
        }
    }
    cleaned_span.push_str(&span[cursor..]);
    // A kept term that now ends the span must not keep the separator that
    // chained it to the removed tail.
    let cleaned_span = cleaned_span.trim_end_matches(is_separator).to_string();

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..run.start]);
    out.push_str(cleaned_span.trim_start_matches(is_separator));
    out.push_str(&text[run.end..]);

    // Join tidy: when the left side ends mid-sentence and content follows,
    // one separator reads better than nothing; when the left side ended a
    // sentence, whatever remains starts clean.
    let left_open = out[..run.start]
        .chars()
        .next_back()
        .map(|c| {
            !matches!(c, '。' | '！' | '？' | '!' | '?' | '\n' | '；' | ';') && !is_separator(c)
        })
        .unwrap_or(false);
    if left_open {
        let right_len = out.len() - run.start;
        let right = out.split_off(run.start);
        out.push('，');
        out.push_str(right.trim_start_matches(is_separator));
        let _ = right_len;
    }
    let mut text = out.trim_end().to_string();
    // A wholesale removal can butt sentence punctuation together
    // ("。嗯。" + "。"); collapse runs of one identical mark to a single one.
    for mark in ['。', '！', '？', '!', '?'] {
        let doubled: String = std::iter::repeat_n(mark, 2).collect();
        while text.contains(&doubled) {
            text = text.replace(&doubled, &mark.to_string());
        }
    }
    // An echo hides behind a hesitation; once its run is gone the stranded
    // trailing "嗯。" goes too.
    for filler in ["嗯。", "呃。", "啊。", "哦。"] {
        if let Some(stripped) = text.strip_suffix(filler) {
            let boundary = stripped
                .chars()
                .next_back()
                .map(|c| c.is_whitespace() || matches!(c, '。' | '！' | '？' | '\n'))
                .unwrap_or(true);
            if boundary {
                text = stripped.to_string();
            }
            break;
        }
    }
    EchoCleaned {
        text: text.trim().to_string(),
        removed,
        kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_chained_echo_after_sentence_end() {
        let text = "我们下周三开会讨论预算。嗯。Lumen、Tauri、sherpa-onnx。";
        let run = detect_term_echo(text, &terms(&["Lumen", "Tauri", "sherpa-onnx"]))
            .expect("echo run detected");
        assert_eq!(run.terms.len(), 3);
        assert!(text[run.start..run.end].contains("Lumen"));
        assert!(!text[..run.start].ends_with("预算"));
    }

    #[test]
    fn detects_echo_with_ascii_separators_at_text_start() {
        let text = "Kubernetes, CPU, scale out。";
        let run = detect_term_echo(text, &terms(&["Kubernetes", "CPU", "scale out"]))
            .expect("run at text start is still an echo signature");
        assert_eq!(run.terms.len(), 3);
    }

    #[test]
    fn mixed_script_terms_chain_detected() {
        let text = "开会讨论预算。嗯。sherpa-onnx、欧阳明远、Qwen3-ASR。";
        let run = detect_term_echo(
            text,
            &terms(&["sherpa-onnx", "欧阳明远", "Qwen3-ASR", "Lumen"]),
        )
        .expect("mixed-script chain");
        assert_eq!(run.terms.len(), 3);
        assert!(!run.terms.contains(&"Lumen".to_string()));
    }

    #[test]
    fn recited_list_inside_a_sentence_is_not_echo() {
        // "和" is a content char between terms: not a pure separator chain,
        // and the run is not at a sentence boundary.
        let text = "我们的术语表里有 sherpa-onnx、diarization 和 EditFlow。";
        assert!(
            detect_term_echo(text, &terms(&["sherpa-onnx", "diarization", "EditFlow"])).is_none()
        );
    }

    #[test]
    fn mid_sentence_natural_mention_is_not_echo() {
        let text = "我用 Kubernetes 和 CPU 谈了 scale out 的事";
        assert!(detect_term_echo(text, &terms(&["Kubernetes", "CPU", "scale out"])).is_none());
    }

    #[test]
    fn single_term_or_no_terms_is_not_echo() {
        assert!(detect_term_echo("用了 Lumen。", &terms(&["Lumen"])).is_none());
        assert!(detect_term_echo("随便说点什么", &terms(&["Lumen", "Tauri"])).is_none());
        // One-char terms are ignored entirely.
        assert!(detect_term_echo("。A、B。", &terms(&["A", "B"])).is_none());
    }

    #[test]
    fn sentence_final_spoken_term_survives_cleaning() {
        let text = "我们下周三开会讨论预算。嗯。Lumen、Tauri、sherpa-onnx。";
        let run = detect_term_echo(text, &terms(&["Lumen", "Tauri", "sherpa-onnx"])).unwrap();
        // The user really said Tauri (and only that) in the contrast pass.
        let cleaned = clean_with_contrast(text, &run, "我们下周三开会讨论预算，用了 Tauri 记录");
        // The spoken term stays where it was said; cleaning never invents
        // text — the contrast only decides which terms survive.
        assert_eq!(cleaned.text, "我们下周三开会讨论预算。嗯。Tauri。");
        assert!(
            cleaned.text.contains("Tauri"),
            "spoken term survives: {}",
            cleaned.text
        );
        assert!(!cleaned.text.contains("Lumen"));
        assert!(!cleaned.text.contains("sherpa-onnx"));
        assert_eq!(cleaned.removed.len(), 2);
        assert_eq!(cleaned.kept, vec!["Tauri".to_string()]);
    }

    #[test]
    fn fully_unspoken_echo_run_is_removed_wholesale() {
        let text = "下午三点开会。嗯。Lumen、Tauri。";
        let run = detect_term_echo(text, &terms(&["Lumen", "Tauri"])).unwrap();
        let cleaned = clean_with_contrast(text, &run, "下午三点开会");
        assert_eq!(cleaned.text, "下午三点开会。");
        assert_eq!(cleaned.removed.len(), 2);
    }

    #[test]
    fn fully_spoken_run_is_untouched() {
        let text = "演示开始。Lumen、Tauri。";
        let run = detect_term_echo(text, &terms(&["Lumen", "Tauri"])).unwrap();
        let cleaned = clean_with_contrast(text, &run, "演示开始 Lumen Tauri");
        assert_eq!(cleaned.text, text);
        assert!(cleaned.removed.is_empty());
        assert_eq!(cleaned.kept.len(), 2);
    }

    #[test]
    fn folded_contrast_matches_case_and_width() {
        // Detection matches ASCII-case-insensitively on the transcript;
        // the CONTRAST side folds fullwidth and case, because a different
        // engine may transcribe the same speech in either width.
        let text = "结束。Kubernetes、CPU。";
        let run = detect_term_echo(text, &terms(&["Kubernetes", "CPU"])).unwrap();
        let cleaned = clean_with_contrast(text, &run, "结束，聊了 ｋｕｂｅｒｎｅｔｅｓ 和 ＣＰＵ");
        assert_eq!(cleaned.text, text, "both folded-contrast terms are spoken");
        assert!(cleaned.removed.is_empty());
    }
}
