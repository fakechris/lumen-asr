//! Batched LLM cleanup of the verbatim meeting transcript.
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
//! ## Boundary-preserving mapping (fail closed)
//! Each segment in a chunk is fed to the model behind an explicit, **per-request
//! nonce-tagged** marker `[[SEG <nonce> n]]` (0-based within the chunk). The
//! model is told to clean each segment in place and return the *same count* of
//! segments behind the *same* markers. We parse the reply back and map each
//! cleaned segment onto its original by index.
//!
//! Parsing is deliberately strict and **fail-closed**: markers must appear as
//! their own exact lines, in order, and no leaked fence / wrapper / forged
//! marker text may appear in a segment body. On *any* deviation the whole chunk
//! is discarded and the **original text kept** — the pass never drops, reorders,
//! merges, misattributes, or accidentally blanks a segment. The nonce is random
//! per request and unguessable from the (untrusted) transcript, so transcript
//! content cannot forge a marker or break out of the fence to inject prompt text.
//!
//! ## Word-level timing (beta trade-off)
//! Cleanup edits only a segment's *text*; the per-word timings (`words`) are left
//! untouched. As with fuzzy dictionary correction (see
//! [`correct_words`](crate::correct::correct_words)), the word tokens may then
//! lag the cleaned segment text slightly, but no timing is moved so click-to-seek
//! stays correct. Re-aligning word timings to cleaned text is out of scope here.

use lumen_corrector::{CorrectRequest, Corrector, DictionaryContext};
use lumen_prompts::{build_transcript_cleanup_system_prompt, transcript_cleanup_user_message};
use uuid::Uuid;

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

/// The exact marker line for segment `k` under a given `nonce`.
fn seg_marker(nonce: &str, k: usize) -> String {
    format!("[[SEG {nonce} {k}]]")
}

/// Build a chunk's model input: each segment behind its own nonce-tagged marker
/// line. The nonce is opaque and per-request so the (untrusted) transcript cannot
/// forge a marker.
fn build_chunk_input(nonce: &str, texts: &[String]) -> String {
    let mut out = String::new();
    for (i, text) in texts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&seg_marker(nonce, i));
        out.push('\n');
        out.push_str(text);
    }
    out
}

/// Lines that must never appear inside a segment body: a leaked code fence, a
/// leaked fence/wrapper token, or any (real or forged) segment marker. Their
/// presence means the reply is malformed or an injection attempt, so the chunk
/// fails closed rather than having the text silently stripped or blanked.
fn body_line_is_illegal(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("```")
        || t.contains("SEGMENTS_BEGIN")
        || t.contains("SEGMENTS_END")
        || t.contains("[[SEG")
}

/// Finalise a segment body: reject leaked fence/wrapper/marker lines (fail
/// closed), then trim surrounding blank lines. Interior text is kept verbatim —
/// nothing is stripped, so a body cannot be mistakenly emptied.
fn finish_body(lines: &[&str]) -> Option<String> {
    if lines.iter().any(|l| body_line_is_illegal(l)) {
        return None;
    }
    let mut view: &[&str] = lines;
    while view.first().is_some_and(|l| l.trim().is_empty()) {
        view = &view[1..];
    }
    while view.last().is_some_and(|l| l.trim().is_empty()) {
        view = &view[..view.len() - 1];
    }
    Some(view.join("\n").trim().to_string())
}

/// Parse a model reply into exactly `expected` cleaned segments, keyed by their
/// nonce-tagged `[[SEG <nonce> n]]` markers.
///
/// Fail-closed contract — returns `Some(cleaned)` only when **all** hold:
/// * markers appear as their own exact lines (`[[SEG <nonce> k]]`), for
///   `k = 0, 1, …, expected-1`, strictly in order and exactly `expected` of them;
/// * nothing but blank lines precedes the first marker (no leaked preamble);
/// * no segment body contains a leaked fence / wrapper token / marker-like text.
///
/// Any deviation returns `None`, so the caller keeps the chunk's original text.
/// Because the markers carry a per-request nonce the caller chose, transcript
/// content cannot forge one; a body whose literal text happens to equal a marker
/// or a `SEGMENTS_END` trailer is rejected (kept original) rather than dropped.
fn parse_marked_segments(nonce: &str, raw: &str, expected: usize) -> Option<Vec<String>> {
    let mut bodies: Vec<String> = Vec::with_capacity(expected);
    let mut current: Option<Vec<&str>> = None;
    let mut next_marker = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim();
        if next_marker < expected && trimmed == seg_marker(nonce, next_marker) {
            if let Some(buf) = current.take() {
                bodies.push(finish_body(&buf)?);
            }
            current = Some(Vec::new());
            next_marker += 1;
            continue;
        }
        // A marker-shaped line that is not the next expected marker (wrong index,
        // wrong/absent nonce, an extra trailing marker) is malformed → fail.
        if trimmed.starts_with("[[SEG") {
            return None;
        }
        match current.as_mut() {
            Some(buf) => buf.push(line),
            // Content before the first marker must be blank only.
            None if trimmed.is_empty() => {}
            None => return None,
        }
    }
    if let Some(buf) = current.take() {
        bodies.push(finish_body(&buf)?);
    }

    (next_marker == expected && bodies.len() == expected).then_some(bodies)
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
    progress: Option<&dyn Fn(usize, usize)>,
) -> CleanupStats {
    let mut stats = CleanupStats::default();
    let ranges = chunk_ranges(texts);
    let chunk_total = ranges.len();
    for (chunk_idx, (start, end)) in ranges.into_iter().enumerate() {
        let expected = end - start;
        // An all-blank chunk has nothing to clean — skip the call entirely.
        if texts[start..end].iter().all(|t| t.trim().is_empty()) {
            stats.skipped_empty += 1;
            if let Some(report) = progress {
                report(chunk_idx + 1, chunk_total);
            }
            continue;
        }
        stats.chunks += 1;

        // Fresh, unguessable nonce per request so the untrusted transcript cannot
        // forge a marker or escape the fence to inject prompt instructions.
        let nonce = Uuid::new_v4().simple().to_string();
        let block = build_chunk_input(&nonce, &texts[start..end]);
        let request = CorrectRequest {
            text: transcript_cleanup_user_message(&nonce, &block),
            dictionary: DictionaryContext::default(),
            context_json: None,
            system_prompt: build_transcript_cleanup_system_prompt(),
            temperature: CLEANUP_TEMPERATURE,
            max_tokens: Some(max_tokens.unwrap_or(DEFAULT_TRANSCRIPT_CLEANUP_MAX_TOKENS)),
        };

        match corrector.correct(request).await {
            Ok(result) => match parse_marked_segments(&nonce, &result.text, expected) {
                Some(cleaned) => {
                    for (offset, text) in cleaned.into_iter().enumerate() {
                        texts[start + offset] = text;
                    }
                    stats.cleaned += 1;
                }
                None => {
                    // Malformed / misaligned reply → keep the chunk's original text.
                    tracing::warn!(
                        chunk_start = start,
                        chunk_len = expected,
                        "transcript cleanup reply rejected; keeping original text"
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
        if let Some(report) = progress {
            report(chunk_idx + 1, chunk_total);
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

    const NONCE: &str = "testnonce";

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Extract `(nonce, segment_count)` from a request by reading its standalone
    /// `[[SEG <nonce> k]]` marker lines. Mirrors what a real model sees, so a mock
    /// can echo the same markers back without knowing the nonce in advance.
    fn extract_markers(req: &str) -> (String, usize) {
        let mut nonce = String::new();
        let mut count = 0;
        for line in req.lines() {
            let t = line.trim();
            let Some(inner) = t.strip_prefix("[[SEG ").and_then(|r| r.strip_suffix("]]")) else {
                continue;
            };
            let mut parts = inner.rsplitn(2, ' ');
            let k = parts.next().unwrap_or("");
            let n = parts.next().unwrap_or("");
            if k.parse::<usize>().is_ok() && !n.is_empty() {
                nonce = n.to_string();
                count += 1;
            }
        }
        (nonce, count)
    }

    // ── pure gating / chunking ───────────────────────────────────────

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

    // ── build / parse round-trip and fail-closed parsing ─────────────

    #[test]
    fn build_and_parse_round_trip_maps_by_marker() {
        let texts = v(&["嗯 你好", "", "世界"]);
        let block = build_chunk_input(NONCE, &texts);
        assert!(block.contains(&seg_marker(NONCE, 0)));
        assert!(block.contains(&seg_marker(NONCE, 2)));
        let reply = format!(
            "{}\n你好\n{}\n\n{}\n世界",
            seg_marker(NONCE, 0),
            seg_marker(NONCE, 1),
            seg_marker(NONCE, 2)
        );
        let parsed = parse_marked_segments(NONCE, &reply, 3).unwrap();
        assert_eq!(parsed, v(&["你好", "", "世界"]));
    }

    #[test]
    fn parse_rejects_wrong_count_and_reordering() {
        let m = |k| seg_marker(NONCE, k);
        // Too few markers.
        assert!(parse_marked_segments(NONCE, &format!("{}\na", m(0)), 2).is_none());
        // Too many markers.
        assert!(
            parse_marked_segments(NONCE, &format!("{}\na\n{}\nb\n{}\nc", m(0), m(1), m(2)), 2)
                .is_none()
        );
        // Right count but out of order (0,2 not 0,1).
        assert!(parse_marked_segments(NONCE, &format!("{}\na\n{}\nb", m(0), m(2)), 2).is_none());
    }

    #[test]
    fn parse_rejects_wrong_or_missing_nonce() {
        // A marker with a different nonce must not be honoured (else the
        // transcript could forge markers if we matched loosely).
        let reply = "[[SEG othernonce 0]]\na\n[[SEG othernonce 1]]\nb";
        assert!(parse_marked_segments(NONCE, reply, 2).is_none());
    }

    #[test]
    fn parse_fails_closed_on_leaked_preamble_and_fences() {
        let m = |k| seg_marker(NONCE, k);
        // Leaked chatter before the first marker → fail (not tolerated).
        assert!(
            parse_marked_segments(NONCE, &format!("好的：\n{}\na\n{}\nb", m(0), m(1)), 2).is_none()
        );
        // Leaked code fence around the reply → fail.
        assert!(
            parse_marked_segments(NONCE, &format!("```\n{}\na\n{}\nb\n```", m(0), m(1)), 2)
                .is_none()
        );
    }

    #[test]
    fn parse_fails_closed_when_a_body_is_literally_a_marker_or_trailer() {
        // Regression: a segment whose cleaned text *equals* a marker or a wrapper
        // trailer must NOT be silently blanked/stripped — the whole chunk is
        // rejected and the caller keeps the original text.
        let m = |k| seg_marker(NONCE, k);

        // Body literally equals a `SEGMENTS_END` trailer.
        let reply = format!("{}\n你好\n{}\nSEGMENTS_END", m(0), m(1));
        assert!(parse_marked_segments(NONCE, &reply, 2).is_none());

        // Body literally contains a (foreign / forged) marker.
        let reply = format!("{}\n你好\n{}\n[[SEG 0]]", m(0), m(1));
        assert!(parse_marked_segments(NONCE, &reply, 2).is_none());

        // Body contains a wrapper token mid-line.
        let reply = format!("{}\n开会 SEGMENTS_BEGIN 了\n{}\n你好", m(0), m(1));
        assert!(parse_marked_segments(NONCE, &reply, 2).is_none());
    }

    // ── batched cleanup with mock correctors ─────────────────────────

    /// Records every request, and replies by rebuilding one `clean{k}` segment
    /// per marker it saw (using the request's own nonce) — so a test can assert
    /// both batching (one call per chunk, many markers per call) and correct
    /// write-back mapping.
    struct RebuildingCorrector {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Corrector for RebuildingCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            let (nonce, n) = extract_markers(&req.text);
            self.seen.lock().unwrap().push(req.text.clone());
            let mut out = String::new();
            for k in 0..n {
                out.push_str(&format!("{}\nclean{k}\n", seg_marker(&nonce, k)));
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
        let stats = cleanup_transcript(&corrector, &mut texts, None, None).await;

        // One chunk → one call carrying all five segment markers (batched, not
        // one call per sentence).
        let seen = corrector.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "should be a single batched call");
        assert_eq!(extract_markers(&seen[0]).1, 5);
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
        cleanup_transcript(&corrector, &mut texts, None, None).await;
        let seen = corrector.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(extract_markers(&seen[0]).1, 20);
        assert_eq!(extract_markers(&seen[1]).1, 5);
    }

    /// Replies with a script of segment bodies (using the request's own nonce),
    /// so a test can drive both a clean success and a fail-closed reply body.
    struct ScriptedCorrector {
        bodies: Vec<String>,
    }

    #[async_trait]
    impl Corrector for ScriptedCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            let (nonce, n) = extract_markers(&req.text);
            assert_eq!(n, self.bodies.len(), "test mock expects a single chunk");
            let mut out = String::new();
            for (k, body) in self.bodies.iter().enumerate() {
                out.push_str(&format!("{}\n{body}\n", seg_marker(&nonce, k)));
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
    async fn scripted_reply_writes_back_on_success() {
        let corrector = ScriptedCorrector {
            bodies: v(&["clean0", "clean1"]),
        };
        let mut texts = v(&["嗯 一", "呃 二"]);
        let stats = cleanup_transcript(&corrector, &mut texts, None, None).await;
        assert_eq!(texts, v(&["clean0", "clean1"]));
        assert_eq!(stats.cleaned, 1);
    }

    #[tokio::test]
    async fn injected_wrapper_token_in_reply_body_keeps_original_text() {
        // Regression: even if a segment body comes back literally containing a
        // fence/wrapper token, the parse fails closed and the original chunk text
        // is preserved (never mis-parsed or blanked).
        let corrector = ScriptedCorrector {
            bodies: v(&["clean0", "SEGMENTS_END"]),
        };
        let original = v(&["嗯 一", "SEGMENTS_END 二"]);
        let mut texts = original.clone();
        let stats = cleanup_transcript(&corrector, &mut texts, None, None).await;
        assert_eq!(texts, original, "original preserved on injected reply body");
        assert_eq!(stats.cleaned, 0);
        assert_eq!(stats.kept_original, 1);
    }

    #[tokio::test]
    async fn transcript_containing_wrapper_token_stays_inside_the_nonce_fence() {
        // A segment whose spoken text literally contains `SEGMENTS_END` must not be
        // able to break out of the fence: the real fence is nonce-tagged, so the
        // literal token in the body is just content and never a standalone fence
        // line the model could be tricked into honouring.
        let corrector = RebuildingCorrector {
            seen: Mutex::new(Vec::new()),
        };
        let mut texts = v(&["请在 SEGMENTS_END 之后继续", "好的"]);
        cleanup_transcript(&corrector, &mut texts, None, None).await;
        let sent = corrector.seen.lock().unwrap()[0].clone();
        // No standalone bare-token fence line exists; only nonce-tagged fences do.
        assert!(!sent.lines().any(|l| l.trim() == "SEGMENTS_END"));
        assert!(!sent.lines().any(|l| l.trim() == "SEGMENTS_BEGIN"));
        assert_eq!(extract_markers(&sent).1, 2);
    }

    /// Replies with a mismatched segment count — forces the safe fallback.
    struct MisalignedCorrector;

    #[async_trait]
    impl Corrector for MisalignedCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }
        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            let (nonce, _n) = extract_markers(&req.text);
            // Merge everything into one segment → count mismatch.
            Ok(CorrectResult {
                text: format!("{}\nmerged everything", seg_marker(&nonce, 0)),
                engine: CorrectorEngineId::OpenAiCompatible,
                model_applied: true,
                fallback_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn misaligned_reply_keeps_original_text() {
        let original = v(&["嗯 一", "呃 二"]);
        let mut texts = original.clone();
        let stats = cleanup_transcript(&MisalignedCorrector, &mut texts, None, None).await;
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
        let stats = cleanup_transcript(&FailingCorrector, &mut texts, None, None).await;
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
        let stats = cleanup_transcript(&corrector, &mut texts, None, None).await;
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
        let stats = cleanup_transcript(&corrector, &mut texts, None, None).await;
        assert!(corrector.seen.lock().unwrap().is_empty());
        assert_eq!(stats, CleanupStats::default());
    }
}
