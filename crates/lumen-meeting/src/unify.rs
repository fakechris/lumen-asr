//! Cross-track speaker unification for dual-track meetings (L4b, offline).
//!
//! Dual-track recordings diarize the mic and system WAVs independently, so the
//! same person can end up as two speaker rows — e.g. the meeting is played
//! through a loudspeaker and the local mic picks the remote voice up again, or
//! one enrolled person is recognized on both tracks. This pass runs at the very
//! end of the offline pipeline (after voiceprint auto-identification and manual
//! annotation reconciliation, before persistence) and merges a mic-track
//! speaker with a system-track speaker **only on strong evidence**:
//!
//! 1. **Same verified identity** — both rows were auto-identified
//!    (`attribution_origin = verification`) against the *same* enrolled
//!    `identity_id`.
//! 2. **Same manual attribution** — both rows carry `manual` attribution with
//!    the same `identity_id`, or (when neither has an identity link) the same
//!    non-empty `display_name`.
//! 3. **Strong echo evidence** — the echo-suppression diagnostics sidecar
//!    (`<stem>.echo_suppression.json`) shows at least
//!    [`ECHO_UNIFY_MIN_PAIRS`] mic segments of speaker X suppressed as echo
//!    copies of system speaker Y, and those pairs make up at least
//!    [`ECHO_UNIFY_MIN_RATIO`] of X's pre-suppression segment count — the
//!    residual (unsuppressed) mic segments of X are then almost certainly Y
//!    picked up through the loudspeaker. A missing or unreadable sidecar means
//!    this evidence is simply absent — never a merge, never an error.
//!
//! Raw centroid cosine similarity is deliberately **not** evidence: merging
//! two unknown people wrongly is far worse than showing a duplicate
//! participant, so without one of the three signals above nothing is merged.
//!
//! **Conflict guard**: when both rows carry a non-empty `display_name` and the
//! names differ, the pair is never merged — even with echo evidence — because
//! each side was positively attributed to a different person.
//!
//! Merging keeps the row with the stronger provenance (manual > verification >
//! none; ties keep the row with more segments), reassigns every segment of the
//! other row onto it, and drops the now-empty row. Each speaker participates
//! in at most one merge; candidate pairs are applied greedily strongest
//! evidence first (manual > verification > echo).
//!
//! Pure logic over already-assembled speakers/segments — no store, no audio,
//! no platform gating — fully unit-testable with stub data.

use std::collections::BTreeMap;

use lumen_core::{attribution_origin, SegmentChannel, Speaker, TranscriptSegment};
use uuid::Uuid;

use crate::assemble::speaker_label;

/// Evidence 3: minimum number of suppressed mic↔system echo pairs between the
/// two speakers before the residual mic segments are attributed to the system
/// speaker.
pub const ECHO_UNIFY_MIN_PAIRS: usize = 2;

/// Evidence 3: minimum fraction of the mic speaker's pre-suppression segments
/// that were suppressed as echoes of the system speaker.
pub const ECHO_UNIFY_MIN_RATIO: f64 = 0.5;

/// Which of the three admissible evidences justified a merge. Ordered by
/// strength (strongest first) for the greedy pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifyEvidence {
    /// Both rows manually attributed to the same person.
    Manual,
    /// Both rows voiceprint-verified against the same enrolled identity.
    VerifiedIdentity,
    /// Strong echo-suppression evidence (see module docs).
    Echo,
}

/// One applied merge, for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerUnification {
    /// The row that was removed (its segments were moved).
    pub removed_speaker_id: Uuid,
    /// Engine label of the removed row (safe to log; never a name).
    pub removed_label: String,
    /// The surviving row.
    pub into_speaker_id: Uuid,
    /// Engine label of the surviving row.
    pub into_label: String,
    pub evidence: UnifyEvidence,
    /// How many segments were reassigned onto the surviving row.
    pub moved_segments: usize,
}

/// Echo evidence 3 for one mic-speaker/system-speaker pair, already mapped
/// from the diagnostics sidecar onto speaker rows (see
/// [`echo_evidence_from_diagnostics`]).
#[derive(Debug, Clone, PartialEq)]
pub struct EchoSpeakerEvidence {
    pub mic_speaker_id: Uuid,
    pub system_speaker_id: Uuid,
    /// Mic segments of this speaker suppressed as echo copies of this system
    /// speaker.
    pub suppressed_pairs: usize,
    /// The mic speaker's total segment count *before* echo suppression.
    pub mic_segments_before_suppression: usize,
}

/// Map the echo-suppression diagnostics sidecar onto speaker rows: each
/// suppressed entry names the mic and system engine speaker ids (in each
/// track's own id space), which resolve to rows via the `S{id+1}` label — the
/// system id shifted by the same `system_speaker_id_offset` that
/// `merge_tracks` applied. Entries missing speaker ids (a pre-v2 sidecar) or
/// whose label no longer resolves (e.g. a fully suppressed mic speaker has no
/// row) contribute nothing — evidence absent, fail open to "no merge".
pub(crate) fn echo_evidence_from_diagnostics(
    diagnostics: &crate::echo::EchoDiagnostics,
    speakers: &[Speaker],
    system_speaker_id_offset: u32,
) -> Vec<EchoSpeakerEvidence> {
    let row_for_label =
        |label: &str| -> Option<Uuid> { speakers.iter().find(|s| s.label == label).map(|s| s.id) };
    // (mic row, system row, mic engine id) → suppressed pair count. The engine
    // id rides along to look up the pre-suppression denominator.
    let mut pairs: BTreeMap<(Uuid, Uuid, u32), usize> = BTreeMap::new();
    for entry in &diagnostics.entries {
        if !entry.suppressed {
            continue;
        }
        let (Some(mic_engine), Some(system_engine)) = (entry.mic_speaker, entry.system_speaker)
        else {
            continue;
        };
        let Some(mic_id) = row_for_label(&speaker_label(mic_engine)) else {
            continue;
        };
        let system_label = speaker_label(system_engine.saturating_add(system_speaker_id_offset));
        let Some(system_id) = row_for_label(&system_label) else {
            continue;
        };
        *pairs.entry((mic_id, system_id, mic_engine)).or_default() += 1;
    }
    pairs
        .into_iter()
        .filter_map(|((mic_id, system_id, mic_engine), suppressed_pairs)| {
            // No denominator (pre-v2 sidecar) → evidence incomplete → absent.
            let before = diagnostics.mic_speaker_segments.get(&mic_engine).copied()?;
            Some(EchoSpeakerEvidence {
                mic_speaker_id: mic_id,
                system_speaker_id: system_id,
                suppressed_pairs,
                mic_segments_before_suppression: before,
            })
        })
        .collect()
}

/// Merge mic-track and system-track speaker rows that the three admissible
/// evidences say are one person (see module docs). Mutates
/// `speakers`/`segments` in place *before* persistence; returns the applied
/// merges. Single-track meetings (no system-channel segments) are a no-op.
pub fn unify_cross_track_speakers(
    speakers: &mut Vec<Speaker>,
    segments: &mut [TranscriptSegment],
    echo_evidence: &[EchoSpeakerEvidence],
) -> Vec<SpeakerUnification> {
    let tracks = speaker_tracks(segments);
    let mut segment_counts: BTreeMap<Uuid, usize> = BTreeMap::new();
    for segment in segments.iter() {
        if let Some(id) = segment.speaker_id {
            *segment_counts.entry(id).or_default() += 1;
        }
    }

    // Candidate pairs, strongest evidence first (greedy: one merge per
    // speaker, a stronger evidence consumes the speaker before a weaker one
    // sees it).
    let mut candidates: Vec<(Uuid, Uuid, UnifyEvidence)> = Vec::new();
    candidates.extend(attribution_pairs(speakers, &tracks, UnifyEvidence::Manual));
    candidates.extend(attribution_pairs(
        speakers,
        &tracks,
        UnifyEvidence::VerifiedIdentity,
    ));
    candidates.extend(echo_pairs(echo_evidence, &tracks));

    let mut used: Vec<Uuid> = Vec::new();
    let mut applied = Vec::new();
    for (mic_id, system_id, evidence) in candidates {
        if used.contains(&mic_id) || used.contains(&system_id) {
            continue; // one merge per speaker
        }
        let Some(mic_index) = speakers.iter().position(|s| s.id == mic_id) else {
            continue;
        };
        let Some(system_index) = speakers.iter().position(|s| s.id == system_id) else {
            continue;
        };
        // Conflict guard: two different non-empty names means two different
        // people were positively attributed — never merge, whatever the
        // evidence.
        let mic_name = trimmed_name(&speakers[mic_index]);
        let system_name = trimmed_name(&speakers[system_index]);
        if let (Some(a), Some(b)) = (&mic_name, &system_name) {
            if a != b {
                tracing::info!(
                    mic_label = %speakers[mic_index].label,
                    system_label = %speakers[system_index].label,
                    evidence = ?evidence,
                    "cross-track unification skipped: both speakers carry different names"
                );
                continue;
            }
        }
        // Keep the row with the stronger provenance (manual > verification >
        // none); ties keep the row with more segments, and a full tie keeps
        // the mic row (deterministic). The kept row's label, name, provenance,
        // and (later-persisted) centroid embedding all survive unchanged —
        // the simple choice, since the dropped row's label is never persisted.
        let mic_rank = provenance_rank(&speakers[mic_index]);
        let system_rank = provenance_rank(&speakers[system_index]);
        let mic_count = segment_counts.get(&mic_id).copied().unwrap_or(0);
        let system_count = segment_counts.get(&system_id).copied().unwrap_or(0);
        let keep_mic = (mic_rank, mic_count) >= (system_rank, system_count);
        let (kept_index, removed_index) = if keep_mic {
            (mic_index, system_index)
        } else {
            (system_index, mic_index)
        };
        let kept_id = speakers[kept_index].id;
        let removed = speakers.remove(removed_index);
        let mut moved = 0usize;
        for segment in segments.iter_mut() {
            if segment.speaker_id == Some(removed.id) {
                segment.speaker_id = Some(kept_id);
                moved += 1;
            }
        }
        let kept_count = segment_counts.get(&kept_id).copied().unwrap_or(0);
        segment_counts.insert(kept_id, kept_count + moved);
        segment_counts.remove(&removed.id);
        used.push(mic_id);
        used.push(system_id);
        let kept_label = speakers
            .iter()
            .find(|s| s.id == kept_id)
            .map(|s| s.label.clone())
            .unwrap_or_default();
        tracing::info!(
            removed_label = %removed.label,
            into_label = %kept_label,
            evidence = ?evidence,
            moved_segments = moved,
            "cross-track speakers unified"
        );
        applied.push(SpeakerUnification {
            removed_speaker_id: removed.id,
            removed_label: removed.label,
            into_speaker_id: kept_id,
            into_label: kept_label,
            evidence,
            moved_segments: moved,
        });
    }
    applied
}

/// Each speaker's capture track, inferred from its segments' channels
/// (`None` reads as mic, matching storage semantics). Speakers with no
/// segments or with segments on both tracks are excluded — their track is
/// unknown, so they can never pair.
fn speaker_tracks(segments: &[TranscriptSegment]) -> BTreeMap<Uuid, SegmentChannel> {
    let mut tracks: BTreeMap<Uuid, Option<SegmentChannel>> = BTreeMap::new();
    for segment in segments {
        let Some(speaker_id) = segment.speaker_id else {
            continue;
        };
        let channel = segment.channel.unwrap_or(SegmentChannel::Mic);
        match tracks.get(&speaker_id) {
            None => {
                tracks.insert(speaker_id, Some(channel));
            }
            Some(Some(existing)) if *existing != channel => {
                tracks.insert(speaker_id, None); // mixed → unknown
            }
            _ => {}
        }
    }
    tracks
        .into_iter()
        .filter_map(|(id, channel)| channel.map(|c| (id, c)))
        .collect()
}

/// Attribution key for evidences 1–2: the identity link when present,
/// otherwise (manual only) the non-empty display name.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
enum AttributionKey {
    Identity(Uuid),
    Name(String),
}

/// Evidence 1–2 pairing: mic-track and system-track speakers whose
/// attribution (of the given origin) points at the same person. Only
/// unambiguous 1↔1 matches pair — two same-track speakers claiming one
/// identity is a diarization anomaly this pass must not guess about.
fn attribution_pairs(
    speakers: &[Speaker],
    tracks: &BTreeMap<Uuid, SegmentChannel>,
    evidence: UnifyEvidence,
) -> Vec<(Uuid, Uuid, UnifyEvidence)> {
    let origin = match evidence {
        UnifyEvidence::Manual => attribution_origin::MANUAL,
        UnifyEvidence::VerifiedIdentity => attribution_origin::VERIFICATION,
        UnifyEvidence::Echo => return Vec::new(),
    };
    let mut mic: BTreeMap<AttributionKey, Vec<Uuid>> = BTreeMap::new();
    let mut system: BTreeMap<AttributionKey, Vec<Uuid>> = BTreeMap::new();
    for speaker in speakers {
        if speaker.attribution_origin.as_deref() != Some(origin) {
            continue;
        }
        let key = match (speaker.identity_id, evidence) {
            (Some(identity), _) => AttributionKey::Identity(identity),
            // A verification hit always links an identity; without one the
            // evidence does not apply.
            (None, UnifyEvidence::VerifiedIdentity) => continue,
            (None, _) => match trimmed_name(speaker) {
                Some(name) => AttributionKey::Name(name),
                None => continue,
            },
        };
        match tracks.get(&speaker.id) {
            Some(SegmentChannel::Mic) => mic.entry(key).or_default().push(speaker.id),
            Some(SegmentChannel::System) => system.entry(key).or_default().push(speaker.id),
            None => {}
        }
    }
    let mut pairs = Vec::new();
    for (key, mic_ids) in &mic {
        let Some(system_ids) = system.get(key) else {
            continue;
        };
        if let ([mic_id], [system_id]) = (mic_ids.as_slice(), system_ids.as_slice()) {
            pairs.push((*mic_id, *system_id, evidence));
        }
    }
    pairs
}

/// Evidence 3 pairing: qualifying echo evidence (pair count and ratio
/// thresholds, correct tracks). A mic speaker with two equally strong system
/// partners is ambiguous and pairs with neither.
fn echo_pairs(
    evidence: &[EchoSpeakerEvidence],
    tracks: &BTreeMap<Uuid, SegmentChannel>,
) -> Vec<(Uuid, Uuid, UnifyEvidence)> {
    let mut by_mic: BTreeMap<Uuid, Vec<&EchoSpeakerEvidence>> = BTreeMap::new();
    for entry in evidence {
        if entry.suppressed_pairs < ECHO_UNIFY_MIN_PAIRS
            || entry.mic_segments_before_suppression == 0
        {
            continue;
        }
        let ratio = entry.suppressed_pairs as f64 / entry.mic_segments_before_suppression as f64;
        if ratio < ECHO_UNIFY_MIN_RATIO {
            continue;
        }
        // Defensive: the pair must actually span mic → system.
        if tracks.get(&entry.mic_speaker_id) != Some(&SegmentChannel::Mic)
            || tracks.get(&entry.system_speaker_id) != Some(&SegmentChannel::System)
        {
            continue;
        }
        by_mic.entry(entry.mic_speaker_id).or_default().push(entry);
    }
    let mut pairs = Vec::new();
    for (mic_id, mut entries) in by_mic {
        entries.sort_by(|a, b| b.suppressed_pairs.cmp(&a.suppressed_pairs));
        if entries.len() > 1 && entries[0].suppressed_pairs == entries[1].suppressed_pairs {
            continue; // two equally strong partners → ambiguous, merge neither
        }
        pairs.push((mic_id, entries[0].system_speaker_id, UnifyEvidence::Echo));
    }
    pairs
}

fn trimmed_name(speaker: &Speaker) -> Option<String> {
    speaker
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Provenance strength for choosing the surviving row.
fn provenance_rank(speaker: &Speaker) -> u8 {
    match speaker.attribution_origin.as_deref() {
        Some(attribution_origin::MANUAL) => 2,
        Some(attribution_origin::VERIFICATION) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A speaker plus `n` segments on the given channel.
    fn speaker_with_segments(
        meeting_id: Uuid,
        label: &str,
        channel: SegmentChannel,
        n: usize,
        seq_from: u32,
    ) -> (Speaker, Vec<TranscriptSegment>) {
        let speaker = Speaker::new(meeting_id, label);
        let segments = (0..n)
            .map(|i| {
                let start = f64::from(seq_from) + i as f64;
                let mut segment = TranscriptSegment::new(
                    meeting_id,
                    seq_from + i as u32,
                    start,
                    start + 0.9,
                    "…",
                );
                segment.speaker_id = Some(speaker.id);
                segment.channel = Some(channel);
                segment
            })
            .collect();
        (speaker, segments)
    }

    fn verified(speaker: &mut Speaker, name: &str, identity: Uuid) {
        speaker.display_name = Some(name.to_string());
        speaker.identity_id = Some(identity);
        speaker.attribution_origin = Some(attribution_origin::VERIFICATION.to_string());
        speaker.attribution_confidence = Some(0.9);
    }

    fn manual(speaker: &mut Speaker, name: &str, identity: Option<Uuid>) {
        speaker.display_name = Some(name.to_string());
        speaker.identity_id = identity;
        speaker.attribution_origin = Some(attribution_origin::MANUAL.to_string());
        speaker.attribution_confidence = None;
    }

    /// Standard fixture: one mic speaker (2 segments) + one system speaker
    /// (3 segments).
    fn dual_track() -> (Uuid, Vec<Speaker>, Vec<TranscriptSegment>) {
        let meeting_id = Uuid::new_v4();
        let (mic, mut segments) =
            speaker_with_segments(meeting_id, "S1", SegmentChannel::Mic, 2, 0);
        let (system, mut more) =
            speaker_with_segments(meeting_id, "S2", SegmentChannel::System, 3, 2);
        segments.append(&mut more);
        (meeting_id, vec![mic, system], segments)
    }

    #[test]
    fn same_verified_identity_across_tracks_merges() {
        let (_, mut speakers, mut segments) = dual_track();
        let identity = Uuid::new_v4();
        verified(&mut speakers[0], "李明", identity);
        verified(&mut speakers[1], "李明", identity);
        let system_id = speakers[1].id;

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &[]);

        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::VerifiedIdentity);
        // Equal provenance rank → the row with more segments (system, 3) wins.
        assert_eq!(merges[0].into_speaker_id, system_id);
        assert_eq!(merges[0].moved_segments, 2);
        assert_eq!(speakers.len(), 1, "participant list shrinks to one person");
        assert!(segments.iter().all(|s| s.speaker_id == Some(system_id)));
        // Surviving provenance is intact.
        assert_eq!(
            speakers[0].attribution_origin.as_deref(),
            Some(attribution_origin::VERIFICATION)
        );
        assert_eq!(speakers[0].identity_id, Some(identity));
    }

    #[test]
    fn different_verified_identities_never_merge() {
        let (_, mut speakers, mut segments) = dual_track();
        verified(&mut speakers[0], "李明", Uuid::new_v4());
        verified(&mut speakers[1], "张三", Uuid::new_v4());

        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &[]).is_empty());
        assert_eq!(speakers.len(), 2);
    }

    #[test]
    fn same_manual_identity_merges() {
        let (_, mut speakers, mut segments) = dual_track();
        let identity = Uuid::new_v4();
        manual(&mut speakers[0], "李明", Some(identity));
        manual(&mut speakers[1], "李明", Some(identity));
        let mic_id = speakers[0].id;
        let system_id = speakers[1].id;

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &[]);
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::Manual);
        // Equal (manual) rank → more segments wins (system).
        assert_eq!(merges[0].into_speaker_id, system_id);
        assert!(!speakers.iter().any(|s| s.id == mic_id));

        // Mixed origins never pair: manual on one track + verification on the
        // other is not "both manual" nor "both verified", even with one
        // identity — the evidence definitions are strict.
        let (_, mut speakers2, mut segments2) = dual_track();
        let identity2 = Uuid::new_v4();
        manual(&mut speakers2[0], "李明", Some(identity2));
        verified(&mut speakers2[1], "李明", identity2);
        assert!(unify_cross_track_speakers(&mut speakers2, &mut segments2, &[]).is_empty());
        assert_eq!(speakers2.len(), 2);
    }

    #[test]
    fn stronger_provenance_survives_over_more_segments() {
        // Echo evidence pairs a manually named mic speaker (2 segments) with
        // an unnamed system speaker (3 segments): the manual row survives —
        // provenance rank beats segment count.
        let (_, mut speakers, mut segments) = dual_track();
        manual(&mut speakers[0], "李明", None);
        let mic_id = speakers[0].id;
        let system_id = speakers[1].id;
        let evidence = vec![EchoSpeakerEvidence {
            mic_speaker_id: mic_id,
            system_speaker_id: system_id,
            suppressed_pairs: 3,
            mic_segments_before_suppression: 4,
        }];

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &evidence);
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::Echo);
        assert_eq!(merges[0].into_speaker_id, mic_id);
        assert_eq!(merges[0].moved_segments, 3);
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));
        assert_eq!(
            speakers[0].attribution_origin.as_deref(),
            Some(attribution_origin::MANUAL)
        );
        assert!(segments.iter().all(|s| s.speaker_id == Some(mic_id)));
    }

    #[test]
    fn same_manual_name_without_identity_merges() {
        let (_, mut speakers, mut segments) = dual_track();
        manual(&mut speakers[0], "客户A", None);
        manual(&mut speakers[1], "客户A", None);

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &[]);
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::Manual);
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].display_name.as_deref(), Some("客户A"));
    }

    #[test]
    fn no_evidence_never_merges_even_for_similar_voices() {
        // Two unnamed clusters — their centroids may be arbitrarily similar,
        // but centroid similarity is not admissible evidence.
        let (_, mut speakers, mut segments) = dual_track();
        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &[]).is_empty());
        assert_eq!(speakers.len(), 2);
    }

    #[test]
    fn strong_echo_evidence_merges_residual_mic_speaker_into_system_speaker() {
        let (_, mut speakers, mut segments) = dual_track();
        let mic_id = speakers[0].id;
        let system_id = speakers[1].id;
        // 3 of the mic speaker's 5 pre-suppression segments were suppressed as
        // echoes of the system speaker: pairs ≥ 2 and ratio 0.6 ≥ 0.5.
        let evidence = vec![EchoSpeakerEvidence {
            mic_speaker_id: mic_id,
            system_speaker_id: system_id,
            suppressed_pairs: 3,
            mic_segments_before_suppression: 5,
        }];

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &evidence);
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::Echo);
        // Both unnamed (rank 0) → more segments (system) survives.
        assert_eq!(merges[0].into_speaker_id, system_id);
        assert_eq!(speakers.len(), 1);
        assert!(segments.iter().all(|s| s.speaker_id == Some(system_id)));
    }

    #[test]
    fn weak_echo_evidence_never_merges() {
        let (_, mut speakers, mut segments) = dual_track();
        let mic_id = speakers[0].id;
        let system_id = speakers[1].id;
        // Only one suppressed pair (< ECHO_UNIFY_MIN_PAIRS).
        let one_pair = vec![EchoSpeakerEvidence {
            mic_speaker_id: mic_id,
            system_speaker_id: system_id,
            suppressed_pairs: 1,
            mic_segments_before_suppression: 2,
        }];
        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &one_pair).is_empty());
        // Enough pairs but a low share of the speaker's segments.
        let low_ratio = vec![EchoSpeakerEvidence {
            mic_speaker_id: mic_id,
            system_speaker_id: system_id,
            suppressed_pairs: 2,
            mic_segments_before_suppression: 10,
        }];
        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &low_ratio).is_empty());
        assert_eq!(speakers.len(), 2);
    }

    #[test]
    fn conflicting_names_block_the_merge_even_with_echo_evidence() {
        let (_, mut speakers, mut segments) = dual_track();
        // Each side was positively attributed to a different person.
        manual(&mut speakers[0], "李明", None);
        verified(&mut speakers[1], "张三", Uuid::new_v4());
        let evidence = vec![EchoSpeakerEvidence {
            mic_speaker_id: speakers[0].id,
            system_speaker_id: speakers[1].id,
            suppressed_pairs: 4,
            mic_segments_before_suppression: 5,
        }];

        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &evidence).is_empty());
        assert_eq!(speakers.len(), 2);
    }

    #[test]
    fn single_track_meeting_is_a_noop() {
        let meeting_id = Uuid::new_v4();
        // Legacy single-track: no channel tags at all.
        let (mut s1, mut segments) =
            speaker_with_segments(meeting_id, "S1", SegmentChannel::Mic, 2, 0);
        let (mut s2, mut more) = speaker_with_segments(meeting_id, "S2", SegmentChannel::Mic, 2, 2);
        for segment in segments.iter_mut().chain(more.iter_mut()) {
            segment.channel = None;
        }
        segments.append(&mut more);
        let identity = Uuid::new_v4();
        verified(&mut s1, "李明", identity);
        verified(&mut s2, "李明", identity);
        let mut speakers = vec![s1, s2];
        let before = speakers.clone();

        // Even a (bogus) shared identity cannot pair two same-track speakers.
        assert!(unify_cross_track_speakers(&mut speakers, &mut segments, &[]).is_empty());
        assert_eq!(speakers, before);
    }

    #[test]
    fn each_speaker_merges_at_most_once_and_manual_beats_echo() {
        let meeting_id = Uuid::new_v4();
        let (mut mic, mut segments) =
            speaker_with_segments(meeting_id, "S1", SegmentChannel::Mic, 2, 0);
        let (mut system_a, mut more_a) =
            speaker_with_segments(meeting_id, "S2", SegmentChannel::System, 3, 2);
        let (system_b, mut more_b) =
            speaker_with_segments(meeting_id, "S3", SegmentChannel::System, 1, 5);
        segments.append(&mut more_a);
        segments.append(&mut more_b);
        // Manual evidence links mic ↔ system A…
        manual(&mut mic, "李明", None);
        manual(&mut system_a, "李明", None);
        // …while echo evidence (weaker) links the same mic speaker to system B.
        let evidence = vec![EchoSpeakerEvidence {
            mic_speaker_id: mic.id,
            system_speaker_id: system_b.id,
            suppressed_pairs: 3,
            mic_segments_before_suppression: 4,
        }];
        let system_a_id = system_a.id;
        let system_b_id = system_b.id;
        let mut speakers = vec![mic, system_a, system_b];

        let merges = unify_cross_track_speakers(&mut speakers, &mut segments, &evidence);

        // Only the manual merge applied; the mic speaker is consumed, so the
        // echo pair is skipped and system B stays a separate participant.
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].evidence, UnifyEvidence::Manual);
        assert_eq!(merges[0].into_speaker_id, system_a_id);
        assert_eq!(speakers.len(), 2);
        assert!(speakers.iter().any(|s| s.id == system_b_id));
    }

    /// A sidecar diagnostics entry with only the fields the mapping cares
    /// about varying.
    fn sidecar_entry(
        mic_speaker: Option<u32>,
        system_speaker: Option<u32>,
        suppressed: bool,
    ) -> crate::echo::EchoDiagnosticEntry {
        crate::echo::EchoDiagnosticEntry {
            mic_index: 0,
            system_index: 0,
            mic_speaker,
            system_speaker,
            mic_start: 0.0,
            mic_end: 1.0,
            system_start: 0.0,
            system_end: 1.0,
            mic_text_chars: 10,
            system_text_chars: 10,
            mic_text_preview: String::new(),
            system_text_preview: String::new(),
            delay_s: 0.1,
            coverage: 0.9,
            text_similarity: 0.9,
            text_contains: false,
            xcorr_peak: Some(0.8),
            suppressed,
        }
    }

    #[test]
    fn sidecar_diagnostics_map_onto_speaker_rows_via_labels() {
        let meeting_id = Uuid::new_v4();
        // Merged id space: mic engine 0 → S1; system engine 0 + offset 1 → S2.
        let speakers = vec![
            Speaker::new(meeting_id, "S1"),
            Speaker::new(meeting_id, "S2"),
            Speaker::new(meeting_id, "S3"),
        ];
        let diagnostics = crate::echo::EchoDiagnostics {
            version: 2,
            mic_segments: 5,
            system_segments: 3,
            mic_speaker_segments: BTreeMap::from([(0, 5)]),
            system_skew_seconds: 0.0,
            candidates: 5,
            suppressed: 3,
            entries: vec![
                sidecar_entry(Some(0), Some(0), true),
                sidecar_entry(Some(0), Some(0), true),
                sidecar_entry(Some(0), Some(0), true),
                // Not suppressed → not a pair.
                sidecar_entry(Some(0), Some(0), false),
                // Pre-v2 entry without speaker ids → contributes nothing.
                sidecar_entry(None, None, true),
                // Unresolvable system label (engine 7 → S9 has no row).
                sidecar_entry(Some(0), Some(7), true),
                // Resolvable mic label (engine 2 → S3) but no pre-suppression
                // denominator recorded → dropped.
                sidecar_entry(Some(2), Some(0), true),
            ],
        };

        let evidence = echo_evidence_from_diagnostics(&diagnostics, &speakers, 1);

        assert_eq!(
            evidence,
            vec![EchoSpeakerEvidence {
                mic_speaker_id: speakers[0].id,
                system_speaker_id: speakers[1].id,
                suppressed_pairs: 3,
                mic_segments_before_suppression: 5,
            }]
        );
    }
}
