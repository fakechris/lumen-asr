//! Live diarization session summary: capture real-time speaker clusters and
//! utterance boundaries from `meeting_live`, persist as a sidecar, and use them
//! to prevent/rescue offline single-speaker diarization collapse.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::assemble::DiarTurn;

/// One speaker cluster snapshot from the live recording session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveClusterSnapshot {
    pub label: String,
    pub centroid: Vec<f32>,
    pub count: u32,
    pub voiced_seconds: f64,
}

/// One verified utterance segment from the live recording session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveSegmentSnapshot {
    pub track: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub speaker_label: String,
}

/// The complete live diarization summary persisted alongside the meeting audio.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LiveDiarSessionSummary {
    pub clusters: Vec<LiveClusterSnapshot>,
    pub segments: Vec<LiveSegmentSnapshot>,
}

impl LiveDiarSessionSummary {
    /// Return the count of distinct stable speakers recorded live.
    pub fn stable_speaker_count(&self) -> usize {
        self.clusters.len()
    }
}

/// Derive the sidecar file path for the live diarization summary.
pub fn live_diar_sidecar_path(audio_path: &Path) -> PathBuf {
    audio_path.with_extension("live_diar.json")
}

/// Persist the live diarization summary next to the meeting audio.
pub fn write_live_diar_summary(audio_path: &Path, summary: &LiveDiarSessionSummary) -> std::io::Result<()> {
    let sidecar_path = live_diar_sidecar_path(audio_path);
    let json = serde_json::to_string_pretty(summary)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&sidecar_path, json)
}

/// Read the live diarization summary sidecar if present.
pub fn read_live_diar_summary(audio_path: &Path) -> Option<LiveDiarSessionSummary> {
    let sidecar_path = live_diar_sidecar_path(audio_path);
    let json = std::fs::read_to_string(&sidecar_path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Rescue single-speaker collapsed turns using real-time live speaker priors.
///
/// If offline diarization yielded only 1 speaker, but the real-time live session
/// identified >= 2 stable speakers, this function re-attributes each offline turn
/// based on maximum time overlap with the live utterance segments.
pub fn rescue_turns_with_live_prior(
    turns: &[DiarTurn],
    summary: &LiveDiarSessionSummary,
) -> Vec<DiarTurn> {
    if turns.is_empty() || summary.clusters.len() < 2 || summary.segments.is_empty() {
        return turns.to_vec();
    }

    let offline_speakers: BTreeSet<u32> = turns.iter().map(|t| t.speaker).collect();
    // Only intervene if offline diarization collapsed to a single speaker
    if offline_speakers.len() > 1 {
        return turns.to_vec();
    }

    // Assign integer speaker IDs to each unique live cluster label
    let mut speaker_map: BTreeMap<String, u32> = BTreeMap::new();
    for (i, cluster) in summary.clusters.iter().enumerate() {
        speaker_map.insert(cluster.label.clone(), i as u32);
    }

    let mut rescued = Vec::with_capacity(turns.len());
    let mut last_speaker = 0u32;

    for turn in turns {
        // Find all live segments overlapping this turn
        let mut overlap_per_speaker: BTreeMap<u32, f64> = BTreeMap::new();
        for seg in &summary.segments {
            let ov_start = turn.start.max(seg.start_seconds);
            let ov_end = turn.end.min(seg.end_seconds);
            if ov_end > ov_start {
                let overlap = ov_end - ov_start;
                if let Some(&spk_id) = speaker_map.get(&seg.speaker_label) {
                    *overlap_per_speaker.entry(spk_id).or_insert(0.0) += overlap;
                }
            }
        }

        // Pick speaker with highest overlap; fallback to last_speaker if no overlap
        let chosen_speaker = overlap_per_speaker
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(spk_id, _)| spk_id)
            .unwrap_or(last_speaker);

        last_speaker = chosen_speaker;
        rescued.push(DiarTurn::new(turn.start, turn.end, chosen_speaker));
    }

    rescued
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescue_recovers_multi_speaker_from_live_summary() {
        let raw_turns = vec![
            DiarTurn::new(0.0, 10.0, 0),
            DiarTurn::new(10.0, 20.0, 0),
            DiarTurn::new(20.0, 30.0, 0),
        ];

        let summary = LiveDiarSessionSummary {
            clusters: vec![
                LiveClusterSnapshot {
                    label: "说话人1".into(),
                    centroid: vec![1.0, 0.0],
                    count: 10,
                    voiced_seconds: 20.0,
                },
                LiveClusterSnapshot {
                    label: "说话人2".into(),
                    centroid: vec![0.0, 1.0],
                    count: 5,
                    voiced_seconds: 10.0,
                },
            ],
            segments: vec![
                LiveSegmentSnapshot {
                    track: "mic".into(),
                    start_seconds: 0.0,
                    end_seconds: 9.5,
                    speaker_label: "说话人1".into(),
                },
                LiveSegmentSnapshot {
                    track: "mic".into(),
                    start_seconds: 10.5,
                    end_seconds: 19.5,
                    speaker_label: "说话人2".into(),
                },
                LiveSegmentSnapshot {
                    track: "mic".into(),
                    start_seconds: 20.5,
                    end_seconds: 29.5,
                    speaker_label: "说话人1".into(),
                },
            ],
        };

        let rescued = rescue_turns_with_live_prior(&raw_turns, &summary);
        assert_eq!(rescued.len(), 3);
        assert_eq!(rescued[0].speaker, 0);
        assert_eq!(rescued[1].speaker, 1);
        assert_eq!(rescued[2].speaker, 0);
    }

    #[test]
    fn does_not_modify_already_separated_offline_turns() {
        let raw_turns = vec![
            DiarTurn::new(0.0, 10.0, 0),
            DiarTurn::new(10.0, 20.0, 1),
        ];
        let summary = LiveDiarSessionSummary {
            clusters: vec![
                LiveClusterSnapshot {
                    label: "A".into(),
                    centroid: vec![],
                    count: 2,
                    voiced_seconds: 5.0,
                },
                LiveClusterSnapshot {
                    label: "B".into(),
                    centroid: vec![],
                    count: 2,
                    voiced_seconds: 5.0,
                },
            ],
            segments: vec![],
        };
        let out = rescue_turns_with_live_prior(&raw_turns, &summary);
        assert_eq!(out, raw_turns);
    }
}
