//! Offline reconciliation of recording-time manual speaker annotations (L2).
//!
//! While a meeting records, the user can mark "who is speaking" on individual
//! live caption lines. Speaker rows do not exist at that point (the offline
//! pipeline creates them after stop), so each mark is persisted as a
//! [`LiveAnnotation`]: a time range on the meeting's **unified timeline**
//! (shared `t0` across tracks) plus the capture track it was made on. After
//! stop, [`reconcile_annotations`] matches those ranges against the diarized
//! segments and applies the manual names.
//!
//! ## Attribution priority
//! Manual annotations always win: reconciliation runs *after* voiceprint
//! auto-identification, and a manual name overwrites whatever the automatic
//! pass assigned. Priority order: manual > verified (voiceprint) >
//! offline_diarization > unknown.
//!
//! ## Annotation kinds
//! A **closed** annotation ("仅此句") carries a time range and applies to
//! segments it overlaps enough (symmetric ratio, see
//! [`ANNOTATION_MIN_OVERLAP_RATIO`]). An **open-ended** annotation
//! ("此句及之后", `end_seconds` NULL) means "this person speaks from here on":
//! it applies to every segment from its start until the next open-ended
//! annotation begins on the same track. A closed annotation is more specific
//! and wins for the segments it covers without ending the open range.
//!
//! ## Timeline conversion
//! Offline segments carry times in their own track's WAV timeline; each
//! track's WAV starts a little after `t0` (the offsets live in the
//! `<meeting-id>.timeline.json` sidecar). A segment is lifted onto the
//! unified timeline by adding its track's offset (mic offset ≈ 0; the system
//! offset covers the tap's later start) before overlap-matching against the
//! annotation ranges.
//!
//! ## Cluster handling
//! A matched segment adopts its annotation's name, but clusters are never
//! blindly renamed: only when *every* segment of a diarized cluster is
//! covered by the same manual name is that cluster's speaker renamed in
//! place. Otherwise the covered segments are split out — reassigned (via the
//! same `speaker_id` mechanism the manual reassign command uses) to a
//! speaker created for the manual name — and the rest of the cluster is left
//! untouched. So one cluster covering two different manual names is split per
//! segment rather than renamed wholesale.
//!
//! Pure logic over already-assembled speakers/segments — no store, no audio,
//! no platform gating — fully unit-testable with stub data.

use std::collections::BTreeMap;
use std::path::Path;

use lumen_core::{LiveAnnotation, SegmentChannel, Speaker, TranscriptSegment};
use lumen_identity::IdentityStore;
use uuid::Uuid;

/// Minimum overlap fraction for a closed annotation to cover a segment. The
/// ratio is **symmetric**: the overlap must reach this fraction of the
/// segment's duration *or* of the annotation's own duration. The second
/// branch is essential: live caption lines split on streaming-ASR endpoints
/// (~1 s silences), while the offline diarizer merges continuous speech into
/// much longer speaker turns — so a correct annotation on one live line often
/// covers only a small slice of the diarized segment it belongs to. Requiring
/// the overlap to reach half of the *segment* alone silently dropped every
/// such annotation.
pub const ANNOTATION_MIN_OVERLAP_RATIO: f64 = 0.5;

/// Label prefix for speaker rows created by reconciliation for manual names.
/// Distinct from the diarization `S{n}` space so a manual speaker can never
/// collide with an engine cluster label (e.g. in embedding persistence, which
/// looks rows up by `S{n}` label).
const MANUAL_LABEL_PREFIX: &str = "M";

/// What [`reconcile_annotations`] changed, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnnotationReconciliation {
    /// Clusters renamed in place (every segment covered by one manual name):
    /// `(speaker_id, manual name)`.
    pub renamed_speakers: Vec<(Uuid, String)>,
    /// Ids of speaker rows created for manual names that split a cluster.
    pub new_speakers: Vec<Uuid>,
    /// Segments moved off their original cluster: `(segment_id, new speaker_id)`.
    pub reassigned_segments: Vec<(Uuid, Uuid)>,
}

/// Resolve each annotation's final display name: an enrolled identity's
/// *current* name when `identity_id` is set and still enrolled, otherwise the
/// `display_name` snapshot taken at annotate time. Annotations whose resolved
/// name is blank are dropped. A missing/unopenable identity library degrades
/// to snapshots only — it never fails the pipeline.
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
/// (see the module docs for the matching, timeline, and cluster-split rules).
/// Mutates in place *before* persistence, so the stored rows, the minutes
/// pass, and every later read all see the manual attribution. Returns what
/// changed; with no matching annotation nothing is touched.
///
/// `annotations` must already be name-resolved ([`resolve_annotation_names`])
/// and are expected oldest-first (the store's list order): among several
/// annotations covering one segment, the newest `created_at` wins.
pub fn reconcile_annotations(
    meeting_id: Uuid,
    speakers: &mut Vec<Speaker>,
    segments: &mut [TranscriptSegment],
    annotations: &[LiveAnnotation],
    mic_offset_seconds: f64,
    system_offset_seconds: f64,
) -> AnnotationReconciliation {
    let mut outcome = AnnotationReconciliation::default();
    if annotations.is_empty() || segments.is_empty() {
        return outcome;
    }

    // 0. Drop annotations with corrupt time bounds (non-finite start/end, or
    //    an end at/before the start) so they can never poison the overlap
    //    arithmetic below. Valid annotations are unaffected.
    let annotations: Vec<LiveAnnotation> = annotations
        .iter()
        .filter(|a| {
            let valid = a.start_seconds.is_finite()
                && a.end_seconds
                    .is_none_or(|end| end.is_finite() && end > a.start_seconds);
            if !valid {
                tracing::warn!(
                    annotation_id = %a.id,
                    "skipping live annotation with non-finite or inverted time range"
                );
            }
            valid
        })
        .cloned()
        .collect();
    if annotations.is_empty() {
        return outcome;
    }

    // 1. Per segment: the manual name attributed to it, if any.
    //    - **Closed** annotations ("仅此句") are range marks: the newest one
    //      (created_at; list order breaks ties) on the same channel whose
    //      range overlaps the segment enough wins.
    //    - **Open-ended** annotations ("此句及之后", `end_seconds` NULL) mean
    //      "this person speaks from here until the next open-ended mark on
    //      this track": the one with the greatest start not after the segment
    //      applies, i.e. a later open-ended mark supersedes an earlier one
    //      from its own start onward. A closed annotation is more specific
    //      than an inherited open-ended range, so it wins for the segments it
    //      covers without terminating the open-ended range for later ones.
    let manual_names: Vec<Option<String>> = segments
        .iter()
        .map(|segment| {
            let channel = segment.channel.unwrap_or(SegmentChannel::Mic);
            let offset = match channel {
                SegmentChannel::Mic => mic_offset_seconds,
                SegmentChannel::System => system_offset_seconds,
            };
            let start = segment.start_seconds + offset;
            let end = segment.end_seconds + offset;
            let closed = annotations
                .iter()
                .filter(|a| {
                    a.channel == channel
                        && a.end_seconds.is_some()
                        && annotation_covers(a, start, end)
                })
                .max_by(|a, b| a.created_at.cmp(&b.created_at));
            if let Some(a) = closed {
                return Some(a.display_name.clone());
            }
            annotations
                .iter()
                .filter(|a| {
                    a.channel == channel && a.end_seconds.is_none() && a.start_seconds <= end
                })
                .max_by(|a, b| {
                    a.start_seconds
                        .partial_cmp(&b.start_seconds)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.created_at.cmp(&b.created_at))
                })
                .map(|a| a.display_name.clone())
        })
        .collect();
    if manual_names.iter().all(Option::is_none) {
        return outcome;
    }

    // 2. Group segment indexes by their diarized cluster.
    let mut clusters: BTreeMap<Option<Uuid>, Vec<usize>> = BTreeMap::new();
    for (index, segment) in segments.iter().enumerate() {
        clusters.entry(segment.speaker_id).or_default().push(index);
    }

    // 3. First pass — whole-cluster renames: every segment of the cluster is
    //    covered and they all agree on one name. The manual name overwrites
    //    any auto-identified display_name (manual wins).
    let mut speaker_for_name: BTreeMap<String, Uuid> = BTreeMap::new();
    for (speaker_id, indexes) in &clusters {
        let Some(speaker_id) = speaker_id else {
            continue; // unattributed segments are handled by the split pass
        };
        let names: Vec<&String> = indexes
            .iter()
            .filter_map(|&i| manual_names[i].as_ref())
            .collect();
        if names.len() != indexes.len() {
            continue; // not fully covered → split below
        }
        let name = names[0];
        if !names.iter().all(|n| *n == name) {
            continue; // two different manual names → split below
        }
        if let Some(speaker) = speakers.iter_mut().find(|s| s.id == *speaker_id) {
            speaker.display_name = Some(name.clone());
            outcome.renamed_speakers.push((*speaker_id, name.clone()));
            speaker_for_name.entry(name.clone()).or_insert(*speaker_id);
        }
    }

    // 4. Second pass — split: every remaining annotated segment is reassigned
    //    to the manual name's speaker (a renamed cluster from pass 3 when the
    //    name matches, otherwise a new `M{k}` row created here). Untouched:
    //    the cluster's other segments.
    let renamed: Vec<Uuid> = outcome
        .renamed_speakers
        .iter()
        .map(|(id, _)| id)
        .copied()
        .collect();
    let mut next_manual_label = 1usize;
    for index in 0..segments.len() {
        let Some(name) = manual_names[index].as_ref() else {
            continue;
        };
        if let Some(speaker_id) = segments[index].speaker_id {
            if renamed.contains(&speaker_id) {
                continue; // already handled by the whole-cluster rename
            }
        }
        let target = match speaker_for_name.get(name) {
            Some(id) => *id,
            None => {
                let mut speaker = Speaker::new(
                    meeting_id,
                    next_manual_label_for(speakers, &mut next_manual_label),
                );
                speaker.display_name = Some(name.clone());
                let id = speaker.id;
                speakers.push(speaker);
                speaker_for_name.insert(name.clone(), id);
                outcome.new_speakers.push(id);
                id
            }
        };
        if segments[index].speaker_id != Some(target) {
            segments[index].speaker_id = Some(target);
            outcome
                .reassigned_segments
                .push((segments[index].id, target));
        }
    }
    outcome
}

/// Does this **closed** annotation's range cover the segment span
/// `[start, end]` (both on the unified timeline) enough to adopt the manual
/// name? The overlap must reach [`ANNOTATION_MIN_OVERLAP_RATIO`] of the
/// segment's duration **or** of the annotation's own duration (symmetric):
/// the second branch keeps a short live-line annotation effective when the
/// offline diarizer merged that speech into a much longer turn, while a mere
/// edge graze (small against both spans) still never matches. Open-ended
/// annotations are handled by the caller's nearest-preceding rule instead.
fn annotation_covers(annotation: &LiveAnnotation, start: f64, end: f64) -> bool {
    let Some(annotation_end) = annotation.end_seconds else {
        return false;
    };
    let segment_length = end - start;
    // Zero-length, inverted, or NaN spans can never be covered.
    if !segment_length.is_finite() || segment_length <= 0.0 {
        return false;
    }
    let overlap = annotation_end.min(end) - annotation.start_seconds.max(start);
    if overlap <= 0.0 {
        return false;
    }
    // The corrupt-bounds filter guarantees annotation_end > start_seconds.
    let annotation_length = annotation_end - annotation.start_seconds;
    overlap / segment_length >= ANNOTATION_MIN_OVERLAP_RATIO
        || overlap / annotation_length >= ANNOTATION_MIN_OVERLAP_RATIO
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

    fn annotation(
        meeting_id: Uuid,
        start: f64,
        end: Option<f64>,
        channel: SegmentChannel,
        name: &str,
        created_offset_s: i64,
    ) -> LiveAnnotation {
        let mut a = LiveAnnotation::new(meeting_id, start, end, channel, None, name);
        a.created_at = Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap()
            + chrono::Duration::seconds(created_offset_s);
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

    #[test]
    fn low_overlap_never_matches_and_half_overlap_does() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 4.0), (10.0, 14.0)], 0);
        let mut speakers = vec![speaker];
        // An edge graze that is small against BOTH spans never matches: 1s of
        // the 4s first segment (25%) and 1s of the 10s annotation (10%).
        // Covers exactly 2s of the 4s second segment (50% = threshold): match.
        let annotations = vec![
            annotation(meeting_id, 3.0, Some(13.0), SegmentChannel::Mic, "李明", 0),
            annotation(meeting_id, 12.0, Some(14.0), SegmentChannel::Mic, "李明", 1),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Partial coverage of the cluster → split, not a rename.
        assert!(outcome.renamed_speakers.is_empty());
        assert_eq!(outcome.new_speakers.len(), 1);
        assert_eq!(outcome.reassigned_segments.len(), 1);
        assert_eq!(outcome.reassigned_segments[0].0, segments[1].id);
        // The unmatched segment keeps its original cluster.
        assert_eq!(segments[0].speaker_id, Some(speakers[0].id));
        // The manual speaker carries the name and a non-engine label.
        let manual = speakers
            .iter()
            .find(|s| s.id == outcome.new_speakers[0])
            .unwrap();
        assert_eq!(manual.display_name.as_deref(), Some("李明"));
        assert_eq!(manual.label, "M1");
        assert_eq!(segments[1].speaker_id, Some(manual.id));
    }

    #[test]
    fn fully_covered_cluster_with_one_name_is_renamed_in_place() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut all) = cluster(meeting_id, "S1", &[(0.0, 2.0), (5.0, 7.0)], 0);
        let (other, mut other_segments) = cluster(meeting_id, "S2", &[(2.0, 5.0)], 2);
        all.append(&mut other_segments);
        // The auto-identify pass had guessed a (wrong) name — manual wins.
        let mut speakers = vec![speaker, other];
        speakers[0].display_name = Some("王五".into());
        let annotations = vec![
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "李明", 0),
            annotation(meeting_id, 5.0, Some(7.0), SegmentChannel::Mic, "李明", 1),
        ];

        let outcome =
            reconcile_annotations(meeting_id, &mut speakers, &mut all, &annotations, 0.0, 0.0);

        assert_eq!(
            outcome.renamed_speakers,
            vec![(speakers[0].id, "李明".to_string())]
        );
        assert!(outcome.new_speakers.is_empty());
        assert!(outcome.reassigned_segments.is_empty());
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));
        // The uncovered other cluster is untouched.
        assert_eq!(speakers[1].display_name, None);
        assert_eq!(all[2].speaker_id, Some(speakers[1].id));
    }

    #[test]
    fn cluster_covering_two_names_is_split_per_segment_not_renamed() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 2.0), (5.0, 7.0)], 0);
        let original = speaker.id;
        let mut speakers = vec![speaker];
        let annotations = vec![
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "李明", 0),
            annotation(meeting_id, 5.0, Some(7.0), SegmentChannel::Mic, "张三", 1),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Never renamed wholesale; both segments split out to per-name rows.
        assert!(outcome.renamed_speakers.is_empty());
        assert_eq!(outcome.new_speakers.len(), 2);
        assert_eq!(outcome.reassigned_segments.len(), 2);
        let s0 = speakers
            .iter()
            .find(|s| Some(s.id) == segments[0].speaker_id)
            .unwrap();
        let s1 = speakers
            .iter()
            .find(|s| Some(s.id) == segments[1].speaker_id)
            .unwrap();
        assert_eq!(s0.display_name.as_deref(), Some("李明"));
        assert_eq!(s1.display_name.as_deref(), Some("张三"));
        // The original cluster row keeps its (unnamed) identity.
        let original = speakers.iter().find(|s| s.id == original).unwrap();
        assert_eq!(original.display_name, None);
    }

    #[test]
    fn system_segments_are_lifted_by_the_system_offset_before_matching() {
        let meeting_id = Uuid::new_v4();
        let speaker = Speaker::new(meeting_id, "S1");
        // System-track segment at 10–12s in the *system WAV's* own timeline;
        // the tap started 3s after t0, so it spans 13–15s unified.
        let mut segment = TranscriptSegment::new(meeting_id, 0, 10.0, 12.0, "…");
        segment.speaker_id = Some(speaker.id);
        segment.channel = Some(SegmentChannel::System);
        let mut speakers = vec![speaker];
        let mut segments = vec![segment];
        // Annotated 13–15s on the unified timeline: only matches when the
        // system offset is applied. Same range on the mic channel must not
        // match a system segment.
        let matching = vec![annotation(
            meeting_id,
            13.0,
            Some(15.0),
            SegmentChannel::System,
            "客户A",
            0,
        )];
        let wrong_channel = vec![annotation(
            meeting_id,
            13.0,
            Some(15.0),
            SegmentChannel::Mic,
            "客户A",
            0,
        )];

        let miss = reconcile_annotations(
            meeting_id,
            &mut speakers.clone(),
            &mut segments.clone(),
            &matching,
            0.0,
            0.0, // no offset: 10–12 vs 13–15 → no overlap
        );
        assert_eq!(miss, AnnotationReconciliation::default());

        let cross = reconcile_annotations(
            meeting_id,
            &mut speakers.clone(),
            &mut segments.clone(),
            &wrong_channel,
            0.0,
            3.0,
        );
        assert_eq!(cross, AnnotationReconciliation::default());

        let hit = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &matching,
            0.0,
            3.0,
        );
        assert_eq!(hit.renamed_speakers.len(), 1);
        assert_eq!(speakers[0].display_name.as_deref(), Some("客户A"));
    }

    #[test]
    fn open_ended_annotation_matches_by_point_containment() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(4.0, 8.0)], 0);
        let mut speakers = vec![speaker];
        // Annotated while the live line was still partial: no end recorded.
        let annotations = vec![annotation(
            meeting_id,
            5.0,
            None,
            SegmentChannel::Mic,
            "李明",
            0,
        )];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(outcome.renamed_speakers.len(), 1);
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));

        // A point outside the segment does not match.
        let mut speakers2 = vec![Speaker::new(meeting_id, "S1")];
        let (_, mut segments2) = cluster(meeting_id, "S1", &[(4.0, 8.0)], 0);
        segments2[0].speaker_id = Some(speakers2[0].id);
        let outside = vec![annotation(
            meeting_id,
            9.0,
            None,
            SegmentChannel::Mic,
            "李明",
            0,
        )];
        let outcome2 = reconcile_annotations(
            meeting_id,
            &mut speakers2,
            &mut segments2,
            &outside,
            0.0,
            0.0,
        );
        assert_eq!(outcome2, AnnotationReconciliation::default());
    }

    /// P1 regression: a live caption line is one streaming-ASR utterance
    /// (a few seconds), but the offline diarizer merges continuous speech
    /// into far longer turns. The annotation's overlap is small against the
    /// *segment* but total against the *annotation* — it must still apply.
    #[test]
    fn short_line_annotation_covers_a_much_longer_diarized_turn() {
        let meeting_id = Uuid::new_v4();
        // One 40s diarized turn; the user annotated a single ~6.5s live line
        // inside it (12.4–18.9 unified, arrival-stamped).
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(10.0, 50.0)], 0);
        let mut speakers = vec![speaker];
        let annotations = vec![annotation(
            meeting_id,
            12.4,
            Some(18.9),
            SegmentChannel::Mic,
            "张三",
            0,
        )];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // 6.5s / 40s = 16% of the segment — the old segment-only rule dropped
        // this. 6.5s / 6.5s = 100% of the annotation — the symmetric rule keeps it.
        assert_eq!(
            outcome.renamed_speakers,
            vec![(speakers[0].id, "张三".to_string())]
        );
        assert_eq!(speakers[0].display_name.as_deref(), Some("张三"));
    }

    /// P3: "此句及之后" — an open-ended annotation applies from its start
    /// until the next open-ended annotation begins on the same track.
    #[test]
    fn open_ended_annotations_partition_the_track_by_start_order() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(
            meeting_id,
            "S1",
            &[(30.0, 35.0), (90.0, 95.0), (120.0, 125.0)],
            0,
        );
        let mut speakers = vec![speaker];
        // Open A (0s, 张三) then open B (60s, 李四): 30s belongs to 张三,
        // 90s and 120s to 李四 (B supersedes A from its own start onward).
        let annotations = vec![
            annotation(meeting_id, 0.0, None, SegmentChannel::Mic, "张三", 0),
            annotation(meeting_id, 60.0, None, SegmentChannel::Mic, "李四", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        let name_of = |segment: &TranscriptSegment, speakers: &[Speaker]| {
            speakers
                .iter()
                .find(|s| Some(s.id) == segment.speaker_id)
                .and_then(|s| s.display_name.clone())
        };
        assert_eq!(name_of(&segments[0], &speakers).as_deref(), Some("张三"));
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("李四"));
        assert_eq!(name_of(&segments[2], &speakers).as_deref(), Some("李四"));
    }

    /// A closed annotation is more specific than an inherited open-ended
    /// range: it wins for the segment it covers, and the open-ended range
    /// resumes for later segments. Segments before the open start are
    /// untouched.
    #[test]
    fn closed_annotation_overrides_open_range_only_where_it_covers() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(
            meeting_id,
            "S1",
            &[(0.0, 5.0), (20.0, 25.0), (40.0, 45.0), (60.0, 65.0)],
            0,
        );
        let mut speakers = vec![speaker];
        let annotations = vec![
            // 李四 from 10s onward…
            annotation(meeting_id, 10.0, None, SegmentChannel::Mic, "李四", 0),
            // …except the 40–45s line, which the user marked 王五 "仅此句".
            annotation(meeting_id, 40.0, Some(45.0), SegmentChannel::Mic, "王五", 1),
        ];

        reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        let name_of = |segment: &TranscriptSegment, speakers: &[Speaker]| {
            speakers
                .iter()
                .find(|s| Some(s.id) == segment.speaker_id)
                .and_then(|s| s.display_name.clone())
        };
        // Before the open start: no manual attribution.
        assert_eq!(name_of(&segments[0], &speakers), None);
        assert_eq!(name_of(&segments[1], &speakers).as_deref(), Some("李四"));
        assert_eq!(name_of(&segments[2], &speakers).as_deref(), Some("王五"));
        assert_eq!(name_of(&segments[3], &speakers).as_deref(), Some("李四"));
    }

    #[test]
    fn corrupt_time_bounds_are_ignored_without_disturbing_valid_annotations() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 2.0), (5.0, 7.0)], 0);
        let mut speakers = vec![speaker];
        // Corrupt rows (edge writes / damaged storage): NaN start, infinite
        // end, and an end at/before the start. Newer than the valid row, so
        // any of them slipping through would beat it in last-write-wins.
        let annotations = vec![
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "李明", 0),
            annotation(
                meeting_id,
                f64::NAN,
                Some(2.0),
                SegmentChannel::Mic,
                "坏数据",
                1,
            ),
            annotation(
                meeting_id,
                0.0,
                Some(f64::INFINITY),
                SegmentChannel::Mic,
                "坏数据",
                2,
            ),
            annotation(meeting_id, 2.0, Some(1.0), SegmentChannel::Mic, "坏数据", 3),
            annotation(meeting_id, 1.0, Some(1.0), SegmentChannel::Mic, "坏数据", 4),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // Only the valid annotation took effect: the covered segment split
        // out under 李明, nothing was attributed to 坏数据.
        assert!(outcome.renamed_speakers.is_empty());
        assert_eq!(outcome.new_speakers.len(), 1);
        assert_eq!(outcome.reassigned_segments.len(), 1);
        let manual = speakers
            .iter()
            .find(|s| s.id == outcome.new_speakers[0])
            .unwrap();
        assert_eq!(manual.display_name.as_deref(), Some("李明"));
        assert_eq!(segments[0].speaker_id, Some(manual.id));
        assert!(speakers
            .iter()
            .all(|s| s.display_name.as_deref() != Some("坏数据")));

        // Nothing but corrupt rows → strict no-op.
        let (speaker2, mut segments2) = cluster(meeting_id, "S1", &[(0.0, 2.0)], 0);
        let mut speakers2 = vec![speaker2];
        let corrupt_only = vec![annotation(
            meeting_id,
            f64::NAN,
            None,
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
    }

    #[test]
    fn newest_annotation_wins_on_the_same_range() {
        let meeting_id = Uuid::new_v4();
        let (speaker, mut segments) = cluster(meeting_id, "S1", &[(0.0, 2.0)], 0);
        let mut speakers = vec![speaker];
        // The user corrected themselves: annotated 李明, then 张三 on the same
        // line. Rows are append-only; the newer created_at wins.
        let annotations = vec![
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "李明", 0),
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "张三", 5),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        assert_eq!(
            outcome.renamed_speakers,
            vec![(speakers[0].id, "张三".to_string())]
        );
        assert_eq!(speakers[0].display_name.as_deref(), Some("张三"));
    }

    #[test]
    fn same_name_across_split_clusters_reuses_one_manual_speaker() {
        let meeting_id = Uuid::new_v4();
        // Two clusters, each only partially covered by the same person.
        let (s1, mut segments) = cluster(meeting_id, "S1", &[(0.0, 2.0), (2.0, 4.0)], 0);
        let (s2, mut more) = cluster(meeting_id, "S2", &[(4.0, 6.0), (6.0, 8.0)], 2);
        segments.append(&mut more);
        let mut speakers = vec![s1, s2];
        let annotations = vec![
            annotation(meeting_id, 0.0, Some(2.0), SegmentChannel::Mic, "李明", 0),
            annotation(meeting_id, 4.0, Some(6.0), SegmentChannel::Mic, "李明", 1),
        ];

        let outcome = reconcile_annotations(
            meeting_id,
            &mut speakers,
            &mut segments,
            &annotations,
            0.0,
            0.0,
        );

        // One shared manual speaker, both covered segments moved onto it.
        assert_eq!(outcome.new_speakers.len(), 1);
        assert_eq!(outcome.reassigned_segments.len(), 2);
        assert_eq!(segments[0].speaker_id, segments[2].speaker_id);
        // Uncovered segments stay on their original clusters.
        assert_eq!(segments[1].speaker_id, Some(speakers[0].id));
        assert_eq!(segments[3].speaker_id, Some(speakers[1].id));
    }

    #[test]
    fn resolve_prefers_current_identity_name_and_falls_back_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut identities = IdentityStore::open(dir.path()).unwrap();
        let embedding: Vec<f32> = (0..lumen_identity::EMBEDDING_DIM)
            .map(|i| (i as f32).sin())
            .collect();
        let enrolled = identities.enroll("李明", &embedding, 5000, None).unwrap();
        let enrolled_id = enrolled.id;
        let meeting_id = Uuid::new_v4();

        let mut linked = LiveAnnotation::new(
            meeting_id,
            0.0,
            Some(1.0),
            SegmentChannel::Mic,
            None,
            "旧名字",
        );
        linked.identity_id = Some(enrolled_id);
        // identity_id points at nothing (identity removed since) → snapshot.
        let mut stale = LiveAnnotation::new(
            meeting_id,
            1.0,
            Some(2.0),
            SegmentChannel::Mic,
            None,
            "客户A",
        );
        stale.identity_id = Some(Uuid::new_v4());
        // Ad-hoc typed name, whitespace trimmed; blank names are dropped.
        let adhoc = LiveAnnotation::new(
            meeting_id,
            2.0,
            Some(3.0),
            SegmentChannel::Mic,
            None,
            " 张三 ",
        );
        let blank =
            LiveAnnotation::new(meeting_id, 3.0, Some(4.0), SegmentChannel::Mic, None, "  ");

        let resolved = resolve_annotation_names(&[linked, stale, adhoc, blank], Some(dir.path()));

        let names: Vec<&str> = resolved.iter().map(|a| a.display_name.as_str()).collect();
        assert_eq!(names, vec!["李明", "客户A", "张三"]);

        // No identity dir at all → snapshots only, still no failure.
        let snapshot_only = resolve_annotation_names(
            &[LiveAnnotation::new(
                meeting_id,
                0.0,
                None,
                SegmentChannel::Mic,
                None,
                "王五",
            )],
            None,
        );
        assert_eq!(snapshot_only[0].display_name, "王五");
    }
}
