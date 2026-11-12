//! Per-clip results, per-set aggregates, the JSON report, and the
//! baseline comparison that turns a report into a gate verdict.

use std::collections::BTreeMap;

use crate::checks::{
    cleanup_checks, speaker_count_guard, term_hits, CheckFailure, SpeakerGuard, TermHits,
};
use crate::corpus::{Corpus, CorpusError, EvalSet};
use crate::metrics::error_rate;

/// One clip's outcome.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipResult {
    pub clip_id: String,
    pub set: EvalSet,
    pub error_rate: f64,
    #[serde(flatten)]
    pub terms: TermHits,
    pub check_failures: Vec<CheckFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_guard: Option<SpeakerGuard>,
    /// The raw hypothesis text, kept for local debugging; omitted from
    /// `--out` files when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hypothesis: String,
}

/// Aggregate for one set under one engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SetSummary {
    pub set: EvalSet,
    pub clip_count: usize,
    pub mean_error_rate: f64,
    pub max_error_rate: f64,
    pub term_hit_ratio: f64,
    pub check_failure_count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvalReport {
    pub engine: String,
    pub clips: Vec<ClipResult>,
    pub sets: Vec<SetSummary>,
    /// `report-only` when no baseline entry covers the engine.
    pub mode: EvalMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalMode {
    Gated,
    ReportOnly,
}

impl EvalMode {
    pub fn label(&self) -> &'static str {
        match self {
            EvalMode::Gated => "gated",
            EvalMode::ReportOnly => "report-only",
        }
    }
}

/// What a pipeline reports for one clip: the hypothesis text plus, for
/// meeting-style clips, the speaker count the diarization found.
#[derive(Debug, Clone, Default)]
pub struct Hypothesis {
    pub text: String,
    pub speakers_found: Option<u32>,
}

impl Hypothesis {
    pub fn text(text: impl Into<String>) -> Hypothesis {
        Hypothesis {
            text: text.into(),
            speakers_found: None,
        }
    }
}

impl EvalReport {
    /// Evaluate a corpus through a hypothesis supplier: `transcribe` is
    /// called once per clip with the clip's resolved spoken text and
    /// returns the pipeline's cleaned output (for ASR-only scoring pass
    /// the raw transcript back). Pure with respect to the metrics — all
    /// side effects live in the supplier.
    pub fn run<F>(
        engine: &str,
        corpus: &Corpus,
        mut transcribe: F,
    ) -> Result<EvalReport, CorpusError>
    where
        F: FnMut(&crate::corpus::EvalClip, &str) -> Hypothesis,
    {
        let mut clips = Vec::new();
        for clip in &corpus.clips {
            let (text, clean) = corpus.resolved_text(clip)?;
            let hypothesis = transcribe(clip, &text);
            // The scoring contract is end-to-end: the reference is what a
            // perfect pipeline inserts (`clean`). Filler residue is free
            // through scoring normalization; self-correction phrases the
            // cleanup should have dropped are visible, small, and absorbed
            // by the per-engine baselines when scoring raw ASR output.
            let er = error_rate(&hypothesis.text, &clean);
            let terms = term_hits(&hypothesis.text, &corpus.resolved_terms(clip));
            let must = corpus.resolved_must(clip);
            let must_not = if clip.source_clip_ids.is_empty() {
                clip.must_not.clone()
            } else {
                clip.source_clip_ids
                    .iter()
                    .filter_map(|id| corpus.clip(id))
                    .flat_map(|s| s.must_not.iter().cloned())
                    .collect()
            };
            let failures = cleanup_checks(&text, &clean, &hypothesis.text, clip, &must, &must_not);
            let guard = match (clip.speakers, hypothesis.speakers_found) {
                (Some(expected), Some(found)) => Some(speaker_count_guard(expected, found)),
                _ => None,
            };
            clips.push(ClipResult {
                clip_id: clip.id.clone(),
                set: clip.set,
                error_rate: (er.rate * 10_000.0).round() / 10_000.0,
                terms,
                check_failures: failures,
                speaker_guard: guard,
                hypothesis: hypothesis.text,
            });
        }
        let sets = summarize(&clips);
        Ok(EvalReport {
            engine: engine.to_string(),
            clips,
            sets,
            mode: EvalMode::ReportOnly,
        })
    }

    pub fn set_summary(&self, set: EvalSet) -> Option<&SetSummary> {
        self.sets.iter().find(|s| s.set == set)
    }
}

fn summarize(clips: &[ClipResult]) -> Vec<SetSummary> {
    let mut by_set: BTreeMap<EvalSet, Vec<&ClipResult>> = BTreeMap::new();
    for clip in clips {
        by_set.entry(clip.set).or_default().push(clip);
    }
    by_set
        .into_iter()
        .map(|(set, clips)| summarize_one(set, &clips))
        .collect()
}

/// Re-aggregate an edited clip list (used by tests that simulate
/// degradation by rewriting per-clip numbers).
pub fn summarize_public(clips: &[ClipResult]) -> Vec<SetSummary> {
    let mut by_set: BTreeMap<EvalSet, Vec<&ClipResult>> = BTreeMap::new();
    for clip in clips {
        by_set.entry(clip.set).or_default().push(clip);
    }
    by_set
        .into_iter()
        .map(|(set, clips)| summarize_one(set, &clips))
        .collect()
}

fn summarize_one(set: EvalSet, clips: &[&ClipResult]) -> SetSummary {
    let n = clips.len().max(1) as f64;
    let mean = clips.iter().map(|c| c.error_rate).sum::<f64>() / n;
    let max = clips.iter().map(|c| c.error_rate).fold(0.0_f64, f64::max);
    let hit_total: usize = clips.iter().map(|c| c.terms.total).sum();
    let hit_count: usize = clips.iter().map(|c| c.terms.hits).sum();
    SetSummary {
        set,
        clip_count: clips.len(),
        mean_error_rate: (mean * 10_000.0).round() / 10_000.0,
        max_error_rate: (max * 10_000.0).round() / 10_000.0,
        term_hit_ratio: if hit_total == 0 {
            1.0
        } else {
            ((hit_count as f64 / hit_total as f64) * 10_000.0).round() / 10_000.0
        },
        check_failure_count: clips.iter().map(|c| c.check_failures.len()).sum(),
    }
}

/// One set's gate thresholds under one engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BaselineEntry {
    pub max_mean_error_rate: f64,
    #[serde(default)]
    pub min_term_hit_ratio: f64,
    #[serde(default)]
    pub allow_check_failures: usize,
}

/// All gates for one engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineBaseline {
    #[serde(default)]
    pub provisional: bool,
    pub sets: BTreeMap<String, BaselineEntry>,
}

/// The baselines file (`data/baselines.json` shape).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Baselines {
    pub version: u32,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub engines: BTreeMap<String, EngineBaseline>,
}

impl Baselines {
    pub fn embedded() -> Result<Baselines, String> {
        serde_json::from_str(crate::corpus::BASELINES_JSON).map_err(|e| e.to_string())
    }

    pub fn from_file(path: &std::path::Path) -> Result<Baselines, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Turn a green report into baseline entries (rounded up on error rate,
    /// down on term ratio, observed check-failure residue allowed) so a
    /// local run can seed the repo file. In raw-ASR mode fillers survive
    /// by design, so the recorded allowance is the observed count; the
    /// strict zero belongs to end-to-end cleanup evaluation.
    pub fn from_report(report: &EvalReport, provisional: bool) -> Baselines {
        let mut sets = BTreeMap::new();
        for summary in &report.sets {
            let entry = BaselineEntry {
                // +25% headroom, rounded up to a hundredth, plus a flat
                // 0.05 so a near-zero mean is not gated at zero.
                max_mean_error_rate: ((summary.mean_error_rate * 1.25) * 100.0).ceil() / 100.0
                    + 0.05,
                min_term_hit_ratio: if summary.term_hit_ratio >= 1.0 {
                    1.0
                } else {
                    summary.term_hit_ratio
                },
                allow_check_failures: summary.check_failure_count,
            };
            sets.insert(summary.set.as_str().to_string(), entry);
        }
        Baselines {
            version: 1,
            notes:
                "Recorded from a local eval run; review before pasting into the repository file."
                    .into(),
            engines: [(report.engine.clone(), EngineBaseline { provisional, sets })]
                .into_iter()
                .collect(),
        }
    }
}

/// Gate outcome for a report against baselines.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GateVerdict {
    pub pass: bool,
    pub mode: EvalMode,
    pub breaches: Vec<String>,
}

/// Compare a report with the baselines. Sets without an entry are not
/// gated; an engine with no entry at all makes the run report-only.
pub fn compare_with_baselines(report: &EvalReport, baselines: &Baselines) -> GateVerdict {
    let Some(engine_baseline) = baselines.engines.get(&report.engine) else {
        return GateVerdict {
            pass: true,
            mode: EvalMode::ReportOnly,
            breaches: vec![format!(
                "engine `{}` has no baseline entry — report-only; record one with --record-baseline",
                report.engine
            )],
        };
    };
    let mut breaches = Vec::new();
    for summary in &report.sets {
        let Some(entry) = engine_baseline.sets.get(summary.set.as_str()) else {
            continue;
        };
        if summary.mean_error_rate > entry.max_mean_error_rate {
            breaches.push(format!(
                "{}: mean error rate {:.4} > baseline {:.4}",
                summary.set.as_str(),
                summary.mean_error_rate,
                entry.max_mean_error_rate
            ));
        }
        if summary.term_hit_ratio < entry.min_term_hit_ratio {
            breaches.push(format!(
                "{}: term hit ratio {:.4} < baseline {:.4}",
                summary.set.as_str(),
                summary.term_hit_ratio,
                entry.min_term_hit_ratio
            ));
        }
        if summary.check_failure_count > entry.allow_check_failures {
            breaches.push(format!(
                "{}: {} cleanup check failures (baseline allows {})",
                summary.set.as_str(),
                summary.check_failure_count,
                entry.allow_check_failures
            ));
        }
        for clip in &report.clips {
            if clip.set == summary.set {
                if let Some(guard) = &clip.speaker_guard {
                    if !guard.pass {
                        breaches.push(format!(
                            "{}: speaker guard expected {} found {}",
                            clip.clip_id, guard.expected, guard.found
                        ));
                    }
                }
            }
        }
    }
    GateVerdict {
        pass: breaches.is_empty(),
        mode: EvalMode::Gated,
        breaches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::Corpus;

    fn perfect_report(engine: &str) -> EvalReport {
        let corpus = Corpus::embedded().unwrap();
        EvalReport::run(engine, &corpus, |clip, text| {
            let clean = if clip.source_clip_ids.is_empty() {
                clip.clean.clone().unwrap_or_else(|| text.to_string())
            } else {
                corpus.resolved_text(clip).unwrap().1
            };
            Hypothesis::text(clean)
        })
        .unwrap()
    }

    #[test]
    fn perfect_clean_hypotheses_score_low_and_pass_zero_baselines() {
        let report = perfect_report("test-engine");
        assert_eq!(report.clips.len(), 23);
        assert!(
            report.sets.iter().all(|s| s.mean_error_rate < 0.2),
            "{:?}",
            report.sets
        );
        let failing: Vec<(String, Vec<crate::checks::CheckFailure>)> = report
            .clips
            .iter()
            .filter(|c| !c.check_failures.is_empty())
            .map(|c| (c.clip_id.clone(), c.check_failures.clone()))
            .collect();
        assert!(failing.is_empty(), "{failing:?}");

        // Strict baselines: near-zero error, full term hits, no failures.
        let baselines = Baselines {
            version: 1,
            notes: String::new(),
            engines: [(
                "test-engine".to_string(),
                EngineBaseline {
                    provisional: false,
                    sets: report
                        .sets
                        .iter()
                        .map(|s| {
                            (
                                s.set.as_str().to_string(),
                                BaselineEntry {
                                    max_mean_error_rate: 0.05,
                                    min_term_hit_ratio: 1.0,
                                    allow_check_failures: 0,
                                },
                            )
                        })
                        .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };
        let verdict = compare_with_baselines(&report, &baselines);
        assert!(verdict.pass, "{:?}", verdict.breaches);
    }

    #[test]
    fn unknown_engine_is_report_only_and_passes() {
        let report = perfect_report("mystery");
        let baselines = Baselines::embedded().unwrap();
        let verdict = compare_with_baselines(&report, &baselines);
        assert!(verdict.pass);
        assert_eq!(verdict.mode, EvalMode::ReportOnly);
    }

    #[test]
    fn from_report_seeds_slightly_loose_baselines() {
        let report = perfect_report("seed");
        let baselines = Baselines::from_report(&report, true);
        let entry = &baselines.engines["seed"].sets["zh-plain"];
        assert!(
            entry.max_mean_error_rate
                >= report
                    .set_summary(crate::corpus::EvalSet::ZhPlain)
                    .unwrap()
                    .mean_error_rate
        );
        assert!(baselines.engines["seed"].provisional);
    }

    #[test]
    fn recorded_allowance_covers_the_recording_run() {
        // Whatever residue the recording run saw is allowed; one more
        // failure than recorded fails the gate.
        let report = perfect_report("seed");
        let recorded = Baselines::from_report(&report, false);
        let verdict = compare_with_baselines(&report, &recorded);
        assert!(verdict.pass, "{:?}", verdict.breaches);
    }

    #[test]
    fn recorded_thresholds_stay_fine_grained() {
        // A 0.13 mean must not seed a ~1.0 threshold.
        let mut report = perfect_report("seed");
        for clip in report.clips.iter_mut() {
            clip.error_rate = 0.13;
        }
        report.sets = crate::report::summarize_public(&report.clips);
        let recorded = Baselines::from_report(&report, false);
        let entry = &recorded.engines["seed"].sets["zh-plain"];
        assert!(
            entry.max_mean_error_rate < 0.30,
            "{}",
            entry.max_mean_error_rate
        );
    }
}
