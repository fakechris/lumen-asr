//! Local-only meeting-detection counters (quantifies false-positive rate).
//!
//! Every counter increment does two things, both strictly on this machine:
//! a structured `tracing` line (the decision trail) and an atomic rewrite of a
//! small JSON file under the app data dir (`detection_stats.json`), so the
//! numbers survive restarts. Nothing here ever touches the network — the file
//! exists so the user (or a future settings page) can see how often detection
//! prompted, how often they accepted, and how often the end-of-meeting
//! suggestion was right.
//!
//! Persistence is deliberately best-effort: a corrupt/missing file resets the
//! counts to zero, and a failed write only logs a warning. Stats must never be
//! able to break detection itself.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The persisted counter set. Field names are the on-disk JSON keys; unknown
/// or missing keys are tolerated (`serde(default)`) so the file survives
/// schema growth in either direction.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectionStatsCounters {
    /// A "looks like a meeting — record?" prompt was shown.
    pub prompt_shown: u64,
    /// The user accepted a prompt (a recording started).
    pub prompt_accepted: u64,
    /// The user dismissed a prompt.
    pub prompt_dismissed: u64,
    /// A "meeting seems over — stop?" suggestion was shown.
    pub stop_suggested: u64,
    /// The user accepted a stop suggestion (recording stopped).
    pub stop_accepted: u64,
    /// The user declined a stop suggestion (kept recording).
    pub stop_declined: u64,
}

/// Which counter to bump. Keeping this an enum (rather than stringly-typed
/// method args) means a typo is a compile error.
#[derive(Debug, Clone, Copy)]
pub enum StatCounter {
    PromptShown,
    PromptAccepted,
    PromptDismissed,
    StopSuggested,
    StopAccepted,
    StopDeclined,
}

impl StatCounter {
    fn name(self) -> &'static str {
        match self {
            StatCounter::PromptShown => "prompt_shown",
            StatCounter::PromptAccepted => "prompt_accepted",
            StatCounter::PromptDismissed => "prompt_dismissed",
            StatCounter::StopSuggested => "stop_suggested",
            StatCounter::StopAccepted => "stop_accepted",
            StatCounter::StopDeclined => "stop_declined",
        }
    }
}

/// Thread-safe counter store bound to one JSON file.
pub struct DetectionStats {
    path: PathBuf,
    counters: Mutex<DetectionStatsCounters>,
}

impl DetectionStats {
    /// Load existing counts from `path` (zeroes on any read/parse problem).
    pub fn load(path: PathBuf) -> Self {
        let counters = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            counters: Mutex::new(counters),
        }
    }

    /// Bump one counter, log it, and persist the file atomically.
    pub fn increment(&self, counter: StatCounter) {
        let snapshot = {
            let Ok(mut counters) = self.counters.lock() else {
                return;
            };
            let slot = match counter {
                StatCounter::PromptShown => &mut counters.prompt_shown,
                StatCounter::PromptAccepted => &mut counters.prompt_accepted,
                StatCounter::PromptDismissed => &mut counters.prompt_dismissed,
                StatCounter::StopSuggested => &mut counters.stop_suggested,
                StatCounter::StopAccepted => &mut counters.stop_accepted,
                StatCounter::StopDeclined => &mut counters.stop_declined,
            };
            *slot = slot.saturating_add(1);
            tracing::info!(
                counter = counter.name(),
                total = *slot,
                "meeting detection stat"
            );
            counters.clone()
        };
        self.persist(&snapshot);
    }

    /// Current counts (for the `get_meeting_detection_stats` command).
    pub fn snapshot(&self) -> DetectionStatsCounters {
        self.counters.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Atomic write: serialize to a sibling temp file, then rename over the
    /// real one, so a crash mid-write can never leave a half-written file.
    fn persist(&self, counters: &DetectionStatsCounters) {
        let Ok(json) = serde_json::to_string_pretty(counters) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(error = %e, "could not create detection stats dir");
                return;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let result = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            tracing::warn!(error = %e, "could not persist detection stats");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increments_persist_and_reload() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-detstats-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let path = dir.join("detection_stats.json");
        let stats = DetectionStats::load(path.clone());
        stats.increment(StatCounter::PromptShown);
        stats.increment(StatCounter::PromptShown);
        stats.increment(StatCounter::StopSuggested);
        let snap = stats.snapshot();
        assert_eq!(snap.prompt_shown, 2);
        assert_eq!(snap.stop_suggested, 1);
        assert_eq!(snap.prompt_accepted, 0);
        // A fresh load sees the persisted numbers.
        let reloaded = DetectionStats::load(path).snapshot();
        assert_eq!(reloaded.prompt_shown, 2);
        assert_eq!(reloaded.stop_suggested, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_resets_to_zero() {
        let dir =
            std::env::temp_dir().join(format!("lumen-detstats-corrupt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("detection_stats.json");
        std::fs::write(&path, "not json").expect("write corrupt file");
        let stats = DetectionStats::load(path);
        assert_eq!(stats.snapshot().prompt_shown, 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
