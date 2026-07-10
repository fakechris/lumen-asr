//! Tauri IPC for store, dictionary, and edit learning (M1).

use crate::AppState;
use lumen_core::{
    EditSource, FocusInfo, InsertStrategy, SessionRecord, SessionStatus,
};
use lumen_dictionary::{candidates_from_edit, DictionaryEntry, LearnCandidate};
use lumen_platform::{default_data_dir, default_db_path};
use lumen_store::EditEventRecord;
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

fn with_store<T>(
    state: &State<'_, AppState>,
    f: impl FnOnce(&lumen_store::Store) -> Result<T, String>,
) -> Result<T, String> {
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database not available".to_string())?;
    f(store)
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub app: String,
    pub version: String,
    pub data_dir: String,
    pub db_path: String,
    pub db_ok: bool,
    pub session_count: u32,
    pub dictionary_count: u32,
}

#[tauri::command]
pub fn app_health(state: State<'_, AppState>) -> Health {
    let (db_ok, session_count, dictionary_count) = match state.store.lock() {
        Ok(g) => match g.as_ref() {
            Some(s) => {
                let sc = s.list_sessions(10_000).map(|v| v.len() as u32).unwrap_or(0);
                let dc = s.list_dictionary().map(|v| v.len() as u32).unwrap_or(0);
                (true, sc, dc)
            }
            None => (false, 0, 0),
        },
        Err(_) => (false, 0, 0),
    };

    Health {
        app: "Lumen ASR".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: default_data_dir().display().to_string(),
        db_path: default_db_path().display().to_string(),
        db_ok,
        session_count,
        dictionary_count,
    }
}

// ── Sessions ──────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>, limit: Option<u32>) -> Result<Vec<SessionRecord>, String> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    with_store(&state, |s| s.list_sessions(limit).map_err(|e| e.to_string()))
}

#[tauri::command]
pub fn get_session(state: State<'_, AppState>, id: String) -> Result<Option<SessionRecord>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    with_store(&state, |s| s.get_session(id).map_err(|e| e.to_string()))
}

#[tauri::command]
pub fn delete_session(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    with_store(&state, |s| s.delete_session(id).map_err(|e| e.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionInput {
    pub asr_raw: Option<String>,
    pub corrected: Option<String>,
    pub pasted: Option<String>,
    pub focused_app: Option<String>,
    /// When true, also record an edit event if asr/corrected differ from pasted.
    pub record_edit_if_changed: Option<bool>,
}

/// Save a completed (or partial) session — used by sample seed and the dictation loop.
#[tauri::command]
pub fn save_session(
    state: State<'_, AppState>,
    input: CreateSessionInput,
) -> Result<SessionRecord, String> {
    let mut rec = SessionRecord::new();
    rec.status = SessionStatus::Completed;
    rec.insert_strategy = InsertStrategy::CopyOnly;
    rec.asr_engine = Some("manual".into());
    rec.corrector_engine = Some("none".into());
    rec.asr_raw = input.asr_raw.clone();
    rec.corrected = input.corrected.clone();
    rec.pasted = input
        .pasted
        .clone()
        .or_else(|| input.corrected.clone())
        .or_else(|| input.asr_raw.clone());
    rec.focus = FocusInfo {
        app_name: input.focused_app,
        bundle_id: None,
        window_title: None,
    };

    with_store(&state, |s| {
        s.save_session(&rec).map_err(|e| e.to_string())?;

        if input.record_edit_if_changed.unwrap_or(false) {
            let before = rec
                .corrected
                .clone()
                .or_else(|| rec.asr_raw.clone())
                .unwrap_or_default();
            let after = rec.pasted.clone().unwrap_or_default();
            if !before.is_empty() && before != after {
                s.add_edit_event(rec.id, EditSource::PreInsertUi, &before, &after)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(rec)
    })
}

/// Seed one sample history row for empty-state UI testing.
#[tauri::command]
pub fn seed_demo_session(state: State<'_, AppState>) -> Result<SessionRecord, String> {
    save_session(
        state,
        CreateSessionInput {
            asr_raw: Some("你好  世界 用脱肯鉴权".into()),
            corrected: Some("你好，世界。用 Token 鉴权。".into()),
            pasted: Some("你好，世界。用 Token 鉴权。".into()),
            focused_app: Some("Notes".into()),
            record_edit_if_changed: Some(false),
        },
    )
}

// ── Edit events & learning ────────────────────────────────────────────────

#[tauri::command]
pub fn list_edit_events(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<EditEventRecord>, String> {
    let id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    with_store(&state, |s| s.list_edit_events(id).map_err(|e| e.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordEditInput {
    pub session_id: String,
    pub before_text: String,
    pub after_text: String,
    /// pre_insert_ui | post_paste_ax | manual
    pub source: Option<String>,
}

#[tauri::command]
pub fn record_edit_event(
    state: State<'_, AppState>,
    input: RecordEditInput,
) -> Result<String, String> {
    let session_id = Uuid::parse_str(&input.session_id).map_err(|e| e.to_string())?;
    let source = match input.source.as_deref() {
        Some("post_paste_ax") => EditSource::PostPasteAx,
        Some("manual") => EditSource::Manual,
        _ => EditSource::PreInsertUi,
    };
    with_store(&state, |s| {
        // Ensure session exists
        if s.get_session(session_id)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("session not found".into());
        }
        s.add_edit_event(session_id, source, &input.before_text, &input.after_text)
            .map(|id| id.to_string())
            .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub fn suggest_from_edit(before: String, after: String) -> Vec<LearnCandidate> {
    candidates_from_edit(&before, &after)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmLearnInput {
    pub kind: String, // term | replacement
    pub term: Option<String>,
    pub from_text: Option<String>,
    pub to_text: Option<String>,
    /// Optional session to attach an edit_event audit trail.
    pub session_id: Option<String>,
    pub before_text: Option<String>,
    pub after_text: Option<String>,
}

#[tauri::command]
pub fn confirm_learn(
    state: State<'_, AppState>,
    input: ConfirmLearnInput,
) -> Result<DictionaryEntry, String> {
    let mut entry = match input.kind.as_str() {
        "replacement" => {
            let from = input
                .from_text
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "from_text required for replacement".to_string())?;
            let to = input
                .to_text
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "to_text required for replacement".to_string())?;
            let mut e = DictionaryEntry::replacement(from, to);
            e.source = lumen_core::DictEntrySource::Learned;
            e
        }
        "term" => {
            let term = input
                .term
                .or(input.to_text)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "term required".to_string())?;
            let mut e = DictionaryEntry::term(term);
            e.source = lumen_core::DictEntrySource::Learned;
            e
        }
        other => return Err(format!("unknown kind: {other}")),
    };

    // Manual add from settings still uses confirmed=true via constructors.
    entry.confirmed = true;

    with_store(&state, |s| {
        if let (Some(sid), Some(before), Some(after)) =
            (input.session_id.as_ref(), input.before_text.as_ref(), input.after_text.as_ref())
        {
            if let Ok(uuid) = Uuid::parse_str(sid) {
                if s.get_session(uuid).ok().flatten().is_some() && before != after {
                    let _ = s.add_edit_event(uuid, EditSource::Manual, before, after);
                }
            }
        }
        s.upsert_dictionary_entry(&entry)
            .map_err(|e| e.to_string())?;
        Ok(entry)
    })
}

// ── Dictionary CRUD ───────────────────────────────────────────────────────

#[tauri::command]
pub fn list_dictionary(state: State<'_, AppState>) -> Result<Vec<DictionaryEntry>, String> {
    with_store(&state, |s| s.list_dictionary().map_err(|e| e.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTermInput {
    pub term: String,
}

#[tauri::command]
pub fn add_dictionary_term(
    state: State<'_, AppState>,
    input: AddTermInput,
) -> Result<DictionaryEntry, String> {
    let term = input.term.trim();
    if term.is_empty() {
        return Err("term cannot be empty".into());
    }
    let entry = DictionaryEntry::term(term);
    with_store(&state, |s| {
        s.upsert_dictionary_entry(&entry)
            .map_err(|e| e.to_string())?;
        Ok(entry)
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddReplacementInput {
    pub from_text: String,
    pub to_text: String,
}

#[tauri::command]
pub fn add_dictionary_replacement(
    state: State<'_, AppState>,
    input: AddReplacementInput,
) -> Result<DictionaryEntry, String> {
    let from = input.from_text.trim();
    let to = input.to_text.trim();
    if from.is_empty() || to.is_empty() {
        return Err("from_text and to_text required".into());
    }
    let entry = DictionaryEntry::replacement(from, to);
    with_store(&state, |s| {
        s.upsert_dictionary_entry(&entry)
            .map_err(|e| e.to_string())?;
        Ok(entry)
    })
}

#[tauri::command]
pub fn delete_dictionary_entry(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    with_store(&state, |s| {
        s.delete_dictionary_entry(id).map_err(|e| e.to_string())
    })
}
