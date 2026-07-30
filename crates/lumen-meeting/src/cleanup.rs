//! Batched LLM cleanup of the verbatim meeting transcript (M4a, feedback #5).
//!
//! The post-ASR dictionary pass ([`correct`](crate::correct)) only repairs
//! near-miss names/jargon; the transcript still carries filler words, ragged
//! punctuation and messy Chinese/English code-switching. This module adds a
//! **conservative LLM cleanup pass** that improves whole-segment readability
//! without changing meaning.
//!
//! ## Batched by chunk, not per sentence
//! Diarization has already split the transcript into many short segments (each
//! with a speaker + timestamps). Sending one LLM request *per segment* would be
//! slow, expensive and starved of context, so we group **consecutive** segments
//! into a chunk (bounded by [`CLEANUP_MAX_SEGMENTS_PER_CHUNK`] and
//! [`CLEANUP_MAX_CHARS_PER_CHUNK`]) and clean a whole chunk in one call.
//!
//! ## Boundary-preserving mapping (safety first)
//! Each segment in a chunk is fed to the model behind an explicit `[[SEG n]]`
//! marker (0-based within the chunk). The model is told to clean each segment
//! **in place** and return the *same count* of segments behind the *same*
//! markers. We parse the reply back and map each cleaned segment onto its
//! original by index. If the reply's marker count or ordering does not line up
//! exactly, the whole chunk is **discarded and the original text kept** — the
//! pass never drops, reorders, merges or misattributes a segment. Segment
//! boundaries, speakers and timestamps are therefore always preserved (they live
//! on the surrounding turn/segment rows, which this pass never touches).
//!
//! ## Word-level timing (beta trade-off)
//! Cleanup edits only a segment's *text*; the per-word timings (`words`) are left
//! untouched. As with fuzzy dictionary correction (see [`correct::correct_words`]
//! (crate::correct::correct_words)), the word tokens may then lag the cleaned
//! segment text slightly, but no timing is moved so click-to-seek stays correct.
//! Re-aligning word timings to cleaned text is out of scope for beta.

use lumen_corrector::{CorrectRequest, Corrector, DictionaryContext};
use lumen_prompts::{build_transcript_cleanup_system_prompt, transcript_cleanup_user_message};

/// Max segments grouped into one cleanup chunk. Chosen so a chunk is large
/// enough to give the model context and amortise the call, but small enough that
/// a single reply is unlikely to be truncated.
pub const CLEANUP_MAX_SEGMENTS_PER_CHUNK: usize = 20;

/// Max input characters (summed over a chunk's segment texts) before a new chunk
/// is started. Keeps the reply within [`DEFAULT_TRANSCRIPT_CLEANUP_MAX_TOKENS`]
/// so it is not cut off mid-chunk (a cut-off reply just falls back to original).
pub const CLEANUP_MAX_CHARS_PER_CHUNK: usize = 1000;

/// Default output-token budget for a cleanup call. Cleaning shortens text, so a
/// budget comfortably above [`CLEANUP_MAX_CHARS_PER_CHUNK`] (plus marker
/// overhead) leaves headroom and avoids truncation.
pub const DEFAULT_TRANSCRIPT_CLEANUP_MAX_TOKENS: u32 = 2048;

/// Sampling temperature for the cleanup call: low, because the pass must be
/// conservative (repair, not rewrite).
const CLEANUP_TEMPERATURE: f32 = 0.2;

/// Outcome tallies for one [`cleanup_transcript`] run (for logging only).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupStats {
    /// Chunks that were sent to the model.
    pub chunks: usize,
    /// Chunks whose reply parsed cleanly and was written back.
    pub cleaned: usize,
    /// Chunks whose reply misaligned or failed — original text kept (safe).
    pub kept_original: usize,
    /// All-blank chunks skipped without a model call.
    pub skipped_empty: usize,
}

/// Gate for the cleanup pass: run only when the caller opted in (`enabled`) AND
/// an LLM corrector is available (`has_corrector`). Pure, so the gating decision
/// is unit-testable independently of the (diarization-gated) pipeline.
pub fn should_cleanup(enabled: bool, has_corrector: bool) -> bool {
    enabled && has_corrector
}

/// Group consecutive segment indices into chunk ranges `[start, end)`, bounded by
/// [`CLEANUP_MAX_SEGMENTS_PER_CHUNK`] and [`CLEANUP_MAX_CHARS_PER_CHUNK`].
///
/// A single segment longer than the char budget still forms its own chunk (it is
/// never split), because segment boundaries are load-bearing.
fn chunk_ranges(texts: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut count = 0usize;
    let mut chars = 0usize;
    for (i, text) in texts.iter().enumerate() {
        let len = text.chars().count();
        // Close the current chunk before adding segment `i` when adding it would
        // exceed either bound — but never emit an empty chunk (`count > 0`).
        if count > 0
            && (count + 1 > CLEANUP_MAX_SEGMENTS_PER_CHUNK
                || chars + len > CLEANUP_MAX_CHARS_PER_CHUNK)
        {
            ranges.push((start, i));
            start = i;
            count = 0;
            chars = 0;
        }
        count += 1;
        chars += len;
    }
    if start < texts.len() {
        ranges.push((start, texts.len()));
    }
    ranges
}

/// Build a chunk's model input: each segment behind a 0-based `[[SEG n]]` marker.
fn build_chunk_input(texts: &[String]) -> String {
    let mut out = String::new();
    for (i, text) in texts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("[[SEG {i}]]\n{text}"));
    }
    out
}

/// Parse a model reply into exactly `expected` cleaned segments, keyed by their
/// `[[SEG n]]` markers.
///
/// Returns `Some(cleaned)` only when the reply contains exactly `expected`
/// markers numbered `0, 1, …, expected-1` **in order**; any mismatch returns
/// `None` so the caller keeps the original text (the safe fallback). Content
/// between a marker and the next (or end) is the segment body; surrounding blank
/// lines and leaked fence / `SEGMENTS_END` trailers are stripped.
fn parse_marked_segments(raw: &str, expected: usize) -> Option<Vec<String>> {
    // (segment index, byte offset of the marker start, byte offset of content).
    let mut markers: Vec<(usize, usize, usize)> = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = raw[search..].find("[[SEG") {
        let marker_start = search + rel;
        let after = marker_start + "[[SEG".len();
        let Some(close_rel) = raw[after..].find("]]") else {
            break;
        };
        let close = after + close_rel;
        let Ok(idx) = raw[after..close].trim().parse::<usize>() else {
            // Not a `[[SEG <number>]]` marker — skip past and keep scanning.
            search = close + 2;
            continue;
        };
        let content_start = close + 2;
        markers.push((idx, marker_start, content_start));
        search = content_start;
    }

    if markers.len() != expected {
        return None;
    }
    // Markers must be exactly 0..expected, in order (no gaps, no reordering).
    if markers.iter().enumerate().any(|(k, (idx, _, _))| *idx != k) {
        return None;
    }

    let mut cleaned = Vec::with_capacity(expected);
    for k in 0..expected {
        let content_start = markers[k].2;
        let content_end = markers
            .get(k + 1)
            .map(|(_, next_marker_start, _)| *next_marker_start)
            .unwrap_or(raw.len());
        cleaned.push(sanitize_segment(&raw[content_start..content_end]));
    }
    Some(cleaned)
}

/// Trim a parsed segment body: drop surrounding blank lines and any leaked
/// code-fence or `SEGMENTS_END` trailer lines the model may have echoed.
fn sanitize_segment(content: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    let is_noise = |line: &str| {
        let t = line.trim();
        t.is_empty() || t.starts_with("```") || t == "SEGMENTS_END" || t == "SEGMENTS_BEGIN"
    };
    while lines.first().is_some_and(|l| is_noise(l)) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| is_noise(l)) {
        lines.pop();
    }
    lines.join("\n").trim().to_string()
}

/// Run the batched, boundary-preserving LLM cleanup pass over `texts` in place.
///
/// Splits `texts` into chunks, cleans each chunk in a single LLM call and writes
/// the result back only when it maps 1:1 onto the chunk's segments. Any LLM error
/// or a marker/count mismatch leaves that chunk's segments untouched, so the pass
/// is best-effort and can never fail the surrounding pipeline. `max_tokens`
/// overrides the per-call output budget ([`DEFAULT_TRANSCRIPT_CLEANUP_MAX_TOKENS`]
/// when `None`).
pub async fn cleanup_transcript(
    corrector: &dyn Corrector,
    texts: &mut [String],
    max_tokens: Option<u32>,
) -> CleanupStats {
    let mut stats = CleanupStats::default();
    for (start, end) in chunk_ranges(texts) {
        let expected = end - start;
        // An all-blank chunk has nothing to clean — skip the call entirely.
        if texts[start..end].iter().all(|t| t.trim().is_empty()) {
            stats.skipped_empty += 1;
            continue;
        }
        stats.chunks += 1;

        let block = build_chunk_input(&texts[start..end]);
        let request = CorrectRequest {
            text: transcript_cleanup_user_message(&block),
            dictionary: DictionaryContext::default(),
            context_json: None,
            system_prompt: build_transcript_cleanup_system_prompt(),
            temperature: CLEANUP_TEMPERATURE,
            max_tokens: Some(max_tokens.unwrap_or(DEFAULT_TRANSCRIPT_CLEANUP_MAX_TOKENS)),
        };

        match corrector.correct(request).await {
            Ok(result) => match parse_marked_segments(&result.text, expected) {
                Some(cleaned) => {
                    for (offset, text) in cleaned.into_iter().enumerate() {
                        texts[start + offset] = text;
                    }
                    stats.cleaned += 1;
                }
                None => {
                    // Marker/count mismatch → keep the whole chunk's original text.
                    tracing::warn!(
                        chunk_start = start,
                        chunk_len = expected,
                        "transcript cleanup reply misaligned; keeping original text"
                    );
                    stats.kept_original += 1;
                }
            },
            Err(e) => {
                tracing::warn!(
                    chunk_start = start,
                    chunk_len = expected,
                    error = %e,
                    "transcript cleanup llm call failed; keeping original text"
                );
                stats.kept_original += 1;
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lumen_core::CorrectorEngineId;
    use lumen_corrector::{CorrectResult, CorrectorError};
    use std::sync::Mutex;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Count only *numeric* `[[SEG n]]` markers, ignoring the literal `[[SEG n]]`
    /// the prompt wrapper mentions in its instructions.
    fn numeric_seg_count(s: &str) -> usize {
        let mut n = 0;
        let mut search = 0;
        while let Some(rel) = s[search..].find("[[SEG") {
            let after = search + rel + "[[SEG".len();
            let Some(cr) = s[after..].find("]]") else {
                break;
            };
            let close = after + cr;
            if s[after..close].trim().parse::<usize>().is_ok() {
                n += 1;
            }
            search = close + 2;
        }
        n
    }

    // ── pure chunking / parsing ──────────────────────────────────────

    #[test]
    fn should_cleanup_needs_both_switch_and_corrector() {
        assert!(should_cleanup(true, true));
        assert!(!should_cleanup(true, false));
        assert!(!should_cleanup(false, true));
        assert!(!should_cleanup(false, false));
    }

    #[test]
    fn chunk_ranges_split_by_segment_count() {
        // 45 tiny segments, cap 20 → 20 + 20 + 5.
        let texts: Vec<String> = (0..45).map(|i| format!("s{i}")).collect();
        assert_eq!(chunk_ranges(&texts), vec![(0, 20), (20, 40), (40, 45)]);
    }

    #[test]
    fn chunk_ranges_split_by_char_budget() {
        // Each segment ~600 chars; two fit (1200 > 1000 forces a split after 1).
        let big = "字".repeat(600);
        let texts = vec![big.clone(), big.clone(), big];
        assert_eq!(chunk_ranges(&texts), vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn chunk_ranges_oversized_single_segment_is_its_own_chunk() {
        let huge = "字".repeat(5_000);
        let texts = vec!["short".to_string(), huge, "tail".to_string()];
        // huge exceeds the budget alone; it still forms one chunk, never split.
        assert_eq!(chunk_ranges(&texts), vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn build_and_parse_round_trip_maps_by_marker() {
        let texts = v(&["嗯 你好", "", "世界"]);
        let block = build_chunk_input(&texts);
        assert!(block.contains("[[SEG 0]]"));
        assert!(block.contains("[[SEG 2]]"));
        // A well-formed reply parses back to exactly the segment bodies.
        let reply = "[[SEG 0]]\n你好\n[[SEG 1]]\n\n[[SEG 2]]\n世界";
        let parsed = parse_marked_segments(reply, 3).unwrap();
        assert_eq!(parsed, v(&["你好", "", "世界"]));
    }

    #[test]
    fn parse_rejects_wrong_count_and_reordering() {
        // Too few markers.
        assert!(parse_marked_segments("[[SEG 0]]\na", 2).is_none());
        // Too many markers.
        assert!(parse_marked_segments("[[SEG 0]]\na\n[[SEG 1]]\nb\n[[SEG 2]]\nc", 2).is_none());
        // Right count but out of order (0,2 not 0,1).
        assert!(parse_marked_segments("[[SEG 0]]\na\n[[SEG 2]]\nb", 2).is_none());
    }

    #[test]
    fn parse_strips_leaked_fences_and_end_markers() {
        let reply = "```\n[[SEG 0]]\n你好\n[[SEG 1]]\n世界\nSEGMENTS_END\n```";
        let parsed = parse_marked_segments(reply, 2).unwrap();
        assert_eq!(parsed, v(&["你好", "世界"]));
    }

    #[test]
    fn parse_tolerates_leading_prose_and_missing_space() {
        // Leading chatter before the first marker is ignored; `[[SEG0]]` (no
        // space) still parses.
        let reply = "好的：\n[[SEG0]]\n你好\n[[SEG1]]\n世界";
        let parsed = parse_marked_segments(reply, 2).unwrap();
        assert_eq!(parsed, v(&["你好", "世界"]));
    }

    // ── batched cleanup with mock correctors ─────────────────────────

    /// Records every request text, and replies by rebuilding one `[[SEG k]]`
    /// segment per marker it saw — so a test can assert both batching (one call
    /// per chunk, many markers per call) and correct write-back mapping.
    struct RebuildingCorrector {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Corrector for RebuildingCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            let n = numeric_seg_count(&req.text);
            self.seen.lock().unwrap().push(req.text.clone());
            let mut out = String::new();
            for k in 0..n {
                out.push_str(&format!("[[SEG {k}]]\nclean{k}\n"));
            }
            Ok(CorrectResult {
                text: out,
                engine: CorrectorEngineId::OpenAiCompatible,
                model_applied: true,
                fallback_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn cleans_a_whole_chunk_in_one_batched_call() {
        let corrector = RebuildingCorrector {
            seen: Mutex::new(Vec::new()),
        };
        let mut texts = v(&["嗯 a", "呃 b", "c", "d", "e"]);
        let stats = cleanup_transcript(&corrector, &mut texts, None).await;

        // One chunk → one call carrying all five segment markers (batched, not
        // one call per sentence).
        let seen = corrector.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "should be a single batched call");
        assert_eq!(numeric_seg_count(&seen[0]), 5);
        // Write-back maps each cleaned segment onto its original position.
        assert_eq!(
            texts,
            v(&["clean0", "clean1", "clean2", "clean3", "clean4"])
        );
        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.cleaned, 1);
        assert_eq!(stats.kept_original, 0);
    }

    #[tokio::test]
    async fn multiple_chunks_each_get_their_own_call() {
        let corrector = RebuildingCorrector {
            seen: Mutex::new(Vec::new()),
        };
        // 25 short segments → two chunks (20 + 5).
        let mut texts: Vec<String> = (0..25).map(|i| format!("s{i}")).collect();
        cleanup_transcript(&corrector, &mut texts, None).await;
        let seen = corrector.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(numeric_seg_count(&seen[0]), 20);
        assert_eq!(numeric_seg_count(&seen[1]), 5);
    }

    /// Replies with the wrong number of segments — forces the safe fallback.
    struct MisalignedCorrector;

    #[async_trait]
    impl Corrector for MisalignedCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, _req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            // Merged two segments into one → count mismatch.
            Ok(CorrectResult {
                text: "[[SEG 0]]\nmerged everything".into(),
                engine: CorrectorEngineId::OpenAiCompatible,
                model_applied: true,
                fallback_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn misaligned_reply_keeps_original_text() {
        let corrector = MisalignedCorrector;
        let original = v(&["嗯 一", "呃 二"]);
        let mut texts = original.clone();
        let stats = cleanup_transcript(&corrector, &mut texts, None).await;
        assert_eq!(texts, original, "original text preserved on mismatch");
        assert_eq!(stats.cleaned, 0);
        assert_eq!(stats.kept_original, 1);
    }

    /// Always errors — the failure must fall back to original text, not panic.
    struct FailingCorrector;

    #[async_trait]
    impl Corrector for FailingCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, _req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            Err(CorrectorError::Timeout)
        }
    }

    #[tokio::test]
    async fn llm_failure_keeps_original_text() {
        let original = v(&["嗯 一", "呃 二"]);
        let mut texts = original.clone();
        let stats = cleanup_transcript(&FailingCorrector, &mut texts, None).await;
        assert_eq!(texts, original);
        assert_eq!(stats.kept_original, 1);
        assert_eq!(stats.cleaned, 0);
    }

    #[tokio::test]
    async fn all_blank_chunk_is_skipped_without_a_call() {
        let corrector = RebuildingCorrector {
            seen: Mutex::new(Vec::new()),
        };
        let mut texts = v(&["", "   ", ""]);
        let stats = cleanup_transcript(&corrector, &mut texts, None).await;
        assert!(
            corrector.seen.lock().unwrap().is_empty(),
            "no call for blank chunk"
        );
        assert_eq!(stats.skipped_empty, 1);
        assert_eq!(stats.chunks, 0);
    }

    #[tokio::test]
    async fn empty_transcript_is_a_noop() {
        let corrector = RebuildingCorrector {
            seen: Mutex::new(Vec::new()),
        };
        let mut texts: Vec<String> = Vec::new();
        let stats = cleanup_transcript(&corrector, &mut texts, None).await;
        assert!(corrector.seen.lock().unwrap().is_empty());
        assert_eq!(stats, CleanupStats::default());
    }
}
