//! Edit learning pipeline (M6): record edits, suggest candidates, optional auto-promote,
//! optional post-paste capture of user corrections in the target app.

use crate::config::LearningConfig;
use crate::edit_attribution::{EditProjection, InsertionAnchor};
use crate::AppState;
use lumen_core::{DictEntryKind, DictEntrySource, EditSource};
use lumen_dictionary::{candidates_from_edit, DictionaryEntry, LearnCandidate};
use lumen_platform_macos::{
    focused_text_field_snapshot, FocusedTextFieldSnapshot, FrontmostTarget,
};
use lumen_store::EditAttribution;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PostInsertWatchRequest {
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub inserted_text: String,
    pub target: FrontmostTarget,
}

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
    process_edit_from_state(&state, input, None)
}

#[derive(Debug)]
struct PendingProjection {
    after_text: String,
    field_after_hash: String,
    stable_since: std::time::Instant,
}

#[derive(Debug)]
struct ObservedEdit {
    after_text: String,
    target_fingerprint_hash: String,
    field_before_hash: String,
    field_after_hash: String,
}

#[derive(Debug)]
struct PreparedEditWatch {
    anchor: InsertionAnchor,
    target_fingerprint_hash: String,
    field_before_hash: String,
}

#[derive(Debug)]
enum PinnedFieldProjection {
    FieldChanged,
    Current {
        projection: EditProjection,
        field_hash: String,
    },
}

/// Watch only the target field and only the span inserted by this attempt.
pub async fn spawn_post_insert_watch(
    app: AppHandle,
    request: PostInsertWatchRequest,
    seconds: u64,
) -> bool {
    if request.inserted_text.is_empty() {
        return false;
    }
    // Capture the original inserted span before debug/audio persistence can delay observation
    // and before a fast user correction removes the original text.
    let request_for_prepare = request.clone();
    let prepared =
        match tokio::task::spawn_blocking(move || prepare_edit_watch(&request_for_prepare)).await {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(reason)) => {
                tracing::warn!(
                    session_id = %request.session_id,
                    attempt_id = %request.attempt_id,
                    target_bundle_id = ?request.target.bundle_id,
                    %reason,
                    "edit watch could not anchor immediately after insertion"
                );
                return false;
            }
            Err(error) => {
                let reason = format!("anchor_task_failed:{error}");
                tracing::warn!(
                    session_id = %request.session_id,
                    attempt_id = %request.attempt_id,
                    target_bundle_id = ?request.target.bundle_id,
                    %reason,
                    "edit watch could not anchor immediately after insertion"
                );
                return false;
            }
        };
    tracing::info!(
        session_id = %request.session_id,
        attempt_id = %request.attempt_id,
        target_bundle_id = ?request.target.bundle_id,
        seconds,
        "edit watch started"
    );
    tauri::async_runtime::spawn(async move {
        let Some(observed) = observe_prepared_edit(&request, seconds, prepared).await else {
            tracing::info!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                "edit watch finished without a confirmed same-span edit"
            );
            return;
        };
        if !wait_for_session_persistence(&app, request.session_id, seconds).await {
            tracing::warn!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                "edit watch observed a correction but its session was not persisted in time"
            );
            return;
        }
        persist_and_emit_edit(&app, &request, &observed);
    });
    true
}

fn prepare_edit_watch(request: &PostInsertWatchRequest) -> Result<PreparedEditWatch, String> {
    let initial = read_pinned_field(&request.target)
        .ok_or_else(|| "pinned_target_field_unavailable".to_owned())?;
    let anchor = InsertionAnchor::from_post_insert(&initial.value, &request.inserted_text)?;
    Ok(PreparedEditWatch {
        anchor,
        target_fingerprint_hash: field_fingerprint(&request.target, &initial),
        field_before_hash: text_hash(&initial.value),
    })
}

#[cfg(test)]
async fn observe_post_insert(
    request: &PostInsertWatchRequest,
    seconds: u64,
) -> Option<ObservedEdit> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let prepared = loop {
        let request = request.clone();
        let observation = tokio::task::spawn_blocking(move || prepare_edit_watch(&request))
            .await
            .ok()
            .and_then(Result::ok);
        if observation.is_some() || std::time::Instant::now() >= deadline {
            break observation;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    let Some(prepared) = prepared else {
        tracing::debug!(
            session_id = %request.session_id,
            "edit watch could not read the pinned target field"
        );
        return None;
    };
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    observe_prepared_edit(request, remaining.as_secs().max(1), prepared).await
}

async fn observe_prepared_edit(
    request: &PostInsertWatchRequest,
    seconds: u64,
    prepared: PreparedEditWatch,
) -> Option<ObservedEdit> {
    const STABLE_EDIT_DURATION: std::time::Duration = std::time::Duration::from_millis(1_200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let PreparedEditWatch {
        anchor,
        target_fingerprint_hash,
        field_before_hash,
    } = prepared;
    let mut pending: Option<PendingProjection> = None;

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let target = request.target.clone();
        let anchor_for_probe = anchor.clone();
        let expected_fingerprint = target_fingerprint_hash.clone();
        let observation = tokio::task::spawn_blocking(move || {
            let current = read_pinned_field(&target)?;
            if field_fingerprint(&target, &current) != expected_fingerprint {
                return Some(PinnedFieldProjection::FieldChanged);
            }
            Some(PinnedFieldProjection::Current {
                projection: project_for_target(&target, &anchor_for_probe, &current.value),
                field_hash: text_hash(&current.value),
            })
        })
        .await
        .ok()
        .flatten();
        let Some(observation) = observation else {
            continue;
        };
        let PinnedFieldProjection::Current {
            projection,
            field_hash,
        } = observation
        else {
            tracing::info!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                "edit watch stopped because the focused field changed"
            );
            return None;
        };
        match projection {
            EditProjection::Unchanged => pending = None,
            EditProjection::Unrelated => {
                tracing::info!(
                    session_id = %request.session_id,
                    attempt_id = %request.attempt_id,
                    "edit watch stopped because text outside the inserted span changed"
                );
                return None;
            }
            EditProjection::Edited { after_text } => match pending.as_mut() {
                Some(value) if value.after_text == after_text => {
                    value.field_after_hash = field_hash;
                    if value.stable_since.elapsed() >= STABLE_EDIT_DURATION {
                        return Some(ObservedEdit {
                            after_text: value.after_text.clone(),
                            target_fingerprint_hash,
                            field_before_hash,
                            field_after_hash: value.field_after_hash.clone(),
                        });
                    }
                }
                _ => {
                    pending = Some(PendingProjection {
                        after_text,
                        field_after_hash: field_hash,
                        stable_since: std::time::Instant::now(),
                    });
                }
            },
        }
    }
    pending
        .filter(|pending| pending.stable_since.elapsed() >= STABLE_EDIT_DURATION)
        .map(|pending| ObservedEdit {
            after_text: pending.after_text,
            target_fingerprint_hash,
            field_before_hash,
            field_after_hash: pending.field_after_hash,
        })
}

fn project_for_target(
    target: &FrontmostTarget,
    anchor: &InsertionAnchor,
    field_value: &str,
) -> EditProjection {
    let terminal_bundle = matches!(
        target.bundle_id.as_deref(),
        Some(
            "com.mitchellh.ghostty"
                | "com.apple.Terminal"
                | "com.googlecode.iterm2"
                | "dev.warp.Warp-Stable"
        )
    );
    if terminal_bundle {
        anchor.project_terminal_current_line(field_value)
    } else {
        anchor.project(field_value)
    }
}

fn read_pinned_field(target: &FrontmostTarget) -> Option<FocusedTextFieldSnapshot> {
    let field = focused_text_field_snapshot()?;
    let owner = FrontmostTarget {
        name: (!field.owner_name.is_empty()).then(|| field.owner_name.clone()),
        bundle_id: (!field.owner_bundle_id.is_empty()).then(|| field.owner_bundle_id.clone()),
    };
    same_target(target, &owner).then_some(field)
}

async fn wait_for_session_persistence(app: &AppHandle, session_id: Uuid, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds.clamp(2, 10));
    loop {
        let ready = {
            let state = app.state::<AppState>();
            state
                .store
                .lock()
                .ok()
                .and_then(|guard| {
                    guard
                        .as_ref()
                        .and_then(|store| store.get_session(session_id).ok().flatten())
                })
                .is_some()
        };
        if ready {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn same_target(expected: &FrontmostTarget, actual: &FrontmostTarget) -> bool {
    match expected.bundle_id.as_deref() {
        Some(bundle_id) => actual.bundle_id.as_deref() == Some(bundle_id),
        None => expected.name.is_some() && expected.name == actual.name,
    }
}

fn field_fingerprint(target: &FrontmostTarget, field: &FocusedTextFieldSnapshot) -> String {
    let material = format!(
        "{}\u{001f}{}\u{001f}{}",
        target.bundle_id.as_deref().unwrap_or_default(),
        target.name.as_deref().unwrap_or_default(),
        field.fingerprint_material()
    );
    text_hash(&material)
}

fn text_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn persist_and_emit_edit(
    app: &AppHandle,
    request: &PostInsertWatchRequest,
    observed: &ObservedEdit,
) {
    let attribution = EditAttribution {
        attempt_id: Some(request.attempt_id),
        target_app_name: request.target.name.clone(),
        target_bundle_id: request.target.bundle_id.clone(),
        observer: Some("focused_field_poll_v2".into()),
        target_fingerprint_hash: Some(observed.target_fingerprint_hash.clone()),
        field_before_hash: Some(observed.field_before_hash.clone()),
        field_after_hash: Some(observed.field_after_hash.clone()),
        status: "confirmed_same_field_span".into(),
        ..EditAttribution::default()
    };
    let state = app.state::<AppState>();
    match process_edit_from_state(
        &state,
        ProcessEditInput {
            before_text: request.inserted_text.clone(),
            after_text: observed.after_text.clone(),
            session_id: Some(request.session_id.to_string()),
            source: Some("post_paste_ax".into()),
            record_event: Some(true),
        },
        Some(attribution),
    ) {
        Ok(result) => {
            if result.edit_event_id.is_none() {
                tracing::warn!(
                    session_id = %request.session_id,
                    attempt_id = %request.attempt_id,
                    "attributed edit was observed but could not be attached to its session"
                );
                return;
            }
            let _ = app.emit(
                "edit-feedback-captured",
                EditFeedbackEvent {
                    edit_event_id: result.edit_event_id,
                    session_id: request.session_id.to_string(),
                    before_text: request.inserted_text.clone(),
                    after_text: observed.after_text.clone(),
                    source: "post_paste_ax".into(),
                    candidates: result.candidates,
                    auto_promoted: result.auto_promoted,
                    message: result.message,
                },
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %request.session_id,
                error = %error,
                "failed to persist attributed edit feedback"
            );
        }
    }
}

fn process_edit_from_state(
    state: &AppState,
    input: ProcessEditInput,
    attribution: Option<EditAttribution>,
) -> Result<ProcessEditResult, String> {
    let before = input.before_text;
    let after = input.after_text;
    if before == after {
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
                    let id = match attribution.as_ref() {
                        Some(attribution) => store.add_edit_event_with_attribution(
                            uuid,
                            source,
                            &before,
                            &after,
                            attribution,
                        ),
                        None => store.add_edit_event(uuid, source, &before, &after),
                    }
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFeedbackEvent {
    pub edit_event_id: Option<String>,
    pub session_id: String,
    pub before_text: String,
    pub after_text: String,
    pub source: String,
    pub candidates: Vec<LearnCandidate>,
    pub auto_promoted: Vec<DictionaryEntry>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_inject::TextInjectorBackend;
    use std::fs;
    use std::process::Command;
    use std::time::Duration;

    fn ghostty_window_count() -> usize {
        let output = Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to tell process \"Ghostty\" to return count of windows",
            ])
            .output()
            .expect("query Ghostty windows");
        assert!(output.status.success(), "Ghostty must be running");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("Ghostty window count")
    }

    struct GhosttyTestWindow;

    impl Drop for GhosttyTestWindow {
        fn drop(&mut self) {
            let _ = Command::new("osascript")
                .args([
                    "-e",
                    "tell application \"System Events\"",
                    "-e",
                    "tell process \"Ghostty\"",
                    "-e",
                    "keystroke \"u\" using control down",
                    "-e",
                    "keystroke \"w\" using command down",
                    "-e",
                    "end tell",
                    "-e",
                    "end tell",
                ])
                .status();
        }
    }

    #[tokio::test]
    #[ignore = "requires Ghostty, a logged-in macOS session, and Accessibility permission"]
    async fn live_ghostty_fast_edit_is_attributed_to_the_inserted_span() {
        let _live_test_guard = crate::MACOS_LIVE_TEST_LOCK.lock().await;
        let original_window_count = ghostty_window_count();
        let open_status = Command::new("osascript")
            .args([
                "-e",
                "tell application \"Ghostty\" to activate",
                "-e",
                "delay 0.2",
                "-e",
                "tell application \"System Events\" to keystroke \"n\" using command down",
            ])
            .status()
            .expect("open a dedicated Ghostty window");
        assert!(open_status.success());
        let window_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while ghostty_window_count() <= original_window_count {
            assert!(
                std::time::Instant::now() < window_deadline,
                "Ghostty did not open a dedicated test window"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _test_window = GhosttyTestWindow;
        let activate_status = Command::new("osascript")
            .args([
                "-e",
                "tell application \"Ghostty\" to activate",
                "-e",
                "delay 0.4",
                "-e",
                "tell application \"System Events\" to tell process \"Ghostty\" to set frontmost to true",
            ])
            .status()
            .expect("activate the dedicated Ghostty window");
        assert!(activate_status.success());
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut ready_hash: Option<(String, std::time::Instant)> = None;
        loop {
            if let Some(field) = focused_text_field_snapshot().filter(|field| {
                field.owner_bundle_id == "com.mitchellh.ghostty"
                    && field.role == "AXTextArea"
                    && field.value.lines().count() >= 2
            }) {
                let hash = text_hash(&field.value);
                match ready_hash.as_mut() {
                    Some((previous, since)) if *previous == hash => {
                        if since.elapsed() >= Duration::from_millis(500) {
                            break;
                        }
                    }
                    _ => ready_hash = Some((hash, std::time::Instant::now())),
                }
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "dedicated Ghostty window never became the focused AX text area"
            );
            let _ = Command::new("osascript")
                .args(["-e", "tell application \"Ghostty\" to activate"])
                .status();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let nonce = Uuid::new_v4().simple().to_string();
        let inserted = format!("LUMEN_ORIGINAL_{nonce}");
        let edited = format!("LUMEN_EDITED_{nonce}");
        lumen_platform_macos::MacTextInjectorBackend
            .type_unicode(&inserted)
            .await
            .expect("type the inserted text");
        let field_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let initial_field = loop {
            if let Some(field) = focused_text_field_snapshot() {
                if field.value.matches(&inserted).count() == 1 {
                    break field;
                }
            }
            if std::time::Instant::now() >= field_deadline {
                let evidence = focused_text_field_snapshot().map(|field| {
                    (
                        field.owner_name,
                        field.owner_bundle_id,
                        field.role,
                        field.value.len(),
                        field.value.lines().count(),
                        text_hash(&field.value),
                    )
                });
                panic!("Ghostty did not expose the inserted command line; current={evidence:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let request = PostInsertWatchRequest {
            session_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            inserted_text: inserted,
            target: FrontmostTarget {
                name: Some("Ghostty".into()),
                bundle_id: Some("com.mitchellh.ghostty".into()),
            },
        };
        let edited_for_script = edited.clone();
        let editor = async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let clear_status = Command::new("osascript")
                .args([
                    "-e",
                    "tell application \"System Events\"",
                    "-e",
                    "tell process \"Ghostty\"",
                    "-e",
                    "keystroke \"u\" using control down",
                    "-e",
                    "end tell",
                    "-e",
                    "end tell",
                ])
                .status()
                .expect("clear the inserted Ghostty command line");
            lumen_platform_macos::MacTextInjectorBackend
                .type_unicode(&edited_for_script)
                .await
                .expect("type the edited Ghostty command line");
            clear_status
        };

        let (observed, edit_status) = tokio::join!(observe_post_insert(&request, 5), editor);
        let failure_evidence = observed.is_none().then(|| {
            let current = focused_text_field_snapshot();
            let projection =
                InsertionAnchor::from_post_insert(&initial_field.value, &request.inserted_text)
                    .ok()
                    .and_then(|anchor| {
                        current
                            .as_ref()
                            .map(|field| project_for_target(&request.target, &anchor, &field.value))
                    });
            current.map(|field| {
                let fingerprint = field_fingerprint(&request.target, &field);
                let value_len = field.value.len();
                let common_prefix = initial_field
                    .value
                    .bytes()
                    .zip(field.value.bytes())
                    .take_while(|(left, right)| left == right)
                    .count();
                let common_suffix = initial_field
                    .value
                    .bytes()
                    .rev()
                    .zip(field.value.bytes().rev())
                    .take_while(|(left, right)| left == right)
                    .count();
                let inserted_offset = initial_field.value.find(&request.inserted_text);
                (
                    field.owner_name,
                    field.owner_bundle_id,
                    field.role,
                    fingerprint,
                    value_len,
                    inserted_offset,
                    common_prefix,
                    common_suffix,
                    projection,
                )
            })
        });
        assert!(edit_status.success());
        assert_eq!(
            observed
                .unwrap_or_else(|| {
                    panic!(
                        "fast Ghostty edit should be observed; initial_fingerprint={}; \
                         initial_len={}; current={failure_evidence:?}",
                        field_fingerprint(&request.target, &initial_field),
                        initial_field.value.len()
                    )
                })
                .after_text,
            edited
        );
    }

    #[tokio::test]
    #[ignore = "requires a logged-in macOS session and Accessibility permission"]
    async fn live_textedit_edit_is_attributed_to_the_inserted_span() {
        let _live_test_guard = crate::MACOS_LIVE_TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let document = directory.path().join("lumen-edit-feedback-e2e.txt");
        let nonce = Uuid::new_v4().simple().to_string();
        let prefix = format!("before-{nonce}\n");
        let inserted = format!("这是需要编辑的识别结果-{nonce}");
        let edited = format!("这是已经编辑的识别结果-{nonce}");
        let suffix = format!("\nafter-{nonce}");
        fs::write(&document, format!("{prefix}{inserted}{suffix}")).unwrap();
        assert!(Command::new("open")
            .args(["-a", "TextEdit"])
            .arg(&document)
            .status()
            .unwrap()
            .success());
        let target = FrontmostTarget {
            name: Some("TextEdit".into()),
            bundle_id: Some("com.apple.TextEdit".into()),
        };
        let field_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let actual_field = loop {
            if let Some(field) = focused_text_field_snapshot() {
                if field.value.contains(&inserted) {
                    break field;
                }
            }
            assert!(
                std::time::Instant::now() < field_deadline,
                "TextEdit did not expose the opened document's focused text area"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let actual_target =
            lumen_platform_macos::frontmost_target().expect("TextEdit was not frontmost");
        assert!(same_target(&target, &actual_target));
        assert_eq!(actual_field.value.matches(&inserted).count(), 1);
        let request = PostInsertWatchRequest {
            session_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            inserted_text: inserted,
            target,
        };
        let replacement = actual_field
            .value
            .replacen(&request.inserted_text, &edited, 1);
        let editor = async move {
            tokio::time::sleep(Duration::from_millis(900)).await;
            let script = format!(
                "tell application \"TextEdit\" to set text of front document to \"{}\"",
                replacement.replace('\\', "\\\\").replace('"', "\\\"")
            );
            Command::new("osascript")
                .args(["-e", &script])
                .status()
                .unwrap()
        };

        let (observed, edit_status) = tokio::join!(observe_post_insert(&request, 6), editor);
        let _ = Command::new("osascript")
            .args([
                "-e",
                "tell application \"TextEdit\" to close front document saving no",
            ])
            .status();

        assert!(edit_status.success());
        let observed = observed.unwrap_or_else(|| {
            let current_target = lumen_platform_macos::frontmost_target();
            let current_field = focused_text_field_snapshot();
            let projection = match InsertionAnchor::from_post_insert(
                &actual_field.value,
                &request.inserted_text,
            )
            .ok()
            .and_then(|anchor| {
                current_field
                    .as_ref()
                    .map(|field| anchor.project(&field.value))
            }) {
                Some(EditProjection::Unchanged) => "unchanged",
                Some(EditProjection::Edited { .. }) => "edited",
                Some(EditProjection::Unrelated) => "unrelated",
                None => "unavailable",
            };
            let current_evidence = current_field.as_ref().map(|field| {
                (
                    field.role.as_str(),
                    field_fingerprint(&request.target, field),
                    text_hash(&field.value),
                    field.value.len(),
                )
            });
            panic!(
                "same-field edit was not observed; target={current_target:?}; \
                 initial_fingerprint={}; initial_value_hash={}; initial_value_len={}; \
                 current_evidence={current_evidence:?}; projection={projection}",
                field_fingerprint(&request.target, &actual_field),
                text_hash(&actual_field.value),
                actual_field.value.len(),
            );
        });
        assert_eq!(observed.after_text, edited);
        assert!(!observed.target_fingerprint_hash.is_empty());
        assert_ne!(observed.field_before_hash, observed.field_after_hash);
    }
}
