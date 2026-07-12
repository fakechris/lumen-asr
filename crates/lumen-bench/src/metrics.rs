use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EditScore {
    pub edits: usize,
    pub reference_units: usize,
    pub rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextScore {
    pub strict: EditScore,
    pub content: EditScore,
    pub strict_exact: bool,
    pub content_exact: bool,
}

pub fn score_text(reference: &str, hypothesis: &str) -> TextScore {
    let strict_reference = normalize_strict(reference);
    let strict_hypothesis = normalize_strict(hypothesis);
    let content_reference = normalize_content(reference);
    let content_hypothesis = normalize_content(hypothesis);

    TextScore {
        strict: edit_score(&strict_reference, &strict_hypothesis),
        content: edit_score(&content_reference, &content_hypothesis),
        strict_exact: strict_reference == strict_hypothesis,
        content_exact: content_reference == content_hypothesis,
    }
}

fn edit_score(reference: &[char], hypothesis: &[char]) -> EditScore {
    let edits = levenshtein(reference, hypothesis);
    let reference_units = reference.len();
    let rate = match reference_units {
        0 if hypothesis.is_empty() => 0.0,
        0 => 1.0,
        n => edits as f64 / n as f64,
    };
    EditScore {
        edits,
        reference_units,
        rate,
    }
}

fn normalize_strict(text: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut pending_space = false;
    for c in text.trim().chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

fn normalize_content(text: &str) -> Vec<char> {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn levenshtein<T: Eq>(reference: &[T], hypothesis: &[T]) -> usize {
    if reference.is_empty() {
        return hypothesis.len();
    }
    if hypothesis.is_empty() {
        return reference.len();
    }

    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    let mut current = vec![0; hypothesis.len() + 1];
    for (i, reference_unit) in reference.iter().enumerate() {
        current[0] = i + 1;
        for (j, hypothesis_unit) in hypothesis.iter().enumerate() {
            let substitution = previous[j] + usize::from(reference_unit != hypothesis_unit);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[hypothesis.len()]
}

#[cfg(test)]
mod tests {
    use super::score_text;

    #[test]
    fn content_cer_ignores_case_spacing_and_punctuation() {
        let score = score_text("你好，Lumen ASR！", "你好 lumen-asr");

        assert_eq!(score.strict.edits, 3);
        assert_eq!(score.content.edits, 0);
        assert!(score.content_exact);
        assert!(!score.strict_exact);
    }

    #[test]
    fn insertions_can_make_cer_greater_than_one() {
        let score = score_text("你", "你好世界");

        assert_eq!(score.content.edits, 3);
        assert_eq!(score.content.reference_units, 1);
        assert_eq!(score.content.rate, 3.0);
    }
}
