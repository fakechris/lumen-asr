//! Lightweight text normalization before the model.

use regex::Regex;
use std::sync::LazyLock;

static MULTI_SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").unwrap());
static SPACE_BEFORE_CN_PUNCT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+([，、。；：！？])").unwrap());
static SPACE_AFTER_CN_PUNCT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([，、。；：！？])\s+").unwrap());

/// Rule preprocess only — not a substitute for the model corrector.
pub fn preprocess(text: &str) -> String {
    let mut s = text.trim().to_string();
    s = MULTI_SPACE.replace_all(&s, " ").into_owned();
    s = SPACE_BEFORE_CN_PUNCT.replace_all(&s, "$1").into_owned();
    s = SPACE_AFTER_CN_PUNCT.replace_all(&s, "$1").into_owned();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_spaces_and_cn_punct() {
        assert_eq!(preprocess("  你好  ，  世界  "), "你好，世界");
    }
}
