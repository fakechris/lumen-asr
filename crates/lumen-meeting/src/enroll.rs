//! Label → enroll: after a meeting is attributed, add each **manually named**
//! speaker's voiceprint to the global identity library so future meetings
//! auto-identify the same person (cross-meeting propagation).
//!
//! The user's name is authoritative for who a cluster is, so a named cluster is
//! enrolled under that name — accumulating a sample when the name already
//! exists, creating the identity otherwise ([`IdentityStore::enroll`] keys by
//! name). The voiceprint only **vetoes**: when the cluster's voice strongly
//! matches a *different* already-named identity, the enrollment is withheld and
//! the mismatch recorded, rather than merging two names under one voice.
//!
//! Not eligible: auto-identified rows (`verification` / library hits — enrolling
//! those would feed the library its own guesses), and clusters below
//! [`lumen_identity::MIN_VOICED_MS`] of voiced audio. Pure over the assembled
//! rows + centroids; the only side effect is the local identity-store write.

use std::collections::BTreeMap;

use lumen_core::{attribution_origin, Speaker};
use lumen_identity::IdentityStore;
use uuid::Uuid;

/// Minimum cosine for the voiceprint to be treated as "already this other
/// person" — stricter than the auto-tag floor (0.70). A veto relabels nothing;
/// it only *withholds* an enrollment and flags a conflict, so it should fire
/// only on a confident different-name match.
pub const ENROLL_CONFLICT_THRESHOLD: f32 = 0.80;

/// A named cluster whose voiceprint strongly matches a *different* enrolled
/// name — recorded for the user to resolve later instead of enrolled.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollConflict {
    /// The name the user gave the cluster this meeting.
    pub name: String,
    /// The existing enrolled identity the voiceprint matched instead.
    pub existing_name: String,
    /// Cosine of the winning existing sample.
    pub score: f32,
}

/// What [`auto_enroll_named_speakers`] did (counts are logged; names are
/// personal data). Conflicts are returned for the caller to surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoEnrollOutcome {
    /// Names enrolled (a new identity or an accumulated sample).
    pub enrolled: Vec<String>,
    /// Conflicts withheld from the library.
    pub conflicts: Vec<EnrollConflict>,
}

/// Enroll every manually-named speaker's centroid into `store` (see the module
/// docs). `centroids`/`voiced_ms` are keyed by the assembled speaker row id.
pub fn auto_enroll_named_speakers(
    store: &mut IdentityStore,
    speakers: &[Speaker],
    centroids: &BTreeMap<Uuid, Vec<f32>>,
    voiced_ms: &BTreeMap<Uuid, u64>,
    meeting_id: Uuid,
) -> AutoEnrollOutcome {
    let mut outcome = AutoEnrollOutcome::default();
    for speaker in speakers {
        // Only user-rooted attributions are eligible.
        match speaker.attribution_origin.as_deref() {
            Some(attribution_origin::MANUAL) | Some(attribution_origin::MANUAL_SPREAD) => {}
            _ => continue,
        }
        let Some(name) = speaker
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let Some(centroid) = centroids.get(&speaker.id) else {
            continue; // e.g. a pure manual M-row with no cluster centroid
        };
        let voiced = voiced_ms.get(&speaker.id).copied().unwrap_or(0);
        if voiced < lumen_identity::MIN_VOICED_MS {
            continue;
        }
        // Veto: does this voice strongly match a *different* enrolled name?
        // Own the match before the mutable `enroll` borrow below.
        let matched = store
            .match_speaker_with_thresholds(
                centroid,
                ENROLL_CONFLICT_THRESHOLD,
                ENROLL_CONFLICT_THRESHOLD,
            )
            .map(|(existing, score)| (existing.to_string(), score));
        if let Some((existing, score)) = matched {
            if existing != name {
                outcome.conflicts.push(EnrollConflict {
                    name: name.to_string(),
                    existing_name: existing,
                    score,
                });
                continue;
            }
        }
        match store.enroll(name, centroid, voiced, Some(meeting_id)) {
            Ok(_) => outcome.enrolled.push(name.to_string()),
            // Voice-too-short etc.: skip, never fail the meeting.
            Err(error) => {
                tracing::warn!(error = %error, "auto-enroll skipped for a named speaker")
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_identity::EMBEDDING_DIM;

    fn emb(seed: f32) -> Vec<f32> {
        (0..EMBEDDING_DIM)
            .map(|i| ((i as f32) * 0.05 + seed).sin())
            .collect()
    }

    fn named(meeting: Uuid, label: &str, name: &str, origin: &str) -> Speaker {
        let mut s = Speaker::new(meeting, label);
        s.display_name = Some(name.to_string());
        s.attribution_origin = Some(origin.to_string());
        s
    }

    fn store() -> (tempfile::TempDir, IdentityStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn enrolls_a_new_named_speaker_and_accumulates_across_meetings() {
        let (_dir, mut store) = store();
        let m1 = Uuid::new_v4();
        let a = named(m1, "S1", "海燕", attribution_origin::MANUAL);
        let centroids = BTreeMap::from([(a.id, emb(0.10))]);
        let voiced = BTreeMap::from([(a.id, 8_000u64)]);

        let out = auto_enroll_named_speakers(&mut store, &[a], &centroids, &voiced, m1);
        assert_eq!(out.enrolled, vec!["海燕"]);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].samples.len(), 1);

        // A second meeting with the same person + name accumulates a sample.
        let m2 = Uuid::new_v4();
        let a2 = named(m2, "S1", "海燕", attribution_origin::MANUAL_SPREAD);
        let c2 = BTreeMap::from([(a2.id, emb(0.12))]);
        let v2 = BTreeMap::from([(a2.id, 9_000u64)]);
        let out2 = auto_enroll_named_speakers(&mut store, &[a2], &c2, &v2, m2);
        assert_eq!(out2.enrolled, vec!["海燕"]);
        assert_eq!(store.list().len(), 1, "same name → one identity");
        assert_eq!(store.list()[0].samples.len(), 2, "sample accumulated");
    }

    #[test]
    fn withholds_and_flags_a_same_voice_different_name_conflict() {
        let (_dir, mut store) = store();
        let m1 = Uuid::new_v4();
        let a = named(m1, "S1", "A", attribution_origin::MANUAL);
        let ca = BTreeMap::from([(a.id, emb(0.10))]);
        let va = BTreeMap::from([(a.id, 8_000u64)]);
        auto_enroll_named_speakers(&mut store, &[a], &ca, &va, m1);

        // A later meeting labels the *same voice* "B".
        let m2 = Uuid::new_v4();
        let b = named(m2, "S1", "B", attribution_origin::MANUAL);
        let cb = BTreeMap::from([(b.id, emb(0.10))]); // identical voice
        let vb = BTreeMap::from([(b.id, 8_000u64)]);
        let out = auto_enroll_named_speakers(&mut store, &[b], &cb, &vb, m2);

        assert!(out.enrolled.is_empty(), "not enrolled");
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.conflicts[0].name, "B");
        assert_eq!(out.conflicts[0].existing_name, "A");
        // Library is unchanged (still just A).
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].name, "A");
    }

    #[test]
    fn skips_auto_identified_and_short_and_centroidless_rows() {
        let (_dir, mut store) = store();
        let m = Uuid::new_v4();
        let auto = named(m, "S1", "从库里认的", attribution_origin::VERIFICATION);
        let short = named(m, "S2", "太短", attribution_origin::MANUAL);
        let no_centroid = named(m, "M1", "无质心", attribution_origin::MANUAL);
        let centroids = BTreeMap::from([(auto.id, emb(0.1)), (short.id, emb(0.2))]);
        let voiced = BTreeMap::from([(auto.id, 9_000u64), (short.id, 1_000u64)]);

        let out = auto_enroll_named_speakers(
            &mut store,
            &[auto, short, no_centroid],
            &centroids,
            &voiced,
            m,
        );
        assert!(out.enrolled.is_empty());
        assert!(store.list().is_empty());
    }
}
