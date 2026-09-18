//! Best-effort system notifications and main-window surfacing for meetings.
//!
//! Two user-visible jobs; both are fire-and-forget — a failed notification or
//! window show never affects the underlying meeting flow:
//!
//! 1. The detection / stop-suggest prompts render inside the main window, so
//!    a hidden or miniaturized window means the user never sees them. Before
//!    those events are emitted we un-hide the window WITHOUT stealing focus
//!    (`show()` only orders the window front; the meeting app the user just
//!    joined keeps the keyboard).
//! 2. When the offline meeting pipeline finishes we post a system
//!    notification ("纪要已生成") with the resulting stats. Desktop plugins
//!    expose no notification-click callback, so navigating back to the
//!    meeting rides on `RunEvent::Reopen` (macOS dock / notification
//!    activation): [`handle_reopen`] surfaces the window and hands the
//!    stashed meeting id to the front-end via the `app-reopened` event.
//!
//! Notification permission is not managed here: on desktop the plugin reports
//! granted and lets the OS auto-prompt on first delivery (macOS attributes
//! dev-mode notifications to Terminal), and a denied or failed post simply
//! silences the feature.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// The label of the main window in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";

/// Un-hide (and de-miniaturize) the main window without taking focus. A no-op
/// when the window is already on screen or cannot be found.
pub fn ensure_main_window_visible(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    let _ = win.unminimize();
    if matches!(win.is_visible(), Ok(false)) {
        // `show()` orders the window front but does not make it key, so the
        // meeting app the user just joined stays frontmost for the keyboard.
        let _ = win.show();
    }
}

/// Stats shown in the pipeline-completion notification, mirroring the shape of
/// the competitor card: duration, transcript size, minutes size.
pub struct MeetingDoneStats {
    pub title: Option<String>,
    pub duration_seconds: Option<f64>,
    pub transcript_chars: usize,
    /// `None` when no minutes were generated (no LLM configured or the pass
    /// produced nothing) — the body then only reports the transcript.
    pub minutes_chars: Option<usize>,
}

/// Count "words" the way a Chinese reader does: non-whitespace characters.
pub fn count_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Build the notification body from the stats: "时长6分钟 · 记录1133字 ·
/// 纪要1668字", omitting the parts we do not know.
pub fn format_minutes_body(stats: &MeetingDoneStats) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(secs) = stats.duration_seconds {
        if secs.is_finite() && secs > 0.0 {
            let mins = (secs / 60.0).round() as i64;
            parts.push(if mins < 1 {
                "时长不足1分钟".to_string()
            } else {
                format!("时长{mins}分钟")
            });
        }
    }
    if stats.transcript_chars > 0 {
        parts.push(format!("记录{}字", stats.transcript_chars));
    }
    if let Some(chars) = stats.minutes_chars {
        if chars > 0 {
            parts.push(format!("纪要{chars}字"));
        }
    }
    if parts.is_empty() {
        "打开 Lumen 查看".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Post the "检测到会议" heads-up that accompanies a shown detection prompt.
/// The in-app prompt stays the only way to actually start recording.
pub fn notify_meeting_detected(app: &AppHandle, display_name: &str) {
    send(
        app,
        &format!("检测到{display_name}会议"),
        "Lumen 在等你确认——点「开始记录」后才会开始录音",
    );
}

/// Post the completion notification for a finished offline pipeline.
pub fn notify_minutes_ready(app: &AppHandle, stats: &MeetingDoneStats) {
    let title = match stats.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => format!("「{t}」纪要已生成"),
        _ => "会议纪要已生成".to_string(),
    };
    send(app, &title, &format_minutes_body(stats));
}

/// `RunEvent::Reopen` (macOS): the user clicked the dock icon (or a
/// notification banner). Surface the window — this time taking focus, it is
/// an explicit user intent — and tell the front-end which meeting to open.
pub fn handle_reopen(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
    let meeting_id = app.try_state::<crate::AppState>().and_then(|state| {
        let slot = state.last_finished_meeting.lock().ok()?;
        slot.clone()
    });
    let _ = app.emit("app-reopened", AppReopenedEvent { meeting_id });
}

/// Payload of the `app-reopened` event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppReopenedEvent {
    pub meeting_id: Option<String>,
}

fn send(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(
        title: Option<&str>,
        duration_seconds: Option<f64>,
        transcript_chars: usize,
        minutes_chars: Option<usize>,
    ) -> MeetingDoneStats {
        MeetingDoneStats {
            title: title.map(String::from),
            duration_seconds,
            transcript_chars,
            minutes_chars,
        }
    }

    #[test]
    fn body_reports_all_known_stats() {
        assert_eq!(
            format_minutes_body(&stats(Some("周会"), Some(366.0), 1133, Some(1668))),
            "时长6分钟 · 记录1133字 · 纪要1668字"
        );
    }

    #[test]
    fn body_omits_unknown_parts_and_sub_minute_rounds_down() {
        assert_eq!(
            format_minutes_body(&stats(None, Some(20.0), 350, None)),
            "时长不足1分钟 · 记录350字"
        );
        assert_eq!(
            format_minutes_body(&stats(None, None, 0, None)),
            "打开 Lumen 查看"
        );
    }

    #[test]
    fn body_ignores_missing_duration_and_empty_minutes() {
        assert_eq!(
            format_minutes_body(&stats(Some("x"), None, 42, Some(0))),
            "记录42字"
        );
    }

    #[test]
    fn count_chars_skips_whitespace_only() {
        assert_eq!(count_chars("你好，世界 hello\n\t world"), 15);
        assert_eq!(count_chars("  \n"), 0);
    }
}
