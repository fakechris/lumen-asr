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
//! (`~/Library/Application Support/Lumen/identity/` on macOS): name plus a
//! list of voiceprint **samples** (one 256-d vector each, with enrollment time,
//! voiced duration, and source meeting). Re-enrolling the same person appends a
//! sample instead of overwriting, so the identity accumulates the person's
//! voice across microphones/rooms/days — capped at
//! [`MAX_SAMPLES_PER_IDENTITY`], evicting the oldest. Files written before the
//! multi-sample format (a single top-level `embedding`) still load: they read
//! as an identity with one sample and are rewritten in the new format on the
//! next enrollment. Everything stays local — nothing here talks to the network.
//!
//! ## Matching
//! Cosine similarity against **every** stored sample, combined with a
//! two-threshold consensus rule (see [`IdentityStore::match_speaker`]):
//!
//! - any single sample ≥ [`AUTO_TAG_THRESHOLD`] is a confident hit on its own;
//! - otherwise, at least half of the person's samples (and no fewer than two)
//!   must reach [`CONSENSUS_THRESHOLD`] — several independent recordings
//!   agreeing "sounds like this person" substitutes for one high-confidence
//!   score, while one lukewarm sample alone never tags anyone.
//!
//! The thresholds stay deliberately conservative: a false positive (silently
//! mislabeling a stranger as an enrolled person) is worse than a false negative
//! (leaving "说话人N" for the user to confirm manually).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Dimensionality of the speaker embeddings this library stores (WeSpeaker
/// ResNet34-LM x-vectors as produced by diar-rs).
pub const EMBEDDING_DIM: usize = 256;

/// Minimum cosine similarity for a **single sample** to auto-tag on its own.
///
/// Trade-off: raw WeSpeaker cosine similarity for the *same* speaker across
/// recording sessions typically lands around 0.55–0.85, while different
/// speakers usually score below ~0.4. `0.70` is the high-confidence end of
/// that band: one sample this close is enough evidence by itself. Scores in
/// the grey zone below rely on the consensus rule instead. Tune here if field
/// data shows it is too strict/loose.
pub const AUTO_TAG_THRESHOLD: f32 = 0.70;

/// Minimum cosine similarity for a sample to count as one **consensus vote**.
///
/// A single sample at `0.60–0.70` is ambiguous (same speaker on a different
/// microphone, or just a similar voice). But when at least half of a person's
/// samples — recorded in different meetings/conditions — independently score
/// ≥ this value, the agreement itself is the evidence, so the match is
/// accepted without any sample reaching [`AUTO_TAG_THRESHOLD`].
pub const CONSENSUS_THRESHOLD: f32 = 0.60;

/// Upper bound on stored samples per identity. Enrolling beyond it evicts the
/// oldest sample: recent recordings track the person's current voice/devices
/// better, and the cap keeps per-speaker matching cost bounded.
pub const MAX_SAMPLES_PER_IDENTITY: usize = 10;

/// Minimum voiced audio (ms) for a **live** utterance to earn even a
/// provisional label. Below ~2 s a single-utterance embedding is too noisy to
/// suggest anyone (the offline path keeps the stricter [`MIN_VOICED_MS`]).
pub const LIVE_PROVISIONAL_MIN_VOICED_MS: u64 = 2000;

/// Minimum voiced audio (ms) for a live utterance to be auto-verified. Same
/// floor as enrollment/offline matching ([`MIN_VOICED_MS`]): 2–3 s utterances
/// stay provisional, ≥ 3 s ones may upgrade when the score evidence agrees.
pub const LIVE_VERIFIED_MIN_VOICED_MS: u64 = MIN_VOICED_MS;

/// Minimum margin (`best_score − runner_up_score`) for a live auto-verify.
///
/// Trade-off: two enrolled people with similar voices can both score past
/// [`AUTO_TAG_THRESHOLD`] on one utterance; requiring the best identity to
/// beat the runner-up by ≥ 0.08 keeps ambiguous "could be either" utterances
/// at provisional instead of silently committing to the wrong person. 0.08 is
/// roughly the intra-speaker session-to-session score jitter, so a genuine
/// same-person hit clears it comfortably.
pub const LIVE_VERIFIED_MIN_MARGIN: f32 = 0.08;

/// Minimum total voiced audio (milliseconds) required to enroll a voiceprint.
///
/// Centroids averaged over less speech than this are too noisy to be worth
/// storing — a 2-second snippet can land anywhere in embedding space and then
/// mislabels people forever. The meeting pipeline applies the same floor
/// before *matching* a diarized speaker (`IDENTIFY_MIN_VOICED_MS` in
/// `lumen-meeting` reuses this constant), so both sides of the comparison are
/// backed by enough material.
pub const MIN_VOICED_MS: u64 = 3000;

/// One stored voiceprint sample: a centroid embedding from a single meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceprintSample {
    /// Centroid voiceprint embedding ([`EMBEDDING_DIM`] floats).
    pub embedding: Vec<f32>,
    /// Total voiced milliseconds that backed this centroid. `0` for samples
    /// migrated from the legacy single-embedding format (duration was not
    /// recorded then); informational only — matching never reads it.
    #[serde(default)]
    pub voiced_ms: u64,
    pub enrolled_at: DateTime<Utc>,
    /// The meeting this sample was enrolled from, when known.
    pub source_meeting_id: Option<Uuid>,
    /// Path to the recording this sample was embedded from, when it maps to a
    /// single playable file (e.g. a dictation WAV for self-enrollment). Lets the
    /// UI play the sample back so the user can confirm it's really them, and
    /// dedupe re-scans by source. `None` for meeting centroids (averaged over
    /// many turns) and legacy samples.
    #[serde(default)]
    pub source_audio_path: Option<String>,
    /// A short human label for the sample — e.g. what was said in that
    /// recording — so the list is recognizable at a glance. `None` when unknown.
    #[serde(default)]
    pub source_label: Option<String>,
}

/// Provenance for a newly enrolled sample (see [`IdentityStore::enroll_sample`]).
/// All fields optional — `default()` records no provenance, the plain
/// [`enroll`](IdentityStore::enroll) behavior.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampleSource {
    /// The meeting this sample came from, when it came from a meeting.
    pub meeting_id: Option<Uuid>,
    /// Path to the single recording this sample was embedded from, when one
    /// exists (e.g. a dictation WAV) — makes the sample playable.
    pub audio_path: Option<String>,
    /// A short human label (e.g. what was said), for a recognizable list.
    pub label: Option<String>,
}

/// One enrolled identity: a real name bound to one or more voiceprint samples.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnrolledIdentity {
    pub id: Uuid,
    /// The person's real name, e.g. "李明". Unique within the store (re-enroll
    /// with the same name appends a sample to the same identity).
    pub name: String,
    /// Voiceprint samples, oldest first; never empty, at most
    /// [`MAX_SAMPLES_PER_IDENTITY`].
    pub samples: Vec<VoiceprintSample>,
}

impl EnrolledIdentity {
    /// The most recently enrolled sample (identities always hold ≥ 1 sample,
    /// so this is `None` only for a hand-built empty value).
    pub fn latest_sample(&self) -> Option<&VoiceprintSample> {
        self.samples.iter().max_by_key(|s| s.enrolled_at)
    }
}

/// On-disk identity shape covering both formats: the current multi-`samples`
/// layout and the legacy single-`embedding` layout (pre-multi-sample files).
/// Legacy fields are only read — [`IdentityStore`] always writes the new
/// format.
#[derive(Deserialize)]
struct PersistedIdentity {
    id: Uuid,
    name: String,
    #[serde(default)]
    samples: Vec<VoiceprintSample>,
    // Legacy single-embedding format (one sample inlined at the top level).
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    enrolled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    source_meeting_id: Option<Uuid>,
}

impl PersistedIdentity {
    /// Normalize into the in-memory shape, lifting a legacy single embedding
    /// into one sample. Returns `None` when no valid-dimension sample remains.
    fn into_identity(self) -> Option<EnrolledIdentity> {
        let mut samples = self.samples;
        if samples.is_empty() {
            samples.push(VoiceprintSample {
                embedding: self.embedding?,
                voiced_ms: 0, // unknown for legacy records
                enrolled_at: self.enrolled_at?,
                source_meeting_id: self.source_meeting_id,
                source_audio_path: None,
                source_label: None,
            });
        }
        samples.retain(|s| s.embedding.len() == EMBEDDING_DIM);
        samples.sort_by(|a, b| a.enrolled_at.cmp(&b.enrolled_at));
        if samples.is_empty() {
            return None;
        }
        Some(EnrolledIdentity {
            id: self.id,
            name: self.name,
            samples,
        })
    }
}

/// Full match evidence for one enrolled identity against a probe embedding —
/// the "deep" verification interface. It reports *who the best candidate is
/// and how strong the evidence is* without making any accept/reject decision;
/// decisions live in the policies layered on top ([`live_decision`] for the
/// real-time path, [`IdentityStore::match_speaker`] for the offline path).
#[derive(Debug, Clone, PartialEq)]
pub struct VerificationReport {
    /// Id of the best-matching enrolled identity.
    pub identity_id: Uuid,
    /// That identity's current real name.
    pub display_name: String,
    /// Highest cosine similarity across the identity's stored samples.
    pub best_score: f32,
    /// Best-sample score of the strongest *other* enrolled identity, or `-1.0`
    /// (the cosine floor) when this is the only identity — so `margin` is then
    /// maximally permissive rather than undefined.
    pub runner_up_score: f32,
    /// `best_score - runner_up_score`: how clearly the winner beats the field.
    pub margin: f32,
    /// Samples of the winning identity scoring ≥ [`CONSENSUS_THRESHOLD`].
    pub consensus_votes: usize,
    /// Total stored samples of the winning identity.
    pub sample_count: usize,
}

/// Decision of the **real-time** policy ([`live_decision`]) for one finalized
/// live utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveDecision {
    /// Strong evidence: show the name as auto-verified.
    VerifiedAuto,
    /// Plausible but not conclusive: show the name tentatively ("李明?").
    Provisional,
    /// Not enough evidence to suggest anyone.
    NoMatch,
}

/// Real-time decision policy over a [`VerificationReport`] (deliberately
/// separate from the offline consensus policy in
/// [`IdentityStore::match_speaker`]: live utterances are single short spans,
/// so the tiers lean on duration + margin instead of multi-sample consensus):
///
/// - `voiced_ms ≥ 3000` **and** `best_score ≥` [`AUTO_TAG_THRESHOLD`] **and**
///   `margin ≥` [`LIVE_VERIFIED_MIN_MARGIN`] → [`LiveDecision::VerifiedAuto`];
/// - otherwise `voiced_ms ≥ 2000` and `best_score ≥` [`CONSENSUS_THRESHOLD`]
///   → [`LiveDecision::Provisional`] (grey-zone score, or a verified-grade
///   score on a 2–3 s utterance that is too short to trust outright);
/// - otherwise [`LiveDecision::NoMatch`].
pub fn live_decision(report: &VerificationReport, voiced_ms: u64) -> LiveDecision {
    if voiced_ms >= LIVE_VERIFIED_MIN_VOICED_MS
        && report.best_score >= AUTO_TAG_THRESHOLD
        && report.margin >= LIVE_VERIFIED_MIN_MARGIN
    {
        return LiveDecision::VerifiedAuto;
    }
    if voiced_ms >= LIVE_PROVISIONAL_MIN_VOICED_MS && report.best_score >= CONSENSUS_THRESHOLD {
        return LiveDecision::Provisional;
    }
    LiveDecision::NoMatch
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
    #[error("voiced audio too short to enroll: {voiced_ms} ms (need at least {MIN_VOICED_MS} ms)")]
    VoiceTooShort { voiced_ms: u64 },
    #[error("no enrolled identity with id {0}")]
    NotFound(Uuid),
    #[error("an identity named {0:?} already exists")]
    NameExists(String),
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
    /// identity file — both the current multi-sample format and the legacy
    /// single-embedding one. Unreadable/invalid files are skipped rather than
    /// failing the whole store, so one corrupt record never disables
    /// enrollment.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, IdentityError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        // Voiceprints are biometric data: keep the library owner-only (0700
        // dir / 0600 files; no-op on non-unix).
        restrict_permissions(&dir, 0o700)?;
        let mut identities = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(IdentityError::from)
                .and_then(|text| {
                    serde_json::from_str::<PersistedIdentity>(&text).map_err(Into::into)
                }) {
                Ok(persisted) => {
                    if let Some(identity) = persisted.into_identity() {
                        identities.push(identity);
                    }
                }
                _ => continue, // skip corrupt records
            }
        }
        // Stable order for list/UI regardless of directory iteration order.
        identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { dir, identities })
    }

    /// Enroll one voiceprint sample for `name`. A new name creates the
    /// identity; an existing name **appends** the sample (多次注册 = 多样本),
    /// evicting the oldest sample beyond [`MAX_SAMPLES_PER_IDENTITY`].
    ///
    /// `voiced_ms` is the total voiced audio behind `embedding`; below
    /// [`MIN_VOICED_MS`] the enrollment is rejected
    /// ([`IdentityError::VoiceTooShort`]) because such centroids are too noisy
    /// to be trusted for future auto-identification.
    pub fn enroll(
        &mut self,
        name: &str,
        embedding: &[f32],
        voiced_ms: u64,
        source_meeting_id: Option<Uuid>,
    ) -> Result<EnrolledIdentity, IdentityError> {
        self.enroll_sample(
            name,
            embedding,
            voiced_ms,
            SampleSource {
                meeting_id: source_meeting_id,
                ..SampleSource::default()
            },
        )
    }

    /// Like [`enroll`](Self::enroll), but records the sample's full provenance
    /// ([`SampleSource`]) — its source recording path and a human label — so the
    /// UI can play the sample back and dedupe re-scans by source.
    pub fn enroll_sample(
        &mut self,
        name: &str,
        embedding: &[f32],
        voiced_ms: u64,
        source: SampleSource,
    ) -> Result<EnrolledIdentity, IdentityError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(IdentityError::EmptyName);
        }
        if embedding.len() != EMBEDDING_DIM {
            return Err(IdentityError::BadDimension(embedding.len()));
        }
        if voiced_ms < MIN_VOICED_MS {
            return Err(IdentityError::VoiceTooShort { voiced_ms });
        }
        // Build the updated identity without touching memory, write it to disk,
        // then swap it in — a failed write leaves memory and disk consistent.
        let mut identity = self
            .identities
            .iter()
            .find(|i| i.name == name)
            .cloned()
            .unwrap_or_else(|| EnrolledIdentity {
                id: Uuid::new_v4(),
                name: name.to_string(),
                samples: Vec::new(),
            });
        identity.samples.push(VoiceprintSample {
            embedding: embedding.to_vec(),
            voiced_ms,
            enrolled_at: Utc::now(),
            source_meeting_id: source.meeting_id,
            source_audio_path: source.audio_path,
            source_label: source.label,
        });
        // Samples are kept oldest-first (stable sort, so same-instant samples
        // keep insertion order); evict from the front when over the cap.
        identity
            .samples
            .sort_by(|a, b| a.enrolled_at.cmp(&b.enrolled_at));
        if identity.samples.len() > MAX_SAMPLES_PER_IDENTITY {
            let excess = identity.samples.len() - MAX_SAMPLES_PER_IDENTITY;
            identity.samples.drain(..excess);
        }
        self.write_identity(&identity)?;
        self.identities.retain(|i| i.id != identity.id);
        self.identities.push(identity.clone());
        self.identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(identity)
    }

    /// Match a speaker centroid against the enrolled set with the default
    /// thresholds ([`AUTO_TAG_THRESHOLD`] / [`CONSENSUS_THRESHOLD`]).
    ///
    /// A person is a candidate when **either**:
    /// - any one of their samples scores ≥ `AUTO_TAG_THRESHOLD` (single
    ///   high-confidence hit), or
    /// - at least **half** of their samples — and no fewer than **two** — score
    ///   ≥ `CONSENSUS_THRESHOLD` (several independent recordings agreeing
    ///   beats one lukewarm score; the two-sample floor stops a lone
    ///   just-past-0.60 sample from tagging anyone).
    ///
    /// Among multiple candidates, the highest best-sample score wins. Returns
    /// that name and score, or `None` when the store is empty or nobody
    /// qualifies.
    pub fn match_speaker(&self, embedding: &[f32]) -> Option<(&str, f32)> {
        self.match_speaker_with_thresholds(embedding, AUTO_TAG_THRESHOLD, CONSENSUS_THRESHOLD)
    }

    /// [`match_speaker`](Self::match_speaker) with explicit thresholds
    /// (`auto_tag` for the single-sample rule, `consensus` for the
    /// half-of-samples rule). A thin decision policy over the shared scoring
    /// pass ([`score_identities`](Self::score_identities)): candidates must
    /// qualify by threshold/consensus, then the highest best-sample score wins.
    pub fn match_speaker_with_thresholds(
        &self,
        embedding: &[f32],
        auto_tag: f32,
        consensus: f32,
    ) -> Option<(&str, f32)> {
        self.score_identities(embedding, consensus)
            .into_iter()
            .filter(|s| s.qualifies(auto_tag))
            .max_by(|a, b| a.best.total_cmp(&b.best))
            .map(|s| (self.identities[s.index].name.as_str(), s.best))
    }

    /// [`match_speaker`](Self::match_speaker) (same offline decision policy),
    /// but returning the winner's full [`VerificationReport`] so callers can
    /// persist provenance (identity id + confidence) alongside the name.
    pub fn match_speaker_report(&self, embedding: &[f32]) -> Option<VerificationReport> {
        let scored = self.score_identities(embedding, CONSENSUS_THRESHOLD);
        let winner = scored
            .iter()
            .filter(|s| s.qualifies(AUTO_TAG_THRESHOLD))
            .max_by(|a, b| a.best.total_cmp(&b.best))?;
        Some(self.report_for(winner, &scored))
    }

    /// The deep verification interface: score the probe `embedding` against
    /// **every** enrolled identity and return the best candidate's full
    /// evidence — no thresholds, no decision (see [`VerificationReport`]).
    /// `None` only when the store is empty.
    pub fn verify_speaker(&self, embedding: &[f32]) -> Option<VerificationReport> {
        let scored = self.score_identities(embedding, CONSENSUS_THRESHOLD);
        let winner = scored.iter().max_by(|a, b| a.best.total_cmp(&b.best))?;
        Some(self.report_for(winner, &scored))
    }

    /// One scoring pass shared by every matching entry point: per identity,
    /// the best sample score plus the number of samples ≥ `consensus`.
    fn score_identities(&self, embedding: &[f32], consensus: f32) -> Vec<IdentityScore> {
        self.identities
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let mut best = f32::NEG_INFINITY;
                let mut votes = 0usize;
                for sample in &identity.samples {
                    let score = cosine_similarity(embedding, &sample.embedding);
                    best = best.max(score);
                    if score >= consensus {
                        votes += 1;
                    }
                }
                IdentityScore {
                    index,
                    best,
                    votes,
                    sample_count: identity.samples.len(),
                }
            })
            .collect()
    }

    /// Assemble the [`VerificationReport`] for `winner`, deriving the
    /// runner-up score from the best of every *other* identity (`-1.0`, the
    /// cosine floor, when there is none).
    fn report_for(&self, winner: &IdentityScore, scored: &[IdentityScore]) -> VerificationReport {
        let runner_up_score = scored
            .iter()
            .filter(|s| s.index != winner.index)
            .map(|s| s.best)
            .fold(f32::NEG_INFINITY, f32::max);
        let runner_up_score = if runner_up_score.is_finite() {
            runner_up_score
        } else {
            -1.0
        };
        let identity = &self.identities[winner.index];
        VerificationReport {
            identity_id: identity.id,
            display_name: identity.name.clone(),
            best_score: winner.best,
            runner_up_score,
            margin: winner.best - runner_up_score,
            consensus_votes: winner.votes,
            sample_count: winner.sample_count,
        }
    }

    /// All enrolled identities, name-ordered.
    pub fn list(&self) -> &[EnrolledIdentity] {
        &self.identities
    }

    /// Remove an identity by id (disk, then memory) — the whole person, all
    /// samples. Returns `true` if it existed. Disk first — mirroring
    /// [`enroll`](Self::enroll) — so a failed file deletion leaves memory and
    /// disk consistent (the identity stays listed and enrolled) instead of
    /// resurrecting on the next open.
    pub fn remove(&mut self, id: Uuid) -> Result<bool, IdentityError> {
        if !self.identities.iter().any(|i| i.id == id) {
            return Ok(false);
        }
        match fs::remove_file(self.identity_path(id)) {
            Ok(()) => {}
            // Already gone on disk (e.g. deleted externally): still drop it
            // from memory below.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.identities.retain(|i| i.id != id);
        Ok(true)
    }

    /// Rename identity `id` to `new_name` (keeping its id and samples).
    ///
    /// The name is the identity's key for the user, but the file is named by
    /// id, so a rename only rewrites the `name` field of the same file. Renaming
    /// to the identity's current name is a no-op; renaming onto a *different*
    /// existing identity's name is rejected ([`IdentityError::NameExists`]) —
    /// the caller should [`merge`](Self::merge) those two instead, which is the
    /// explicit "these are the same person" operation.
    pub fn rename(&mut self, id: Uuid, new_name: &str) -> Result<EnrolledIdentity, IdentityError> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(IdentityError::EmptyName);
        }
        if self
            .identities
            .iter()
            .any(|i| i.id != id && i.name == new_name)
        {
            return Err(IdentityError::NameExists(new_name.to_string()));
        }
        let mut identity = self
            .identities
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or(IdentityError::NotFound(id))?;
        if identity.name == new_name {
            return Ok(identity); // no-op; nothing to rewrite
        }
        identity.name = new_name.to_string();
        self.write_identity(&identity)?;
        self.identities.retain(|i| i.id != id);
        self.identities.push(identity.clone());
        self.identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(identity)
    }

    /// Merge identity `from` into `into`: move every sample of `from` onto
    /// `into` (keeping `into`'s id and name), then delete `from`. This is the
    /// "same person enrolled twice under different names" fix — e.g. resolving a
    /// cross-meeting label conflict.
    ///
    /// Samples are combined oldest-first and capped at
    /// [`MAX_SAMPLES_PER_IDENTITY`], evicting the oldest — so the merged
    /// identity keeps the most recent voiceprints from both. `into` is written
    /// before `from` is deleted, so an interrupted merge never loses samples
    /// (at worst `from` lingers and can be merged again). `from == into` is a
    /// no-op.
    pub fn merge(&mut self, from: Uuid, into: Uuid) -> Result<EnrolledIdentity, IdentityError> {
        if from == into {
            return self
                .identities
                .iter()
                .find(|i| i.id == into)
                .cloned()
                .ok_or(IdentityError::NotFound(into));
        }
        let from_samples = self
            .identities
            .iter()
            .find(|i| i.id == from)
            .ok_or(IdentityError::NotFound(from))?
            .samples
            .clone();
        let mut target = self
            .identities
            .iter()
            .find(|i| i.id == into)
            .cloned()
            .ok_or(IdentityError::NotFound(into))?;
        target.samples.extend(from_samples);
        target
            .samples
            .sort_by(|a, b| a.enrolled_at.cmp(&b.enrolled_at));
        if target.samples.len() > MAX_SAMPLES_PER_IDENTITY {
            let excess = target.samples.len() - MAX_SAMPLES_PER_IDENTITY;
            target.samples.drain(..excess);
        }
        self.write_identity(&target)?;
        self.remove(from)?; // disk + memory; `into` already persisted above
        self.identities.retain(|i| i.id != target.id);
        self.identities.push(target.clone());
        self.identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(target)
    }

    /// Delete a single voiceprint sample from identity `id` by its index into
    /// the (oldest-first) [`samples`](EnrolledIdentity::samples) list — e.g. the
    /// user pruning a bad recording. Removing the **last** sample removes the
    /// whole identity, since an identity must never have zero samples (matching
    /// and [`latest_sample`](EnrolledIdentity::latest_sample) assume non-empty).
    /// Returns `true` if a sample (or the identity) was removed, `false` if `id`
    /// is unknown or `index` is out of range.
    pub fn remove_sample(&mut self, id: Uuid, index: usize) -> Result<bool, IdentityError> {
        let Some(identity) = self.identities.iter().find(|i| i.id == id) else {
            return Ok(false);
        };
        if index >= identity.samples.len() {
            return Ok(false);
        }
        if identity.samples.len() == 1 {
            // Last sample → drop the whole identity rather than leave it empty.
            return self.remove(id);
        }
        let mut identity = identity.clone();
        identity.samples.remove(index);
        self.write_identity(&identity)?;
        self.identities.retain(|i| i.id != id);
        self.identities.push(identity);
        self.identities.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(true)
    }

    fn identity_path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Atomic write: serialize to `<id>.json.tmp`, then rename over the final
    /// path, so a crash mid-write never leaves a truncated identity file. The
    /// file is made owner-only *before* the rename so the voiceprint is never
    /// visible at the final path with looser permissions.
    fn write_identity(&self, identity: &EnrolledIdentity) -> Result<(), IdentityError> {
        let path = self.identity_path(identity.id);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(identity)?)?;
        restrict_permissions(&tmp, 0o600)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Per-identity outcome of one scoring pass (internal; indexes into
/// `IdentityStore::identities`).
struct IdentityScore {
    index: usize,
    /// Highest cosine similarity across the identity's samples.
    best: f32,
    /// Samples scoring ≥ the consensus threshold of the pass.
    votes: usize,
    sample_count: usize,
}

impl IdentityScore {
    /// The offline qualification rule (see [`IdentityStore::match_speaker`]).
    fn qualifies(&self, auto_tag: f32) -> bool {
        self.best >= auto_tag || (self.votes >= 2 && self.votes * 2 >= self.sample_count)
    }
}

/// Restrict `path` to owner-only access (unix `chmod`; no-op elsewhere).
#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<(), IdentityError> {
    Ok(())
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

    /// Comfortably above the enrollment gate.
    const VOICED_OK: u64 = 5000;

    fn emb(seed: f32) -> Vec<f32> {
        // Deterministic non-degenerate vector; different seeds are (almost)
        // orthogonal enough after the alternating pattern below.
        (0..EMBEDDING_DIM)
            .map(|i| ((i as f32) * seed).sin())
            .collect()
    }

    /// A unit query vector along dim 0.
    fn query() -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = 1.0;
        v
    }

    /// A unit vector whose cosine similarity with [`query`] is exactly
    /// `cosine`, using a distinct orthogonal component per `k`.
    fn directed(cosine: f32, k: usize) -> Vec<f32> {
        assert!(k > 0 && k < EMBEDDING_DIM);
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = cosine;
        v[k] = (1.0 - cosine * cosine).sqrt();
        v
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
        store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
        let (name, score) = store.match_speaker(&emb(0.1)).expect("should match");
        assert_eq!(name, "李明");
        assert!(score > 0.999, "self-similarity should be ~1.0, got {score}");
    }

    #[test]
    fn dissimilar_embedding_does_not_match() {
        let (_dir, mut store) = store();
        store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
        // A very different pattern scores far below both thresholds.
        assert!(store.match_speaker(&emb(7.7)).is_none());
    }

    #[test]
    fn single_sample_above_auto_tag_threshold_hits() {
        let (_dir, mut store) = store();
        store
            .enroll("A", &directed(0.75, 1), VOICED_OK, None)
            .unwrap();
        let (name, score) = store.match_speaker(&query()).expect("0.75 ≥ auto-tag");
        assert_eq!(name, "A");
        assert!((score - 0.75).abs() < 1e-3, "got {score}");
    }

    #[test]
    fn single_sample_just_past_consensus_threshold_does_not_hit() {
        // One lukewarm sample (0.65: past CONSENSUS, short of AUTO_TAG) is not
        // enough evidence on its own — consensus needs at least two votes.
        let (_dir, mut store) = store();
        store
            .enroll("A", &directed(0.65, 1), VOICED_OK, None)
            .unwrap();
        assert!(store.match_speaker(&query()).is_none());
    }

    #[test]
    fn majority_of_samples_past_consensus_threshold_hits() {
        // 3 samples at 0.65 / 0.62 / 0.30: none reaches AUTO_TAG (0.70), but
        // 2 of 3 ≥ CONSENSUS (0.60) → majority consensus → hit, reported at
        // the best sample's score.
        let (_dir, mut store) = store();
        store
            .enroll("A", &directed(0.65, 1), VOICED_OK, None)
            .unwrap();
        store
            .enroll("A", &directed(0.62, 2), VOICED_OK, None)
            .unwrap();
        store
            .enroll("A", &directed(0.30, 3), VOICED_OK, None)
            .unwrap();
        let (name, score) = store.match_speaker(&query()).expect("2/3 consensus");
        assert_eq!(name, "A");
        assert!((score - 0.65).abs() < 1e-3, "got {score}");
    }

    #[test]
    fn minority_of_samples_past_consensus_threshold_does_not_hit() {
        // 3 samples at 0.65 / 0.50 / 0.30: only 1 of 3 ≥ CONSENSUS and none
        // ≥ AUTO_TAG → no consensus → no match.
        let (_dir, mut store) = store();
        store
            .enroll("A", &directed(0.65, 1), VOICED_OK, None)
            .unwrap();
        store
            .enroll("A", &directed(0.50, 2), VOICED_OK, None)
            .unwrap();
        store
            .enroll("A", &directed(0.30, 3), VOICED_OK, None)
            .unwrap();
        assert!(store.match_speaker(&query()).is_none());
    }

    #[test]
    fn competing_identities_highest_best_sample_score_wins() {
        let (_dir, mut store) = store();
        store
            .enroll("甲", &directed(0.72, 1), VOICED_OK, None)
            .unwrap();
        store
            .enroll("乙", &directed(0.78, 2), VOICED_OK, None)
            .unwrap();
        let (name, score) = store.match_speaker(&query()).expect("both qualify");
        assert_eq!(name, "乙");
        assert!((score - 0.78).abs() < 1e-3, "got {score}");
    }

    #[test]
    fn thresholds_are_inclusive() {
        let (_dir, mut store) = store();
        store.enroll("A", &emb(0.1), VOICED_OK, None).unwrap();
        // Exactly at the auto-tag threshold → match (self-similarity ~1.0 needs
        // the f32 rounding slack); an impossible pair → no match.
        let self_score = store.match_speaker(&emb(0.1)).unwrap().1;
        assert!(store
            .match_speaker_with_thresholds(&emb(0.1), self_score, self_score)
            .is_some());
        assert!(store
            .match_speaker_with_thresholds(&emb(7.7), AUTO_TAG_THRESHOLD, CONSENSUS_THRESHOLD)
            .is_none());
    }

    #[test]
    fn best_of_multiple_identities_wins() {
        let (_dir, mut store) = store();
        store.enroll("甲", &emb(0.1), VOICED_OK, None).unwrap();
        store.enroll("乙", &emb(7.7), VOICED_OK, None).unwrap();
        let (name, _) = store.match_speaker(&emb(7.7)).expect("should match 乙");
        assert_eq!(name, "乙");
    }

    #[test]
    fn persistence_roundtrip_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let meeting = Uuid::new_v4();
        {
            let mut store = IdentityStore::open(dir.path()).unwrap();
            store
                .enroll("李明", &emb(0.1), VOICED_OK, Some(meeting))
                .unwrap();
        }
        let store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(store.list().len(), 1);
        let identity = &store.list()[0];
        assert_eq!(identity.name, "李明");
        assert_eq!(identity.samples.len(), 1);
        assert_eq!(identity.samples[0].source_meeting_id, Some(meeting));
        assert_eq!(identity.samples[0].voiced_ms, VOICED_OK);
        assert_eq!(identity.samples[0].embedding.len(), EMBEDDING_DIM);
        assert!(store.match_speaker(&emb(0.1)).is_some());
    }

    #[test]
    fn reenroll_same_name_appends_sample_and_keeps_one_record() {
        let (dir, mut store) = store();
        let first = store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
        let second = store.enroll("李明", &emb(7.7), VOICED_OK, None).unwrap();
        assert_eq!(first.id, second.id, "same person keeps one identity id");
        assert_eq!(second.samples.len(), 2, "re-enroll appends a sample");
        assert_eq!(store.list().len(), 1);
        // Both the old and the new voice now match this person.
        assert_eq!(store.match_speaker(&emb(0.1)).unwrap().0, "李明");
        assert_eq!(store.match_speaker(&emb(7.7)).unwrap().0, "李明");
        // Exactly one file on disk.
        let files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| is_identity_file(&e.as_ref().unwrap().path()))
            .count();
        assert_eq!(files, 1);
    }

    #[test]
    fn samples_are_capped_evicting_oldest() {
        let (_dir, mut store) = store();
        for i in 0..(MAX_SAMPLES_PER_IDENTITY + 2) {
            store
                .enroll("李明", &directed(0.9, i + 1), VOICED_OK, None)
                .unwrap();
        }
        let identity = &store.list()[0];
        assert_eq!(identity.samples.len(), MAX_SAMPLES_PER_IDENTITY);
        // The two oldest samples (orthogonal components 1 and 2) were evicted;
        // the newest survives. Recover each sample's `k` from its embedding.
        let survivors: Vec<usize> = identity
            .samples
            .iter()
            .map(|s| {
                s.embedding
                    .iter()
                    .enumerate()
                    .skip(1)
                    .find(|(_, &v)| v != 0.0)
                    .map(|(k, _)| k)
                    .unwrap()
            })
            .collect();
        assert!(!survivors.contains(&1), "{survivors:?}");
        assert!(!survivors.contains(&2), "{survivors:?}");
        assert!(survivors.contains(&(MAX_SAMPLES_PER_IDENTITY + 2)));
    }

    #[test]
    fn legacy_single_embedding_file_reads_as_one_sample() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let meeting = Uuid::new_v4();
        let embedding = emb(0.1);
        // Exact pre-multi-sample on-disk shape (#69): a single top-level
        // embedding + enrollment metadata.
        let legacy = serde_json::json!({
            "id": id,
            "name": "李明",
            "embedding": embedding,
            "enrolled_at": "2026-07-01T00:00:00Z",
            "source_meeting_id": meeting,
        });
        std::fs::write(
            dir.path().join(format!("{id}.json")),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let mut store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(store.list().len(), 1);
        let identity = &store.list()[0];
        assert_eq!(identity.id, id);
        assert_eq!(identity.samples.len(), 1);
        assert_eq!(identity.samples[0].voiced_ms, 0, "legacy duration unknown");
        assert_eq!(identity.samples[0].source_meeting_id, Some(meeting));
        assert_eq!(store.match_speaker(&emb(0.1)).unwrap().0, "李明");

        // Re-enrolling appends to the migrated identity and rewrites the file
        // in the new multi-sample format.
        let updated = store.enroll("李明", &emb(0.2), VOICED_OK, None).unwrap();
        assert_eq!(updated.id, id);
        assert_eq!(updated.samples.len(), 2);
        let text = std::fs::read_to_string(dir.path().join(format!("{id}.json"))).unwrap();
        assert!(text.contains("\"samples\""));
        let reopened = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(reopened.list()[0].samples.len(), 2);
    }

    #[test]
    fn enroll_rejects_insufficient_voiced_audio() {
        let (_dir, mut store) = store();
        let result = store.enroll("李明", &emb(0.1), MIN_VOICED_MS - 1, None);
        assert!(matches!(
            result,
            Err(IdentityError::VoiceTooShort { voiced_ms }) if voiced_ms == MIN_VOICED_MS - 1
        ));
        assert!(store.list().is_empty(), "nothing was stored");
        // Exactly at the floor is accepted.
        store
            .enroll("李明", &emb(0.1), MIN_VOICED_MS, None)
            .unwrap();
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn remove_deletes_record_and_file() {
        let (dir, mut store) = store();
        let identity = store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn identity_dir_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("identity");
        let mut store = IdentityStore::open(&library).unwrap();
        let identity = store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();

        let dir_mode = std::fs::metadata(&library).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "identity dir should be 0700");
        let file = library.join(format!("{}.json", identity.id));
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "identity file should be 0600");
    }

    #[cfg(unix)]
    #[test]
    fn failed_file_deletion_keeps_remove_consistent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = IdentityStore::open(dir.path()).unwrap();
        let identity = store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();

        // Make the directory non-writable so the unlink fails.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.remove(identity.id);
        // Restore before asserting so the tempdir can be cleaned up either way.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(result, Err(IdentityError::Io(_))));
        // Memory and disk stayed consistent: still enrolled, file still there.
        assert_eq!(store.list().len(), 1);
        assert!(dir.path().join(format!("{}.json", identity.id)).exists());
        // And with the permission restored, removal succeeds normally.
        assert!(store.remove(identity.id).unwrap());
        assert!(store.list().is_empty());
    }

    #[test]
    fn remove_drops_memory_even_when_file_already_gone() {
        let (dir, mut store) = store();
        let identity = store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
        std::fs::remove_file(dir.path().join(format!("{}.json", identity.id))).unwrap();
        assert!(store.remove(identity.id).unwrap());
        assert!(store.list().is_empty());
    }

    #[test]
    fn enroll_sample_persists_source_provenance() {
        let (dir, mut store) = store();
        store
            .enroll_sample(
                "我",
                &emb(0.1),
                VOICED_OK,
                SampleSource {
                    audio_path: Some("/debug/123/audio_16k.wav".into()),
                    label: Some("你好世界".into()),
                    ..SampleSource::default()
                },
            )
            .unwrap();
        // Survives a reopen (written to disk, not just memory).
        let store = IdentityStore::open(dir.path()).unwrap();
        let sample = &store.list()[0].samples[0];
        assert_eq!(
            sample.source_audio_path.as_deref(),
            Some("/debug/123/audio_16k.wav")
        );
        assert_eq!(sample.source_label.as_deref(), Some("你好世界"));
        // Plain enroll leaves provenance empty.
        assert_eq!(store.list()[0].samples[0].source_meeting_id, None);
    }

    #[test]
    fn rename_rewrites_name_and_keeps_id_and_samples() {
        let (dir, mut store) = store();
        let id = store.enroll("旧名", &emb(0.1), VOICED_OK, None).unwrap().id;
        let renamed = store.rename(id, " 新名 ").unwrap();
        assert_eq!(renamed.id, id, "id preserved");
        assert_eq!(renamed.name, "新名", "trimmed");
        assert_eq!(renamed.samples.len(), 1, "samples preserved");
        // Same id/new name across reopen (file named by id, not name).
        let store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].name, "新名");
        assert_eq!(store.list()[0].id, id);
    }

    #[test]
    fn rename_onto_a_different_existing_name_is_rejected() {
        let (_dir, mut store) = store();
        let a = store.enroll("甲", &emb(0.1), VOICED_OK, None).unwrap().id;
        store.enroll("乙", &emb(0.5), VOICED_OK, None).unwrap();
        assert!(matches!(
            store.rename(a, "乙"),
            Err(IdentityError::NameExists(n)) if n == "乙"
        ));
        // Renaming to its own current name is a no-op success.
        assert_eq!(store.rename(a, "甲").unwrap().name, "甲");
        assert!(matches!(
            store.rename(Uuid::new_v4(), "丙"),
            Err(IdentityError::NotFound(_))
        ));
    }

    #[test]
    fn merge_moves_samples_and_deletes_the_source() {
        let (dir, mut store) = store();
        let a = store.enroll("甲", &emb(0.1), VOICED_OK, None).unwrap().id;
        let b = store.enroll("乙", &emb(0.5), VOICED_OK, None).unwrap().id;
        let merged = store.merge(b, a).unwrap();
        assert_eq!(merged.id, a, "target id kept");
        assert_eq!(merged.name, "甲", "target name kept");
        assert_eq!(merged.samples.len(), 2, "both samples present");
        assert_eq!(store.list().len(), 1, "source gone");
        // Source file deleted; survives reopen.
        assert!(!dir.path().join(format!("{b}.json")).exists());
        let store = IdentityStore::open(dir.path()).unwrap();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].samples.len(), 2);
    }

    #[test]
    fn merge_is_noop_on_self_and_errors_on_missing() {
        let (_dir, mut store) = store();
        let a = store.enroll("甲", &emb(0.1), VOICED_OK, None).unwrap().id;
        assert_eq!(store.merge(a, a).unwrap().id, a, "self-merge is a no-op");
        assert!(matches!(
            store.merge(Uuid::new_v4(), a),
            Err(IdentityError::NotFound(_))
        ));
        assert!(matches!(
            store.merge(a, Uuid::new_v4()),
            Err(IdentityError::NotFound(_))
        ));
    }

    #[test]
    fn remove_sample_prunes_one_and_drops_identity_on_last() {
        let (_dir, mut store) = store();
        store.enroll("甲", &emb(0.1), VOICED_OK, None).unwrap();
        let id = store.enroll("甲", &emb(0.2), VOICED_OK, None).unwrap().id;
        assert_eq!(store.list()[0].samples.len(), 2);
        // Prune the oldest (index 0); one sample remains.
        assert!(store.remove_sample(id, 0).unwrap());
        assert_eq!(store.list()[0].samples.len(), 1);
        // Out-of-range index is a no-op false.
        assert!(!store.remove_sample(id, 5).unwrap());
        // Removing the last sample drops the whole identity.
        assert!(store.remove_sample(id, 0).unwrap());
        assert!(store.list().is_empty());
        assert!(!store.remove_sample(Uuid::new_v4(), 0).unwrap());
    }

    #[test]
    fn enroll_rejects_empty_name_and_bad_dimension() {
        let (_dir, mut store) = store();
        assert!(matches!(
            store.enroll("  ", &emb(0.1), VOICED_OK, None),
            Err(IdentityError::EmptyName)
        ));
        assert!(matches!(
            store.enroll("李明", &[0.5; 8], VOICED_OK, None),
            Err(IdentityError::BadDimension(8))
        ));
    }

    #[test]
    fn corrupt_identity_file_is_skipped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = IdentityStore::open(dir.path()).unwrap();
            store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
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
    fn latest_sample_is_the_most_recent() {
        let (_dir, mut store) = store();
        store.enroll("李明", &emb(0.1), VOICED_OK, None).unwrap();
        let meeting = Uuid::new_v4();
        store
            .enroll("李明", &emb(0.2), VOICED_OK + 1, Some(meeting))
            .unwrap();
        let latest = store.list()[0].latest_sample().unwrap();
        assert_eq!(latest.voiced_ms, VOICED_OK + 1);
        assert_eq!(latest.source_meeting_id, Some(meeting));
    }

    // ---- verify_speaker (deep interface, no decision) --------------------

    #[test]
    fn verify_speaker_reports_full_evidence_for_the_best_identity() {
        let (_dir, mut store) = store();
        // 甲: two samples (0.72 best, 0.61 also above consensus).
        store
            .enroll("甲", &directed(0.72, 1), VOICED_OK, None)
            .unwrap();
        store
            .enroll("甲", &directed(0.61, 2), VOICED_OK, None)
            .unwrap();
        // 乙: one weaker sample — becomes the runner-up.
        store
            .enroll("乙", &directed(0.55, 3), VOICED_OK, None)
            .unwrap();

        let report = store.verify_speaker(&query()).expect("non-empty store");
        let jia = store.list().iter().find(|i| i.name == "甲").unwrap();
        assert_eq!(report.identity_id, jia.id);
        assert_eq!(report.display_name, "甲");
        assert!((report.best_score - 0.72).abs() < 1e-3, "{report:?}");
        assert!((report.runner_up_score - 0.55).abs() < 1e-3, "{report:?}");
        assert!((report.margin - 0.17).abs() < 1e-2, "{report:?}");
        assert_eq!(report.consensus_votes, 2);
        assert_eq!(report.sample_count, 2);
    }

    #[test]
    fn verify_speaker_makes_no_decision_and_single_identity_margin_uses_floor() {
        let (_dir, mut store) = store();
        // Far below every threshold — verify still reports the evidence.
        store
            .enroll("甲", &directed(0.20, 1), VOICED_OK, None)
            .unwrap();
        let report = store
            .verify_speaker(&query())
            .expect("evidence, not decision");
        assert!((report.best_score - 0.20).abs() < 1e-3);
        // Only identity → runner-up pinned at the cosine floor.
        assert_eq!(report.runner_up_score, -1.0);
        assert!((report.margin - 1.20).abs() < 1e-3);

        let empty = IdentityStore::open(tempfile::tempdir().unwrap().path()).unwrap();
        assert!(empty.verify_speaker(&query()).is_none());
    }

    #[test]
    fn match_speaker_report_applies_the_offline_policy() {
        let (_dir, mut store) = store();
        // Below auto-tag with a single sample → offline policy rejects, while
        // the deep interface still reports it.
        store
            .enroll("甲", &directed(0.65, 1), VOICED_OK, None)
            .unwrap();
        assert!(store.match_speaker_report(&query()).is_none());
        assert!(store.verify_speaker(&query()).is_some());

        store
            .enroll("乙", &directed(0.75, 2), VOICED_OK, None)
            .unwrap();
        let report = store
            .match_speaker_report(&query())
            .expect("0.75 ≥ auto-tag");
        assert_eq!(report.display_name, "乙");
        assert!((report.best_score - 0.75).abs() < 1e-3);
        assert!((report.runner_up_score - 0.65).abs() < 1e-3);
        // And it agrees with the thin (name, score) wrapper.
        let (name, score) = store.match_speaker(&query()).unwrap();
        assert_eq!(name, report.display_name);
        assert_eq!(score, report.best_score);
    }

    // ---- live decision policy --------------------------------------------

    fn report(best: f32, margin: f32) -> VerificationReport {
        VerificationReport {
            identity_id: Uuid::new_v4(),
            display_name: "甲".into(),
            best_score: best,
            runner_up_score: best - margin,
            margin,
            consensus_votes: 1,
            sample_count: 1,
        }
    }

    #[test]
    fn live_policy_verifies_only_long_confident_wide_margin_hits() {
        // ≥3 s + best ≥ 0.70 + margin ≥ 0.08 → auto-verified (inclusive bounds).
        assert_eq!(
            live_decision(&report(0.70, 0.08), LIVE_VERIFIED_MIN_VOICED_MS),
            LiveDecision::VerifiedAuto
        );
        assert_eq!(
            live_decision(&report(0.9, 0.5), 10_000),
            LiveDecision::VerifiedAuto
        );
        // Any failed verify criterion degrades to provisional (score allows it)…
        assert_eq!(
            live_decision(&report(0.69, 0.5), 5000),
            LiveDecision::Provisional,
            "score below auto-tag"
        );
        assert_eq!(
            live_decision(&report(0.9, 0.07), 5000),
            LiveDecision::Provisional,
            "margin too narrow"
        );
        assert_eq!(
            live_decision(&report(0.9, 0.5), LIVE_VERIFIED_MIN_VOICED_MS - 1),
            LiveDecision::Provisional,
            "2–3 s utterance stays provisional however strong the score"
        );
    }

    #[test]
    fn live_policy_rejects_short_or_weak_utterances() {
        // Below the provisional duration floor → nothing, even at score 0.9.
        assert_eq!(
            live_decision(&report(0.9, 0.5), LIVE_PROVISIONAL_MIN_VOICED_MS - 1),
            LiveDecision::NoMatch
        );
        // Below the consensus score floor → nothing, however long.
        assert_eq!(
            live_decision(&report(0.59, 0.5), 60_000),
            LiveDecision::NoMatch
        );
        // At the provisional floor exactly → provisional (inclusive).
        assert_eq!(
            live_decision(&report(0.60, 0.0), LIVE_PROVISIONAL_MIN_VOICED_MS),
            LiveDecision::Provisional
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
