//! Edit learning pipeline (M6): record edits, suggest candidates, optional auto-promote,
//! optional post-paste capture of user corrections in the target app.

use crate::config::LearningConfig;
use crate::AppState;
use lumen_core::{DictEntryKind, DictEntrySource, EditSource};
use lumen_dictionary::{candidates_from_edit, DictionaryEntry, LearnCandidate};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningConfigDto {
    pub auto_promote: bool,
    pub auto_promote_threshold: u32,
    pub post_paste_capture: bool,
    pub post_paste_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningConfigInput {
    pub auto_promote: Option<bool>,
    pub auto_promote_threshold: Option<u32>,
    pub post_paste_capture: Option<bool>,
    pub post_paste_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEditResult {
    pub edit_event_id: Option<String>,
    pub candidates: Vec<LearnCandidate>,
    pub auto_promoted: Vec<DictionaryEntry>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEditInput {
    pub before_text: String,
    pub after_text: String,
    pub session_id: Option<String>,
    /// pre_insert_ui | post_paste_ax | manual
    pub source: Option<String>,
    /// When false, only suggest (no edit_event write). Default true.
    pub record_event: Option<bool>,
}

#[tauri::command]
pub fn get_learning_config(state: State<'_, AppState>) -> Result<LearningConfigDto, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(dto(&cfg.learning))
}

#[tauri::command]
pub fn save_learning_config(
    state: State<'_, AppState>,
    input: LearningConfigInput,
) -> Result<LearningConfigDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    if let Some(v) = input.auto_promote {
        guard.learning.auto_promote = v;
    }
    if let Some(v) = input.auto_promote_threshold {
        guard.learning.auto_promote_threshold = v.max(2);
    }
    if let Some(v) = input.post_paste_capture {
        guard.learning.post_paste_capture = v;
    }
    if let Some(v) = input.post_paste_seconds {
        guard.learning.post_paste_seconds = v.clamp(5, 120);
    }
    guard.save()?;
    Ok(dto(&guard.learning))
}

fn dto(c: &LearningConfig) -> LearningConfigDto {
    LearningConfigDto {
        auto_promote: c.auto_promote,
        auto_promote_threshold: c.auto_promote_threshold,
        post_paste_capture: c.post_paste_capture,
        post_paste_seconds: c.post_paste_seconds,
    }
}

/// Record an edit (optional), generate candidates, apply auto-promote policy.
#[tauri::command]
pub fn process_edit(
    state: State<'_, AppState>,
    input: ProcessEditInput,
) -> Result<ProcessEditResult, String> {
    let before = input.before_text.trim().to_string();
    let after = input.after_text.trim().to_string();
    if before.is_empty() || after.is_empty() || before == after {
        return Ok(ProcessEditResult {
            edit_event_id: None,
            candidates: vec![],
            auto_promoted: vec![],
            message: "no meaningful edit".into(),
        });
    }

    let source = match input.source.as_deref() {
        Some("post_paste_ax") => EditSource::PostPasteAx,
        Some("manual") => EditSource::Manual,
        _ => EditSource::PreInsertUi,
    };

    let learning = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .learning
        .clone();

    let store_guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let store = store_guard
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;

    let mut edit_event_id = None;
    if input.record_event.unwrap_or(true) {
        if let Some(sid) = input.session_id.as_ref() {
            if let Ok(uuid) = Uuid::parse_str(sid) {
                if store.get_session(uuid).ok().flatten().is_some() {
                    let id = store
                        .add_edit_event(uuid, source, &before, &after)
                        .map_err(|e| e.to_string())?;
                    edit_event_id = Some(id.to_string());
                }
            }
        }
    }

    let candidates = candidates_from_edit(&before, &after);
    let mut auto_promoted = Vec::new();

    if learning.auto_promote {
        for c in &candidates {
            if c.kind != DictEntryKind::Replacement {
                continue;
            }
            let (Some(from), Some(to)) = (&c.from_text, &c.to_text) else {
                continue;
            };
            // Count identical full-string edits + hit_count on existing entry
            let edit_hits = store
                .count_identical_edits(&before, &after)
                .unwrap_or(0)
                .max(1);
            let mut entry = store
                .find_replacement(from, to)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| {
                    let mut e = DictionaryEntry::replacement(from, to);
                    e.source = DictEntrySource::Learned;
                    e.confirmed = false;
                    e.hit_count = 0;
                    e
                });

            entry.hit_count = entry.hit_count.saturating_add(1).max(edit_hits);
            entry.updated_at = chrono::Utc::now();

            if !entry.confirmed && entry.hit_count >= learning.auto_promote_threshold {
                entry.confirmed = true;
                entry.source = DictEntrySource::Learned;
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|e| e.to_string())?;
                auto_promoted.push(entry);
            } else if !entry.confirmed {
                // Keep unconfirmed progress for threshold tracking
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    let message = if !auto_promoted.is_empty() {
        format!("auto-promoted {} dictionary entr(y/ies)", auto_promoted.len())
    } else if candidates.is_empty() {
        "no dictionary candidates for this edit".into()
    } else {
        format!("{} candidate(s) ready to confirm", candidates.len())
    };

    Ok(ProcessEditResult {
        edit_event_id,
        candidates,
        auto_promoted,
        message,
    })
}

/// Best-effort read of focused text field via System Events (needs Accessibility).
pub fn read_focused_text_osascript() -> Option<String> {
    let script = r#"
tell application "System Events"
  try
    set frontProc to first application process whose frontmost is true
    tell frontProc
      set fe to value of attribute "AXFocusedUIElement"
      try
        return value of fe as text
      on error
        try
          return value of attribute "AXValue" of fe as text
        end try
      end try
    end tell
  end try
end tell
return ""
"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// After paste, watch for user edits in the focused field and emit learn suggestions.
pub fn spawn_post_paste_watch(
    app: AppHandle,
    session_id: Uuid,
    pasted: String,
    seconds: u64,
) {
    if pasted.is_empty() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut last = pasted.clone();
        // Initial settle
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        while std::time::Instant::now() < deadline {
            let current = tokio::task::spawn_blocking(read_focused_text_osascript)
                .await
                .ok()
                .flatten();

            if let Some(cur) = current {
                // Only care if field still contains (or evolved from) our paste.
                if cur != last
                    && (cur.contains(pasted.chars().take(12).collect::<String>().as_str())
                        || last.contains(cur.chars().take(12).collect::<String>().as_str())
                        || pasted.len() > 8 && cur.len() > 4)
                {
                    // Meaningful divergence from last snapshot
                    if cur != pasted && cur != last {
                        let before = pasted.clone();
                        let after = cur.clone();
                        let state = app.state::<AppState>();
                        let result = {
                            // Call process_edit logic inline
                            process_edit_from_state(
                                &state,
                                ProcessEditInput {
                                    before_text: before.clone(),
                                    after_text: after.clone(),
                                    session_id: Some(session_id.to_string()),
                                    source: Some("post_paste_ax".into()),
                                    record_event: Some(true),
                                },
                            )
                        };
                        if let Ok(res) = result {
                            if !res.candidates.is_empty() || !res.auto_promoted.is_empty() {
                                let _ = app.emit(
                                    "learn-suggestion",
                                    LearnSuggestionEvent {
                                        session_id: session_id.to_string(),
                                        before_text: before,
                                        after_text: after,
                                        source: "post_paste_ax".into(),
                                        candidates: res.candidates,
                                        auto_promoted: res.auto_promoted,
                                        message: res.message,
                                    },
                                );
                                // One suggestion burst is enough
                                return;
                            }
                        }
                        last = cur;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

fn process_edit_from_state(
    state: &AppState,
    input: ProcessEditInput,
) -> Result<ProcessEditResult, String> {
    // Duplicate of process_edit without Tauri State wrapper
    let before = input.before_text.trim().to_string();
    let after = input.after_text.trim().to_string();
    if before.is_empty() || after.is_empty() || before == after {
        return Ok(ProcessEditResult {
            edit_event_id: None,
            candidates: vec![],
            auto_promoted: vec![],
            message: "no meaningful edit".into(),
        });
    }

    let source = match input.source.as_deref() {
        Some("post_paste_ax") => EditSource::PostPasteAx,
        Some("manual") => EditSource::Manual,
        _ => EditSource::PreInsertUi,
    };

    let learning = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .learning
        .clone();

    let store_guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let store = store_guard
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;

    let mut edit_event_id = None;
    if input.record_event.unwrap_or(true) {
        if let Some(sid) = input.session_id.as_ref() {
            if let Ok(uuid) = Uuid::parse_str(sid) {
                if store.get_session(uuid).ok().flatten().is_some() {
                    let id = store
                        .add_edit_event(uuid, source, &before, &after)
                        .map_err(|e| e.to_string())?;
                    edit_event_id = Some(id.to_string());
                }
            }
        }
    }

    let candidates = candidates_from_edit(&before, &after);
    let mut auto_promoted = Vec::new();

    if learning.auto_promote {
        for c in &candidates {
            if c.kind != DictEntryKind::Replacement {
                continue;
            }
            let (Some(from), Some(to)) = (&c.from_text, &c.to_text) else {
                continue;
            };
            let edit_hits = store
                .count_identical_edits(&before, &after)
                .unwrap_or(0)
                .max(1);
            let mut entry = store
                .find_replacement(from, to)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| {
                    let mut e = DictionaryEntry::replacement(from, to);
                    e.source = DictEntrySource::Learned;
                    e.confirmed = false;
                    e.hit_count = 0;
                    e
                });
            entry.hit_count = entry.hit_count.saturating_add(1).max(edit_hits);
            entry.updated_at = chrono::Utc::now();
            if !entry.confirmed && entry.hit_count >= learning.auto_promote_threshold {
                entry.confirmed = true;
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|e| e.to_string())?;
                auto_promoted.push(entry);
            } else if !entry.confirmed {
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(ProcessEditResult {
        edit_event_id,
        candidates,
        auto_promoted,
        message: "ok".into(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnSuggestionEvent {
    pub session_id: String,
    pub before_text: String,
    pub after_text: String,
    pub source: String,
    pub candidates: Vec<LearnCandidate>,
    pub auto_promoted: Vec<DictionaryEntry>,
    pub message: String,
}
