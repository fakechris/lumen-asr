//! Learning configuration and explicit edit processing.
//!
//! Automatic post-insert observation lives in `lumen-edit-learning`; this
//! module keeps the manual command surface for the desktop UI.

use crate::config::LearningConfig;
use crate::AppState;
use lumen_core::{DictEntryKind, DictEntrySource, EditSource};
use lumen_dictionary::{candidates_from_edit, DictionaryEntry, LearnCandidate};
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningConfigDto {
    pub auto_promote: bool,
    pub auto_promote_threshold: u32,
    pub post_paste_capture: bool,
    /// Retained for configuration compatibility. The persistent engine uses
    /// semantic sessions and an independent retention policy.
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
    /// pre_insert_ui | post_paste_ax | post_paste_pane | manual
    pub source: Option<String>,
    /// When false, only suggest (no edit_event write). Default true.
    pub record_event: Option<bool>,
}

#[tauri::command]
pub fn get_learning_config(state: State<'_, AppState>) -> Result<LearningConfigDto, String> {
    let config = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_owned())?;
    Ok(dto(&config.learning))
}

#[tauri::command]
pub fn save_learning_config(
    state: State<'_, AppState>,
    input: LearningConfigInput,
) -> Result<LearningConfigDto, String> {
    let mut config = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_owned())?;
    if let Some(value) = input.auto_promote {
        config.learning.auto_promote = value;
    }
    if let Some(value) = input.auto_promote_threshold {
        config.learning.auto_promote_threshold = value.max(2);
    }
    if let Some(value) = input.post_paste_capture {
        config.learning.post_paste_capture = value;
    }
    if let Some(value) = input.post_paste_seconds {
        config.learning.post_paste_seconds = value.clamp(5, 120);
    }
    config.save()?;
    Ok(dto(&config.learning))
}

fn dto(config: &LearningConfig) -> LearningConfigDto {
    LearningConfigDto {
        auto_promote: config.auto_promote,
        auto_promote_threshold: config.auto_promote_threshold,
        post_paste_capture: config.post_paste_capture,
        post_paste_seconds: config.post_paste_seconds,
    }
}

#[tauri::command]
pub fn process_edit(
    state: State<'_, AppState>,
    input: ProcessEditInput,
) -> Result<ProcessEditResult, String> {
    let before = input.before_text;
    let after = input.after_text;
    if before == after {
        return Ok(ProcessEditResult {
            edit_event_id: None,
            candidates: Vec::new(),
            auto_promoted: Vec::new(),
            message: "no meaningful edit".into(),
        });
    }

    let source = match input.source.as_deref() {
        Some("post_paste_pane") => EditSource::PostPastePane,
        Some("post_paste_ax") => EditSource::PostPasteAx,
        Some("manual") => EditSource::Manual,
        _ => EditSource::PreInsertUi,
    };
    let learning = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_owned())?
        .learning
        .clone();
    let store_guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())?;
    let store = store_guard
        .as_ref()
        .ok_or_else(|| "database not available".to_owned())?;

    let mut edit_event_id = None;
    if input.record_event.unwrap_or(true) {
        if let Some(session_id) = input
            .session_id
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|session_id| store.get_session(*session_id).ok().flatten().is_some())
        {
            let id = store
                .add_edit_event(session_id, source, &before, &after)
                .map_err(|error| error.to_string())?;
            edit_event_id = Some(id.to_string());
        }
    }

    let candidates = candidates_from_edit(&before, &after);
    let mut auto_promoted = Vec::new();
    if learning.auto_promote {
        for candidate in &candidates {
            if candidate.kind != DictEntryKind::Replacement {
                continue;
            }
            let (Some(from), Some(to)) = (&candidate.from_text, &candidate.to_text) else {
                continue;
            };
            let edit_hits = store
                .count_identical_edits(&before, &after)
                .unwrap_or(0)
                .max(1);
            let mut entry = store
                .find_replacement(from, to)
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| {
                    let mut entry = DictionaryEntry::replacement(from, to);
                    entry.source = DictEntrySource::Learned;
                    entry.confirmed = false;
                    entry.hit_count = 0;
                    entry
                });
            entry.hit_count = entry.hit_count.saturating_add(1).max(edit_hits);
            entry.updated_at = chrono::Utc::now();
            if !entry.confirmed && entry.hit_count >= learning.auto_promote_threshold {
                entry.confirmed = true;
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|error| error.to_string())?;
                auto_promoted.push(entry);
            } else if !entry.confirmed {
                store
                    .upsert_dictionary_entry(&entry)
                    .map_err(|error| error.to_string())?;
            }
        }
    }

    let message = if !auto_promoted.is_empty() {
        format!(
            "auto-promoted {} dictionary entr(y/ies)",
            auto_promoted.len()
        )
    } else if candidates.is_empty() {
        "edit captured; no dictionary candidates".into()
    } else {
        format!(
            "edit captured; {} candidate(s) ready to confirm",
            candidates.len()
        )
    };
    Ok(ProcessEditResult {
        edit_event_id,
        candidates,
        auto_promoted,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_dto_preserves_legacy_config_while_engine_owns_retention() {
        let config = LearningConfig {
            auto_promote: true,
            auto_promote_threshold: 3,
            post_paste_capture: true,
            post_paste_seconds: 20,
        };

        let dto = dto(&config);

        assert!(dto.auto_promote);
        assert_eq!(dto.auto_promote_threshold, 3);
        assert!(dto.post_paste_capture);
        assert_eq!(dto.post_paste_seconds, 20);
    }
}
