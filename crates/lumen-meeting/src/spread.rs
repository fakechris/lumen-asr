//! Spread manual speaker annotations to *unlabelled* segments by voiceprint.
//!
//! ## The problem
//! The offline annotation reconciliation ([`reconcile_annotations`]) is a strict
//! **timeline** model: a manual mark attributes only the time range it governs.
//! So when the user marks "张宏伟" on one stretch, the *same person's* speech in
//! a stretch they never marked keeps its raw diarization cluster — and if that
//! cluster is a different `S{n}`, 张宏伟 ends up split into a named speaker plus a
//! stray "说话人N". The user's mark was high-signal; the rest of that voice
//! should ride along.
//!
//! ## The idea
//! A manual mark is a **voiceprint seed**. Each marked stretch fell inside some
//! diarization cluster, and that cluster already has a centroid embedding (the
//! same per-cluster x-vector auto-identification uses). So:
//!
//! 1. **Seeds** — per manual name, find the diar cluster its marks fall in most
//!    (by overlapped seconds) and take that cluster's centroid as the name's
//!    representative voiceprint. Reuses the existing centroid — no new audio,
//!    no new embedding pass.
//! 2. **Candidates** — every diar cluster the user *never* touched (no manual
//!    mark overlaps it) with enough voiced audio to trust its centroid.
//! 3. **Match** — cosine each candidate against every seed; a confident, clearly
//!    winning match ([`SPREAD_MIN_SCORE`] / [`SPREAD_MIN_MARGIN`]) renames that
//!    whole cluster to the seed's name, provenance
//!    [`attribution_origin::MANUAL_SPREAD`] carrying the score.
//! 4. **Own cluster** — the seed's *own* dominant cluster (the one the marks fall
//!    in) is the same speaker by definition, so its remaining unmarked segments
//!    inherit the name directly (no voiceprint gate). Otherwise a person whose
//!    cluster the reconciliation only partly relabelled would stay split between
//!    the manual name and a stray "说话人N" — the reported "标注没传导到其他段落".
//!
//! ## Why rename the cluster, not move its segments
//! This mirrors [`auto_identify_speakers`](crate::auto_identify_speakers): a
//! confident hit sets the cluster row's `display_name`/`identity_id`/origin, and
//! its segments already point at it — nothing is split or moved. Two rows can
//! then share one name (the precise `manual` M-row and the spread `S`-row),
//! exactly as two clusters auto-identified to one enrolled person already do;
//! the UI/minutes group by name. The precise `manual` M-row is never touched;
//! its dominant `S`-cluster, however, is back-filled to the same name (step 4)
//! so the speaker is whole. The pass is a no-op without embeddings, without
//! manual seeds, or when nothing matches and no dominant cluster owns a segment.
//!
//! ## Priority
//! Runs **after** [`reconcile_annotations`] (precise manual marks already
//! placed) and **before** cross-track unification, giving the final order
//! `manual` > `manual_spread` > `verification` > raw diarization. Spread rows
//! carry a distinct origin so unification's [`provenance_rank`] weighs them
//! correctly.
//!
//! Pure logic over already-computed centroids and assembled rows — no models,
//! no store, no platform gating — fully unit-testable with mock vectors.

use std::collections::BTreeMap;

use lumen_core::{attribution_origin, Speaker, TranscriptSegment};
use lumen_identity::cosine_similarity;
use uuid::Uuid;

use crate::identify::IDENTIFY_MIN_VOICED_MS;

/// Minimum cosine similarity for a candidate cluster to inherit a manual name.
///
/// Deliberately stricter than the cross-meeting library floor
/// ([`lumen_identity::AUTO_TAG_THRESHOLD`] = 0.70): the seed here is a *single*
/// in-meeting centroid (no multi-sample consensus to lean on), and a wrong
/// spread silently relabels a whole cluster as the wrong person — worse than
/// leaving it "说话人N". `0.75` sits at the confident end of the same-speaker
/// cosine band (~0.55–0.85 across sessions; the two centroids here are even
/// from the *same* recording, so a true same-person pair clears it easily).
pub const SPREAD_MIN_SCORE: f32 = 0.75;

/// Minimum lead (`best − runner_up`) the winning seed must hold over the next
/// name for a spread. Two similar voices can both score high against one
/// cluster; requiring the winner to clear the field by ≥ 0.08 (roughly the
/// intra-speaker session jitter, matching
/// [`lumen_identity::LIVE_VERIFIED_MIN_MARGIN`]) keeps "could be either"
/// clusters on their diarization label instead of committing to the wrong name.
pub const SPREAD_MIN_MARGIN: f32 = 0.08;

/// One manual name's voiceprint seed: the manual speaker row it names and the
/// centroid of the diar cluster its marks fell in most.
#[derive(Debug, Clone, PartialEq)]
pub struct Seed {
    /// The manual (`M`) speaker row this seed represents.
    pub manual_speaker_id: Uuid,
    /// The diar cluster the manual marks overlap most — the same speaker as the
    /// annotation. Its remaining (unlabelled) segments inherit the manual name
    /// directly (within-cluster spread), separate from the voiceprint match to
    /// *other* clusters.
    pub dominant_cluster_id: Uuid,
    /// The seed's representative embedding (its dominant cluster's centroid).
    pub embedding: Vec<f32>,
}

/// One unlabelled diar cluster eligible to inherit a manual name.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The cluster (`S`) speaker row.
    pub cluster_speaker_id: Uuid,
    /// The cluster's centroid embedding.
    pub embedding: Vec<f32>,
}

/// A resolved spread: rename `cluster_speaker_id` to the name of
/// `manual_speaker_id`, provenance `manual_spread` with `score`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpreadAssignment {
    pub cluster_speaker_id: Uuid,
    pub manual_speaker_id: Uuid,
    /// Cosine similarity of the winning seed (in `[-1, 1]`).
    pub score: f32,
}

/// What [`spread_annotations`] changed, for logging/tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpreadOutcome {
    /// Cluster rows renamed by spread: `(cluster_speaker_id, name)`.
    pub spread_speakers: Vec<(Uuid, String)>,
}

/// Core matcher: pair each candidate cluster with the single best manual seed by
/// cosine, gated on [`SPREAD_MIN_SCORE`] and a [`SPREAD_MIN_MARGIN`] lead over
/// the runner-up seed. Highest-scoring seed wins a multi-name contest; a
/// candidate that clears neither gate keeps its diarization label (no
/// assignment). A candidate is matched against at most one name — the winner.
///
/// Pure over the given vectors; no ordering assumptions (deterministic:
/// candidates are scanned in input order and each yields at most one
/// assignment).
pub fn match_clusters_to_seeds(seeds: &[Seed], candidates: &[Candidate]) -> Vec<SpreadAssignment> {
    if seeds.is_empty() {
        return Vec::new();
    }
    let mut assignments = Vec::new();
    for candidate in candidates {
        // Best and runner-up seed for this candidate (best-score wins; the
        // runner-up is the next distinct seed, for the margin gate).
        let mut best: Option<(&Seed, f32)> = None;
        let mut runner_up = f32::NEG_INFINITY;
        for seed in seeds {
            let score = cosine_similarity(&candidate.embedding, &seed.embedding);
            match best {
                Some((_, best_score)) if score > best_score => {
                    runner_up = best_score;
                    best = Some((seed, score));
                }
                Some((_, best_score)) => {
                    // Not a new best — but may be the strongest runner-up.
                    if score > runner_up && score <= best_score {
                        runner_up = score;
                    }
                }
                None => best = Some((seed, score)),
            }
        }
        let Some((seed, score)) = best else { continue };
        // A lone seed has no runner-up: `-1.0` (the cosine floor) makes the
        // margin maximally permissive rather than undefined — one seed always
        // clears the margin gate, exactly like a lone enrolled identity.
        let runner_up = if runner_up.is_finite() {
            runner_up
        } else {
            -1.0
        };
        if score >= SPREAD_MIN_SCORE && (score - runner_up) >= SPREAD_MIN_MARGIN {
            assignments.push(SpreadAssignment {
                cluster_speaker_id: candidate.cluster_speaker_id,
                manual_speaker_id: seed.manual_speaker_id,
                score,
            });
        }
    }
    assignments
}

/// Overlap (seconds) of two `[start, end)` spans; `0.0` when disjoint.
fn overlap_seconds(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

/// Whether two segments share a capture track (a `None` channel reads as mic,
/// matching the rest of the pipeline).
fn same_channel(a: &TranscriptSegment, b: &TranscriptSegment) -> bool {
    use lumen_core::SegmentChannel::Mic;
    a.channel.unwrap_or(Mic) == b.channel.unwrap_or(Mic)
}

/// Spread manual annotations across unlabelled diar clusters by voiceprint,
/// mutating the assembled `speakers` in place (renaming matched cluster rows).
/// Returns the resolved assignments (empty on any no-op path).
///
/// Inputs:
/// - `speakers` — the assembled rows *after* [`reconcile_annotations`]: manual
///   (`M`) rows plus the diar (`S`) cluster rows.
/// - `pre_segments` — the assembled segments *before* reconciliation, i.e. each
///   still carrying its diar cluster's `speaker_id`. This is what maps a manual
///   mark's time back to the cluster it fell in.
/// - `post_segments` — the segments *after* reconciliation: manual pieces point
///   at `M` rows, unlabelled pieces still at their `S` cluster.
/// - `cluster_centroids` — `S`-cluster row id → centroid embedding.
/// - `cluster_voiced_ms` — `S`-cluster row id → total voiced ms (candidate gate).
///
/// Seeds: per manual row, the cluster its post-reconciliation segments overlap
/// most (by seconds, same track) supplies the seed centroid. Candidates: every
/// centroid-bearing cluster the manual marks never touched (no time overlap on
/// its own track) with ≥ [`IDENTIFY_MIN_VOICED_MS`] voiced audio. A confident
/// match renames the candidate cluster to the seed's name/identity with
/// [`attribution_origin::MANUAL_SPREAD`] and the score.
pub fn spread_annotations(
    speakers: &mut [Speaker],
    pre_segments: &[TranscriptSegment],
    post_segments: &[TranscriptSegment],
    cluster_centroids: &BTreeMap<Uuid, Vec<f32>>,
    cluster_voiced_ms: &BTreeMap<Uuid, u64>,
) -> SpreadOutcome {
    let outcome = SpreadOutcome::default();
    if cluster_centroids.is_empty() {
        return outcome;
    }

    // Manual rows (precise annotations already placed by reconciliation).
    let manual_ids: Vec<Uuid> = speakers
        .iter()
        .filter(|s| s.attribution_origin.as_deref() == Some(attribution_origin::MANUAL))
        .map(|s| s.id)
        .collect();
    if manual_ids.is_empty() {
        return outcome;
    }

    // Post-reconciliation segments belonging to each manual row — the time
    // ranges the user actually marked.
    let manual_segments: Vec<&TranscriptSegment> = post_segments
        .iter()
        .filter(|s| s.speaker_id.is_some_and(|id| manual_ids.contains(&id)))
        .collect();
    if manual_segments.is_empty() {
        return outcome;
    }

    // For each manual row, the diar cluster its marks overlap most (seconds) →
    // its seed centroid. `annotated_clusters` collects *every* cluster any
    // manual mark touched, so those clusters are excluded as candidates (they
    // are, at least partly, the user's own precise marks).
    let mut per_manual_cluster_secs: BTreeMap<Uuid, BTreeMap<Uuid, f64>> = BTreeMap::new();
    let mut annotated_clusters: std::collections::BTreeSet<Uuid> = Default::default();
    for manual in &manual_segments {
        let Some(manual_id) = manual.speaker_id else {
            continue;
        };
        for pre in pre_segments {
            let Some(cluster_id) = pre.speaker_id else {
                continue;
            };
            if !cluster_centroids.contains_key(&cluster_id) || !same_channel(manual, pre) {
                continue;
            }
            let secs = overlap_seconds(
                manual.start_seconds,
                manual.end_seconds,
                pre.start_seconds,
                pre.end_seconds,
            );
            if secs > 0.0 {
                annotated_clusters.insert(cluster_id);
                *per_manual_cluster_secs
                    .entry(manual_id)
                    .or_default()
                    .entry(cluster_id)
                    .or_default() += secs;
            }
        }
    }

    let seeds: Vec<Seed> = per_manual_cluster_secs
        .into_iter()
        .filter_map(|(manual_id, clusters)| {
            // Dominant cluster: most overlapped seconds (ties broken by the
            // smaller cluster id for determinism).
            let dominant = clusters
                .into_iter()
                .max_by(|a, b| {
                    a.1.partial_cmp(&b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(b.0.cmp(&a.0))
                })
                .map(|(cluster_id, _)| cluster_id)?;
            Some(Seed {
                manual_speaker_id: manual_id,
                dominant_cluster_id: dominant,
                embedding: cluster_centroids.get(&dominant)?.clone(),
            })
        })
        .collect();
    if seeds.is_empty() {
        return outcome;
    }

    // Candidate clusters: centroid-bearing, never touched by a manual mark,
    // enough voiced audio to trust the centroid, still owning ≥1 segment.
    let owns_segment = |cluster_id: Uuid| -> bool {
        post_segments
            .iter()
            .any(|s| s.speaker_id == Some(cluster_id))
    };
    let candidates: Vec<Candidate> = cluster_centroids
        .iter()
        .filter(|(cluster_id, _)| !annotated_clusters.contains(*cluster_id))
        .filter(|(cluster_id, _)| {
            cluster_voiced_ms.get(*cluster_id).copied().unwrap_or(0) >= IDENTIFY_MIN_VOICED_MS
        })
        .filter(|(cluster_id, _)| owns_segment(**cluster_id))
        .map(|(cluster_id, embedding)| Candidate {
            cluster_speaker_id: *cluster_id,
            embedding: embedding.clone(),
        })
        .collect();
    let mut assignments = if candidates.is_empty() {
        Vec::new()
    } else {
        match_clusters_to_seeds(&seeds, &candidates)
    };

    // Within-cluster spread: each seed's dominant cluster is the SAME speaker as
    // the annotation (the marks overlap it most), so its still-unlabelled
    // segments should carry the manual name too — otherwise a person whose diar
    // cluster the reconciliation only partly relabelled stays split between the
    // manual row and a stray "说话人N". The dominant cluster is excluded from the
    // voiceprint candidates above (it is a marked cluster), so add it directly.
    // Only when the cluster row still owns a segment and no voiceprint match
    // already claimed it.
    for seed in &seeds {
        let dominant = seed.dominant_cluster_id;
        if assignments.iter().any(|a| a.cluster_speaker_id == dominant) {
            continue;
        }
        if !owns_segment(dominant) {
            continue;
        }
        assignments.push(SpreadAssignment {
            cluster_speaker_id: dominant,
            manual_speaker_id: seed.manual_speaker_id,
            // Same cluster as the annotation → certain, not a voiceprint guess.
            score: 1.0,
        });
    }
    if assignments.is_empty() {
        return outcome;
    }
    apply_spread(speakers, &assignments)
}

/// Apply resolved spread assignments: copy each seed manual row's
/// name/identity onto the matched cluster row with
/// [`attribution_origin::MANUAL_SPREAD`] provenance and the match score. Never
/// touches a row that already carries a precise `manual` attribution (a
/// candidate is, by construction, never such a row — this is a defensive
/// guard). Returns what was renamed.
fn apply_spread(speakers: &mut [Speaker], assignments: &[SpreadAssignment]) -> SpreadOutcome {
    let mut outcome = SpreadOutcome::default();
    for assignment in assignments {
        // The name/identity to copy comes from the seed's manual row.
        let Some((name, identity)) = speakers
            .iter()
            .find(|s| s.id == assignment.manual_speaker_id)
            .and_then(|m| m.display_name.clone().map(|n| (n, m.identity_id)))
        else {
            continue;
        };
        let Some(cluster) = speakers
            .iter_mut()
            .find(|s| s.id == assignment.cluster_speaker_id)
        else {
            continue;
        };
        // Never override a precise manual mark (defensive: candidates exclude
        // annotated clusters, so this should not arise).
        if cluster.attribution_origin.as_deref() == Some(attribution_origin::MANUAL) {
            continue;
        }
        cluster.display_name = Some(name.clone());
        cluster.identity_id = identity;
        cluster.attribution_origin = Some(attribution_origin::MANUAL_SPREAD.to_string());
        cluster.attribution_confidence = Some(f64::from(assignment.score));
        outcome
            .spread_speakers
            .push((assignment.cluster_speaker_id, name));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::{SegmentChannel, Speaker, TranscriptSegment};
    use lumen_identity::EMBEDDING_DIM;

    /// A deterministic unit-ish embedding seeded by `seed`; nearby seeds are
    /// nearly parallel (high cosine), far seeds are ~orthogonal.
    fn emb(seed: f32) -> Vec<f32> {
        (0..EMBEDDING_DIM)
            .map(|i| ((i as f32) * 0.05 + seed).sin())
            .collect()
    }

    fn seed(id: Uuid, e: Vec<f32>) -> Seed {
        Seed {
            manual_speaker_id: id,
            // Not used by `match_clusters_to_seeds` (voiceprint matcher tests).
            dominant_cluster_id: Uuid::nil(),
            embedding: e,
        }
    }

    fn cand(id: Uuid, e: Vec<f32>) -> Candidate {
        Candidate {
            cluster_speaker_id: id,
            embedding: e,
        }
    }

    #[test]
    fn a_like_cluster_matches_its_seed_and_dissimilar_does_not() {
        let a = Uuid::new_v4();
        let seeds = vec![seed(a, emb(0.10))];
        let like_a = Uuid::new_v4();
        let unlike = Uuid::new_v4();
        // `like_a` is a hair off the A seed (same voice, another cluster);
        // `unlike` is a very different vector.
        let candidates = vec![cand(like_a, emb(0.11)), cand(unlike, emb(9.0))];

        let out = match_clusters_to_seeds(&seeds, &candidates);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cluster_speaker_id, like_a);
        assert_eq!(out[0].manual_speaker_id, a);
        assert!(out[0].score >= SPREAD_MIN_SCORE);
    }

    #[test]
    fn below_threshold_is_not_spread() {
        let a = Uuid::new_v4();
        let seeds = vec![seed(a, emb(0.10))];
        // Tuned to a middling cosine below 0.75.
        let mut candidates = vec![cand(Uuid::new_v4(), emb(0.9))];
        // Sanity: the score really is in the grey zone, not accidentally high.
        let s = cosine_similarity(&candidates[0].embedding, &seeds[0].embedding);
        assert!(
            s < SPREAD_MIN_SCORE,
            "fixture must sit below the gate, got {s}"
        );
        candidates.push(cand(Uuid::new_v4(), emb(0.10))); // this one *would* match
        let out = match_clusters_to_seeds(&seeds, &candidates);
        assert_eq!(out.len(), 1, "only the on-seed cluster spreads");
    }

    #[test]
    fn margin_gate_blocks_ambiguous_two_name_contest() {
        // Two seeds whose embeddings are very close to each other; a candidate
        // near both clears the score gate but not the margin → no spread.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let seeds = vec![seed(a, emb(0.10)), seed(b, emb(0.1005))];
        let candidate = cand(Uuid::new_v4(), emb(0.1002));
        let best = cosine_similarity(&candidate.embedding, &seeds[0].embedding);
        let other = cosine_similarity(&candidate.embedding, &seeds[1].embedding);
        assert!(best >= SPREAD_MIN_SCORE && other >= SPREAD_MIN_SCORE);
        assert!(
            (best - other).abs() < SPREAD_MIN_MARGIN,
            "fixture must be ambiguous"
        );

        let out = match_clusters_to_seeds(&seeds, &[candidate]);
        assert!(out.is_empty(), "ambiguous match must not spread");
    }

    #[test]
    fn highest_scoring_name_wins_a_clear_contest() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // A far from B; the candidate sits right on A.
        let seeds = vec![seed(a, emb(0.10)), seed(b, emb(9.0))];
        let candidate = cand(Uuid::new_v4(), emb(0.10));

        let out = match_clusters_to_seeds(&seeds, &[candidate]);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].manual_speaker_id, a, "closest name wins");
    }

    #[test]
    fn no_seeds_is_empty() {
        let out = match_clusters_to_seeds(&[], &[cand(Uuid::new_v4(), emb(0.1))]);
        assert!(out.is_empty());
    }

    #[test]
    fn lone_seed_clears_the_margin_gate() {
        // A single seed has no runner-up; a strong score alone must suffice.
        let a = Uuid::new_v4();
        let seeds = vec![seed(a, emb(0.10))];
        let out = match_clusters_to_seeds(&seeds, &[cand(Uuid::new_v4(), emb(0.10))]);
        assert_eq!(out.len(), 1);
        assert!(out[0].score > 0.999);
    }

    // ── end-to-end `spread_annotations` (the user's scenario) ───────────

    fn manual_speaker(meeting: Uuid, label: &str, name: &str) -> Speaker {
        let mut s = Speaker::new(meeting, label);
        s.display_name = Some(name.to_string());
        s.attribution_origin = Some(attribution_origin::MANUAL.to_string());
        s
    }

    fn seg(meeting: Uuid, seq: u32, start: f64, end: f64, speaker: Uuid) -> TranscriptSegment {
        let mut s = TranscriptSegment::new(meeting, seq, start, end, "…");
        s.speaker_id = Some(speaker);
        s.channel = Some(SegmentChannel::Mic);
        s
    }

    /// The reported complaint: a manual annotation covers only PART of a
    /// person's diar cluster (reconciliation relabelled one turn), leaving the
    /// rest of that same cluster a stray "说话人N". Within-cluster spread must
    /// relabel the annotation's own dominant cluster to the manual name — even
    /// when no *other* cluster voiceprint-matches.
    #[test]
    fn spreads_the_annotations_own_dominant_cluster_to_its_remaining_segments() {
        let meeting = Uuid::new_v4();
        let s1 = Speaker::new(meeting, "S1"); // 海燕's cluster
        let s2 = Speaker::new(meeting, "S2"); // a different person
        let ma = manual_speaker(meeting, "M1", "海燕");
        let mut speakers = vec![s1.clone(), s2.clone(), ma.clone()];

        // Pre-reconcile: S1 owns two turns (0–10, 20–30), S2 owns one (10–20).
        let pre = vec![
            seg(meeting, 0, 0.0, 10.0, s1.id),
            seg(meeting, 1, 10.0, 20.0, s2.id),
            seg(meeting, 2, 20.0, 30.0, s1.id),
        ];
        // Post-reconcile: the mark relabelled S1's first turn to 海燕; S1's second
        // turn (and S2) stay on their clusters.
        let post = vec![
            seg(meeting, 0, 0.0, 10.0, ma.id),
            seg(meeting, 1, 10.0, 20.0, s2.id),
            seg(meeting, 2, 20.0, 30.0, s1.id),
        ];
        let centroids = BTreeMap::from([(s1.id, emb(0.10)), (s2.id, emb(5.00))]);
        let voiced = BTreeMap::from([(s1.id, 10_000u64), (s2.id, 10_000u64)]);

        let out = spread_annotations(&mut speakers, &pre, &post, &centroids, &voiced);

        assert_eq!(out.spread_speakers.len(), 1, "only S1 relabelled");
        let s1_row = speakers.iter().find(|s| s.id == s1.id).unwrap();
        assert_eq!(s1_row.display_name.as_deref(), Some("海燕"));
        assert_eq!(
            s1_row.attribution_origin.as_deref(),
            Some(attribution_origin::MANUAL_SPREAD)
        );
        // A different person is never touched; the manual row is unchanged.
        assert_eq!(
            speakers
                .iter()
                .find(|s| s.id == s2.id)
                .unwrap()
                .display_name,
            None
        );
        assert_eq!(
            speakers
                .iter()
                .find(|s| s.id == ma.id)
                .unwrap()
                .attribution_origin
                .as_deref(),
            Some(attribution_origin::MANUAL)
        );
    }

    /// Segment 1 marked A, segment 2 marked B; a later *unlabelled* cluster that
    /// sounds like A is spread to A (not left a new speaker), a dissimilar
    /// cluster is untouched, and the precise manual rows are never altered.
    #[test]
    fn spreads_unlabelled_a_like_cluster_to_a_reproducing_user_scenario() {
        let meeting = Uuid::new_v4();

        // Diar cluster rows (centroids): S1≈A, S2≈B, S3≈A-like, S4 dissimilar.
        let s1 = Speaker::new(meeting, "S1");
        let s2 = Speaker::new(meeting, "S2");
        let s3 = Speaker::new(meeting, "S3");
        let s4 = Speaker::new(meeting, "S4");
        // Manual rows created by reconciliation for the two marks.
        let ma = manual_speaker(meeting, "M1", "A");
        let mb = manual_speaker(meeting, "M2", "B");

        let mut speakers = vec![
            s1.clone(),
            s2.clone(),
            s3.clone(),
            s4.clone(),
            ma.clone(),
            mb.clone(),
        ];

        // Pre-reconciliation: each cluster owns its own turn.
        let pre = vec![
            seg(meeting, 0, 0.0, 10.0, s1.id),  // A's cluster
            seg(meeting, 1, 10.0, 20.0, s2.id), // B's cluster
            seg(meeting, 2, 20.0, 40.0, s3.id), // A-like, unlabelled
            seg(meeting, 3, 40.0, 60.0, s4.id), // dissimilar, unlabelled
        ];
        // Post-reconciliation: S1's turn is now A's manual row, S2's is B's;
        // S3/S4 stay on their clusters.
        let post = vec![
            seg(meeting, 0, 0.0, 10.0, ma.id),
            seg(meeting, 1, 10.0, 20.0, mb.id),
            seg(meeting, 2, 20.0, 40.0, s3.id),
            seg(meeting, 3, 40.0, 60.0, s4.id),
        ];

        let centroids = BTreeMap::from([
            (s1.id, emb(0.10)), // A
            (s2.id, emb(5.00)), // B (far from A)
            (s3.id, emb(0.11)), // A-like
            (s4.id, emb(9.00)), // dissimilar
        ]);
        let voiced = BTreeMap::from([
            (s1.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s2.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s3.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s4.id, IDENTIFY_MIN_VOICED_MS + 1),
        ]);

        let out = spread_annotations(&mut speakers, &pre, &post, &centroids, &voiced);

        // Exactly S3 was spread to A.
        assert_eq!(out.spread_speakers, vec![(s3.id, "A".to_string())]);
        let s3_row = speakers.iter().find(|s| s.id == s3.id).unwrap();
        assert_eq!(s3_row.display_name.as_deref(), Some("A"));
        assert_eq!(
            s3_row.attribution_origin.as_deref(),
            Some(attribution_origin::MANUAL_SPREAD)
        );
        assert!(s3_row.attribution_confidence.unwrap() >= f64::from(SPREAD_MIN_SCORE));
        // S4 (dissimilar) untouched.
        let s4_row = speakers.iter().find(|s| s.id == s4.id).unwrap();
        assert_eq!(s4_row.display_name, None);
        assert_eq!(s4_row.attribution_origin, None);
        // The precise manual rows are exactly as they were.
        assert_eq!(speakers.iter().find(|s| s.id == ma.id).unwrap(), &ma);
        assert_eq!(speakers.iter().find(|s| s.id == mb.id).unwrap(), &mb);
    }

    /// The annotation's own dominant cluster is back-filled: after the user marks
    /// part of a cluster as A, the cluster's remaining unmarked tail inherits A
    /// too (within-cluster spread), so one person is not left split between the
    /// manual name and a stray "说话人N". (This reverses the earlier
    /// "don't back-fill the tail" behavior, which surfaced as annotations that
    /// only labelled part of a speaker.)
    #[test]
    fn annotated_clusters_unmarked_tail_inherits_the_manual_name() {
        let meeting = Uuid::new_v4();
        let s1 = Speaker::new(meeting, "S1");
        let ma = manual_speaker(meeting, "M1", "A");
        let mut speakers = vec![s1.clone(), ma.clone()];

        // One cluster S1 spanning [0,20]; the user marked only [0,10] as A.
        let pre = vec![seg(meeting, 0, 0.0, 20.0, s1.id)];
        let post = vec![
            seg(meeting, 0, 0.0, 10.0, ma.id),  // marked A
            seg(meeting, 1, 10.0, 20.0, s1.id), // unmarked tail, same cluster
        ];
        let centroids = BTreeMap::from([(s1.id, emb(0.10))]);
        let voiced = BTreeMap::from([(s1.id, IDENTIFY_MIN_VOICED_MS + 1)]);

        let out = spread_annotations(&mut speakers, &pre, &post, &centroids, &voiced);

        assert_eq!(out.spread_speakers.len(), 1);
        let s1_row = speakers.iter().find(|s| s.id == s1.id).unwrap();
        assert_eq!(
            s1_row.display_name.as_deref(),
            Some("A"),
            "unmarked tail inherits the manual name"
        );
        assert_eq!(
            s1_row.attribution_origin.as_deref(),
            Some(attribution_origin::MANUAL_SPREAD)
        );
    }

    /// A "无"/unassigned-only meeting produces no manual rows, hence no seeds and
    /// a strict no-op even with candidate clusters present.
    #[test]
    fn no_manual_rows_is_a_noop() {
        let meeting = Uuid::new_v4();
        let s1 = Speaker::new(meeting, "S1");
        let mut speakers = vec![s1.clone()];
        let before = speakers.clone();
        let post = vec![seg(meeting, 0, 0.0, 10.0, s1.id)];
        let centroids = BTreeMap::from([(s1.id, emb(0.10))]);
        let voiced = BTreeMap::from([(s1.id, IDENTIFY_MIN_VOICED_MS + 1)]);

        let out = spread_annotations(&mut speakers, &post.clone(), &post, &centroids, &voiced);

        assert_eq!(out, SpreadOutcome::default());
        assert_eq!(speakers, before);
    }

    /// No embeddings (non-diarizing build) → no-op regardless of annotations.
    #[test]
    fn no_embeddings_is_a_noop() {
        let meeting = Uuid::new_v4();
        let s1 = Speaker::new(meeting, "S1");
        let ma = manual_speaker(meeting, "M1", "A");
        let mut speakers = vec![s1.clone(), ma.clone()];
        let before = speakers.clone();
        let pre = vec![seg(meeting, 0, 0.0, 10.0, s1.id)];
        let post = vec![seg(meeting, 0, 0.0, 10.0, ma.id)];

        let out = spread_annotations(
            &mut speakers,
            &pre,
            &post,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );

        assert_eq!(out, SpreadOutcome::default());
        assert_eq!(speakers, before);
    }

    /// A candidate with too little voiced audio is never spread (its centroid is
    /// too noisy to trust — same floor as auto-identification).
    #[test]
    fn low_voiced_candidate_is_gated_out() {
        let meeting = Uuid::new_v4();
        let s1 = Speaker::new(meeting, "S1"); // A's marked cluster
        let s2 = Speaker::new(meeting, "S2"); // A-like but too short
        let ma = manual_speaker(meeting, "M1", "A");
        let mut speakers = vec![s1.clone(), s2.clone(), ma.clone()];

        let pre = vec![
            seg(meeting, 0, 0.0, 10.0, s1.id),
            seg(meeting, 1, 10.0, 12.0, s2.id),
        ];
        let post = vec![
            seg(meeting, 0, 0.0, 10.0, ma.id),
            seg(meeting, 1, 10.0, 12.0, s2.id),
        ];
        let centroids = BTreeMap::from([(s1.id, emb(0.10)), (s2.id, emb(0.10))]);
        let voiced = BTreeMap::from([
            (s1.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s2.id, IDENTIFY_MIN_VOICED_MS - 1), // below the floor
        ]);

        let out = spread_annotations(&mut speakers, &pre, &post, &centroids, &voiced);

        assert!(out.spread_speakers.is_empty());
        assert_eq!(
            speakers
                .iter()
                .find(|s| s.id == s2.id)
                .unwrap()
                .display_name,
            None
        );
    }
}
