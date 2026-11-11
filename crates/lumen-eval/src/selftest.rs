//! Harness self-check used by CI: validates the embedded corpus and
//! baselines, then proves the gate catches the two regression classes it
//! exists for — a cleanup-rule regression and a diarization speaker
//! collapse. Runs with no audio and no models on any platform.

use crate::corpus::Corpus;
use crate::report::{
    compare_with_baselines, BaselineEntry, Baselines, EngineBaseline, EvalReport, Hypothesis,
};

/// Run every self-check; returns the list of performed checks or the first
/// failure. The desktop CLI surfaces this as `eval selftest` and CI runs
/// it on every macOS build.
pub fn run() -> Result<Vec<String>, String> {
    let mut performed = Vec::new();

    // 1. The embedded corpus and baselines parse and satisfy their own
    //    invariants (>= 20 clips, every set covered, unique ids).
    let corpus = Corpus::embedded().map_err(|e| e.to_string())?;
    performed.push(format!(
        "corpus loads: {} clips, {} dictionary terms",
        corpus.clips.len(),
        corpus.dictionary_terms.len()
    ));
    Baselines::embedded()?;
    performed.push("embedded baselines parse".to_string());

    // 2. A clean pass through the corpus stays green under strict gates.
    let clean_report = EvalReport::run("selftest", &corpus, |clip, text| {
        let clean = if clip.source_clip_ids.is_empty() {
            clip.clean.clone().unwrap_or_else(|| text.to_string())
        } else {
            corpus.resolved_text(clip).unwrap().1
        };
        Hypothesis::text(clean)
    })
    .map_err(|e| e.to_string())?;
    let strict = strict_baselines("selftest", &clean_report);
    let clean_verdict = compare_with_baselines(&clean_report, &strict);
    if !clean_verdict.pass {
        return Err(format!(
            "clean self-check unexpectedly failed: {:?}",
            clean_verdict.breaches
        ));
    }
    performed.push("clean hypotheses pass strict gates".to_string());

    // 3. Fault injection — cleanup regression: the filler rule stops
    //    removing "呃" and the self-correction keeps the replaced phrase.
    //    mustNot checks must flag it and the gate must fail.
    let dirty_report = EvalReport::run("selftest", &corpus, |clip, _text| {
        let degraded = if clip.id == "zh-filler-1" {
            // Buggy cleanup: kept the filler and the replaced day.
            "我们下周二，呃，下周三下午三点开个会，讨论一下预算。".to_string()
        } else if clip.source_clip_ids.is_empty() {
            clip.clean.clone().unwrap_or_default()
        } else {
            corpus.resolved_text(clip).unwrap().1
        };
        Hypothesis::text(degraded)
    })
    .map_err(|e| e.to_string())?;
    let dirty_verdict = compare_with_baselines(&dirty_report, &strict);
    if dirty_verdict.pass {
        return Err("cleanup regression was NOT caught by the gate".to_string());
    }
    let caught_filler = dirty_verdict
        .breaches
        .iter()
        .any(|b| b.contains("cleanup check failures"));
    if !caught_filler {
        return Err(format!(
            "cleanup regression did not surface as check failures: {:?}",
            dirty_verdict.breaches
        ));
    }
    performed.push(
        "injected cleanup regression (filler left, replaced phrase kept) fails the gate"
            .to_string(),
    );

    // 4. Fault injection — diarization speaker collapse: a meeting clip
    //    expects three speakers, the pipeline found one. The speaker
    //    guard must fail the gate.
    let meeting_corpus_json = serde_json::json!({
        "version": 1,
        "dictionary_terms": [],
        "clips": [{
            "id": "meeting-collapse",
            "set": "long",
            "lang": "zh",
            "text": "三个人轮流说话的一段会议录音。",
            "clean": "三个人轮流说话的一段会议录音。",
            "must": [],
            "must_not": [],
            "terms": [],
            "speakers": 3
        }]
    })
    .to_string();
    let meeting_corpus = Corpus::from_json(&meeting_corpus_json).map_err(|e| e.to_string())?;
    let collapsed = EvalReport::run("selftest", &meeting_corpus, |_clip, text| Hypothesis {
        text: text.to_string(),
        speakers_found: Some(1),
    })
    .map_err(|e| e.to_string())?;
    let meeting_strict = strict_baselines("selftest", &collapsed);
    let collapse_verdict = compare_with_baselines(&collapsed, &meeting_strict);
    if collapse_verdict.pass {
        return Err("speaker collapse was NOT caught by the gate".to_string());
    }
    if !collapse_verdict
        .breaches
        .iter()
        .any(|b| b.contains("speaker guard expected 3 found 1"))
    {
        return Err(format!(
            "speaker collapse did not surface as a guard breach: {:?}",
            collapse_verdict.breaches
        ));
    }
    performed
        .push("injected diarization collapse (3 expected, 1 found) fails the gate".to_string());

    Ok(performed)
}

fn strict_baselines(engine: &str, report: &EvalReport) -> Baselines {
    let mut sets = std::collections::BTreeMap::new();
    for summary in &report.sets {
        sets.insert(
            summary.set.as_str().to_string(),
            BaselineEntry {
                max_mean_error_rate: 0.10,
                min_term_hit_ratio: 1.0,
                allow_check_failures: 0,
            },
        );
    }
    Baselines {
        version: 1,
        notes: String::new(),
        engines: [(
            engine.to_string(),
            EngineBaseline {
                provisional: false,
                sets,
            },
        )]
        .into_iter()
        .collect(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_passes_and_reports_all_checks() {
        let performed = super::run().expect("selftest passes");
        assert!(performed.len() >= 5, "{performed:?}");
        assert!(
            performed.iter().any(|c| c.contains("cleanup regression")),
            "{performed:?}"
        );
        assert!(
            performed.iter().any(|c| c.contains("diarization collapse")),
            "{performed:?}"
        );
    }
}
