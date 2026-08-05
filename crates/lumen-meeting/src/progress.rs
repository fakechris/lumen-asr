//! Observability for the offline meeting pipeline: a small, dependency-free
//! progress model the app layer turns into a Tauri event.
//!
//! [`process_meeting`](crate::process_meeting) is platform-agnostic and knows
//! nothing about Tauri, so it reports progress through an injected sink
//! (`&dyn Fn(ProcessingProgress)`). The desktop app supplies a sink that emits
//! `meeting-processing-progress`; every other caller (tests, batch) passes
//! `None` and the pipeline runs exactly as before.
//!
//! ## Stages
//! One offline run is a fixed sequence of [`ProcessingStage`]s. The per-track
//! stages (`diarize`, `voiceprint`, `transcribe`) run once per audio track
//! (`mic`, and `system` for a dual-track meeting); the rest run once globally.
//! The two loop-heavy stages — `transcribe` (per diarized turn) and `cleanup`
//! (per LLM chunk) — carry sub-progress (`done`/`total` + `stage_percent`); the
//! others emit a single "starting" tick.
//!
//! ## Overall percent
//! Each stage has a fixed relative [`weight`](ProcessingStage::weight) — the two
//! big ones, `transcribe` and `cleanup`, dominate — and the plan (dual-track?
//! cleanup on? minutes on?) fixes exactly which stage slots run. The overall
//! percent is the weighted position of the current stage plus its own fraction,
//! normalised over the plan's total weight, so the bar advances monotonically
//! from 0 to 100 across the whole run.
//!
//! ## Throttling
//! The per-turn / per-chunk loops call [`ProgressReporter::tick`], which emits
//! only when the overall percent has advanced ≥ 3 %, at least 1 s has passed, or
//! it is the stage's final item — so a 200-turn meeting never floods the UI.

use std::cell::Cell;
use std::time::{Duration, Instant};

/// A single phase of the offline pipeline. Ordered as they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingStage {
    /// Speaker diarization (segment the track into speaker turns).
    Diarize,
    /// Per-speaker voiceprint centroids (enrollment / auto-identify material).
    Voiceprint,
    /// Per-turn ASR — the loop-heavy stage (`done`/`total` = turns).
    Transcribe,
    /// Post-ASR dictionary correction of names/jargon.
    Correct,
    /// Batched LLM transcript cleanup — loop-heavy (`done`/`total` = chunks).
    Cleanup,
    /// Cross-meeting voiceprint auto-identification (naming enrolled speakers).
    Identify,
    /// Cross-track speaker unification (merge the same person across mic/system).
    Unify,
    /// Structured minutes generation (LLM).
    Minutes,
}

impl ProcessingStage {
    /// Stable machine name emitted to the UI (mapped to a friendly label there).
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessingStage::Diarize => "diarize",
            ProcessingStage::Voiceprint => "voiceprint",
            ProcessingStage::Transcribe => "transcribe",
            ProcessingStage::Correct => "correct",
            ProcessingStage::Cleanup => "cleanup",
            ProcessingStage::Identify => "identify",
            ProcessingStage::Unify => "unify",
            ProcessingStage::Minutes => "minutes",
        }
    }

    /// Relative weight of this stage for the overall-percent estimate. Unitless
    /// (normalised over the plan's total); `transcribe` and `cleanup` are the
    /// heavy hitters, so they get most of the bar.
    fn weight(self) -> f32 {
        match self {
            ProcessingStage::Diarize => 10.0,
            ProcessingStage::Voiceprint => 4.0,
            ProcessingStage::Transcribe => 38.0,
            ProcessingStage::Correct => 2.0,
            ProcessingStage::Cleanup => 22.0,
            ProcessingStage::Identify => 3.0,
            ProcessingStage::Unify => 2.0,
            ProcessingStage::Minutes => 14.0,
        }
    }
}

/// Which audio track a per-track stage is running on. `None` on the event means
/// a global (once-per-run) stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingTrack {
    Mic,
    System,
}

impl ProcessingTrack {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessingTrack::Mic => "mic",
            ProcessingTrack::System => "system",
        }
    }
}

/// One progress update handed to the sink. Everything the UI needs to render
/// "which stage + how far in it + how far overall".
#[derive(Debug, Clone, Copy)]
pub struct ProcessingProgress {
    pub stage: ProcessingStage,
    /// Track for per-track stages; `None` for global stages.
    pub track: Option<ProcessingTrack>,
    /// 1-based position of this stage slot among the plan's stages.
    pub stage_index: u32,
    /// Number of stage slots this run will execute.
    pub stage_total: u32,
    /// Items done within a loop-heavy stage (turns / chunks); 0 otherwise.
    pub done: u32,
    /// Total items in a loop-heavy stage; 0 otherwise.
    pub total: u32,
    /// Percent within the current stage (`Some` only for loop-heavy stages).
    pub stage_percent: Option<f32>,
    /// Estimated overall percent across the whole run (0–100, monotonic).
    pub overall_percent: f32,
}

/// The shape of one run: which optional stages will execute. Fixes the stage
/// slot list (and therefore the overall-percent denominator) up front, so the
/// bar does not jump when an optional stage is skipped.
#[derive(Debug, Clone, Copy)]
pub struct ProcessingPlan {
    /// A synchronized system-audio track is present (mic + system).
    pub dual_track: bool,
    /// The LLM transcript-cleanup stage will run.
    pub cleanup: bool,
    /// The minutes-generation stage will run.
    pub minutes: bool,
}

impl ProcessingPlan {
    /// The ordered stage slots this run will execute. Per-track stages appear
    /// once per track; global stages once. A `(stage, track)` pair is unique in
    /// the list, so it doubles as the slot key.
    fn slots(&self) -> Vec<(ProcessingStage, Option<ProcessingTrack>)> {
        use ProcessingStage::*;
        let mut slots = vec![
            (Diarize, Some(ProcessingTrack::Mic)),
            (Voiceprint, Some(ProcessingTrack::Mic)),
            (Transcribe, Some(ProcessingTrack::Mic)),
        ];
        if self.dual_track {
            slots.push((Diarize, Some(ProcessingTrack::System)));
            slots.push((Voiceprint, Some(ProcessingTrack::System)));
            slots.push((Transcribe, Some(ProcessingTrack::System)));
        }
        slots.push((Correct, None));
        if self.cleanup {
            slots.push((Cleanup, None));
        }
        slots.push((Identify, None));
        if self.dual_track {
            slots.push((Unify, None));
        }
        if self.minutes {
            slots.push((Minutes, None));
        }
        slots
    }
}

/// Locate a `(stage, track)` slot in the plan. Returns
/// `(index0, slot_count, cumulative_weight_before, stage_weight, total_weight)`.
fn plan_position(
    plan: &ProcessingPlan,
    stage: ProcessingStage,
    track: Option<ProcessingTrack>,
) -> Option<(usize, usize, f32, f32, f32)> {
    let slots = plan.slots();
    let total_weight: f32 = slots.iter().map(|(s, _)| s.weight()).sum();
    let mut cumulative = 0.0;
    for (i, (s, t)) in slots.iter().enumerate() {
        if *s == stage && *t == track {
            return Some((i, slots.len(), cumulative, s.weight(), total_weight));
        }
        cumulative += s.weight();
    }
    None
}

/// Estimated overall percent (0–100) at `fraction` (0–1) through the given
/// stage. Pure and total: an unknown slot (should not happen for a well-formed
/// plan) yields 0, and the result is always clamped into `[0, 100]`.
pub fn overall_percent(
    plan: &ProcessingPlan,
    stage: ProcessingStage,
    track: Option<ProcessingTrack>,
    fraction: f32,
) -> f32 {
    match plan_position(plan, stage, track) {
        Some((_, _, cumulative, weight, total)) if total > 0.0 => {
            let done = cumulative + fraction.clamp(0.0, 1.0) * weight;
            (done / total * 100.0).clamp(0.0, 100.0)
        }
        _ => 0.0,
    }
}

/// Reports pipeline progress through an injected sink, with per-loop throttling.
///
/// Interior-mutable throttle state (`Cell`) keeps `tick`/`stage_start` callable
/// behind a shared `&` from the pipeline's `async` body — the meeting future is
/// already `!Send` (thread-affine `Store`), so single-thread `Cell` is fine.
pub struct ProgressReporter<'a> {
    plan: ProcessingPlan,
    sink: &'a dyn Fn(ProcessingProgress),
    last_overall: Cell<f32>,
    last_emit: Cell<Option<Instant>>,
}

impl<'a> ProgressReporter<'a> {
    pub fn new(plan: ProcessingPlan, sink: &'a dyn Fn(ProcessingProgress)) -> Self {
        Self {
            plan,
            sink,
            // Sentinel below any real percent, so the first `tick` always emits.
            last_overall: Cell::new(-100.0),
            last_emit: Cell::new(None),
        }
    }

    fn build(
        &self,
        stage: ProcessingStage,
        track: Option<ProcessingTrack>,
        done: u32,
        total: u32,
        fraction: f32,
        stage_percent: Option<f32>,
    ) -> ProcessingProgress {
        let (stage_index, stage_total) = match plan_position(&self.plan, stage, track) {
            Some((i, n, ..)) => (i as u32 + 1, n as u32),
            None => (0, 0),
        };
        ProcessingProgress {
            stage,
            track,
            stage_index,
            stage_total,
            done,
            total,
            stage_percent,
            overall_percent: overall_percent(&self.plan, stage, track, fraction),
        }
    }

    fn dispatch(&self, progress: ProcessingProgress) {
        self.last_overall.set(progress.overall_percent);
        self.last_emit.set(Some(Instant::now()));
        (self.sink)(progress);
    }

    /// Emit the "entering this stage" tick (fraction 0). Always emitted — it is
    /// one event per stage and anchors the bar at the stage boundary.
    pub fn stage_start(&self, stage: ProcessingStage, track: Option<ProcessingTrack>) {
        let progress = self.build(stage, track, 0, 0, 0.0, None);
        self.dispatch(progress);
    }

    /// Emit sub-progress within a loop-heavy stage, throttled: only when the
    /// overall percent advanced ≥ 3 %, ≥ 1 s elapsed since the last emit, or it
    /// is the final item (`done >= total`). Keeps long meetings from flooding
    /// the UI while still landing a final 100 %-of-stage tick.
    pub fn tick(
        &self,
        stage: ProcessingStage,
        track: Option<ProcessingTrack>,
        done: usize,
        total: usize,
    ) {
        let fraction = if total == 0 {
            1.0
        } else {
            done as f32 / total as f32
        };
        let progress = self.build(
            stage,
            track,
            done as u32,
            total as u32,
            fraction,
            Some((fraction * 100.0).clamp(0.0, 100.0)),
        );
        let is_final = done >= total;
        let advanced = progress.overall_percent - self.last_overall.get() >= 3.0;
        let elapsed = self
            .last_emit
            .get()
            .map(|t| Instant::now().duration_since(t) >= Duration::from_secs(1))
            .unwrap_or(true);
        if is_final || advanced || elapsed {
            self.dispatch(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn overall_percent_spans_zero_to_hundred_monotonically() {
        let plan = ProcessingPlan {
            dual_track: false,
            cleanup: true,
            minutes: true,
        };
        // First stage at fraction 0 is 0%.
        assert!(approx(
            overall_percent(
                &plan,
                ProcessingStage::Diarize,
                Some(ProcessingTrack::Mic),
                0.0
            ),
            0.0
        ));
        // Last stage (minutes) fully done is 100%.
        assert!(approx(
            overall_percent(&plan, ProcessingStage::Minutes, None, 1.0),
            100.0
        ));
        // Monotonic non-decreasing across the plan's slots at fraction 0.
        let ordered = [
            (ProcessingStage::Diarize, Some(ProcessingTrack::Mic)),
            (ProcessingStage::Voiceprint, Some(ProcessingTrack::Mic)),
            (ProcessingStage::Transcribe, Some(ProcessingTrack::Mic)),
            (ProcessingStage::Correct, None),
            (ProcessingStage::Cleanup, None),
            (ProcessingStage::Identify, None),
            (ProcessingStage::Minutes, None),
        ];
        let mut prev = -1.0;
        for (stage, track) in ordered {
            let p = overall_percent(&plan, stage, track, 0.0);
            assert!(
                p >= prev,
                "stage {} regressed: {p} < {prev}",
                stage.as_str()
            );
            prev = p;
        }
    }

    #[test]
    fn transcribe_and_cleanup_dominate_the_bar() {
        let plan = ProcessingPlan {
            dual_track: false,
            cleanup: true,
            minutes: true,
        };
        // The transcribe stage alone should cover a large span of the bar.
        let start = overall_percent(
            &plan,
            ProcessingStage::Transcribe,
            Some(ProcessingTrack::Mic),
            0.0,
        );
        let end = overall_percent(
            &plan,
            ProcessingStage::Transcribe,
            Some(ProcessingTrack::Mic),
            1.0,
        );
        assert!(
            end - start > 30.0,
            "transcribe span too small: {}",
            end - start
        );
    }

    #[test]
    fn dual_track_adds_a_second_transcribe_slot_and_unify() {
        let mono = ProcessingPlan {
            dual_track: false,
            cleanup: false,
            minutes: false,
        };
        let dual = ProcessingPlan {
            dual_track: true,
            cleanup: false,
            minutes: false,
        };
        assert_eq!(mono.slots().len() + 4, dual.slots().len());
        // System transcribe exists only in the dual plan.
        assert!(plan_position(
            &dual,
            ProcessingStage::Transcribe,
            Some(ProcessingTrack::System)
        )
        .is_some());
        assert!(plan_position(
            &mono,
            ProcessingStage::Transcribe,
            Some(ProcessingTrack::System)
        )
        .is_none());
        assert!(plan_position(&dual, ProcessingStage::Unify, None).is_some());
    }

    #[test]
    fn skipped_optional_stages_are_absent_from_the_plan() {
        let plan = ProcessingPlan {
            dual_track: false,
            cleanup: false,
            minutes: false,
        };
        assert!(plan_position(&plan, ProcessingStage::Cleanup, None).is_none());
        assert!(plan_position(&plan, ProcessingStage::Minutes, None).is_none());
        // Identify becomes the final slot, so it reaches 100% at fraction 1.
        assert!(approx(
            overall_percent(&plan, ProcessingStage::Identify, None, 1.0),
            100.0
        ));
    }

    #[test]
    fn stage_index_is_one_based_and_reflects_the_plan() {
        let plan = ProcessingPlan {
            dual_track: false,
            cleanup: true,
            minutes: true,
        };
        let seen: RefCell<Vec<ProcessingProgress>> = RefCell::new(Vec::new());
        let sink = |p: ProcessingProgress| seen.borrow_mut().push(p);
        let reporter = ProgressReporter::new(plan, &sink);
        reporter.stage_start(ProcessingStage::Diarize, Some(ProcessingTrack::Mic));
        let first = seen.borrow()[0];
        assert_eq!(first.stage_index, 1);
        assert_eq!(first.stage_total, plan.slots().len() as u32);
    }

    #[test]
    fn tick_throttles_but_always_emits_the_final_item() {
        let plan = ProcessingPlan {
            dual_track: false,
            cleanup: false,
            minutes: false,
        };
        let count = Cell::new(0usize);
        let last = Cell::new(0u32);
        let sink = |p: ProcessingProgress| {
            count.set(count.get() + 1);
            last.set(p.done);
        };
        let reporter = ProgressReporter::new(plan, &sink);
        // 200 rapid ticks over the transcribe stage: throttling keeps the count
        // far below 200, but the final item (done == total) always lands.
        let total = 200usize;
        for done in 1..=total {
            reporter.tick(
                ProcessingStage::Transcribe,
                Some(ProcessingTrack::Mic),
                done,
                total,
            );
        }
        assert!(
            count.get() < total,
            "no throttling happened: {}",
            count.get()
        );
        assert!(count.get() >= 1);
        assert_eq!(last.get() as usize, total, "final tick must always emit");
    }
}
