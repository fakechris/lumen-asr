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
//! Pure logic over already-computed embeddings — no models, no platform
//! gating — so it is fully unit-testable with mock vectors.

use std::collections::BTreeMap;
use std::path::Path;

use lumen_core::Speaker;
use lumen_identity::IdentityStore;
use lumen_store::Store;

use crate::assemble::speaker_label;

/// One auto-identified speaker: which cluster was recognized as whom, and how
/// confidently (cosine similarity in `[-1, 1]`).
#[derive(Debug, Clone, PartialEq)]
pub struct AutoIdentification {
    /// Engine label of the matched cluster, e.g. `"S2"`.
    pub label: String,
    /// The enrolled identity's real name that was assigned.
    pub name: String,
    /// Cosine similarity of the match (≥ [`lumen_identity::MATCH_THRESHOLD`]).
    pub score: f32,
}

/// Match every still-unnamed speaker's centroid against the enrolled identity
/// library and assign `display_name` on a confident hit. Speakers that already
/// carry a `display_name` (e.g. re-processing after manual confirmation) and
/// speakers without an embedding are left untouched. Returns what was
/// assigned, for logging/diagnostics.
///
/// `embeddings` is keyed by engine speaker id (as produced by diarization);
/// the id maps to a [`Speaker`] row via its `S{id+1}` label.
pub fn auto_identify_speakers(
    speakers: &mut [Speaker],
    embeddings: &BTreeMap<u32, Vec<f32>>,
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
        if let Some((name, score)) = identities.match_speaker(embedding) {
            speaker.display_name = Some(name.to_string());
            assigned.push(AutoIdentification {
                label,
                name: name.to_string(),
                score,
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
    let assigned = auto_identify_speakers(speakers, embeddings, &identities);
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

    fn identity_store_with(entries: &[(&str, f32)]) -> (tempfile::TempDir, IdentityStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = IdentityStore::open(dir.path()).unwrap();
        for (name, seed) in entries {
            store.enroll(name, &emb(*seed), None).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn confident_match_assigns_display_name_and_reports_score() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(2);
        let embeddings = BTreeMap::from([(0, emb(0.1)), (1, emb(7.7))]);

        let assigned = auto_identify_speakers(&mut speakers, &embeddings, &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S1");
        assert_eq!(assigned[0].name, "李明");
        assert!(assigned[0].score > 0.999);
        assert_eq!(speakers[0].display_name.as_deref(), Some("李明"));
        // The dissimilar speaker stays unnamed (说话人2 in the UI).
        assert_eq!(speakers[1].display_name, None);
    }

    #[test]
    fn empty_identity_library_assigns_nothing() {
        let (_dir, identities) = identity_store_with(&[]);
        let mut speakers = speakers(1);
        let embeddings = BTreeMap::from([(0, emb(0.1))]);
        assert!(auto_identify_speakers(&mut speakers, &embeddings, &identities).is_empty());
        assert_eq!(speakers[0].display_name, None);
    }

    #[test]
    fn existing_display_name_is_never_overridden() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(1);
        speakers[0].display_name = Some("张三".to_string());
        let embeddings = BTreeMap::from([(0, emb(0.1))]);

        let assigned = auto_identify_speakers(&mut speakers, &embeddings, &identities);

        assert!(assigned.is_empty());
        assert_eq!(speakers[0].display_name.as_deref(), Some("张三"));
    }

    #[test]
    fn speaker_without_embedding_is_left_alone() {
        let (_dir, identities) = identity_store_with(&[("李明", 0.1)]);
        let mut speakers = speakers(2);
        // Only S2 has an embedding; S1 has none (e.g. all its turns too short).
        let embeddings = BTreeMap::from([(1, emb(0.1))]);

        let assigned = auto_identify_speakers(&mut speakers, &embeddings, &identities);

        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].label, "S2");
        assert_eq!(speakers[0].display_name, None);
        assert_eq!(speakers[1].display_name.as_deref(), Some("李明"));
    }

    #[test]
    fn apply_with_no_identity_dir_is_a_noop() {
        let mut speakers = speakers(1);
        let embeddings = BTreeMap::from([(0, emb(0.1))]);
        assert!(apply_auto_identification(&mut speakers, &embeddings, None).is_empty());
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
