//! Cross-meeting speaker auto-identification (voiceprint matching, M5).
//!
//! After diarization produces per-speaker centroid embeddings, each *unnamed*
//! speaker is matched against the local identity library
//! ([`lumen_identity::IdentityStore`]); a confident hit auto-assigns the real
//! name as the speaker's `display_name` — the same field a manual confirmation
//! sets, so downstream (store, UI, minutes) treats auto-identified speakers
//! exactly like manually confirmed ones. A miss leaves the speaker as
//! `说话人N` (engine label) for manual confirmation.
//!
//! Speakers with less than [`IDENTIFY_MIN_VOICED_MS`] of total voiced audio are
//! never auto-matched: a centroid averaged over a couple of seconds is too
//! noisy, and a wrong auto-assigned name is worse than an unnamed speaker.
//!
//! Pure logic over already-computed embeddings — no models, no platform
//! gating — so it is fully unit-testable with mock vectors.

use std::collections::BTreeMap;
use std::path::Path;

use lumen_core::Speaker;
use lumen_identity::IdentityStore;
use lumen_store::Store;

use crate::assemble::{speaker_label, DiarTurn};

/// Minimum total voiced audio (ms, summed over a speaker's diarized turns)
/// required before that speaker's centroid is auto-matched against the
/// enrolled library. Same floor as enrollment
/// ([`lumen_identity::MIN_VOICED_MS`]): below it the centroid is statistically
/// unreliable, so the speaker keeps the engine label (说话人N) for the user to
/// confirm manually.
pub const IDENTIFY_MIN_VOICED_MS: u64 = lumen_identity::MIN_VOICED_MS;

/// One auto-identified speaker: which cluster was recognized as whom, and how
/// confidently (cosine similarity in `[-1, 1]`).
#[derive(Debug, Clone, PartialEq)]
pub struct AutoIdentification {
    /// Engine label of the matched cluster, e.g. `"S2"`.
    pub label: String,
    /// The enrolled identity's real name that was assigned.
    pub name: String,
    /// Id of the matched enrolled identity (persisted as speaker provenance).
    pub identity_id: uuid::Uuid,
    /// Best-sample cosine similarity of the consensus match (see
    /// [`lumen_identity::IdentityStore::match_speaker`]).
    pub score: f32,
}

/// Sum each speaker's voiced audio (ms) over its diarized turns. Feeds the
/// [`IDENTIFY_MIN_VOICED_MS`] gate and enrollment metadata.
pub fn speaker_voiced_ms(turns: &[DiarTurn]) -> BTreeMap<u32, u64> {
    let mut voiced: BTreeMap<u32, u64> = BTreeMap::new();
    for turn in turns {
        let ms = ((turn.end - turn.start).max(0.0) * 1000.0).round() as u64;
        *voiced.entry(turn.speaker).or_default() += ms;
    }
    voiced
}

/// Match every still-unnamed speaker's centroid against the enrolled identity
/// library and assign `display_name` on a confident hit. Speakers that already
/// carry a `display_name` (e.g. re-processing after manual confirmation),
/// speakers without an embedding, and speakers with less than
/// [`IDENTIFY_MIN_VOICED_MS`] of voiced audio (per `voiced_ms`, keyed like
/// `embeddings`; missing = 0) are left untouched. Returns what was assigned,
/// for logging/diagnostics.
///
/// `embeddings` is keyed by engine speaker id (as produced by diarization);
/// the id maps to a [`Speaker`] row via its `S{id+1}` label.
pub fn auto_identify_speakers(
    speakers: &mut [Speaker],
    embeddings: &BTreeMap<u32, Vec<f32>>,
    voiced_ms: &BTreeMap<u32, u64>,
    identities: &IdentityStore,
) -> Vec<AutoIdentification> {
    let mut assigned = Vec::new();
    if identities.list().is_empty() || embeddings.is_empty() {
        return assigned;
    }
    for (engine_id, embedding) in embeddings {
        let label = speaker_label(*engine_id);
        let Some(speaker) = speakers.iter_mut().find(|s| s.label == label) else {
            continue;
        };
        if speaker.display_name.is_some() {
            continue; // never override a user-confirmed name
        }
        let voiced = voiced_ms.get(engine_id).copied().unwrap_or(0);
        if voiced < IDENTIFY_MIN_VOICED_MS {
            // Too little material for a trustworthy centroid: keep the engine
            // label (说话人N) rather than risk a mislabel.
            tracing::info!(
                label = %label,
                voiced_ms = voiced,
                min_voiced_ms = IDENTIFY_MIN_VOICED_MS,
                "speaker has too little voiced audio; skipping auto-identification"
            );
            continue;
        }
        if let Some(report) = identities.match_speaker_report(embedding) {
            speaker.display_name = Some(report.display_name.clone());
            // Provenance (v13): a voiceprint hit is a `verification`
            // attribution — record which enrolled identity matched and how
            // confidently, so later conflict handling can weigh it against
            // manual marks (manual > verification > offline_diarization).
            speaker.identity_id = Some(report.identity_id);
            speaker.attribution_origin =
                Some(lumen_core::attribution_origin::VERIFICATION.to_string());
            speaker.attribution_confidence = Some(f64::from(report.best_score));
            assigned.push(AutoIdentification {
                label,
                name: report.display_name,
                identity_id: report.identity_id,
                score: report.best_score,
            });
        }
    }
    assigned
}

/// Retroactively re-identify a **stored** meeting's speakers against the
/// current identity library — the same match policy as
/// [`auto_identify_speakers`], but keyed by persisted speaker **row id** rather
/// than engine id, so it can run on a meeting processed before a voiceprint was
/// enrolled ("回溯重认").
///
/// Only still-unnamed speakers (`display_name` is `None`) are touched, so a
/// manual name — or a name assigned by an earlier run — is never overridden;
/// speakers without a stored centroid or below [`IDENTIFY_MIN_VOICED_MS`] are
/// left alone. `centroids`/`voiced_ms` are keyed by `speaker.id`. Mutates the
/// matched speakers in place and returns what changed.
pub fn reidentify_speakers(
    speakers: &mut [Speaker],
    centroids: &BTreeMap<uuid::Uuid, Vec<f32>>,
    voiced_ms: &BTreeMap<uuid::Uuid, u64>,
    identities: &IdentityStore,
) -> Vec<AutoIdentification> {
    let mut assigned = Vec::new();
    if identities.list().is_empty() {
        return assigned;
    }
    for speaker in speakers.iter_mut() {
        if speaker.display_name.is_some() {
            continue; // only fill 说话人N; never override manual or a prior hit
        }
        let Some(embedding) = centroids.get(&speaker.id) else {
            continue; // no stored centroid (pre-v9 meeting / non-diarized)
        };
        if voiced_ms.get(&speaker.id).copied().unwrap_or(0) < IDENTIFY_MIN_VOICED_MS {
            continue;
        }
        if let Some(report) = identities.match_speaker_report(embedding) {
            speaker.display_name = Some(report.display_name.clone());
            speaker.identity_id = Some(report.identity_id);
            speaker.attribution_origin =
                Some(lumen_core::attribution_origin::VERIFICATION.to_string());
            speaker.attribution_confidence = Some(f64::from(report.best_score));
            assigned.push(AutoIdentification {
                label: speaker.label.clone(),
                name: report.display_name,
                identity_id: report.identity_id,
                score: report.best_score,
            });
        }
    }
    assigned
}

/// Open the identity library at `identity_dir` (when configured) and run
/// [`auto_identify_speakers`], logging each hit (cluster label + score; the
/// matched real name is PII and deliberately kept out of logs). Failures to
/// open the library degrade to "no auto-identification" — they must never
/// fail the transcription pipeline.
pub(crate) fn apply_auto_identification(
    speakers: &mut [Speaker],
    embeddings: &BTreeMap<u32, Vec<f32>>,
    voiced_ms: &BTreeMap<u32, u64>,
    identity_dir: Option<&Path>,
) -> Vec<AutoIdentification> {
    let Some(dir) = identity_dir else {
        return Vec::new();
    };
    let identities = match IdentityStore::open(dir) {
        Ok(identities) => identities,
        Err(error) => {
            tracing::warn!(error = %error, "could not open identity library; skipping auto-identification");
            return Vec::new();
        }
    };
    let assigned = auto_identify_speakers(speakers, embeddings, voiced_ms, &identities);
    for hit in &assigned {
        // The matched name is PII; log only which cluster matched and how
        // confidently.
        tracing::info!(
            label = %hit.label,
            score = hit.score,
            "speaker auto-identified from enrolled voiceprint"
        );
    }
    assigned
}

/// Persist each speaker's centroid embedding next to its (already-written)
/// speaker row, keyed engine id → `S{id+1}` label. Called by both pipeline
/// entry points right after the speaker upserts.
pub(crate) fn persist_speaker_embeddings(
    store: &Store,
    speakers: &[Speaker],
    embeddings: &BTreeMap<u32, Vec<f32>>,
) -> anyhow::Result<()> {
    for (engine_id, embedding) in embeddings {
        let label = speaker_label(*engine_id);
        if let Some(speaker) = speakers.iter().find(|s| s.label == label) {
            store.set_speaker_embedding(speaker.id, embedding)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_identity::EMBEDDING_DIM;
    use uuid::Uuid;

    fn emb(seed: f32) -> Vec<f32> {
        (0..EMBEDDING_DIM)
            .map(|i| ((i as f32) * seed).sin())
            .collect()
    }

    fn speakers(n: u32) -> Vec<Speaker> {
        let meeting = Uuid::new_v4();
        (0..n)
            .map(|i| Speaker::new(meeting, speaker_label(i)))
            .collect()
    }

    /// Voiced-duration map that passes the gate for the given engine ids.
    fn voiced_ok(ids: &[u32]) -> BTreeMap<u32, u64> {
        ids.iter()
            .map(|&id| (id, IDENTIFY_MIN_VOICED_MS + 1000))
            .collect()
    }

    fn identity_store_with(entries: &[(&str, f32)]) -> (tempfile::TempDir, IdentityStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = IdentityStore::open(dir.path()).unwrap();
        for (name, seed) in entries {
            store.enroll(name, &emb(*seed), 5000, None).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn confident_match_assigns_display_name_and_reports_score() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(2);
        let embeddings = BTreeMap::from([(0, emb(0.1)), (1, emb(7.7))]);

        let assigned =
            auto_identify_speakers(&mut speakers, &embeddings, &voiced_ok(&[0, 1]), &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S1");
        assert_eq!(assigned[0].name, "李明");
        assert!(assigned[0].score > 0.999);
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));
        // Provenance (v13): the hit records identity link + origin + score.
        let enrolled_id = identities.list()[0].id;
        assert_eq!(assigned[0].identity_id, enrolled_id);
        assert_eq!(speakers[0].identity_id, Some(enrolled_id));
        assert_eq!(
            speakers[0].attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::VERIFICATION)
        );
        assert!(speakers[0].attribution_confidence.unwrap() > 0.999);
        // The dissimilar speaker stays unnamed (说话人2 in the UI) with no
        // provenance written.
        assert_eq!(speakers[1].display_name, None);
        assert_eq!(speakers[1].attribution_origin, None);
    }

    #[test]
    fn reidentify_fills_unnamed_and_never_overrides_a_named_speaker() {
        let (_dir, identities) = identity_store_with(&[("我", 0.1)]);
        let meeting = Uuid::new_v4();
        // S1: unnamed, sounds like 我 → should be filled.
        let s1 = Speaker::new(meeting, "S1");
        // S2: manually named 海燕 but *also* carries the 我 voice → must stay 海燕.
        let mut s2 = Speaker::new(meeting, "S2");
        s2.display_name = Some("海燕".into());
        s2.attribution_origin = Some(lumen_core::attribution_origin::MANUAL.into());
        // S3: unnamed, different voice → stays unnamed.
        let s3 = Speaker::new(meeting, "S3");
        let centroids = BTreeMap::from([(s1.id, emb(0.1)), (s2.id, emb(0.1)), (s3.id, emb(7.7))]);
        let voiced = BTreeMap::from([
            (s1.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s2.id, IDENTIFY_MIN_VOICED_MS + 1),
            (s3.id, IDENTIFY_MIN_VOICED_MS + 1),
        ]);
        let mut speakers = vec![s1, s2, s3];

        let assigned = reidentify_speakers(&mut speakers, &centroids, &voiced, &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S1");
        assert_eq!(assigned[0].name, "我");
        assert_eq!(speakers[0].display_name.as_deref(), Some("我"));
        assert_eq!(
            speakers[0].attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::VERIFICATION)
        );
        // Manual 海燕 untouched even though its voice matches 我.
        assert_eq!(speakers[1].display_name.as_deref(), Some("海燕"));
        assert_eq!(
            speakers[1].attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::MANUAL)
        );
        // Dissimilar speaker stays unnamed. Re-running is a no-op (idempotent).
        assert_eq!(speakers[2].display_name, None);
        assert!(reidentify_speakers(&mut speakers, &centroids, &voiced, &identities).is_empty());
    }

    #[test]
    fn speaker_below_min_voiced_duration_is_never_auto_matched() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(2);
        // Both clusters carry the enrolled voice; only S2 spoke long enough.
        let embeddings = BTreeMap::from([(0, emb(0.1)), (1, emb(0.1))]);
        let voiced = BTreeMap::from([(0, IDENTIFY_MIN_VOICED_MS - 1), (1, IDENTIFY_MIN_VOICED_MS)]);

        let assigned = auto_identify_speakers(&mut speakers, &embeddings, &voiced, &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S2");
        assert_eq!(
            speakers[0].display_name, None,
            "2-second speaker keeps 说话人N"
        );
        assert_eq!(speakers[1].display_name.as_deref(), Some("李明"));
    }

    #[test]
    fn speaker_missing_from_voiced_map_is_treated_as_zero_and_skipped() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(1);
        let embeddings = BTreeMap::from([(0, emb(0.1))]);

        let assigned =
            auto_identify_speakers(&mut speakers, &embeddings, &BTreeMap::new(), &identities);

        assert!(assigned.is_empty());
        assert_eq!(speakers[0].display_name, None);
    }

    #[test]
    fn speaker_voiced_ms_sums_all_turns_per_speaker() {
        let turns = vec![
            DiarTurn::new(0.0, 1.5, 0),
            DiarTurn::new(2.0, 2.2, 0), // short turns still count toward the total
            DiarTurn::new(3.0, 6.0, 1),
        ];
        let voiced = speaker_voiced_ms(&turns);
        assert_eq!(voiced.get(&0).copied(), Some(1700));
        assert_eq!(voiced.get(&1).copied(), Some(3000));
        assert_eq!(voiced.get(&2), None);
    }

    #[test]
    fn empty_identity_library_assigns_nothing() {
        let (_dir, identities) = identity_store_with(&[]);
        let mut speakers = speakers(1);
        let embeddings = BTreeMap::from([(0, emb(0.1))]);
        assert!(
            auto_identify_speakers(&mut speakers, &embeddings, &voiced_ok(&[0]), &identities)
                .is_empty()
        );
        assert_eq!(speakers[0].display_name, None);
    }

    #[test]
    fn existing_display_name_is_never_overridden() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(1);
        speakers[0].display_name = Some("张三".to_string());
        let embeddings = BTreeMap::from([(0, emb(0.1))]);

        let assigned =
            auto_identify_speakers(&mut speakers, &embeddings, &voiced_ok(&[0]), &identities);

        assert!(assigned.is_empty());
        assert_eq!(speakers[0].display_name.as_deref(), Some("张三"));
    }

    #[test]
    fn speaker_without_embedding_is_left_alone() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(2);
        // Only S2 has an embedding; S1 has none (e.g. all its turns too short).
        let embeddings = BTreeMap::from([(1, emb(0.1))]);

        let assigned =
            auto_identify_speakers(&mut speakers, &embeddings, &voiced_ok(&[1]), &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S2");
        assert_eq!(speakers[0].display_name, None);
        assert_eq!(speakers[1].display_name.as_deref(), Some("李明"));
    }

    #[test]
    fn apply_with_no_identity_dir_is_a_noop() {
        let mut speakers = speakers(1);
        let embeddings = BTreeMap::from([(0, emb(0.1))]);
        assert!(
            apply_auto_identification(&mut speakers, &embeddings, &voiced_ok(&[0]), None)
                .is_empty()
        );
        assert_eq!(speakers[0].display_name, None);
    }

    #[test]
    fn persist_speaker_embeddings_round_trips_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.sqlite")).unwrap();
        let mut meeting = lumen_core::Meeting::new();
        meeting.status = lumen_core::MeetingStatus::Processing;
        store.create_meeting(&meeting).unwrap();
        let rows: Vec<Speaker> = (0..2u32)
            .map(|i| Speaker::new(meeting.id, speaker_label(i)))
            .collect();
        for row in &rows {
            store.upsert_speaker(row).unwrap();
        }
        // Engine id 5 has no matching row → skipped, not an error.
        let embeddings = BTreeMap::from([(0, emb(0.1)), (1, emb(0.2)), (5, emb(0.3))]);

        persist_speaker_embeddings(&store, &rows, &embeddings).unwrap();

        assert_eq!(
            store.get_speaker_embedding(rows[0].id).unwrap(),
            Some(emb(0.1))
        );
        assert_eq!(
            store.get_speaker_embedding(rows[1].id).unwrap(),
            Some(emb(0.2))
        );
    }
}
