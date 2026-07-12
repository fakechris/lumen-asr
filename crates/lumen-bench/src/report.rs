use crate::metrics::{EditScore, TextScore};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ScoredSample {
    pub duration_seconds: Option<f64>,
    pub raw: Option<TextScore>,
    pub repaired: Option<TextScore>,
    pub model_applied: bool,
}

impl ScoredSample {
    pub fn success(duration_seconds: Option<f64>, raw: TextScore, repaired: TextScore) -> Self {
        Self::success_with_model(duration_seconds, raw, repaired, false)
    }

    pub fn success_with_model(
        duration_seconds: Option<f64>,
        raw: TextScore,
        repaired: TextScore,
        model_applied: bool,
    ) -> Self {
        Self {
            duration_seconds,
            raw: Some(raw),
            repaired: Some(repaired),
            model_applied,
        }
    }

    pub fn failure(duration_seconds: Option<f64>) -> Self {
        Self {
            duration_seconds,
            raw: None,
            repaired: None,
            model_applied: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSummary {
    pub edits: usize,
    pub reference_units: usize,
    pub rate: f64,
    pub exact_matches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub strict: MetricSummary,
    pub content: MetricSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurationBucket {
    pub label: String,
    pub total: usize,
    pub raw_content_cer: f64,
    pub repaired_content_cer: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub raw: OutputSummary,
    pub repaired: OutputSummary,
    pub repair_wins: usize,
    pub repair_ties: usize,
    pub repair_losses: usize,
    pub model_applied: usize,
    pub duration_buckets: Vec<DurationBucket>,
}

pub fn summarize(samples: &[ScoredSample]) -> BenchmarkSummary {
    let successful: Vec<&ScoredSample> = samples
        .iter()
        .filter(|sample| sample.raw.is_some() && sample.repaired.is_some())
        .collect();
    let succeeded = successful.len();
    let mut repair_wins = 0;
    let mut repair_ties = 0;
    let mut repair_losses = 0;
    for sample in &successful {
        let raw = sample.raw.as_ref().expect("filtered");
        let repaired = sample.repaired.as_ref().expect("filtered");
        match repaired.content.edits.cmp(&raw.content.edits) {
            std::cmp::Ordering::Less => repair_wins += 1,
            std::cmp::Ordering::Equal => repair_ties += 1,
            std::cmp::Ordering::Greater => repair_losses += 1,
        }
    }

    BenchmarkSummary {
        total: samples.len(),
        succeeded,
        failed: samples.len() - succeeded,
        raw: output_summary(successful.iter().filter_map(|sample| sample.raw.as_ref())),
        repaired: output_summary(
            successful
                .iter()
                .filter_map(|sample| sample.repaired.as_ref()),
        ),
        repair_wins,
        repair_ties,
        repair_losses,
        model_applied: successful
            .iter()
            .filter(|sample| sample.model_applied)
            .count(),
        duration_buckets: duration_buckets(&successful),
    }
}

pub fn render_markdown(
    summary: &BenchmarkSummary,
    asr_label: &str,
    corrector_label: &str,
) -> String {
    let mut output = format!(
        "# Reference benchmark\n\n- Samples: {} succeeded / {} total ({} failed)\n- ASR: `{}`\n- Corrector: `{}`\n- Model applied: {} / {} succeeded samples\n\n## Overall\n\n| Output | Content CER | Strict CER | Content exact | Strict exact |\n|---|---:|---:|---:|---:|\n| Raw ASR | {:.2}% | {:.2}% | {} | {} |\n| Repaired | {:.2}% | {:.2}% | {} | {} |\n\nRepair impact: **{} wins / {} ties / {} losses**.\n\n## By duration\n\n| Duration | Samples | Raw content CER | Repaired content CER |\n|---|---:|---:|---:|\n",
        summary.succeeded,
        summary.total,
        summary.failed,
        asr_label,
        corrector_label,
        summary.model_applied,
        summary.succeeded,
        summary.raw.content.rate * 100.0,
        summary.raw.strict.rate * 100.0,
        summary.raw.content.exact_matches,
        summary.raw.strict.exact_matches,
        summary.repaired.content.rate * 100.0,
        summary.repaired.strict.rate * 100.0,
        summary.repaired.content.exact_matches,
        summary.repaired.strict.exact_matches,
        summary.repair_wins,
        summary.repair_ties,
        summary.repair_losses,
    );
    for bucket in &summary.duration_buckets {
        output.push_str(&format!(
            "| {} | {} | {:.2}% | {:.2}% |\n",
            bucket.label,
            bucket.total,
            bucket.raw_content_cer * 100.0,
            bucket.repaired_content_cer * 100.0,
        ));
    }
    output.push_str(
        "\nCER is measured against the dataset's reference text. Treat it as a comparison target, not verified ground truth. Content CER ignores case, whitespace, and punctuation; strict CER keeps punctuation and normalized spacing.\n",
    );
    output
}

fn output_summary<'a>(scores: impl Iterator<Item = &'a TextScore>) -> OutputSummary {
    let scores: Vec<&TextScore> = scores.collect();
    OutputSummary {
        strict: metric_summary(
            scores.iter().map(|score| &score.strict),
            scores.iter().filter(|score| score.strict_exact).count(),
        ),
        content: metric_summary(
            scores.iter().map(|score| &score.content),
            scores.iter().filter(|score| score.content_exact).count(),
        ),
    }
}

fn metric_summary<'a>(scores: impl Iterator<Item = &'a EditScore>, exact: usize) -> MetricSummary {
    let (edits, reference_units) = scores.fold((0, 0), |(edits, units), score| {
        (edits + score.edits, units + score.reference_units)
    });
    MetricSummary {
        edits,
        reference_units,
        rate: if reference_units == 0 {
            0.0
        } else {
            edits as f64 / reference_units as f64
        },
        exact_matches: exact,
    }
}

fn duration_buckets(samples: &[&ScoredSample]) -> Vec<DurationBucket> {
    let definitions: [(&str, f64, Option<f64>); 4] = [
        ("<5s", 0.0, Some(5.0)),
        ("5-15s", 5.0, Some(15.0)),
        ("15-30s", 15.0, Some(30.0)),
        (">=30s", 30.0, None),
    ];
    definitions
        .into_iter()
        .map(|(label, minimum, maximum)| {
            let matching: Vec<&&ScoredSample> = samples
                .iter()
                .filter(|sample| {
                    sample.duration_seconds.is_some_and(|duration| {
                        duration >= minimum && maximum.is_none_or(|maximum| duration < maximum)
                    })
                })
                .collect();
            let raw = metric_summary(
                matching
                    .iter()
                    .filter_map(|sample| sample.raw.as_ref().map(|score| &score.content)),
                0,
            );
            let repaired = metric_summary(
                matching
                    .iter()
                    .filter_map(|sample| sample.repaired.as_ref().map(|score| &score.content)),
                0,
            );
            DurationBucket {
                label: label.to_string(),
                total: matching.len(),
                raw_content_cer: raw.rate,
                repaired_content_cer: repaired.rate,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{summarize, ScoredSample};
    use crate::metrics::score_text;

    #[test]
    fn summary_counts_repair_wins_ties_and_losses() {
        let samples = vec![
            ScoredSample::success(
                Some(2.0),
                score_text("你好世界", "你好世间"),
                score_text("你好世界", "你好世界"),
            ),
            ScoredSample::success(
                Some(8.0),
                score_text("alpha", "alpha"),
                score_text("alpha", "alpha"),
            ),
            ScoredSample::success(
                Some(20.0),
                score_text("保持原样", "保持原样"),
                score_text("保持原样", "改变原样"),
            ),
            ScoredSample::failure(Some(1.0)),
        ];

        let summary = summarize(&samples);

        assert_eq!(summary.total, 4);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.repair_wins, 1);
        assert_eq!(summary.repair_ties, 1);
        assert_eq!(summary.repair_losses, 1);
        assert_eq!(summary.model_applied, 0);
        assert_eq!(summary.duration_buckets[0].label, "<5s");
        assert_eq!(summary.duration_buckets[0].total, 1);
    }
}
