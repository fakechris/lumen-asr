//! Text canonicalization for scoring.
//!
//! Both the reference and the hypothesis go through the same pipeline, so
//! differences that are irrelevant to transcription quality — punctuation,
//! fullwidth/halfwidth, case, Chinese numerals vs digits ("三百五十" vs
//! "350"), English number words ("twelve" vs 12), standalone fillers — do
//! not count as errors. What is left is a canonical string used for
//! substring checks (`must` / `must_not` / terms) and for tokenization.

/// Standalone fillers dropped from both sides during scoring only. Cleanup
/// correctness is enforced per-clip through the corpus `must_not` fields,
/// not through this list. Longest first when stripping so "就是说" wins
/// over shorter prefixes.
const CJK_FILLERS: &[&str] = &["就是说", "嗯", "呃", "啊", "哦", "唉"];
const LATIN_FILLERS: &[&str] = &["um", "uh", "er", "ah", "like", "basically"];

/// Chinese numeral characters recognized by [`parse_chinese_numeral`].
const NUMERAL_DIGITS: &[(char, u32)] = &[
    ('零', 0),
    ('一', 1),
    ('二', 2),
    ('两', 2),
    ('三', 3),
    ('四', 4),
    ('五', 5),
    ('六', 6),
    ('七', 7),
    ('八', 8),
    ('九', 9),
];

fn digit_value(c: char) -> Option<u32> {
    NUMERAL_DIGITS
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| *v)
}

fn unit_value(c: char) -> Option<u64> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1_000),
        _ => None,
    }
}

fn is_numeral_char(c: char) -> bool {
    digit_value(c).is_some() || unit_value(c).is_some()
}

/// Parse a short Chinese numeral run ("三百五十", "十五", "一百零五",
/// "五千三百二十") into a number. Returns `None` for anything the simple
/// section algorithm cannot read; the caller then leaves the run
/// untouched.
fn parse_chinese_numeral(run: &str) -> Option<u64> {
    if run.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut section: u64 = 0;
    let mut cur: u32 = 0;
    let mut saw_numeral = false;
    let mut leading = true;
    for c in run.chars() {
        if let Some(d) = digit_value(c) {
            cur = d;
            saw_numeral = true;
        } else if let Some(unit) = unit_value(c) {
            match unit {
                10 => {
                    // Bare leading 十 means ten.
                    let base = if cur == 0 && section == 0 && total == 0 && leading {
                        1
                    } else {
                        cur
                    };
                    section += base as u64 * 10;
                    cur = 0;
                }
                100 => {
                    section = (section + cur as u64) * 100;
                    cur = 0;
                }
                _ => {
                    // 千 completes the current section.
                    total += (section + cur as u64) * 1_000;
                    section = 0;
                    cur = 0;
                }
            }
            saw_numeral = true;
            leading = false;
        } else {
            return None;
        }
    }
    if !saw_numeral {
        return None;
    }
    Some(total + section + cur as u64)
}

/// Replace every Chinese numeral run (at most four numeral characters) with
/// its Arabic form when it parses. Longer runs are left alone — likely an
/// id or a list, not a number.
fn replace_chinese_numerals(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if is_numeral_char(chars[i]) {
            let start = i;
            // A continuous run is parsed as one unit (五千三百二十 → 5320);
            // unparseable longer runs keep the leave-as-is fallback.
            while i < chars.len() && is_numeral_char(chars[i]) && i - start < 12 {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            match parse_chinese_numeral(&run) {
                Some(v) => out.push_str(&v.to_string()),
                None => out.push_str(&run),
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// English number words replaced token-wise. Compound numbers
/// ("twenty-five") are out of scope; single words cover the corpus.
fn english_number_word(token: &str) -> Option<&'static str> {
    Some(match token {
        "zero" => "0",
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "ten" => "10",
        "eleven" => "11",
        "twelve" => "12",
        "thirteen" => "13",
        "fourteen" => "14",
        "fifteen" => "15",
        "sixteen" => "16",
        "seventeen" => "17",
        "eighteen" => "18",
        "nineteen" => "19",
        "twenty" => "20",
        "thirty" => "30",
        "forty" => "40",
        "fifty" => "50",
        "sixty" => "60",
        "seventy" => "70",
        "eighty" => "80",
        "ninety" => "90",
        "hundred" => "100",
        _ => return None,
    })
}

/// Fullwidth ASCII (U+FF01–U+FF5E) → its ASCII counterpart.
fn map_fullwidth(c: char) -> char {
    let cp = c as u32;
    if (0xFF01..=0xFF5E).contains(&cp) {
        char::from_u32(cp - 0xFEE0).unwrap_or(c)
    } else {
        c
    }
}

fn is_keeper(c: char) -> bool {
    c.is_alphanumeric() || c == '-'
}

/// True for CJK ideographs, used by tokenization and the language-share
/// check.
pub fn is_cjk_letter(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{F900}'..='\u{FAFF}').contains(&c)
}

/// Canonicalize `text` for scoring:
///
/// 1. fullwidth → ASCII, lowercase;
/// 2. CJK filler substrings and standalone Latin fillers dropped;
/// 3. Chinese numeral runs → digits, English number words → digits;
/// 4. punctuation dropped (letters, digits, CJK, `-` kept);
/// 5. the result keeps original order: CJK characters concatenate, two
///    adjacent Latin runs are separated by one space.
pub fn normalize_for_scoring(text: &str) -> String {
    normalize_inner(text, true)
}

/// Same canonicalization without filler removal. Cleanup checks run on
/// this form: a filler that survived the pipeline must stay visible, and a
/// filler stripped from the scoring view must not make a `must_not` phrase
/// vanish into an always-true containment.
pub fn normalize_keep_fillers(text: &str) -> String {
    normalize_inner(text, false)
}

fn normalize_inner(text: &str, strip_fillers: bool) -> String {
    let lowered: String = text
        .chars()
        .map(map_fullwidth)
        .collect::<String>()
        .to_lowercase();
    let mut s = lowered;
    if strip_fillers {
        let mut fillers: Vec<&str> = CJK_FILLERS.to_vec();
        fillers.sort_by_key(|f| std::cmp::Reverse(f.chars().count()));
        for filler in fillers {
            s = s.replace(filler, "");
        }
    }
    let with_digits = replace_chinese_numerals(&s);

    // Atomize in order: single CJK chars, or runs of Latin/digit/hyphen.
    let mut atoms: Vec<String> = Vec::new();
    let mut latin = String::new();
    for c in with_digits.chars() {
        if !is_keeper(c) {
            if !latin.is_empty() {
                atoms.push(std::mem::take(&mut latin));
            }
            continue;
        }
        if is_cjk_letter(c) {
            if !latin.is_empty() {
                atoms.push(std::mem::take(&mut latin));
            }
            atoms.push(c.to_string());
        } else {
            latin.push(c);
        }
    }
    if !latin.is_empty() {
        atoms.push(latin);
    }

    // Number words are standalone Latin atoms by now.
    let atoms: Vec<String> = atoms
        .into_iter()
        .map(|a| match english_number_word(&a) {
            Some(digits) => digits.to_string(),
            None => a,
        })
        .filter(|a| !strip_fillers || !LATIN_FILLERS.contains(&a.as_str()))
        .collect();

    // Join: a space only between two adjacent Latin atoms.
    let mut out = String::with_capacity(s.len());
    let mut prev_is_latin = false;
    for atom in atoms {
        let atom_is_latin = atom
            .chars()
            .next()
            .map(|c| !is_cjk_letter(c))
            .unwrap_or(false);
        if !out.is_empty() && prev_is_latin && atom_is_latin {
            out.push(' ');
        }
        out.push_str(&atom);
        prev_is_latin = atom_is_latin;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_numerals_parse() {
        assert_eq!(parse_chinese_numeral("十"), Some(10));
        assert_eq!(parse_chinese_numeral("十五"), Some(15));
        assert_eq!(parse_chinese_numeral("二十五"), Some(25));
        assert_eq!(parse_chinese_numeral("一百"), Some(100));
        assert_eq!(parse_chinese_numeral("一百零五"), Some(105));
        assert_eq!(parse_chinese_numeral("三百五十"), Some(350));
        assert_eq!(parse_chinese_numeral("五百"), Some(500));
        assert_eq!(parse_chinese_numeral("两千"), Some(2_000));
        assert_eq!(parse_chinese_numeral("五千三百二十"), Some(5_320));
        assert_eq!(parse_chinese_numeral("二千零一十"), Some(2_010));
        assert_eq!(parse_chinese_numeral("三十二"), Some(32));
        assert_eq!(parse_chinese_numeral("不是数字x"), None);
    }

    #[test]
    fn normalization_handles_corpus_shapes() {
        // Punctuation, fillers, and width do not count.
        assert_eq!(
            normalize_for_scoring("嗯，我们下周三！"),
            normalize_for_scoring("我们下周三")
        );
        // Chinese numerals become digits on both sides.
        assert_eq!(
            normalize_for_scoring("内存是三十二G"),
            normalize_for_scoring("内存是32g")
        );
        assert_eq!(
            normalize_for_scoring("超时改成五百毫秒"),
            normalize_for_scoring("超时改成500毫秒")
        );
        // English number words become digits.
        assert_eq!(
            normalize_for_scoring("It costs twelve dollars"),
            normalize_for_scoring("it costs 12 dollars")
        );
        // Hyphenated terms survive as one token.
        assert!(normalize_for_scoring("Qwen3-ASR 跑在 sherpa-onnx 上").contains("sherpa-onnx"));
        // Fullwidth ASCII folds.
        assert_eq!(normalize_for_scoring("ＡＰＩ"), "api");
        // Latin fillers dropped from both sides; "liked" is not "like".
        assert_eq!(
            normalize_for_scoring("Um, I think"),
            normalize_for_scoring("I think")
        );
        assert!(normalize_for_scoring("I liked it").contains("liked"));
        // Order preserved on mixed-script input.
        assert_eq!(
            normalize_for_scoring("我们的服务用 Kubernetes 部署"),
            "我们的服务用kubernetes部署"
        );
    }
}
