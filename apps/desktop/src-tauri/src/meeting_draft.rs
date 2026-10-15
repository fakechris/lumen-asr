//! Draft minutes from the live transcript.
//!
//! The offline pipeline that produces the authoritative transcript and minutes
//! takes a sizeable fraction of the meeting's length (tens of minutes after a
//! long meeting). The live streaming transcript is already complete the moment
//! recording stops, so one LLM call over it gives the user provisional minutes
//! within seconds of stopping.
//!
//! Lifecycle (enforced by the store, see
//! [`lumen_store::Store::save_minutes_draft_if_no_final`] and
//! [`lumen_store::Store::save_summary`]):
//! - the draft is saved as [`SummaryKind::MinutesDraft`] unless final minutes
//!   already exist (the pipeline won the race → the draft is discarded);
//! - saving final minutes deletes the draft;
//! - a failed or minutes-less run leaves the draft in place, labelled as a
//!   draft by the UI.
//!
//! The draft runs outside the processing queue: it is a single LLM call and
//! never blocks or delays the offline pipeline. Every failure is logged only.

use lumen_core::{MeetingSummary, SummaryKind};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::AppState;

/// Below this much recognized text the draft is not worth an LLM call (a
/// short test recording, a silent meeting).
const MIN_DRAFT_TEXT_CHARS: usize = 200;

/// Upper bound on the rendered live transcript sent for the draft. The draft
/// is a preview: past the budget the middle of the meeting is elided, keeping
/// its opening (agenda, context) and its end (decisions, next steps).
const DRAFT_TRANSCRIPT_BUDGET_CHARS: usize = 24_000;

/// Marker line standing in for the elided middle of a long transcript.
const ELIDED_MARKER: &str = "……（会议中段内容较长，草稿中省略）……";

/// One finalized live segment.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveLine {
    pub track: &'static str,
    pub segment_id: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
}

/// The finalized live segments of one recording, keyed by segment id so a
/// re-emitted final replaces its earlier text and an empty final retracts it
/// (the same contract the UI applies to `meeting-live-transcript` events).
#[derive(Debug, Default)]
pub struct LiveTranscriptLog {
    lines: Vec<LiveLine>,
}

impl LiveTranscriptLog {
    pub fn record_final(
        &mut self,
        track: &'static str,
        segment_id: &str,
        start_seconds: f64,
        end_seconds: f64,
        text: &str,
    ) {
        let text = text.trim();
        let existing = self.lines.iter().position(|l| l.segment_id == segment_id);
        match (existing, text.is_empty()) {
            (Some(i), true) => {
                self.lines.remove(i);
            }
            (Some(i), false) => {
                let line = &mut self.lines[i];
                line.start_seconds = start_seconds;
                line.end_seconds = end_seconds;
                line.text = text.to_string();
            }
            (None, true) => {}
            (None, false) => self.lines.push(LiveLine {
                track,
                segment_id: segment_id.to_string(),
                start_seconds,
                end_seconds,
                text: text.to_string(),
            }),
        }
    }

    /// The segments in timeline order (both tracks interleaved).
    pub fn into_lines(mut self) -> Vec<LiveLine> {
        self.lines
            .sort_by(|a, b| a.start_seconds.total_cmp(&b.start_seconds));
        self.lines
    }
}

fn track_label(track: &str) -> &'static str {
    match track {
        "system" => "远端",
        _ => "现场",
    }
}

/// Render the live segments in the minutes prompt's line format
/// (`[start-end] 说话人：内容`, track names in place of speakers, since live
/// text has no diarization). `None` when there is too little text for a
/// useful draft. Over `budget` chars, whole lines from the middle are dropped
/// and replaced by [`ELIDED_MARKER`].
pub fn render_draft_transcript(lines: &[LiveLine], budget: usize) -> Option<String> {
    let text_chars: usize = lines.iter().map(|l| l.text.chars().count()).sum();
    if text_chars < MIN_DRAFT_TEXT_CHARS {
        return None;
    }
    let rendered: Vec<String> = lines
        .iter()
        .map(|l| {
            format!(
                "[{:.1}-{:.1}] {}：{}",
                l.start_seconds,
                l.end_seconds,
                track_label(l.track),
                l.text
            )
        })
        .collect();
    let total: usize = rendered.iter().map(|l| l.chars().count() + 1).sum();
    if total <= budget {
        return Some(rendered.join("\n"));
    }
    // Keep a third of the budget from the start and the rest from the end.
    let head_budget = budget / 3;
    let tail_budget = budget - head_budget;
    let mut head = Vec::new();
    let mut used = 0;
    for line in &rendered {
        let len = line.chars().count() + 1;
        if used + len > head_budget {
            break;
        }
        used += len;
        head.push(line.as_str());
    }
    let mut tail = Vec::new();
    used = 0;
    for line in rendered[head.len()..].iter().rev() {
        let len = line.chars().count() + 1;
        if used + len > tail_budget {
            break;
        }
        used += len;
        tail.push(line.as_str());
    }
    tail.reverse();
    let mut out = head;
    out.push(ELIDED_MARKER);
    out.extend(tail);
    Some(out.join("\n"))
}

/// Generate and store draft minutes for `meeting_id` from its live
/// transcript, in the background. A no-op when no LLM is configured or the
/// live transcript is too short; failures are logged only.
pub fn spawn_draft_minutes(app: AppHandle, meeting_id: Uuid, lines: Vec<LiveLine>) {
    let Some(transcript) = render_draft_transcript(&lines, DRAFT_TRANSCRIPT_BUDGET_CHARS) else {
        return;
    };
    let (corrector_cfg, template_name) = {
        let state = app.state::<AppState>();
        let Ok(cfg) = state.config.lock() else {
            return;
        };
        (cfg.corrector.clone(), cfg.meeting.minutes_template.clone())
    };
    // Same gate as the offline minutes step: without an LLM there are no
    // minutes, draft or final.
    if !corrector_cfg.enabled || corrector_cfg.provider == "none" {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let corrector = match crate::corrector_svc::build_corrector(&corrector_cfg) {
            Ok(corrector) => corrector,
            Err(error) => {
                tracing::warn!(meeting_id = %meeting_id, %error, "draft minutes: no corrector");
                return;
            }
        };
        let notes = with_store(&app, |s| {
            s.get_meeting(meeting_id).map_err(|e| e.to_string())
        })
        .ok()
        .flatten()
        .map(|m| m.notes)
        .unwrap_or_default();
        let template = crate::meeting_cmd::resolve_minutes_template(&template_name);
        let started = std::time::Instant::now();
        let minutes = match lumen_meeting::minutes::generate_minutes(
            corrector.as_ref(),
            &transcript,
            Some(notes.as_str()),
            None,
            Some(&template),
        )
        .await
        {
            Ok(minutes) if !minutes.is_empty() => minutes,
            Ok(_) => {
                tracing::info!(meeting_id = %meeting_id, "draft minutes: model returned nothing usable");
                return;
            }
            Err(error) => {
                tracing::warn!(meeting_id = %meeting_id, %error, "draft minutes generation failed");
                return;
            }
        };
        let content = match serde_json::to_string(&minutes) {
            Ok(content) => content,
            Err(error) => {
                tracing::warn!(meeting_id = %meeting_id, %error, "draft minutes: serialize failed");
                return;
            }
        };
        let mut row = MeetingSummary::new(meeting_id, SummaryKind::MinutesDraft, content);
        let model = corrector_cfg.model.trim();
        if !model.is_empty() {
            row.model = Some(model.to_string());
        }
        match with_store(&app, |s| {
            s.save_minutes_draft_if_no_final(&row)
                .map_err(|e| e.to_string())
        }) {
            Ok(true) => tracing::info!(
                meeting_id = %meeting_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "draft minutes saved from live transcript"
            ),
            Ok(false) => tracing::info!(
                meeting_id = %meeting_id,
                "draft minutes discarded: final minutes already saved"
            ),
            Err(error) => {
                tracing::warn!(meeting_id = %meeting_id, %error, "draft minutes: store write failed")
            }
        }
    });
}

/// Run `f` against the app's store under its lock (held only for the call).
fn with_store<T>(
    app: &AppHandle,
    f: impl FnOnce(&lumen_store::Store) -> Result<T, String>,
) -> Result<T, String> {
    let state = app.state::<AppState>();
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;
    f(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(track: &'static str, id: &str, start: f64, text: &str) -> LiveLine {
        LiveLine {
            track,
            segment_id: id.to_string(),
            start_seconds: start,
            end_seconds: start + 1.0,
            text: text.to_string(),
        }
    }

    #[test]
    fn log_replaces_refinals_and_drops_retractions() {
        let mut log = LiveTranscriptLog::default();
        log.record_final("mic", "mic-0", 0.0, 1.0, "你好");
        log.record_final("system", "system-0", 0.5, 2.0, "大家好");
        log.record_final("mic", "mic-0", 0.0, 1.2, "你好啊");
        log.record_final("mic", "mic-1", 3.0, 4.0, "  ");
        log.record_final("system", "system-0", 0.5, 2.0, "");

        let lines = log.into_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "你好啊");
        assert_eq!(lines[0].end_seconds, 1.2);
    }

    #[test]
    fn log_orders_both_tracks_on_the_timeline() {
        let mut log = LiveTranscriptLog::default();
        log.record_final("system", "system-0", 5.0, 6.0, "后");
        log.record_final("mic", "mic-0", 1.0, 2.0, "前");
        let lines = log.into_lines();
        assert_eq!(lines[0].text, "前");
        assert_eq!(lines[1].text, "后");
    }

    #[test]
    fn too_little_text_yields_no_draft() {
        let lines = vec![line("mic", "mic-0", 0.0, "测试一下")];
        assert_eq!(render_draft_transcript(&lines, 10_000), None);
    }

    #[test]
    fn renders_track_labels_in_minutes_line_format() {
        let long = "内容".repeat(MIN_DRAFT_TEXT_CHARS);
        let lines = vec![
            line("mic", "mic-0", 0.0, &long),
            line("system", "system-0", 2.0, "收到"),
        ];
        let out = render_draft_transcript(&lines, 100_000).unwrap();
        let rows: Vec<&str> = out.lines().collect();
        assert!(rows[0].starts_with("[0.0-1.0] 现场："));
        assert_eq!(rows[1], "[2.0-3.0] 远端：收到");
    }

    #[test]
    fn long_transcript_keeps_head_and_tail_within_budget() {
        let lines: Vec<LiveLine> = (0..400)
            .map(|i| {
                line(
                    "mic",
                    &format!("mic-{i}"),
                    i as f64,
                    &format!("第{i}句话的内容"),
                )
            })
            .collect();
        let budget = 2_000;
        let out = render_draft_transcript(&lines, budget).unwrap();
        assert!(out.chars().count() <= budget + ELIDED_MARKER.chars().count() + 1);
        assert!(out.contains("第0句话"));
        assert!(out.contains("第399句话"));
        assert!(out.contains(ELIDED_MARKER));
        assert!(!out.contains("第200句话"));
    }
}
