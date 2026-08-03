//! Offline reconciliation of recording-time manual speaker annotations (L2),
//! **timeline-boundary model**.
//!
//! While a meeting records, the user marks "who is speaking" on live caption
//! lines. Speaker rows do not exist yet (the offline pipeline creates them after
//! stop), so each mark is persisted as a [`LiveAnnotation`]: a **boundary** at a
//! precise time on the meeting's unified timeline (shared `t0` across tracks)
//! plus the capture track it was made on. The user's real pattern is one person
//! speaking for a long stretch, occasionally interrupted for a few sentences,
//! then speaking again — so a mark is not a per-line label but a boundary that
//! opens a range running until the *next* boundary on the same track.
//!
//! ## The model
//! Per track, the boundaries sorted by time partition the timeline:
//! `A@t1 → B@t3 → …` means A speaks on `[t1, t3)`, B on `[t3, next)`, the last
//! boundary running to `+∞`. A boundary can also be **"无"** (unassigned): it
//! *ends* the current range and attributes no one after it until the next
//! boundary. Two boundaries at the same time resolve last-write-wins by
//! `created_at`.
//!
//! ## Timeline alignment (critical)
//! Live and offline segmentation do **not** align — a live caption line is one
//! streaming-ASR utterance while the offline diarizer merges continuous speech
//! into far longer turns. So the boundary's own `start_seconds` is authoritative;
//! we never trust the offline segment ranges to carry attribution. Offline
//! segment times live in each track's own WAV timeline; each track's WAV starts a
//! little after `t0` (offsets in the `<meeting-id>.timeline.json` sidecar). A
//! segment is lifted onto the unified timeline by adding its track's offset (mic
//! ≈ 0) before comparing with boundary times. A missing sidecar fails open
//! (offset 0).
//!
//! ## Splitting
//! Because one long diarized segment can span several boundaries, each offline
//! segment is **split** at the boundary times that fall inside it. With word-level
//! timings the cut snaps to the nearest word boundary (text splits with the
//! words); without them the text is split by time-proportional character count
//! (approximate). Each resulting sub-segment is attributed by the boundary
//! governing its start: a named boundary reassigns the sub-segment to that name's
//! manual speaker (an `M{k}` row, `attribution_origin = manual`); a "无" boundary
//! (or a sub-segment before the first boundary) leaves the original diarization/
//! voiceprint speaker in place. Manual attribution overrides auto-identification.
//!
//! With no annotations, or when no boundary changes any segment's attribution,
//! the pass is a strict byte-for-byte no-op.
//!
//! Pure logic over already-assembled speakers/segments — no store, no audio,
//! no platform gating — fully unit-testable with stub data.

use std::collections::BTreeMap;
use std::path::Path;

use lumen_core::{LiveAnnotation, SegmentChannel, Speaker, TranscriptSegment};
use lumen_identity::IdentityStore;
use lumen_transcript::Word;
use uuid::Uuid;

/// Label prefix for speaker rows created by reconciliation for manual names.
/// Distinct from the diarization `S{n}` space so a manual speaker can never
/// collide with an engine cluster label (e.g. in embedding persistence, which
/// looks rows up by `S{n}` label).
const MANUAL_LABEL_PREFIX: &str = "M";

/// Epsilon (seconds) for treating a boundary as *inside* a segment rather than
/// grazing its edge: a boundary within this of either end never splits (it just
/// governs from the start / does nothing at the end), avoiding degenerate
/// zero-length sub-segments from floating-point noise.
const EDGE_EPSILON: f64 = 1e-6;

/// What [`reconcile_annotations`] changed, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnnotationReconciliation {
    /// Ids of manual speaker rows created for boundary names.
    pub new_speakers: Vec<Uuid>,
    /// (Sub)segments reassigned to a manual speaker: `(segment_id, speaker_id)`.
    pub reassigned_segments: Vec<(Uuid, Uuid)>,
    /// How many original segments were split at interior boundary times.
    pub split_segments: usize,
}

/// A resolved boundary on one track's timeline: its unified-timeline start and
/// the speaker it opens (`None` = a "无" boundary, i.e. no manual speaker).
#[derive(Debug, Clone, PartialEq)]
struct Boundary {
    start: f64,
    /// `Some((name, identity))` for a named boundary; `None` for "无".
    speaker: Option<(String, Option<Uuid>)>,
}

/// The attribution outcome for a stretch of a segment: `Some((name, identity))`
/// to reassign to a manual speaker, or `None` to keep the original diarization
/// speaker ("无" range, or before the first boundary).
type Outcome = Option<(String, Option<Uuid>)>;

/// Resolve each annotation's final display name: an enrolled identity's
/// *current* name when `identity_id` is set and still enrolled, otherwise the
/// `display_name` snapshot taken at annotate time. Named annotations whose
/// resolved name is blank are dropped. **Unassigned ("无") boundaries are always
/// kept** — they carry no name but are meaningful boundaries. A missing/
/// unopenable identity library degrades to snapshots only — it never fails the
/// pipeline.
pub fn resolve_annotation_names(
    annotations: &[LiveAnnotation],
    identity_dir: Option<&Path>,
) -> Vec<LiveAnnotation> {
    let identities = identity_dir.and_then(|dir| match IdentityStore::open(dir) {
        Ok(identities) => Some(identities),
        Err(error) => {
            tracing::warn!(error = %error, "could not open identity library; using annotation name snapshots");
            None
        }
    });
    annotations
        .iter()
        .filter_map(|annotation| {
            // A "无" boundary carries no name; keep it verbatim.
            if annotation.unassigned {
                return Some(annotation.clone());
            }
            let current =
                annotation
                    .identity_id
                    .zip(identities.as_ref())
                    .and_then(|(id, identities)| {
                        identities
                            .list()
                            .iter()
                            .find(|i| i.id == id)
                            .map(|i| i.name.clone())
                    });
            let name = current.unwrap_or_else(|| annotation.display_name.clone());
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let mut resolved = annotation.clone();
            resolved.display_name = name.to_string();
            Some(resolved)
        })
        .collect()
}

/// Apply manual live annotations to an assembled meeting's speakers/segments
/// under the timeline-boundary model (see the module docs). Splits segments at
/// interior boundary times and reassigns each sub-segment by the boundary
/// governing its start. Mutates in place *before* persistence, so the stored
/// rows, the minutes pass, and every later read all see the manual attribution.
/// Returns what changed; with no annotation — or none that changes any
/// attribution — nothing is touched (byte-for-byte no-op) and `segments` keeps
/// its exact contents.
///
/// `annotations` must already be name-resolved ([`resolve_annotation_names`]).
pub fn reconcile_annotations(
    meeting_id: Uuid,
    speakers: &mut Vec<Speaker>,
    segments: &mut Vec<TranscriptSegment>,
    annotations: &[LiveAnnotation],
    mic_offset_seconds: f64,
    system_offset_seconds: f64,
) -> AnnotationReconciliation {
    let mut outcome = AnnotationReconciliation::default();
    if annotations.is_empty() || segments.is_empty() {
        return outcome;
    }

    // 0. Drop boundaries with a non-finite start — they can never place on the
    //    timeline. (`end_seconds` is irrelevant in this model.)
    let valid: Vec<&LiveAnnotation> = annotations
        .iter()
        .filter(|a| {
            let ok = a.start_seconds.is_finite();
            if !ok {
                tracing::warn!(
                    annotation_id = %a.id,
                    "skipping live annotation with non-finite boundary time"
                );
            }
            ok
        })
        .collect();
    if valid.is_empty() {
        return outcome;
    }

    // 1. Per-track boundary sequences (sorted; same-time collapsed newest-wins).
    let mic = build_boundaries(&valid, SegmentChannel::Mic);
    let system = build_boundaries(&valid, SegmentChannel::System);
    if mic.is_empty() && system.is_empty() {
        return outcome;
    }

    // 2. Walk segments in order, splitting + attributing into a new list.
    let mut speaker_for_name: BTreeMap<String, Uuid> = BTreeMap::new();
    let mut next_manual_label = 1usize;
    let mut new_segments: Vec<TranscriptSegment> = Vec::with_capacity(segments.len());
    let mut changed = false;

    for segment in segments.iter() {
        let channel = segment.channel.unwrap_or(SegmentChannel::Mic);
        let offset = match channel {
            SegmentChannel::Mic => mic_offset_seconds,
            SegmentChannel::System => system_offset_seconds,
        };
        let boundaries = match channel {
            SegmentChannel::Mic => &mic,
            SegmentChannel::System => &system,
        };
        let seg_start_u = segment.start_seconds + offset;
        let seg_end_u = segment.end_seconds + offset;

        // Sub-range starts (unified): the segment start plus every interior
        // boundary time, each carrying the outcome of the boundary governing it.
        let mut ranges: Vec<(f64, Outcome)> =
            vec![(seg_start_u, governing_outcome(boundaries, seg_start_u))];
        for b in boundaries {
            if b.start > seg_start_u + EDGE_EPSILON && b.start < seg_end_u - EDGE_EPSILON {
                ranges.push((b.start, b.speaker.clone()));
            }
        }
        // Coalesce adjacent sub-ranges with identical outcomes so an interior
        // boundary that does not change the attribution (e.g. a lone "无" with
        // no prior name) causes no split.
        let mut coalesced: Vec<(f64, Outcome)> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if coalesced.last().is_some_and(|last| last.1 == range.1) {
                continue;
            }
            coalesced.push(range);
        }

        // Unchanged segment: a single sub-range that keeps the original speaker.
        if coalesced.len() == 1 && coalesced[0].1.is_none() {
            new_segments.push(segment.clone());
            continue;
        }

        // Split at the interior sub-range starts (converted back to segment-local
        // time), attribute each piece by its sub-range outcome, and drop empty
        // slivers so no zero-length or blank line ever reaches the transcript.
        // Emptiness happens at the split extremes: `split_by_words` repeats the
        // last word index once the words run out (zero-length tail pieces), and
        // `split_by_ratio` can map a boundary to a character index that does not
        // advance (a blank piece). A dropped sliver's time is absorbed into a
        // neighbouring kept piece so the sub-segments stay contiguous with no gap
        // (equivalent to not cutting at a boundary that lands on the very edge).
        let cut_local: Vec<f64> = coalesced[1..].iter().map(|(t, _)| t - offset).collect();
        let raw = split_segment(segment, &cut_local);
        let mut produced: Vec<TranscriptSegment> = Vec::with_capacity(raw.len());
        let mut carry_start: Option<f64> = None;
        for (mut piece, (_, piece_outcome)) in raw.into_iter().zip(coalesced.iter()) {
            let is_empty = piece.end_seconds <= piece.start_seconds + EDGE_EPSILON
                || piece.text.trim().is_empty();
            if is_empty {
                // Absorb this sliver's time into a neighbour: extend the previous
                // kept piece, or (leading empties) carry the earliest start onto
                // the next kept piece.
                match produced.last_mut() {
                    Some(last) if piece.end_seconds > last.end_seconds => {
                        last.end_seconds = piece.end_seconds;
                    }
                    Some(_) => {}
                    None => {
                        carry_start = Some(
                            carry_start.map_or(piece.start_seconds, |s| s.min(piece.start_seconds)),
                        );
                    }
                }
                continue;
            }
            if let Some(start) = carry_start.take() {
                if start < piece.start_seconds {
                    piece.start_seconds = start;
                }
            }
            if let Some((name, identity)) = piece_outcome {
                let target = *speaker_for_name.entry(name.clone()).or_insert_with(|| {
                    let mut speaker = Speaker::new(
                        meeting_id,
                        next_manual_label_for(speakers, &mut next_manual_label),
                    );
                    apply_manual_attribution(&mut speaker, name, *identity);
                    let id = speaker.id;
                    speakers.push(speaker);
                    outcome.new_speakers.push(id);
                    id
                });
                if piece.speaker_id != Some(target) {
                    piece.speaker_id = Some(target);
                    outcome.reassigned_segments.push((piece.id, target));
                    changed = true;
                }
            }
            produced.push(piece);
        }
        // Degenerate guard: an original segment with no non-blank content yields
        // no pieces — keep it verbatim rather than dropping it.
        if produced.is_empty() {
            new_segments.push(segment.clone());
            continue;
        }
        if produced.len() > 1 {
            outcome.split_segments += 1;
            changed = true;
        }
        new_segments.extend(produced);
    }

    // 3. Strict no-op when nothing actually changed (e.g. only "无" boundaries,
    //    or every segment falls before the first boundary): leave `segments`
    //    exactly as it was so downstream reads are byte-for-byte identical.
    if !changed {
        return outcome;
    }

    // 4. Commit the split list, renumbering `seq` to the new dense order.
    for (index, seg) in new_segments.iter_mut().enumerate() {
        seg.seq = index as u32;
    }
    *segments = new_segments;
    outcome
}

/// Build one track's boundary sequence from the resolved annotations: filter to
/// the channel, sort by `start_seconds` (then `created_at`), and collapse any
/// boundaries sharing a start to the newest one (last-write-wins). A "无"
/// boundary contributes `speaker = None`.
fn build_boundaries(valid: &[&LiveAnnotation], channel: SegmentChannel) -> Vec<Boundary> {
    let mut rows: Vec<&LiveAnnotation> = valid
        .iter()
        .copied()
        .filter(|a| a.channel == channel)
        .collect();
    rows.sort_by(|a, b| {
        a.start_seconds
            .partial_cmp(&b.start_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.created_at.cmp(&b.created_at))
    });
    let mut out: Vec<Boundary> = Vec::new();
    for a in rows {
        let speaker = if a.unassigned {
            None
        } else {
            Some((a.display_name.clone(), a.identity_id))
        };
        if let Some(last) = out.last_mut() {
            if (last.start - a.start_seconds).abs() < 1e-9 {
                last.speaker = speaker; // same start → newest wins
                continue;
            }
        }
        out.push(Boundary {
            start: a.start_seconds,
            speaker,
        });
    }
    out
}

/// The outcome governing the unified-timeline point `point`: the speaker of the
/// boundary with the greatest start at/before it, or `None` when that boundary
/// is "无" or no boundary precedes the point.
fn governing_outcome(boundaries: &[Boundary], point: f64) -> Outcome {
    boundaries
        .iter()
        .rev()
        .find(|b| b.start <= point + 1e-9)
        .and_then(|b| b.speaker.clone())
}

/// Split one segment at the given segment-local cut times into contiguous
/// sub-segments (each a fresh row copying the parent's meeting/channel/
/// confidence; `seq` is renumbered by the caller). With word-level timings the
/// cut snaps to the nearest word boundary and the words/text divide with it;
/// without them the text divides by time-proportional character count
/// (approximate). Returns `cut_local.len() + 1` pieces (empty `cut_local` → the
/// segment as a single piece, cloned).
fn split_segment(segment: &TranscriptSegment, cut_local: &[f64]) -> Vec<TranscriptSegment> {
    if cut_local.is_empty() {
        return vec![segment.clone()];
    }
    match segment.words.as_ref() {
        Some(words) if !words.is_empty() => split_by_words(segment, cut_local, words),
        _ => split_by_ratio(segment, cut_local),
    }
}

/// Word-aware split: each cut snaps to the nearest word boundary (the start of
/// the following word), keeping cuts monotonic so pieces stay contiguous.
fn split_by_words(
    segment: &TranscriptSegment,
    cut_local: &[f64],
    words: &[Word],
) -> Vec<TranscriptSegment> {
    let n = words.len();
    // For each cut, the word index k (cut *before* word k) whose boundary is
    // nearest the cut time, constrained to stay after the previous cut.
    let mut cut_indices: Vec<usize> = Vec::with_capacity(cut_local.len());
    let mut lo = 0usize;
    for &cut in cut_local {
        let mut best_k = (lo + 1).min(n);
        let mut best_d = f64::INFINITY;
        for k in (lo + 1)..=n {
            // Boundary position before word k: that word's start, or the last
            // word's end for k == n (all remaining words on the left).
            let pos = if k < n {
                words[k].start
            } else {
                words[n - 1].end
            };
            let d = (pos - cut).abs();
            if d < best_d {
                best_d = d;
                best_k = k;
            }
        }
        cut_indices.push(best_k);
        lo = best_k;
    }

    let mut pieces = Vec::with_capacity(cut_indices.len() + 1);
    let mut start_idx = 0usize;
    let mut prev_time = segment.start_seconds;
    for &k in &cut_indices {
        let end_time = if k < n {
            words[k].start
        } else {
            segment.end_seconds
        };
        pieces.push(make_piece(
            segment,
            prev_time,
            end_time,
            &words[start_idx..k],
        ));
        prev_time = end_time;
        start_idx = k;
    }
    pieces.push(make_piece(
        segment,
        prev_time,
        segment.end_seconds,
        &words[start_idx..],
    ));
    pieces
}

/// Build a word-backed sub-segment for `words[..]` spanning `[start, end]`.
/// Its text is the concatenation of the word surface forms; an empty group
/// yields empty text and no words.
fn make_piece(
    segment: &TranscriptSegment,
    start: f64,
    end: f64,
    words: &[Word],
) -> TranscriptSegment {
    let text: String = words.iter().map(|w| w.word.as_str()).collect();
    let mut piece = TranscriptSegment::new(segment.meeting_id, 0, start, end, text);
    piece.speaker_id = segment.speaker_id;
    piece.confidence = segment.confidence;
    piece.channel = segment.channel;
    piece.words = (!words.is_empty()).then(|| words.to_vec());
    piece
}

/// Wordless split: divide the text by character count proportional to each cut's
/// position within the segment's time span (approximate — there is no word
/// timing to snap to). Cuts stay monotonic in both time and character index.
fn split_by_ratio(segment: &TranscriptSegment, cut_local: &[f64]) -> Vec<TranscriptSegment> {
    let chars: Vec<char> = segment.text.chars().collect();
    let total = chars.len();
    let duration = segment.end_seconds - segment.start_seconds;

    let mut pieces = Vec::with_capacity(cut_local.len() + 1);
    let mut prev_time = segment.start_seconds;
    let mut prev_char = 0usize;
    for &cut in cut_local {
        let fraction = if duration > 0.0 {
            ((cut - segment.start_seconds) / duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let char_index = ((fraction * total as f64).round() as usize).clamp(prev_char, total);
        let text: String = chars[prev_char..char_index].iter().collect();
        pieces.push(make_text_piece(segment, prev_time, cut, text));
        prev_time = cut;
        prev_char = char_index;
    }
    let text: String = chars[prev_char..].iter().collect();
    pieces.push(make_text_piece(
        segment,
        prev_time,
        segment.end_seconds,
        text,
    ));
    pieces
}

/// Build a wordless sub-segment spanning `[start, end]` with the given text.
fn make_text_piece(
    segment: &TranscriptSegment,
    start: f64,
    end: f64,
    text: String,
) -> TranscriptSegment {
    let mut piece = TranscriptSegment::new(segment.meeting_id, 0, start, end, text);
    piece.speaker_id = segment.speaker_id;
    piece.confidence = segment.confidence;
    piece.channel = segment.channel;
    piece.words = None;
    piece
}

/// Write a manual attribution onto a speaker row: the name, the boundary's
/// identity link (when the chip picked an enrolled person), and `manual`
/// provenance (v13). Any earlier verification confidence is cleared — manual
/// always wins and carries no score.
fn apply_manual_attribution(speaker: &mut Speaker, name: &str, identity: Option<Uuid>) {
    speaker.display_name = Some(name.to_string());
    speaker.identity_id = identity;
    speaker.attribution_origin = Some(lumen_core::attribution_origin::MANUAL.to_string());
    speaker.attribution_confidence = None;
}

/// The next free `M{k}` label (skipping any label already taken, defensively).
fn next_manual_label_for(speakers: &[Speaker], next: &mut usize) -> String {
    loop {
        let label = format!("{MANUAL_LABEL_PREFIX}{next}");
        *next += 1;
        if !speakers.iter().any(|s| s.label == label) {
            return label;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn at(created_offset_s: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap()
            + chrono::Duration::seconds(created_offset_s)
    }

    /// A named boundary at `start` on `channel`.
    fn boundary(
        meeting_id: Uuid,
        start: f64,
        channel: SegmentChannel,
        name: &str,
        created_offset_s: i64,
    ) -> LiveAnnotation {
        let mut a = LiveAnnotation::new(meeting_id, start, None, channel, None, name);
        a.created_at = at(created_offset_s);
        a
    }

    /// A "无" boundary at `start` on `channel`.
    fn none_boundary(
        meeting_id: Uuid,
        start: f64,
        channel: SegmentChannel,
        created_offset_s: i64,
    ) -> LiveAnnotation {
        let mut a = LiveAnnotation::none_boundary(meeting_id, start, channel);
        a.created_at = at(created_offset_s);
        a
    }

    /// One cluster speaker + its segments at the given `[start, end]` spans.
    fn cluster(
        meeting_id: Uuid,
        label: &str,
        spans: &[(f64, f64)],
        seq_from: u32,
    ) -> (Speaker, Vec<TranscriptSegment>) {
        let speaker = Speaker::new(meeting_id, label);
        let segments = spans
            .iter()
            .enumerate()
            .map(|(i, &(start, end))| {
                let mut seg =
                    TranscriptSegment::new(meeting_id, seq_from + i as u32, start, end, "…");
                seg.speaker_id = Some(speaker.id);
                seg.channel = Some(SegmentChannel::Mic);
                seg
            })
            .collect();
        (speaker, segments)
    }

    fn name_of(segment: &TranscriptSegment, speakers: &[Speaker]) -> Option<String> {
        speakers
            .iter()
            .find(|s| Some(s.id) == segment.speaker_id)
            .and_then(|s| s.display_name.clone())
    }

    #[test]
    fn no_annotations_is_a_noop() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 2.0)], 0);
        let mut speakers = vec![speaker.clone()];
        let before = (speakers.clone(), segments.clone());

        let outcome =
            reconcile_annotations(meeting_id, &mut speakers, &mut segments, &[], 0.0, 0.0);

        assert_eq!(outcome, AnnotationReconciliation::default());
        assert_eq!((speakers, segments), before);
    }

    /// The user-reported bug: one long diarized segment spanning three
    /// boundaries used to be attributed wholesale to the first speaker. It must
    /// now split at the boundary times: 张宏伟 / 其他 / 张宏伟.
    #[test]
    fn long_segment_splits_at_interior_boundaries_reproducing_the_bug() {
        let meeting_id = Uuid::new_v4();
        // One giant diarized turn [43.5, 230] on the mic track, with enough
        // transcript text to divide across the boundaries (no word timings — the
        // wordless, time-proportional path, as the offline final may produce).
        let speaker = Speaker::new(meeting_id, "S1");
        let original_cluster = speaker.id;
        let mut seg = TranscriptSegment::new(meeting_id, 0, 43.5, 230.0, "话".repeat(200));
        seg.speaker_id = Some(speaker.id);
        seg.channel = Some(SegmentChannel::Mic);
        let mut speakers = vec![speaker];
        let mut segments = vec![seg];
        // Mic boundaries: 张宏伟@0, 其他@86, 张宏伟@96.
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张宏伟", 0),
            boundary(meeting_id, 86.0, SegmentChannel::Mic, "其他", 1),
            boundary(meeting_id, 96.0, SegmentChannel::Mic, "张宏伟", 2),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Split into exactly three sub-segments at 86 and 96.
        assert_eq!(outcome.split_segments, 1);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].start_seconds, 43.5);
        assert_eq!(segments[0].end_seconds, 86.0);
        assert_eq!(segments[1].start_seconds, 86.0);
        assert_eq!(segments[1].end_seconds, 96.0);
        assert_eq!(segments[2].start_seconds, 96.0);
        assert_eq!(segments[2].end_seconds, 230.0);
        // Dense seq renumbering.
        assert_eq!(
            segments.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Attribution follows the governing boundary of each piece.
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张宏伟"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("其他"));
        assert_eq!(name_of(&segments[2], &speakers).as_deref(), Some("张宏伟"));
        // 张宏伟's two pieces share one manual speaker; 其他 has its own.
        assert_eq!(segments[0].speaker_id, segments[2].speaker_id);
        assert_ne!(segments[0].speaker_id, segments[1].speaker_id);
        // No piece is left on the original diarization cluster.
        assert!(segments
            .iter()
            .all(|s| s.speaker_id != Some(original_cluster)));
        // Two manual speakers created, both `manual` provenance.
        assert_eq!(outcome.new_speakers.len(), 2);
        for id in &outcome.new_speakers {
            let s = speakers.iter().find(|s| s.id == *id).unwrap();
            assert_eq!(
                s.attribution_origin.as_deref(),
                Some(lumen_core::attribution_origin::MANUAL)
            );
            assert!(s.label.starts_with('M'));
        }
    }

    /// A boundary exactly at a segment's edge does not split it — it just
    /// governs the whole segment (or, at the trailing edge, the next one).
    #[test]
    fn boundary_on_a_segment_edge_does_not_split() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 10.0), (10.0, 20.0)], 0);
        let mut speakers = vec![speaker];
        // Boundary exactly at 10.0 = the shared edge.
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0),
            boundary(meeting_id, 10.0, SegmentChannel::Mic, "李四", 1),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // No split — still two segments.
        assert_eq!(outcome.split_segments, 0);
        assert_eq!(segments.len(), 2);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("李四"));
    }

    /// A "无" boundary inside a named range splits the segment and drops manual
    /// attribution after it (the piece keeps its original diarization speaker).
    #[test]
    fn none_boundary_ends_a_named_range_and_keeps_original_speaker() {
        let meeting_id = Uuid::new_v4();
        // Enough text to divide across the 15 s boundary (wordless path).
        let speaker = Speaker::new(meeting_id, "S1");
        let original = speaker.id;
        let mut seg = TranscriptSegment::new(meeting_id, 0, 0.0, 30.0, "话".repeat(30));
        seg.speaker_id = Some(speaker.id);
        seg.channel = Some(SegmentChannel::Mic);
        let mut speakers = vec![speaker];
        let mut segments = vec![seg];
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0),
            none_boundary(meeting_id, 15.0, SegmentChannel::Mic, 1),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(outcome.split_segments, 1);
        assert_eq!(segments.len(), 2);
        // First piece: 张三. Second piece ("无" range): back to the diar cluster.
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert_eq!(segments[1].speaker_id, Some(original));
        assert_eq!(name_of(&segments[1], &speakers), None);
    }

    /// A lone "无" boundary with no prior named range changes nothing — strict
    /// no-op even though an annotation exists.
    #[test]
    fn lone_none_boundary_is_a_noop() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 30.0)], 0);
        let mut speakers = vec![speaker.clone()];
        let before = (speakers.clone(), segments.clone());
        let annotations = vec![none_boundary(meeting_id, 15.0, SegmentChannel::Mic, 0)];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(outcome, AnnotationReconciliation::default());
        assert_eq!((speakers, segments), before);
    }

    /// Segments before the first boundary keep their original speaker; the
    /// boundary attributes from its start onward across later segments.
    #[test]
    fn boundaries_partition_segments_by_start_order() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(
            meeting_id,
            "S1",
            &[(0.0, 5.0), (30.0, 35.0), (90.0, 95.0), (120.0, 125.0)],
            0,
        );
        let original = speaker.id;
        let mut speakers = vec![speaker];
        // 张三 from 10s, 李四 from 60s.
        let annotations = vec![
            boundary(meeting_id, 10.0, SegmentChannel::Mic, "张三", 0),
            boundary(meeting_id, 60.0, SegmentChannel::Mic, "李四", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Before the first boundary: untouched. Then 张三, then 李四, 李四.
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].speaker_id, Some(original));
        assert_eq!(name_of(&segments[0], &speakers), None);
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("张三"));
        assert_eq!(name_of(&segments[2], &speakers).as_deref(), Some("李四"));
        assert_eq!(name_of(&segments[3], &speakers).as_deref(), Some("李四"));
    }

    /// The split snaps to the nearest word boundary and divides the words/text.
    #[test]
    fn word_level_split_cuts_at_the_nearest_word_boundary() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        let mut segment = TranscriptSegment::new(meeting_id, 0, 0.0, 8.0, "你好世界再见");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::Mic);
        segment.words = Some(vec![
            Word::new("你", 0.0, 1.0),
            Word::new("好", 1.0, 2.0),
            Word::new("世", 4.0, 5.0),
            Word::new("界", 5.0, 6.0),
            Word::new("再", 6.0, 7.0),
            Word::new("见", 7.0, 8.0),
        ]);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        // 张三@0, 李四 boundary at 3.6s → nearest word boundary is the "世"
        // start at 4.0s (gap 0.4) vs the "好" start at 1.0 (gap 2.6): cut before 世.
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0),
            boundary(meeting_id, 3.6, SegmentChannel::Mic, "李四", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "你好");
        assert_eq!(segments[0].end_seconds, 4.0);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert_eq!(segments[1].text, "世界再见");
        assert_eq!(segments[1].start_seconds, 4.0);
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("李四"));
        // Words divided with the text.
        assert_eq!(segments[0].words.as_ref().unwrap().len(), 2);
        assert_eq!(segments[1].words.as_ref().unwrap().len(), 4);
    }

    /// Without word timings the text splits by time-proportional character
    /// count (approximate) and the pieces are attributed correctly.
    #[test]
    fn wordless_split_divides_text_by_time_proportion() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        // 10 chars over [0,10]; a boundary at 4.0 → 4/10 of the text.
        let mut segment = TranscriptSegment::new(meeting_id, 0, 0.0, 10.0, "零一二三四五六七八九");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::Mic);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0),
            boundary(meeting_id, 4.0, SegmentChannel::Mic, "李四", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "零一二三");
        assert_eq!(segments[1].text, "四五六七八九");
        assert!(segments[0].words.is_none());
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("李四"));
    }

    /// A boundary exactly at a segment's start attributes the whole segment with
    /// no split and no empty leading sliver.
    #[test]
    fn boundary_at_segment_start_attributes_whole_segment_without_empty_piece() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(10.0, 20.0)], 0);
        let mut speakers = vec![speaker];
        let annotations = vec![boundary(meeting_id, 10.0, SegmentChannel::Mic, "张三", 0)];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(outcome.split_segments, 0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "…");
        assert_eq!(segments[0].start_seconds, 10.0);
        assert_eq!(segments[0].end_seconds, 20.0);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
    }

    /// More boundaries than words: the word-run-out tail never emits a
    /// zero-length or blank sub-segment, no word content is lost, and seq stays
    /// dense and contiguous.
    #[test]
    fn more_boundaries_than_words_never_emit_empty_segments() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        // Only two words, but four boundaries land inside the segment.
        let mut segment = TranscriptSegment::new(meeting_id, 0, 0.0, 4.0, "甲乙");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::Mic);
        segment.words = Some(vec![Word::new("甲", 0.0, 1.0), Word::new("乙", 2.0, 3.0)]);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "A", 0),
            boundary(meeting_id, 1.4, SegmentChannel::Mic, "B", 1),
            boundary(meeting_id, 2.4, SegmentChannel::Mic, "C", 2),
            boundary(meeting_id, 3.4, SegmentChannel::Mic, "D", 3),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // No empty/zero-length sub-segment survived…
        assert!(segments
            .iter()
            .all(|s| !s.text.trim().is_empty() && s.end_seconds > s.start_seconds));
        // …no character content was lost…
        let joined: String = segments.iter().map(|s| s.text.clone()).collect();
        assert_eq!(joined, "甲乙");
        // …the pieces stay contiguous and densely renumbered.
        for pair in segments.windows(2) {
            assert!((pair[0].end_seconds - pair[1].start_seconds).abs() < 1e-9);
        }
        assert_eq!(
            segments.iter().map(|s| s.seq).collect::<Vec<_>>(),
            (0..segments.len() as u32).collect::<Vec<_>>()
        );
    }

    /// Wordless mode: a boundary whose proportional character index does not
    /// advance yields a blank middle sliver — it is dropped, its time absorbed
    /// into the previous piece, and no text is lost.
    #[test]
    fn wordless_blank_sliver_is_dropped_and_its_time_absorbed() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        // 10 chars over [0,30]; two boundaries straddle the same char index.
        let mut segment = TranscriptSegment::new(meeting_id, 0, 0.0, 30.0, "零一二三四五六七八九");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::Mic);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        // A[0,14.9] → 5 chars, B[14.9,15.1] → 0 chars (blank), C[15.1,30] → 5.
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "A", 0),
            boundary(meeting_id, 14.9, SegmentChannel::Mic, "B", 1),
            boundary(meeting_id, 15.1, SegmentChannel::Mic, "C", 2),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // The blank B sliver is gone; only the two non-blank pieces remain.
        assert_eq!(segments.len(), 2);
        assert!(segments.iter().all(|s| !s.text.trim().is_empty()));
        assert_eq!(segments[0].text, "零一二三四");
        assert_eq!(segments[1].text, "五六七八九");
        // Contiguous with no gap: B's time was absorbed into A.
        assert!((segments[0].end_seconds - segments[1].start_seconds).abs() < 1e-9);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("A"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("C"));
    }

    /// The boundary's own precise time — lifted onto the unified timeline by the
    /// track offset — is what places the cut, not the offline segment range.
    #[test]
    fn system_segments_are_lifted_by_the_system_offset_before_splitting() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        // System-track segment at 10–20s in the *system WAV's* own timeline; the
        // tap started 3s after t0, so it spans 13–23s unified.
        let mut segment = TranscriptSegment::new(meeting_id, 0, 10.0, 20.0, "……");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::System);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        // 客户A@13 (=segment start unified) and 客户B@18 unified → the cut lands
        // at 18 unified = 15 in WAV-local time.
        let annotations = vec![
            boundary(meeting_id, 13.0, SegmentChannel::System, "客户A", 0),
            boundary(meeting_id, 18.0, SegmentChannel::System, "客户B", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            3.0,
        );

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start_seconds, 10.0);
        assert_eq!(segments[0].end_seconds, 15.0); // 18 unified − 3 offset
        assert_eq!(segments[1].start_seconds, 15.0);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("客户A"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("客户B"));
    }

    /// Boundaries are per-track: a mic boundary never governs a system segment.
    #[test]
    fn boundaries_do_not_cross_tracks() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        let mut segment = TranscriptSegment::new(meeting_id, 0, 0.0, 10.0, "……");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::System);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        let before = (speakers.clone(), segments.clone());
        // A mic boundary only — the system segment must be untouched.
        let annotations = vec![boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0)];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(outcome, AnnotationReconciliation::default());
        assert_eq!((speakers, segments), before);
    }

    /// A non-finite boundary time is skipped; only-corrupt annotations are a
    /// strict no-op.
    #[test]
    fn corrupt_boundary_times_are_ignored() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 20.0)], 0);
        let mut speakers = vec![speaker];
        let annotations = vec![
            boundary(meeting_id, f64::NAN, SegmentChannel::Mic, "坏数据", 5),
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 0),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Only 张三 applied; nothing named 坏数据.
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert!(speakers
            .iter()
            .all(|s| s.display_name.as_deref() != Some("坏数据")));

        // Nothing but a corrupt row → strict no-op.
        let (speaker2, mut segments2) = cluster(meeting_id, "S1", &[(0.0, 2.0)], 0);
        let mut speakers2 = vec![speaker2];
        let before = (speakers2.clone(), segments2.clone());
        let corrupt_only = vec![boundary(
            meeting_id,
            f64::NAN,
            SegmentChannel::Mic,
            "坏数据",
            0,
        )];
        let outcome2 = reconcile_annotations(
            meeting_id,
            &mut speakers2,
            &mut segments2,
            &corrupt_only,
            0.0,
            0.0,
        );
        assert_eq!(outcome2, AnnotationReconciliation::default());
        assert_eq!((speakers2, segments2), before);
    }

    /// Two boundaries at the same start resolve last-write-wins by created_at.
    #[test]
    fn newest_boundary_wins_at_the_same_start() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 5.0)], 0);
        let mut speakers = vec![speaker];
        let annotations = vec![
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "李明", 0),
            boundary(meeting_id, 0.0, SegmentChannel::Mic, "张三", 5),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
    }

    #[test]
    fn manual_attribution_overrides_verification_and_records_provenance() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 5.0)], 0);
        let mut speakers = vec![speaker];
        // The voiceprint pass had auto-identified this cluster…
        speakers[0].display_name = Some("王五".into());
        speakers[0].identity_id = Some(Uuid::new_v4());
        speakers[0].attribution_origin = Some(lumen_core::attribution_origin::VERIFICATION.into());
        speakers[0].attribution_confidence = Some(0.8);
        // …but the user marked an enrolled 李明 from the start.
        let enrolled = Uuid::new_v4();
        let mut b = boundary(meeting_id, 0.0, SegmentChannel::Mic, "李明", 0);
        b.identity_id = Some(enrolled);

        let outcome =
            reconcile_annotations(meeting_id, &mut speakers, &mut segments, &[b], 0.0, 0.0);

        // The segment is reassigned to a fresh manual speaker (manual wins).
        assert_eq!(outcome.new_speakers.len(), 1);
        let manual = speakers
            .iter()
            .find(|s| s.id == outcome.new_speakers[0])
            .unwrap();
        assert_eq!(manual.display_name.as_deref(), Some("李明"));
        assert_eq!(manual.identity_id, Some(enrolled));
        assert_eq!(
            manual.attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::MANUAL)
        );
        assert_eq!(manual.attribution_confidence, None);
        assert_eq!(segments[0].speaker_id, Some(manual.id));
    }

    #[test]
    fn same_name_across_segments_reuses_one_manual_speaker() {
        let meeting_id = Uuid::new_v4();
        // Two diarized clusters, both fully inside 李明's range.
        let (s1, mut segments) = cluster(meeting_id, "S1", &[(10.0, 20.0)], 0);
        let (s2, mut more) = cluster(meeting_id, "S2", &[(30.0, 40.0)], 1);
        segments.append(&mut more);
        let mut speakers = vec![s1, s2];
        let annotations = vec![boundary(meeting_id, 0.0, SegmentChannel::Mic, "李明", 0)];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // One shared manual speaker; both segments reassigned to it.
        assert_eq!(outcome.new_speakers.len(), 1);
        assert_eq!(outcome.reassigned_segments.len(), 2);
        assert_eq!(segments[0].speaker_id, segments[1].speaker_id);
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("李明"));
    }

    #[test]
    fn resolve_keeps_none_boundaries_and_prefers_current_identity_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut identities = IdentityStore::open(dir.path()).unwrap();
        let embedding: Vec<f32> = (0..lumen_identity::EMBEDDING_DIM)
            .map(|i| (i as f32).sin())
            .collect();
        let enrolled = identities.enroll("李明", &embedding, 5000, None).unwrap();
        let enrolled_id = enrolled.id;
        let meeting_id = Uuid::new_v4();

        let mut linked =
            LiveAnnotation::new(meeting_id, 0.0, None, SegmentChannel::Mic, None, "旧名字");
        linked.identity_id = Some(enrolled_id);
        // A "无" boundary must survive resolution untouched despite its blank name.
        let none = LiveAnnotation::none_boundary(meeting_id, 5.0, SegmentChannel::Mic);
        // A blank named boundary is still dropped.
        let blank = LiveAnnotation::new(meeting_id, 3.0, None, SegmentChannel::Mic, None, "  ");

        let resolved = resolve_annotation_names(&[linked, none.clone(), blank], Some(dir.path()));

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].display_name, "李明");
        assert!(resolved[1].unassigned);
        assert_eq!(resolved[1].start_seconds, 5.0);
    }
}
