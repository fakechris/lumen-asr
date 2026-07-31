//! Local speaker-identity library (voiceprint enrollment, meeting M5).
//!
//! A user can "enroll" a confirmed meeting speaker: the speaker's centroid
//! voiceprint embedding (WeSpeaker ResNet34-LM, 256-d, produced by the
//! diarization pipeline) is stored under a local identity directory together
//! with the person's real name. Later meetings match each diarized speaker's
//! centroid against the enrolled set by cosine similarity and, on a confident
//! hit, auto-assign the real name.
//!
//! ## Storage
//! One JSON file per identity under the identity directory
//! (`~/Library/Application Support/Lumen/identity/` on macOS): name, 256-d
//! vector, enrollment time, source meeting. Everything stays local — nothing
//! here talks to the network.
//!
//! ## Matching
//! Cosine similarity between L2-normalizable centroids. The default threshold
//! ([`MATCH_THRESHOLD`]) is deliberately conservative: a false positive
//! (silently mislabeling a stranger as an enrolled person) is worse than a
//! false negative (leaving "说话人N" for the user to confirm manually).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Dimensionality of the speaker embeddings this library stores (WeSpeaker
/// ResNet34-LM x-vectors as produced by diar-rs).
pub const EMBEDDING_DIM: usize = 256;

/// Minimum cosine similarity for an automatic identity match.
///
/// Trade-off: raw WeSpeaker cosine similarity for the *same* speaker across
/// recording sessions typically lands around 0.55–0.85, while different
/// speakers usually score below ~0.4. `0.65` sits at the conservative end of
/// the useful 0.6–0.7 band: it favors "no match" (speaker stays `说话人N`,
/// user confirms manually) over a wrong auto-assigned name. Tune here if field
/// data shows it is too strict/loose.
pub const MATCH_THRESHOLD: f32 = 0.65;

/// One enrolled identity: a real name bound to a voiceprint centroid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolledIdentity {
    pub id: Uuid,
    /// The person's real name, e.g. "李明". Unique within the store (re-enroll
    /// with the same name replaces the stored embedding).
    pub name: String,
    /// Centroid voiceprint embedding ([`EMBEDDING_DIM`] floats).
    pub embedding: Vec<f32>,
    pub enrolled_at: DateTime<Utc>,
    /// The meeting this voiceprint was enrolled from, when known.
    pub source_meeting_id: Option<Uuid>,
}

/// Failure modes of the identity store.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity io: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("embedding must have {EMBEDDING_DIM} dims, got {0}")]
    BadDimension(usize),
    #[error("identity name must not be empty")]
    EmptyName,
}

/// File-backed store of enrolled identities: one `<id>.json` per identity
/// under `dir`. Loaded eagerly on [`open`](Self::open); mutations write
/// through to disk (atomic tmp + rename).
#[derive(Debug)]
pub struct IdentityStore {
    dir: PathBuf,
    identities: Vec<EnrolledIdentity>,
}

impl IdentityStore {
    /// Open (creating the directory if needed) and load every `*.json`
    /// identity file. Unreadable/invalid files are skipped rather than failing
    /// the whole store, so one corrupt record never disables enrollment.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, IdentityError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let mut identities = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(IdentityError::from)
                .and_then(|text| {
                    serde_json::from_str::<EnrolledIdentity>(&text).map_err(Into::into)
                }) {
                Ok(identity) if identity.embedding.len() == EMBEDDING_DIM => {
                    identities.push(identity)
                }
                _ => continue, // skip corrupt / wrong-dimension records
            }
        }
        // Stable order for list/UI regardless of directory iteration order.
        identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { dir, identities })
    }

    /// Enroll `name` with the given centroid `embedding`. Re-enrolling an
    /// existing name replaces its embedding/metadata (keeping the identity id),
    /// so "重新注册" refreshes the voiceprint instead of duplicating the person.
    pub fn enroll(
        &mut self,
        name: &str,
        embedding: &[f32],
        source_meeting_id: Option<Uuid>,
    ) -> Result<EnrolledIdentity, IdentityError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(IdentityError::EmptyName);
        }
        if embedding.len() != EMBEDDING_DIM {
            return Err(IdentityError::BadDimension(embedding.len()));
        }
        let existing_id = self
            .identities
            .iter()
            .find(|i| i.name == name)
            .map(|i| i.id);
        let identity = EnrolledIdentity {
            id: existing_id.unwrap_or_else(Uuid::new_v4),
            name: name.to_string(),
            embedding: embedding.to_vec(),
            enrolled_at: Utc::now(),
            source_meeting_id,
        };
        self.write_identity(&identity)?;
        self.identities.retain(|i| i.id != identity.id);
        self.identities.push(identity.clone());
        self.identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(identity)
    }

    /// Match a speaker centroid against the enrolled set with the default
    /// [`MATCH_THRESHOLD`]. Returns the best-scoring name and its cosine
    /// similarity, or `None` when the store is empty or nothing clears the
    /// threshold.
    pub fn match_speaker(&self, embedding: &[f32]) -> Option<(&str, f32)> {
        self.match_speaker_with_threshold(embedding, MATCH_THRESHOLD)
    }

    /// [`match_speaker`](Self::match_speaker) with an explicit threshold.
    pub fn match_speaker_with_threshold(
        &self,
        embedding: &[f32],
        threshold: f32,
    ) -> Option<(&str, f32)> {
        let mut best: Option<(&str, f32)> = None;
        for identity in &self.identities {
            let score = cosine_similarity(embedding, &identity.embedding);
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((identity.name.as_str(), score));
            }
        }
        best.filter(|&(_, score)| score >= threshold)
    }

    /// All enrolled identities, name-ordered.
    pub fn list(&self) -> &[EnrolledIdentity] {
        &self.identities
    }

    /// Remove an identity by id (memory + disk). Returns `true` if it existed.
    pub fn remove(&mut self, id: Uuid) -> Result<bool, IdentityError> {
        let before = self.identities.len();
        self.identities.retain(|i| i.id != id);
        if self.identities.len() == before {
            return Ok(false);
        }
        let path = self.identity_path(id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(true)
    }

    fn identity_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Atomic write: serialize to `<id>.json.tmp`, then rename over the final
    /// path, so a crash mid-write never leaves a truncated identity file.
    fn write_identity(&self, identity: &EnrolledIdentity) -> Result<(), IdentityError> {
        let path = self.identity_path(identity.id);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(identity)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Cosine similarity of two vectors; `0.0` for mismatched lengths or zero
/// vectors (treated as "no similarity" rather than an error, since a degenerate
/// centroid should simply never match).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += f64::from(x) * f64::from(y);
        na += f64::from(x) * f64::from(x);
        nb += f64::from(y) * f64::from(y);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

/// Default identity directory for the Lumen app cluster:
/// `~/Library/Application Support/Lumen/identity` on macOS, `~/.lumen/identity`
/// elsewhere — a sibling of the shared `Lumen/models` root used by
/// `lumen-models`. Embeddings are stored here and only here (local-only).
pub fn default_identity_dir() -> PathBuf {
    let home = user_home_dir();
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Lumen/identity")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".lumen/identity")
    }
}

/// Resolve the user home directory: `HOME` → `USERPROFILE` → temp dir. Mirrors
/// the resolution used by the shared `lumen-models` path layer.
fn user_home_dir() -> PathBuf {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    std::env::temp_dir()
}

/// Convenience for tests/tools: does the path look like an identity file?
pub fn is_identity_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emb(seed: f32) -> Vec<f32> {
        // Deterministic non-degenerate vector; different seeds are (almost)
        // orthogonal enough after the alternating pattern below.
        (0..EMBEDDING_DIM)
            .map(|i| ((i as f32) * seed).sin())
            .collect()
    }

    fn store() -> (tempfile::TempDir, IdentityStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn empty_store_matches_nothing() {
        let (_dir, store) = store();
        assert!(store.list().is_empty());
        assert!(store.match_speaker(&emb(0.1)).is_none());
    }

    #[test]
    fn enroll_then_match_same_embedding_hits_with_perfect_score() {
        let (_dir, mut store) = store();
        store.enroll("李明", &emb(0.1), None).unwrap();
        let (name, score) = store.match_speaker(&emb(0.1)).expect("should match");
        assert_eq!(name, "李明");
        assert!(score > 0.999, "self-similarity should be ~1.0, got {score}");
    }

    #[test]
    fn dissimilar_embedding_does_not_match() {
        let (_dir, mut store) = store();
        store.enroll("李明", &emb(0.1), None).unwrap();
        // A very different pattern scores far below the threshold.
        assert!(store.match_speaker(&emb(7.7)).is_none());
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        let (_dir, mut store) = store();
        store.enroll("A", &emb(0.1), None).unwrap();
        // Exactly at threshold → match; just above → no match.
        assert!(store.match_speaker_with_threshold(&emb(0.1), 1.0).is_some());
        assert!(store
            .match_speaker_with_threshold(&emb(7.7), MATCH_THRESHOLD)
            .is_none());
    }

    #[test]
    fn best_of_multiple_identities_wins() {
        let (_dir, mut store) = store();
        store.enroll("甲", &emb(0.1), None).unwrap();
        store.enroll("乙", &emb(7.7), None).unwrap();
        let (name, _) = store.match_speaker(&emb(7.7)).expect("should match 乙");
        assert_eq!(name, "乙");
    }

    #[test]
    fn persistence_roundtrip_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let meeting = Uuid::new_v4();
        {
            let mut store = IdentityStore::open(dir.path()).unwrap();
            store.enroll("李明", &emb(0.1), Some(meeting)).unwrap();
        }
        let store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(store.list().len(), 1);
        let identity = &store.list()[0];
        assert_eq!(identity.name, "李明");
        assert_eq!(identity.source_meeting_id, Some(meeting));
        assert_eq!(identity.embedding.len(), EMBEDDING_DIM);
        assert!(store.match_speaker(&emb(0.1)).is_some());
    }

    #[test]
    fn reenroll_same_name_replaces_embedding_and_keeps_one_record() {
        let (dir, mut store) = store();
        let first = store.enroll("李明", &emb(0.1), None).unwrap();
        let second = store.enroll("李明", &emb(7.7), None).unwrap();
        assert_eq!(first.id, second.id, "same person keeps one identity id");
        assert_eq!(store.list().len(), 1);
        // Old embedding no longer matches; new one does.
        assert!(store.match_speaker(&emb(7.7)).is_some());
        // Exactly one file on disk.
        let files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| is_identity_file(&e.as_ref().unwrap().path()))
            .count();
        assert_eq!(files, 1);
    }

    #[test]
    fn remove_deletes_record_and_file() {
        let (dir, mut store) = store();
        let identity = store.enroll("李明", &emb(0.1), None).unwrap();
        assert!(store.remove(identity.id).unwrap());
        assert!(store.list().is_empty());
        assert!(
            !store.remove(identity.id).unwrap(),
            "second remove is a no-op"
        );
        let files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| is_identity_file(&e.as_ref().unwrap().path()))
            .count();
        assert_eq!(files, 0);
        // And it stays gone across reopen.
        let store = IdentityStore::open(dir.path()).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn enroll_rejects_empty_name_and_bad_dimension() {
        let (_dir, mut store) = store();
        assert!(matches!(
            store.enroll("  ", &emb(0.1), None),
            Err(IdentityError::EmptyName)
        ));
        assert!(matches!(
            store.enroll("李明", &[0.5; 8], None),
            Err(IdentityError::BadDimension(8))
        ));
    }

    #[test]
    fn corrupt_identity_file_is_skipped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = IdentityStore::open(dir.path()).unwrap();
            store.enroll("李明", &emb(0.1), None).unwrap();
        }
        std::fs::write(dir.path().join("broken.json"), b"{not json").unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(
            store.list().len(),
            1,
            "valid record survives, corrupt skipped"
        );
    }

    #[test]
    fn cosine_similarity_basics() {
        assert!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) > 0.999);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // Length mismatch / zero vectors are "no similarity", not errors.
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
