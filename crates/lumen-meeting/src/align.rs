//! Transcript×speaker alignment: assign ASR sentences to diarization turns by
//! **maximum time overlap**, split the rare sentence that straddles a speaker
//! change, and keep the output chronological regardless of arrival order.
//!
//! The offline pipeline transcribes each diar turn's audio slice separately, so
//! text is nominally attributed by "which slice produced it". Diarization
//! boundaries are imprecise, though: a turn's slice can contain the tail of the
//! previous speaker's sentence (or miss its own), and engine word timestamps
//! can drift past the slice edges. This module re-attributes by *time* instead
//! of by provenance:
//!
//! 1. **Max-overlap matching** — each ASR sentence goes to the diar turn it
//!    overlaps most (ties and gap-fallen sentences resolve deterministically
//!    to the earlier/nearer turn).
//! 2. **Cross-speaker splits** — a sentence whose time span covers turns of
//!    two or more speakers is cut at each speaker-change boundary. The cut
//!    position is estimated by time proportion (refined by word-level
//!    timestamps when the engine provides them: the cut lands before the word
//!    whose interval straddles the boundary), then snapped to the nearest
//!    Chinese/English punctuation mark, else to a whitespace boundary, else
//!    hard-cut at the estimated position.
//! 3. **Backfill** — input sentences may arrive out of order (late or
//!    reordered results); the output is stably sorted by start time, and
//!    [`backfill_fragment`] inserts a late fragment at its chronological
//!    position instead of appending it.
//!
//! Everything here is a pure function over sentences + turns — no I/O, no
//! models — so the whole alignment policy is unit-testable with stub data.

use lumen_transcript::Word;

use crate::assemble::DiarTurn;

/// How far (in characters, each side) the split-point estimate may travel to
/// snap onto a punctuation mark or whitespace boundary. Beyond this window the
/// snap is distrusted and the cut lands at the estimated ratio position.
pub const SNAP_WINDOW_CHARS: usize = 20;

/// One ASR sentence on the absolute media timeline.
///
/// `start`/`end` come from the sentence's word timings when available (else a
/// proportional share of its turn's extent). `words` holds the word-level
/// timings (absolute media time) the sentence was decoded from; empty when the
/// engine reports no alignment (e.g. the SenseVoice fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct AsrSentence {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub words: Vec<Word>,
}

/// A piece of an ASR sentence attributed to one diar turn. A sentence that
/// stays with a single turn yields exactly one fragment (`split: false`); a
/// cross-speaker sentence yields one fragment per spanned speaker stretch.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedFragment {
    /// Index of the source sentence in the `sentences` slice passed to
    /// [`align_sentences`] (lets a caller correlate late arrivals).
    pub sentence: usize,
    /// Index into the `turns` slice passed to [`align_sentences`].
    pub turn: usize,
    /// Engine speaker id of that turn (denormalized for convenience).
    pub speaker: u32,
    /// Fragment time extent on the absolute media timeline.
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// The sentence's words whose time/position falls inside this fragment.
    pub words: Vec<Word>,
    /// True when this fragment came from splitting a cross-speaker sentence.
    pub split: bool,
}

/// Seconds of overlap between half-open intervals `[a_start, a_end)` and
/// `[b_start, b_end)`; `0.0` for disjoint/touching intervals or non-finite
/// input (kept total so NaN timestamps degrade deterministically).
pub fn overlap_seconds(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    if !(a_start.is_finite() && a_end.is_finite() && b_start.is_finite() && b_end.is_finite()) {
        return 0.0;
    }
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

/// Index of the turn with the largest temporal overlap with `[start, end)`.
/// Strictly-greatest wins, so exact ties resolve to the earlier turn. `None`
/// when `turns` is empty or no turn overlaps at all.
fn max_overlap_turn(start: f64, end: f64, turns: &[DiarTurn]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, t) in turns.iter().enumerate() {
        let overlap = overlap_seconds(start, end, t.start, t.end);
        if overlap > 0.0 && best.is_none_or(|(_, b)| overlap > b) {
            best = Some((i, overlap));
        }
    }
    best.map(|(i, _)| i)
}

/// Index of the turn nearest in time to `[start, end)` — the fallback for
/// sentences that overlap no turn (they landed in a silence gap, or have a
/// zero-length extent). Gap 0 means touching/inside; ties resolve to the
/// earlier turn. Non-finite sentence times compare equal-everywhere and thus
/// deterministically pick turn 0.
fn nearest_turn(start: f64, end: f64, turns: &[DiarTurn]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, t) in turns.iter().enumerate() {
        let gap = if start >= t.end {
            start - t.end
        } else if end <= t.start {
            t.start - end
        } else {
            0.0
        };
        if gap.is_finite() && best.is_none_or(|(_, g)| gap < g) {
            best = Some((i, gap));
        }
    }
    // Only reachable with a non-finite gap at every turn (NaN input): turn 0.
    best.map(|(i, _)| i)
        .or_else(|| (!turns.is_empty()).then_some(0))
}

/// True when `chars[i]` ends a sentence: CJK sentence-final punctuation,
/// `!`/`?`, or `.` unless it sits between two ASCII digits (`"3.14"`).
fn is_sentence_ender(chars: &[(usize, char)], i: usize) -> bool {
    match chars[i].1 {
        '。' | '！' | '？' | '…' | '!' | '?' => true,
        '.' => {
            let prev_digit = i > 0 && chars[i - 1].1.is_ascii_digit();
            let next_digit = i + 1 < chars.len() && chars[i + 1].1.is_ascii_digit();
            !(prev_digit && next_digit)
        }
        _ => false,
    }
}

/// True when `chars[i]` is a punctuation mark a split cut may snap onto (the
/// cut lands *after* it). Broader than [`is_sentence_ender`]: clause-level
/// marks (commas, semicolons, colons, enumeration comma) are valid cut points
/// inside a cross-speaker sentence.
fn is_snap_punct(chars: &[(usize, char)], i: usize) -> bool {
    match chars[i].1 {
        '。' | '！' | '？' | '；' | '，' | '、' | '：' | '…' | '!' | '?' | ',' | ';' | ':' => {
            true
        }
        '.' => {
            let prev_digit = i > 0 && chars[i - 1].1.is_ascii_digit();
            let next_digit = i + 1 < chars.len() && chars[i + 1].1.is_ascii_digit();
            !(prev_digit && next_digit)
        }
        _ => false,
    }
}

/// Split `text` into sentence pieces as byte ranges: each piece ends right
/// after a run of sentence-final punctuation, and any following whitespace
/// belongs to the *next* piece (so a moved piece carries its leading
/// separator). The ranges are contiguous and cover the whole text, so
/// concatenating the pieces reproduces `text` exactly.
fn split_sentence_pieces(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut pieces = Vec::new();
    let mut piece_start = 0usize;
    let mut i = 0;
    while i < chars.len() {
        if is_sentence_ender(&chars, i) {
            // Absorb a run of enders ("！？") into one piece ending.
            let mut j = i;
            while j + 1 < chars.len() && is_sentence_ender(&chars, j + 1) {
                j += 1;
            }
            let end_byte = chars[j].0 + chars[j].1.len_utf8();
            pieces.push((piece_start, end_byte));
            piece_start = end_byte;
            i = j + 1;
        } else {
            i += 1;
        }
    }
    if piece_start < text.len() {
        pieces.push((piece_start, text.len()));
    }
    pieces
}

/// Locate each word's surface form in `text`, searching forward from the
/// previous word's end (repeated tokens resolve in order). Returns the byte
/// start offsets, or `None` when any word is missing/empty — the caller then
/// falls back to time-proportional estimates.
fn locate_words(text: &str, words: &[Word]) -> Option<Vec<usize>> {
    let mut offsets = Vec::with_capacity(words.len());
    let mut cursor = 0usize;
    for w in words {
        if w.word.is_empty() {
            return None;
        }
        let found = text[cursor..].find(w.word.as_str())?;
        let offset = cursor + found;
        offsets.push(offset);
        cursor = offset + w.word.len();
    }
    Some(offsets)
}

/// Derive ASR sentences from one turn's decoded text and word timings.
///
/// The text is split after sentence-final punctuation (CJK and English). With
/// locatable word timings each sentence's extent is its first-to-last word
/// span; without them the turn's `[fallback_start, fallback_end)` extent is
/// divided proportionally by character count. Words are assigned to the
/// sentence whose extent contains their midpoint (any straggler goes to the
/// nearest sentence) so no word is ever dropped.
pub fn sentences_from_turn(
    text: &str,
    words: &[Word],
    fallback_start: f64,
    fallback_end: f64,
) -> Vec<AsrSentence> {
    let pieces = split_sentence_pieces(text);
    if pieces.is_empty() {
        return Vec::new();
    }
    // An empty word list is "no timing", not "located zero words" — route it
    // to the proportional fallback below.
    let located = if words.is_empty() {
        None
    } else {
        locate_words(text, words)
    };

    // Word index -> piece index, by located byte offset when available.
    let piece_of_word: Option<Vec<usize>> = located.as_ref().map(|offsets| {
        offsets
            .iter()
            .map(|&off| pieces.partition_point(|&(_, end)| end <= off))
            .collect()
    });

    // Per-piece time extents.
    let mut extents: Vec<(f64, f64)> = vec![(f64::NAN, f64::NAN); pieces.len()];
    if let Some(assignments) = &piece_of_word {
        // Worded pieces span their first-to-last word.
        for (piece_idx, extent) in extents.iter_mut().enumerate() {
            let members: Vec<usize> = assignments
                .iter()
                .enumerate()
                .filter(|(_, p)| **p == piece_idx)
                .map(|(wi, _)| wi)
                .collect();
            if let (Some(&first), Some(&last)) = (members.first(), members.last()) {
                *extent = (words[first].start, words[last].end);
            }
        }
        // Wordless pieces interpolate between their worded neighbours.
        let mut prev_end = fallback_start;
        for piece_idx in 0..pieces.len() {
            if extents[piece_idx].0.is_nan() {
                let next_start = (piece_idx + 1..pieces.len())
                    .find_map(|k| (!extents[k].0.is_nan()).then_some(extents[k].0))
                    .unwrap_or(fallback_end);
                extents[piece_idx] = (prev_end, next_start);
            }
            prev_end = extents[piece_idx].1;
        }
    } else {
        // No usable word timing: divide the fallback extent proportionally by
        // character count.
        let span = (fallback_end - fallback_start).max(0.0);
        let total: usize = pieces
            .iter()
            .map(|(a, b)| text[*a..*b].chars().count())
            .sum();
        let mut acc = 0usize;
        for (piece_idx, (a, b)) in pieces.iter().enumerate() {
            let start_chars = acc;
            acc += text[*a..*b].chars().count();
            let frac = |n: usize| {
                if total == 0 {
                    fallback_start
                } else {
                    fallback_start + span * (n as f64 / total as f64)
                }
            };
            extents[piece_idx] = (frac(start_chars), frac(acc));
        }
    }

    // Assemble sentences; words join the piece whose extent contains their
    // midpoint (byte-offset assignment when location succeeded).
    let mut sentence_words: Vec<Vec<Word>> = vec![Vec::new(); pieces.len()];
    for (wi, w) in words.iter().enumerate() {
        let owner = if let Some(assignments) = &piece_of_word {
            assignments[wi]
        } else {
            let mid = (w.start + w.end) / 2.0;
            let by_extent = extents.partition_point(|&(_, e)| e <= mid);
            by_extent.min(pieces.len() - 1)
        };
        sentence_words[owner].push(w.clone());
    }

    pieces
        .iter()
        .enumerate()
        .map(|(piece_idx, &(a, b))| {
            let (start, end) = extents[piece_idx];
            AsrSentence {
                text: text[a..b].to_string(),
                start: start.min(end),
                end: start.max(end),
                words: std::mem::take(&mut sentence_words[piece_idx]),
            }
        })
        .collect()
}

/// Estimated cut position (char index) within a sentence for a speaker-change
/// boundary at time `bound`: the char offset of the first word whose midpoint
/// is past the boundary when word timings locate cleanly, else the
/// time-proportional character position.
fn estimate_cut_chars(
    s: &AsrSentence,
    located: Option<&[usize]>,
    chars: &[(usize, char)],
    bound: f64,
) -> usize {
    let len = chars.len();
    if let Some(offsets) = located {
        if !s.words.is_empty() {
            let rank = s.words.partition_point(|w| (w.start + w.end) / 2.0 < bound);
            if rank > 0 && rank < s.words.len() {
                return chars.partition_point(|&(byte, _)| byte < offsets[rank]);
            }
        }
    }
    let dur = s.end - s.start;
    if !dur.is_finite() || dur <= 0.0 {
        return len / 2;
    }
    let ratio = ((bound - s.start) / dur).clamp(0.0, 1.0);
    (ratio * len as f64).round() as usize
}

/// Snap an estimated cut position (char index) to the best nearby boundary,
/// in priority order: punctuation (cut lands after the mark) > whitespace >
/// hard cut at the estimate. The result stays in `(min_cut, len)` so fragments
/// are never empty and successive cuts stay monotone; `None` when no valid
/// position remains (the caller drops that boundary, merging the runs).
fn snap_cut(chars: &[(usize, char)], est: usize, min_cut: usize) -> Option<usize> {
    let len = chars.len();
    let lo = min_cut + 1;
    if len < 2 || lo > len - 1 {
        return None;
    }
    let hi = len - 1;
    let in_window = |i: usize| (lo..=hi).contains(&i) && i.abs_diff(est) <= SNAP_WINDOW_CHARS;
    let nearest = |pred: &dyn Fn(usize) -> bool| -> Option<usize> {
        let mut best: Option<usize> = None;
        for i in lo..=hi {
            if !in_window(i) || !pred(i) {
                continue;
            }
            // Tie -> the left candidate (deterministic).
            if best.is_none_or(|b| i.abs_diff(est) < b.abs_diff(est)) {
                best = Some(i);
            }
        }
        best
    };
    // 1. Punctuation: cut right after the mark at char index i-1.
    if let Some(i) = nearest(&|i| is_snap_punct(chars, i - 1)) {
        return Some(i);
    }
    // 2. Whitespace: cut after the space (the space stays with the left
    // fragment, keeping concatenation lossless).
    if let Some(i) = nearest(&|i| chars[i - 1].1.is_whitespace()) {
        return Some(i);
    }
    // 3. Hard cut at the estimated position.
    Some(est.clamp(lo, hi))
}

/// A sentence fragment covering char range `[c0, c1)` / time range
/// `[t0, t1)`, attributed by max overlap against the overlapped turns.
#[allow(clippy::too_many_arguments)]
fn push_fragment(
    out: &mut Vec<AlignedFragment>,
    s: &AsrSentence,
    sentence_idx: usize,
    chars: &[(usize, char)],
    located: Option<&[usize]>,
    c0: usize,
    c1: usize,
    t0: f64,
    t1: f64,
    overlapped: &[usize],
    turns: &[DiarTurn],
) {
    if c0 >= c1 {
        return;
    }
    let byte = |c: usize| -> usize {
        if c >= chars.len() {
            s.text.len()
        } else {
            chars[c].0
        }
    };
    let (b0, b1) = (byte(c0), byte(c1));
    let text = &s.text[b0..b1];
    if text.is_empty() {
        return;
    }
    let turn = overlapped
        .iter()
        .copied()
        .max_by(|&a, &b| {
            let oa = overlap_seconds(t0, t1, turns[a].start, turns[a].end);
            let ob = overlap_seconds(t0, t1, turns[b].start, turns[b].end);
            // Tie -> earlier turn in chronological order (`overlapped` is
            // sorted, and max_by returns the last maximum, so compare with
            // the reversed argument order).
            oa.total_cmp(&ob).then(b.cmp(&a))
        })
        .expect("overlapped is non-empty");
    let words: Vec<Word> = s
        .words
        .iter()
        .enumerate()
        .filter(|(wi, w)| {
            if let Some(offsets) = located {
                let off = offsets[*wi];
                b0 <= off && off < b1
            } else {
                // Without located byte offsets, words are bucketed by time
                // midpoint. The fragments tile only `[s.start, s.end)`, which
                // (on the proportional-extent fallback) need not contain every
                // word time — so the outer fragments are open-ended and a
                // straggler word is pulled into the nearest edge fragment
                // instead of being dropped.
                let lo = if c0 == 0 { f64::NEG_INFINITY } else { t0 };
                let hi = if c1 == chars.len() { f64::INFINITY } else { t1 };
                let mid = (w.start + w.end) / 2.0;
                lo <= mid && mid < hi
            }
        })
        .map(|(_, w)| w.clone())
        .collect();
    out.push(AlignedFragment {
        sentence: sentence_idx,
        turn,
        speaker: turns[turn].speaker,
        start: t0,
        end: t1,
        text: text.to_string(),
        words,
        split: true,
    });
}

/// Split a sentence that spans turns of two or more speakers. `overlapped`
/// holds the indices of the turns the sentence overlaps, sorted by turn start.
fn split_across(
    out: &mut Vec<AlignedFragment>,
    sentence_idx: usize,
    s: &AsrSentence,
    overlapped: &[usize],
    turns: &[DiarTurn],
) {
    // Speaker-change boundaries between consecutive overlapped turns. The
    // handover time is the midpoint between the left turn's end and the right
    // turn's start (identical formula whether the turns abut, gap, or
    // overlap); boundaries outside the sentence need no cut.
    let mut bounds: Vec<f64> = Vec::new();
    for pair in overlapped.windows(2) {
        let (a, b) = (turns[pair[0]], turns[pair[1]]);
        if a.speaker != b.speaker {
            let bound = (a.end + b.start) / 2.0;
            if bound > s.start && bound < s.end && bound.is_finite() {
                bounds.push(bound);
            }
        }
    }
    bounds.sort_by(f64::total_cmp);
    bounds.dedup();

    let chars: Vec<(usize, char)> = s.text.char_indices().collect();
    let located = locate_words(&s.text, &s.words);

    // Resolve each boundary to a snapped char cut, keeping cuts monotone. A
    // boundary that cannot be cut without an empty fragment is dropped (its
    // two runs merge into one fragment, attributed by max overlap).
    let mut cuts: Vec<(usize, f64)> = Vec::new();
    for &bound in &bounds {
        let est = estimate_cut_chars(s, located.as_deref(), &chars, bound);
        let min_cut = cuts.last().map_or(0, |&(c, _)| c);
        if let Some(cut) = snap_cut(&chars, est, min_cut) {
            cuts.push((cut, bound));
        }
    }

    if cuts.is_empty() {
        // No usable split point: keep the sentence whole on its max-overlap
        // turn (e.g. a one-word sentence straddling a boundary).
        if let Some(turn) = max_overlap_turn(s.start, s.end, turns) {
            out.push(AlignedFragment {
                sentence: sentence_idx,
                turn,
                speaker: turns[turn].speaker,
                start: s.start,
                end: s.end,
                text: s.text.clone(),
                words: s.words.clone(),
                split: false,
            });
        }
        return;
    }

    let mut prev_char = 0usize;
    let mut prev_time = s.start;
    for &(cut, bound) in &cuts {
        push_fragment(
            out,
            s,
            sentence_idx,
            &chars,
            located.as_deref(),
            prev_char,
            cut,
            prev_time,
            bound,
            overlapped,
            turns,
        );
        prev_char = cut;
        prev_time = bound;
    }
    push_fragment(
        out,
        s,
        sentence_idx,
        &chars,
        located.as_deref(),
        prev_char,
        chars.len(),
        prev_time,
        s.end,
        overlapped,
        turns,
    );
}

/// Assign every ASR sentence to the diar turn it overlaps most, splitting
/// sentences that straddle a speaker change, and return all fragments in
/// chronological order — regardless of the input order (a late-arriving
/// sentence is *backfilled* into its time position, never appended).
///
/// Deterministic: overlap ties resolve to the earlier turn, gap-fallen
/// sentences attach to the nearest turn (earlier on ties), and the output
/// sort is stable (equal starts keep input order). An empty `turns` slice
/// yields no fragments — there is nothing to attribute to.
pub fn align_sentences(sentences: &[AsrSentence], turns: &[DiarTurn]) -> Vec<AlignedFragment> {
    let mut out: Vec<AlignedFragment> = Vec::new();
    if turns.is_empty() {
        return out;
    }
    for (sentence_idx, s) in sentences.iter().enumerate() {
        if s.text.is_empty() {
            continue;
        }
        let mut overlapped: Vec<usize> = (0..turns.len())
            .filter(|&ti| overlap_seconds(s.start, s.end, turns[ti].start, turns[ti].end) > 0.0)
            .collect();
        if overlapped.is_empty() {
            let turn = nearest_turn(s.start, s.end, turns).expect("turns is non-empty");
            out.push(AlignedFragment {
                sentence: sentence_idx,
                turn,
                speaker: turns[turn].speaker,
                start: s.start,
                end: s.end,
                text: s.text.clone(),
                words: s.words.clone(),
                split: false,
            });
            continue;
        }
        overlapped.sort_by(|&a, &b| turns[a].start.total_cmp(&turns[b].start).then(a.cmp(&b)));
        let single_speaker = overlapped
            .windows(2)
            .all(|pair| turns[pair[0]].speaker == turns[pair[1]].speaker);
        if single_speaker {
            let turn = max_overlap_turn(s.start, s.end, turns).expect("overlap exists");
            out.push(AlignedFragment {
                sentence: sentence_idx,
                turn,
                speaker: turns[turn].speaker,
                start: s.start,
                end: s.end,
                text: s.text.clone(),
                words: s.words.clone(),
                split: false,
            });
        } else {
            split_across(&mut out, sentence_idx, s, &overlapped, turns);
        }
    }
    // Stable chronological sort: out-of-order (late) input sentences are
    // backfilled into position; equal starts keep input order.
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

/// Insert a late-arriving fragment into a chronologically ordered fragment
/// list at its time position (after any equal-start entries), rather than
/// appending it at the end.
pub fn backfill_fragment(fragments: &mut Vec<AlignedFragment>, frag: AlignedFragment) {
    let pos = fragments
        .partition_point(|f| f.start.total_cmp(&frag.start) != std::cmp::Ordering::Greater);
    fragments.insert(pos, frag);
}

/// Re-align one track's per-turn ASR output against its diar turns: every
/// turn's text is split into sentences (using word timings when present), the
/// sentences are aligned by [`align_sentences`], and each turn's text/words
/// are rebuilt from the fragments that landed on it, in chronological order.
///
/// Returns the new `(texts, words)`. When no sentence moves or splits — the
/// common case, since per-turn slicing mostly agrees with the diarization —
/// the input is returned unchanged (byte-for-byte, original word vectors), so
/// engines without word timings and well-behaved runs see zero behavior
/// change. Entries beyond `turns.len()` (a longer `texts` slice) pass through
/// untouched, matching the zip contract of
/// [`assemble_meeting`](crate::assemble::assemble_meeting); conversely a
/// fragment that lands on a turn with no text slot (turns longer than texts)
/// extends the outputs rather than dropping attributed text.
pub fn realign_turn_texts(
    turns: &[DiarTurn],
    texts: &[String],
    words: &[Vec<Word>],
) -> (Vec<String>, Vec<Vec<Word>>) {
    let out_texts: Vec<String> = texts.to_vec();
    let mut out_words: Vec<Vec<Word>> = (0..texts.len())
        .map(|i| words.get(i).cloned().unwrap_or_default())
        .collect();
    let n = turns.len().min(texts.len());

    let mut sentences: Vec<AsrSentence> = Vec::new();
    let mut origin: Vec<usize> = Vec::new();
    for (i, text) in texts.iter().enumerate().take(n) {
        let turn_words: &[Word] = words.get(i).map(Vec::as_slice).unwrap_or(&[]);
        for s in sentences_from_turn(text, turn_words, turns[i].start, turns[i].end) {
            sentences.push(s);
            origin.push(i);
        }
    }
    if sentences.is_empty() {
        return (out_texts, out_words);
    }

    let fragments = align_sentences(&sentences, turns);
    // Fast path: nothing moved or split -> byte-identical passthrough (this
    // also protects non-monotone word timings from reordering a turn's text).
    let unchanged = fragments
        .iter()
        .all(|f| !f.split && f.turn == origin[f.sentence]);
    if unchanged {
        return (out_texts, out_words);
    }

    let mut out_texts = out_texts;
    // Fragments are keyed by turn index; a fragment can legitimately land on a
    // turn that has no text slot yet (turns longer than texts), so the rebuild
    // buffers are sized by turns and any overflow extends the outputs — no
    // attributed text is ever dropped.
    let mut new_texts = vec![String::new(); turns.len()];
    let mut new_words: Vec<Vec<Word>> = vec![Vec::new(); turns.len()];
    for f in fragments {
        new_texts[f.turn].push_str(&f.text);
        new_words[f.turn].extend(f.words);
    }
    for (i, text) in new_texts.into_iter().enumerate() {
        if i < texts.len() {
            if text == texts[i] {
                // Unchanged turn: keep the original word vector verbatim (the
                // rebuilt one could reorder words whose midpoints drifted).
                continue;
            }
            out_texts[i] = text;
            out_words[i] = std::mem::take(&mut new_words[i]);
        } else {
            out_texts.push(text);
            out_words.push(std::mem::take(&mut new_words[i]));
        }
    }
    (out_texts, out_words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start: f64, end: f64, speaker: u32) -> DiarTurn {
        DiarTurn::new(start, end, speaker)
    }

    fn sent(text: &str, start: f64, end: f64) -> AsrSentence {
        AsrSentence {
            text: text.to_string(),
            start,
            end,
            words: Vec::new(),
        }
    }

    fn sent_with_words(text: &str, words: Vec<(&str, f64, f64)>) -> AsrSentence {
        let start = words.first().map(|w| w.1).unwrap_or(0.0);
        let end = words.last().map(|w| w.2).unwrap_or(0.0);
        AsrSentence {
            text: text.to_string(),
            start,
            end,
            words: words
                .into_iter()
                .map(|(w, s, e)| Word::new(w, s, e))
                .collect(),
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // ── overlap_seconds ───────────────────────────────────────────────

    #[test]
    fn overlap_contained_partial_disjoint_touching() {
        // Contained.
        assert!(approx(overlap_seconds(2.0, 4.0, 0.0, 10.0), 2.0));
        // Partial.
        assert!(approx(overlap_seconds(8.0, 12.0, 0.0, 10.0), 2.0));
        // Disjoint.
        assert!(approx(overlap_seconds(20.0, 30.0, 0.0, 10.0), 0.0));
        // Touching half-open intervals share no time.
        assert!(approx(overlap_seconds(10.0, 12.0, 0.0, 10.0), 0.0));
        // Non-finite input degrades to 0.
        assert!(approx(overlap_seconds(f64::NAN, 1.0, 0.0, 10.0), 0.0));
    }

    // ── max-overlap assignment ────────────────────────────────────────

    #[test]
    fn contained_sentence_picks_enclosing_turn() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let frags = align_sentences(&[sent("你好世界", 2.0, 5.0)], &turns);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 0);
        assert_eq!(frags[0].speaker, 0);
        assert!(!frags[0].split);
        assert_eq!(frags[0].text, "你好世界");
    }

    #[test]
    fn straddling_sentence_picks_larger_overlap_share() {
        // 2 s in turn 0, 3 s in turn 1 -> turn 1. Same speaker on both sides
        // so the pure max-overlap assignment is exercised without a split.
        let same = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 0)];
        let frags = align_sentences(&[sent("hello world", 8.0, 13.0)], &same);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 1, "3s in turn 1 beats 2s in turn 0");
        assert!(!frags[0].split);
    }

    #[test]
    fn straddling_sentence_tie_breaks_to_earlier_turn() {
        let same = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 0)];
        let frags = align_sentences(&[sent("hello world", 8.0, 12.0)], &same);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 0, "2s == 2s tie -> earlier turn");
    }

    #[test]
    fn touching_sentence_attaches_to_earlier_turn() {
        // Sentence [10, 12) touches turn 0's end and turn 1's start; both
        // overlaps are 0, so the nearest-turn fallback fires and the tie
        // (gap 0 both ways) resolves to the earlier turn.
        let turns = vec![turn(0.0, 10.0, 0), turn(12.0, 20.0, 1)];
        let frags = align_sentences(&[sent("你好", 10.0, 12.0)], &turns);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 0);
    }

    #[test]
    fn gap_sentence_attaches_to_nearest_turn() {
        let turns = vec![turn(0.0, 10.0, 0), turn(20.0, 30.0, 1)];
        // Closer to turn 1.
        let frags = align_sentences(&[sent("你好", 16.0, 18.0)], &turns);
        assert_eq!(frags[0].turn, 1);
        // Closer to turn 0.
        let frags = align_sentences(&[sent("你好", 11.0, 13.0)], &turns);
        assert_eq!(frags[0].turn, 0);
        // Equidistant -> earlier turn.
        let frags = align_sentences(&[sent("你好", 14.0, 16.0)], &turns);
        assert_eq!(frags[0].turn, 0);
    }

    #[test]
    fn zero_length_sentence_uses_position() {
        let turns = vec![turn(0.0, 10.0, 0), turn(20.0, 30.0, 1)];
        // Inside turn 1.
        let frags = align_sentences(&[sent("嗯", 25.0, 25.0)], &turns);
        assert_eq!(frags[0].turn, 1);
        // In the gap, closer to turn 0.
        let frags = align_sentences(&[sent("嗯", 12.0, 12.0)], &turns);
        assert_eq!(frags[0].turn, 0);
    }

    #[test]
    fn overlapping_diar_turns_still_assign_by_max_overlap() {
        // Diarization handed us overlapping turns (different speakers).
        // Sentence [4,6): 2s in A vs 1s in B -> A. Sentence [9,11): 1s vs 2s
        // -> but different speakers straddled -> split at midpoint 7.5.
        let turns = vec![turn(0.0, 10.0, 0), turn(5.0, 15.0, 1)];
        let frags = align_sentences(&[sent("你好世界", 4.0, 6.0)], &turns);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 0);
        assert!(!frags[0].split);
    }

    // ── degenerate inputs ─────────────────────────────────────────────

    #[test]
    fn empty_turns_or_sentences_yield_nothing() {
        let frags = align_sentences(&[sent("你好", 0.0, 1.0)], &[]);
        assert!(frags.is_empty());
        let turns = vec![turn(0.0, 10.0, 0)];
        assert!(align_sentences(&[], &turns).is_empty());
        // Empty-text sentences are skipped.
        let frags = align_sentences(&[sent("", 0.0, 1.0)], &turns);
        assert!(frags.is_empty());
    }

    #[test]
    fn nan_sentence_times_are_deterministic() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let frags = align_sentences(&[sent("你好", f64::NAN, f64::NAN)], &turns);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].turn, 0);
    }

    #[test]
    fn unsplittable_straddling_sentence_stays_whole() {
        // A one-word sentence straddling a speaker change cannot be split
        // into two non-empty fragments; it stays whole on max overlap.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let frags = align_sentences(&[sent("嗯", 8.0, 13.0)], &turns);
        assert_eq!(frags.len(), 1);
        assert!(!frags[0].split);
        assert_eq!(frags[0].turn, 1, "3s in turn 1 beats 2s in turn 0");
        assert_eq!(frags[0].text, "嗯");
    }

    // ── cross-speaker splitting ───────────────────────────────────────

    #[test]
    fn split_snaps_to_nearest_punctuation() {
        // Boundary at t=10 falls mid-sentence; the comma at char 5 is the
        // nearest snap point to the ratio estimate (9 of 18 chars).
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let s = sent("我们先这样吧，然后你继续说下去吧。", 2.0, 18.0);
        let frags = align_sentences(std::slice::from_ref(&s), &turns);
        assert_eq!(frags.len(), 2);
        assert!(frags.iter().all(|f| f.split));
        assert_eq!(frags[0].speaker, 0);
        assert_eq!(frags[1].speaker, 1);
        assert!(frags[0].text.ends_with('，'), "{:?}", frags[0].text);
        // Lossless: concatenating the fragments reproduces the sentence.
        assert_eq!(format!("{}{}", frags[0].text, frags[1].text), s.text);
        // Time extents split at the boundary.
        assert!(approx(frags[0].start, 2.0));
        assert!(approx(frags[0].end, 10.0));
        assert!(approx(frags[1].start, 10.0));
        assert!(approx(frags[1].end, 18.0));
    }

    #[test]
    fn split_snaps_to_space_when_no_punctuation_nearby() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        // 15 chars, boundary t=10 of [2,18) -> ratio 0.5 -> est char 7-8,
        // nearest space after "hello" (cut 6) or "there" (cut 12).
        let s = sent("hello there world", 2.0, 18.0);
        let frags = align_sentences(std::slice::from_ref(&s), &turns);
        assert_eq!(frags.len(), 2);
        assert!(frags[0].text.ends_with(' '), "{:?}", frags[0].text);
        assert_eq!(format!("{}{}", frags[0].text, frags[1].text), s.text);
    }

    #[test]
    fn split_hard_cuts_by_ratio_when_no_snap_point() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        // 16 CJK chars, no punctuation, no spaces -> hard cut at ratio.
        // Boundary 10 in [2,18) -> ratio 0.5 -> char 8.
        let s = sent("先帝创业未半而中道崩殂今天下三分", 2.0, 18.0);
        let frags = align_sentences(std::slice::from_ref(&s), &turns);
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].text.chars().count(), 8);
        assert_eq!(frags[0].text, "先帝创业未半而中");
        assert_eq!(frags[1].text, "道崩殂今天下三分");
        assert_eq!(format!("{}{}", frags[0].text, frags[1].text), s.text);
    }

    #[test]
    fn split_prefers_punctuation_over_closer_space() {
        // English text where a comma sits 3 chars from the estimate but a
        // space is 1 char away: punctuation still wins (priority, not just
        // distance — the window only gates *how far* a snap may travel).
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        // "aaa bbb,ccc ddd": est at ratio 0.5 of 15 chars -> char 7-8.
        // chars: a a a ' ' b b b , c c c ' ' d d d -> idx 0..14.
        // comma at idx 7 -> cut 8 (dist 0-1); space at idx 11 -> cut 12.
        let s = sent("aaa bbb,ccc ddd", 2.0, 18.0);
        let frags = align_sentences(std::slice::from_ref(&s), &turns);
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].text, "aaa bbb,", "{:?}", frags);
    }

    #[test]
    fn split_ignores_punctuation_outside_snap_window() {
        // The only comma is 25+ chars away from the estimate; a space sits
        // right next to it -> space snap wins, not the distant comma.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        // 60 chars: comma at idx 0 (cut 1), boundary t=10 of [2,18) ->
        // est char 30; comma is 29 away (outside window 20); a space at
        // idx 29 (cut 30) is in-window.
        let text = format!(",{}", "a".repeat(28));
        let text = format!("{text} {}", "b".repeat(29));
        // text: "," + 28*'a' + " " + 29*'b' -> 59 chars; est ~29.5 -> 30.
        let s = sent(&text, 2.0, 18.0);
        let frags = align_sentences(&[s], &turns);
        assert_eq!(frags.len(), 2);
        assert!(
            frags[0].text.ends_with(' '),
            "space snap, not the distant comma: {:?}",
            frags[0]
        );
        assert_eq!(frags[0].text.chars().count(), 30);
    }

    #[test]
    fn split_uses_word_timestamps_to_refine_estimate() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        // Words with real timings; the t=10 boundary falls inside "ccc"
        // (9..11) -> the cut lands *before* "ccc" (char 8), snapping to the
        // space there. A pure ratio estimate (10.5/15*15 ≈ 10-11) would cut
        // inside/after "ccc ".
        let s = sent_with_words(
            "aaa bbb ccc ddd",
            vec![
                ("aaa", 2.0, 5.0),
                ("bbb", 5.0, 9.0),
                ("ccc", 9.0, 11.0),
                ("ddd", 11.0, 17.0),
            ],
        );
        let frags = align_sentences(&[s], &turns);
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].text, "aaa bbb ");
        assert_eq!(frags[1].text, "ccc ddd");
        // Words follow their fragment.
        assert_eq!(
            frags[0]
                .words
                .iter()
                .map(|w| w.word.as_str())
                .collect::<Vec<_>>(),
            vec!["aaa", "bbb"]
        );
        assert_eq!(
            frags[1]
                .words
                .iter()
                .map(|w| w.word.as_str())
                .collect::<Vec<_>>(),
            vec!["ccc", "ddd"]
        );
    }

    #[test]
    fn split_keeps_words_outside_the_proportional_extent() {
        // Word timings that cannot be located in the text (`zzz`/`yyy` never
        // match) force the time-midpoint bucketing, and their times fall
        // OUTSIDE the sentence's proportional extent [2, 18). The edge
        // fragments are open-ended, so no word timing is dropped.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let s = AsrSentence {
            text: "前半部分在这里，后半部分在那边。".to_string(),
            start: 2.0,
            end: 18.0,
            words: vec![Word::new("zzz", 0.0, 1.0), Word::new("yyy", 19.0, 20.0)],
        };
        assert!(locate_words(&s.text, &s.words).is_none());
        let frags = align_sentences(&[s], &turns);
        assert_eq!(frags.len(), 2, "{frags:?}");
        assert_eq!(
            frags.iter().map(|f| f.words.len()).sum::<usize>(),
            2,
            "both straggler words survive on the edge fragments"
        );
        assert_eq!(frags[0].words[0].word, "zzz");
        assert_eq!(frags[1].words[0].word, "yyy");
    }

    #[test]
    fn split_three_speakers_produces_monotone_cuts() {
        let turns = vec![turn(0.0, 5.0, 0), turn(5.0, 8.0, 1), turn(8.0, 12.0, 2)];
        // 22 chars over [1, 11); boundaries at 5 and 8 -> two cuts; the
        // exact snap points don't matter, the structure does.
        let s = sent("aaaaaaaaaabbbbb，bbbbb。ccccc", 1.0, 11.0);
        let frags = align_sentences(std::slice::from_ref(&s), &turns);
        assert_eq!(frags.len(), 3, "{frags:?}");
        assert_eq!(frags[0].speaker, 0);
        assert_eq!(frags[1].speaker, 1);
        assert_eq!(frags[2].speaker, 2);
        assert!(frags.windows(2).all(|w| w[0].end <= w[1].start));
        let joined: String = frags.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(joined, s.text);
    }

    #[test]
    fn split_same_speaker_overlap_does_not_split() {
        // Two overlapping turns of the SAME speaker: no split, max-overlap
        // assignment only.
        let turns = vec![turn(0.0, 10.0, 0), turn(8.0, 20.0, 0)];
        let frags = align_sentences(&[sent("hello world", 5.0, 15.0)], &turns);
        assert_eq!(frags.len(), 1);
        assert!(!frags[0].split);
        assert_eq!(frags[0].turn, 1, "7s in turn 1 beats 5s in turn 0");
    }

    // ── backfill / ordering ───────────────────────────────────────────

    #[test]
    fn out_of_order_sentences_are_backfilled_chronologically() {
        let turns = vec![turn(0.0, 30.0, 0)];
        // Late-arriving: the middle sentence is delivered last.
        let sentences = vec![
            sent("第一句。", 0.0, 5.0),
            sent("第三句。", 10.0, 15.0),
            sent("第二句。", 5.0, 10.0),
        ];
        let frags = align_sentences(&sentences, &turns);
        assert_eq!(
            frags.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            vec!["第一句。", "第二句。", "第三句。"]
        );
        assert_eq!(
            frags.iter().map(|f| f.sentence).collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn backfill_fragment_inserts_at_time_position() {
        let turns = vec![turn(0.0, 30.0, 0)];
        let mut frags = align_sentences(
            &[sent("第一句。", 0.0, 5.0), sent("第三句。", 10.0, 15.0)],
            &turns,
        );
        let late = AlignedFragment {
            sentence: 2,
            turn: 0,
            speaker: 0,
            start: 5.0,
            end: 10.0,
            text: "第二句。".to_string(),
            words: Vec::new(),
            split: false,
        };
        backfill_fragment(&mut frags, late);
        assert_eq!(
            frags.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            vec!["第一句。", "第二句。", "第三句。"]
        );
        // A fragment at an equal start lands after the existing entry
        // (stable), not before it.
        let same_start = AlignedFragment {
            sentence: 3,
            turn: 0,
            speaker: 0,
            start: 5.0,
            end: 6.0,
            text: "同时。".to_string(),
            words: Vec::new(),
            split: false,
        };
        backfill_fragment(&mut frags, same_start);
        assert_eq!(
            frags.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            vec!["第一句。", "第二句。", "同时。", "第三句。"]
        );
    }

    // ── sentences_from_turn ───────────────────────────────────────────

    #[test]
    fn sentences_split_on_cjk_and_english_enders_and_cover_text() {
        let s = sentences_from_turn("你好。世界! 怎么样？3.14是圆周率。尾句", &[], 0.0, 10.0);
        let texts: Vec<&str> = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["你好。", "世界!", " 怎么样？", "3.14是圆周率。", "尾句"]
        );
        // Contiguous coverage -> lossless concat.
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<String>(),
            "你好。世界! 怎么样？3.14是圆周率。尾句"
        );
        // Proportional extents across [0,10): five pieces of 3,3,5,9,2 chars
        // (22 total); boundaries are contiguous.
        assert!(approx(s[0].start, 0.0));
        assert!(approx(s[0].end, 10.0 * 3.0 / 22.0));
        assert!(approx(s[1].start, s[0].end));
        assert!(approx(s[2].start, s[1].end));
        assert!(approx(s[3].start, s[2].end));
        assert!(approx(s[4].start, s[3].end));
        assert!(approx(s[4].end, 10.0));
    }

    #[test]
    fn sentences_use_word_times_for_extents() {
        let words = vec![
            Word::new("你", 1.0, 1.3),
            Word::new("好", 1.3, 1.6),
            Word::new("世", 3.0, 3.3),
            Word::new("界", 3.3, 3.8),
        ];
        let s = sentences_from_turn("你好。世界", &words, 0.0, 10.0);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "你好。");
        assert!(approx(s[0].start, 1.0));
        assert!(approx(s[0].end, 1.6));
        assert_eq!(s[0].words.len(), 2);
        assert_eq!(s[1].text, "世界");
        assert!(approx(s[1].start, 3.0));
        assert!(approx(s[1].end, 3.8));
        assert_eq!(s[1].words.len(), 2);
    }

    #[test]
    fn sentences_interpolate_wordless_pieces() {
        // The middle piece has no words of its own: it interpolates between
        // its worded neighbours.
        let words = vec![Word::new("你", 1.0, 2.0), Word::new("界", 8.0, 9.0)];
        let s = sentences_from_turn("你。好。界", &words, 0.0, 10.0);
        assert_eq!(s.len(), 3);
        assert!(approx(s[1].start, 2.0));
        assert!(approx(s[1].end, 8.0));
    }

    // ── realign_turn_texts ────────────────────────────────────────────

    #[test]
    fn realign_is_identity_without_word_timings() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let texts = vec!["第一句。第二句。".to_string(), "hello world".to_string()];
        let (out_texts, out_words) = realign_turn_texts(&turns, &texts, &[]);
        assert_eq!(out_texts, texts);
        assert_eq!(out_words, vec![Vec::<Word>::new(); 2]);
    }

    #[test]
    fn realign_is_identity_when_sentences_stay_on_their_turn() {
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let texts = vec!["你好。世界。".to_string(), "早上好。".to_string()];
        let words = vec![
            vec![
                Word::new("你", 1.0, 1.2),
                Word::new("好", 1.2, 1.5),
                Word::new("世", 2.0, 2.3),
                Word::new("界", 2.3, 2.6),
            ],
            vec![Word::new("早", 11.0, 11.3)],
        ];
        let original_words = words.clone();
        let (out_texts, out_words) = realign_turn_texts(&turns, &texts, &words);
        assert_eq!(out_texts, texts, "byte-identical passthrough");
        assert_eq!(out_words, original_words, "original word vectors kept");
    }

    #[test]
    fn realign_moves_sentence_to_the_turn_it_overlaps_most() {
        // Word timings drifted past the slice edge: the second sentence of
        // turn 0 was decoded from turn 0's audio but its words all land
        // inside turn 1 — it belongs to S2.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let texts = vec!["这是我的。这是你的。".to_string(), "嗯对。".to_string()];
        let words = vec![
            vec![
                Word::new("这", 8.0, 8.3),
                Word::new("是", 8.3, 8.5),
                Word::new("我", 8.5, 8.8),
                Word::new("的", 8.8, 9.1),
                Word::new("这", 10.5, 10.8),
                Word::new("是", 10.8, 11.1),
                Word::new("你", 11.1, 11.4),
                Word::new("的", 11.4, 11.8),
            ],
            vec![Word::new("嗯", 12.0, 12.4), Word::new("对", 12.4, 12.9)],
        ];
        let (out_texts, out_words) = realign_turn_texts(&turns, &texts, &words);
        assert_eq!(
            out_texts[0], "这是我的。",
            "straddling sentence moved off S1"
        );
        assert_eq!(
            out_texts[1], "这是你的。嗯对。",
            "moved sentence backfilled by time"
        );
        assert_eq!(out_words[0].len(), 4);
        assert_eq!(out_words[1].len(), 6);
    }

    #[test]
    fn realign_splits_cross_speaker_sentence_between_turns() {
        // One decoded sentence spans the 10s speaker boundary: S1 keeps the
        // left fragment, S2 picks up the right one.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let texts = vec!["我们说到一半，然后换你来讲。".to_string(), String::new()];
        let words = vec![vec![
            Word::new("我", 7.0, 7.3),
            Word::new("们", 7.3, 7.6),
            Word::new("说", 7.6, 7.9),
            Word::new("到", 7.9, 8.2),
            Word::new("一", 8.2, 8.5),
            Word::new("半", 8.5, 8.8),
            Word::new("然", 10.2, 10.5),
            Word::new("后", 10.5, 10.8),
            Word::new("换", 10.8, 11.1),
            Word::new("你", 11.1, 11.4),
            Word::new("来", 11.4, 11.7),
            Word::new("讲", 11.7, 12.0),
        ]];
        let (out_texts, out_words) = realign_turn_texts(&turns, &texts, &words);
        assert_eq!(out_texts[0], "我们说到一半，", "{out_texts:?}");
        assert_eq!(out_texts[1], "然后换你来讲。", "{out_texts:?}");
        assert_eq!(out_words[0].len(), 6);
        assert_eq!(out_words[1].len(), 6);
    }

    #[test]
    fn realign_passes_through_excess_texts_beyond_turns() {
        // Zip contract: more texts than turns — the tail passes through.
        let turns = vec![turn(0.0, 10.0, 0)];
        let texts = vec!["有词的。".to_string(), "孤儿文本".to_string()];
        let words = vec![vec![Word::new("有", 1.0, 1.3)], Vec::new()];
        let (out_texts, _) = realign_turn_texts(&turns, &texts, &words);
        assert_eq!(out_texts[1], "孤儿文本");
    }

    #[test]
    fn realign_never_loses_text_or_words() {
        // A split + a move in one go: every character and every word must
        // survive somewhere, in order.
        let turns = vec![turn(0.0, 10.0, 0), turn(10.0, 20.0, 1)];
        let texts = vec!["前半句在这里，后半句在那边。".to_string()];
        let words = vec![vec![
            Word::new("前", 7.0, 7.3),
            Word::new("半", 7.3, 7.6),
            Word::new("句", 7.6, 7.9),
            Word::new("在", 7.9, 8.2),
            Word::new("这", 8.2, 8.5),
            Word::new("里", 8.5, 8.8),
            Word::new("后", 10.2, 10.5),
            Word::new("半", 10.5, 10.8),
            Word::new("句", 10.8, 11.1),
            Word::new("在", 11.1, 11.4),
            Word::new("那", 11.4, 11.7),
            Word::new("边", 11.7, 12.0),
        ]];
        let (out_texts, out_words) = realign_turn_texts(&turns, &texts, &words);
        assert_eq!(out_texts.concat(), texts.concat(), "no text lost");
        assert_eq!(out_words.concat().len(), 12, "no word lost");
    }
}
