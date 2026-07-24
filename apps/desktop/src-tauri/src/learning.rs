//! Edit learning pipeline (M6): record edits, suggest candidates, optional auto-promote,
//! optional post-paste capture of user corrections in the target app.

use crate::config::LearningConfig;
use crate::edit_attribution::{EditProjection, InsertionAnchor, TerminalInsertionAnchor};
use crate::pane_observer::LockedPane;
use crate::AppState;
use chrono::{DateTime, Utc};
use lumen_core::{DictEntryKind, DictEntrySource, EditSource};
use lumen_dictionary::{candidates_from_edit, DictionaryEntry, LearnCandidate};
use lumen_platform_macos::{
    focused_text_field_snapshot, FocusedTextFieldSnapshot, FrontmostTarget,
};
use lumen_store::{EditAttribution, EditObservationRecord};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

static EDIT_WATCH_GENERATION: AtomicU64 = AtomicU64::new(1);
const AX_EDIT_OBSERVER_ID: &str = "focused_field_poll_v4";

#[derive(Debug, Clone)]
pub struct PostInsertWatchRequest {
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub inserted_text: String,
    pub target: FrontmostTarget,
    pub pane_target: Option<LockedPane>,
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
    /// pre_insert_ui | post_paste_ax | post_paste_pane | manual
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
    identity: ObservationIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationIdentity {
    observer_id: String,
    edit_source: EditSource,
    target_fingerprint_hash: String,
    field_before_hash: String,
}

#[derive(Debug)]
struct ObservedEdit {
    after_text: String,
    observer_id: String,
    edit_source: EditSource,
    target_fingerprint_hash: String,
    field_before_hash: String,
    field_after_hash: String,
}

#[derive(Debug)]
struct PreparedEditWatch {
    surface: PreparedEditSurface,
    observer_id: String,
    edit_source: EditSource,
    target_fingerprint_hash: String,
    field_before_hash: String,
}

#[derive(Debug)]
struct PreparedObservationMetadata {
    observer_id: String,
    target_fingerprint_hash: String,
    field_initial_hash: String,
}

#[derive(Debug, Clone)]
enum PreparedEditSurface {
    Accessibility(AccessibilityEditSurface),
    Pane {
        target: LockedPane,
        anchor: TerminalInsertionAnchor,
        accessibility_fallback: Option<AccessibilityEditSurface>,
    },
}

#[derive(Debug, Clone)]
struct AccessibilityEditSurface {
    target: FrontmostTarget,
    anchor: InsertionAnchor,
    expected_fingerprint: String,
    identity: ObservationIdentity,
}

#[derive(Debug)]
enum EditObservationOutcome {
    Edited(ObservedEdit),
    NoEdit {
        reason: &'static str,
        field_final_hash: Option<String>,
    },
    Failed {
        reason: &'static str,
        field_final_hash: Option<String>,
    },
}

#[derive(Debug)]
enum PinnedFieldProjection {
    FieldChanged,
    Current {
        projection: EditProjection,
        field_hash: String,
        identity: Option<ObservationIdentity>,
    },
}

#[derive(Debug)]
enum EditWatchDecision {
    Continue,
    Complete(EditObservationOutcome),
}

#[derive(Debug)]
struct EditWatchTracker {
    observer_id: String,
    edit_source: EditSource,
    target_fingerprint_hash: String,
    field_before_hash: String,
    last_field_hash: Option<String>,
    pending: Option<PendingProjection>,
    consecutive_unavailable: u8,
    consecutive_field_mismatches: u8,
    consecutive_anchor_mismatches: u8,
    stable_edit_duration: std::time::Duration,
}

impl EditWatchTracker {
    const MAX_CONSECUTIVE_MISMATCHES: u8 = 8;

    fn new(
        observer_id: String,
        edit_source: EditSource,
        target_fingerprint_hash: String,
        field_before_hash: String,
        stable_edit_duration: std::time::Duration,
    ) -> Self {
        Self {
            observer_id,
            edit_source,
            target_fingerprint_hash,
            last_field_hash: Some(field_before_hash.clone()),
            field_before_hash,
            pending: None,
            consecutive_unavailable: 0,
            consecutive_field_mismatches: 0,
            consecutive_anchor_mismatches: 0,
            stable_edit_duration,
        }
    }

    fn last_field_hash(&self) -> Option<String> {
        self.last_field_hash.clone()
    }

    fn observe_unavailable(&mut self) -> EditWatchDecision {
        self.consecutive_unavailable = self.consecutive_unavailable.saturating_add(1);
        self.consecutive_field_mismatches = 0;
        self.consecutive_anchor_mismatches = 0;
        self.pending = None;
        if self.consecutive_unavailable >= Self::MAX_CONSECUTIVE_MISMATCHES {
            EditWatchDecision::Complete(EditObservationOutcome::Failed {
                reason: "target_field_unavailable",
                field_final_hash: self.last_field_hash(),
            })
        } else {
            EditWatchDecision::Continue
        }
    }

    fn observe(
        &mut self,
        observation: PinnedFieldProjection,
        observed_at: std::time::Instant,
    ) -> EditWatchDecision {
        self.consecutive_unavailable = 0;
        let PinnedFieldProjection::Current {
            projection,
            field_hash,
            identity,
        } = observation
        else {
            self.consecutive_field_mismatches = self.consecutive_field_mismatches.saturating_add(1);
            self.consecutive_anchor_mismatches = 0;
            self.pending = None;
            return if self.consecutive_field_mismatches >= Self::MAX_CONSECUTIVE_MISMATCHES {
                EditWatchDecision::Complete(EditObservationOutcome::Failed {
                    reason: "focused_field_changed",
                    field_final_hash: self.last_field_hash(),
                })
            } else {
                EditWatchDecision::Continue
            };
        };

        self.consecutive_field_mismatches = 0;
        self.last_field_hash = Some(field_hash.clone());
        match projection {
            EditProjection::Unchanged => {
                self.consecutive_anchor_mismatches = 0;
                self.pending = None;
                EditWatchDecision::Continue
            }
            EditProjection::Unrelated => {
                self.consecutive_anchor_mismatches =
                    self.consecutive_anchor_mismatches.saturating_add(1);
                self.pending = None;
                if self.consecutive_anchor_mismatches >= Self::MAX_CONSECUTIVE_MISMATCHES {
                    EditWatchDecision::Complete(EditObservationOutcome::Failed {
                        reason: "anchor_mismatch",
                        field_final_hash: Some(field_hash),
                    })
                } else {
                    EditWatchDecision::Continue
                }
            }
            EditProjection::Edited { after_text } => {
                self.consecutive_anchor_mismatches = 0;
                let identity = identity.unwrap_or_else(|| ObservationIdentity {
                    observer_id: self.observer_id.clone(),
                    edit_source: self.edit_source,
                    target_fingerprint_hash: self.target_fingerprint_hash.clone(),
                    field_before_hash: self.field_before_hash.clone(),
                });
                match self.pending.as_mut() {
                    Some(value) if value.after_text == after_text && value.identity == identity => {
                        value.field_after_hash = field_hash;
                        if observed_at.saturating_duration_since(value.stable_since)
                            >= self.stable_edit_duration
                        {
                            return EditWatchDecision::Complete(EditObservationOutcome::Edited(
                                ObservedEdit {
                                    after_text: value.after_text.clone(),
                                    observer_id: value.identity.observer_id.clone(),
                                    edit_source: value.identity.edit_source,
                                    target_fingerprint_hash: value
                                        .identity
                                        .target_fingerprint_hash
                                        .clone(),
                                    field_before_hash: value.identity.field_before_hash.clone(),
                                    field_after_hash: value.field_after_hash.clone(),
                                },
                            ));
                        }
                    }
                    _ => {
                        self.pending = Some(PendingProjection {
                            after_text,
                            field_after_hash: field_hash,
                            stable_since: observed_at,
                            identity,
                        });
                    }
                }
                EditWatchDecision::Continue
            }
        }
    }

    fn finish(self) -> EditObservationOutcome {
        match self.pending {
            Some(_) => EditObservationOutcome::Failed {
                reason: "edit_not_stable_before_timeout",
                field_final_hash: self.last_field_hash,
            },
            None if self.consecutive_unavailable > 0 => EditObservationOutcome::Failed {
                reason: "target_field_unrecovered_before_timeout",
                field_final_hash: self.last_field_hash,
            },
            None if self.consecutive_anchor_mismatches > 0 => EditObservationOutcome::Failed {
                reason: "anchor_mismatch_before_timeout",
                field_final_hash: self.last_field_hash,
            },
            None if self.consecutive_field_mismatches > 0 => EditObservationOutcome::Failed {
                reason: "focused_field_unrecovered_before_timeout",
                field_final_hash: self.last_field_hash,
            },
            None => EditObservationOutcome::NoEdit {
                reason: "observation_window_elapsed",
                field_final_hash: self.last_field_hash,
            },
        }
    }
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
    let observation_id = Uuid::new_v4();
    let started_at = Utc::now();
    let watch_generation = EDIT_WATCH_GENERATION.load(Ordering::SeqCst);
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
                schedule_failed_observation(
                    app,
                    request,
                    observation_id,
                    started_at,
                    anchor_failure_reason(&reason),
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
                schedule_failed_observation(
                    app,
                    request,
                    observation_id,
                    started_at,
                    "anchor_task_failed".into(),
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
        let metadata = PreparedObservationMetadata {
            observer_id: prepared.observer_id.clone(),
            target_fingerprint_hash: prepared.target_fingerprint_hash.clone(),
            field_initial_hash: prepared.field_before_hash.clone(),
        };
        let outcome = observe_prepared_edit(&request, seconds, prepared, watch_generation).await;
        if !wait_for_session_persistence(&app, request.session_id, seconds).await {
            tracing::warn!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                "edit watch ended but its session was not persisted in time"
            );
            let record = EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: metadata.observer_id,
                status: "failed".into(),
                end_reason: "session_persistence_timeout".into(),
                target_app_name: request.target.name.clone(),
                target_bundle_id: request.target.bundle_id.clone(),
                target_fingerprint_hash: Some(metadata.target_fingerprint_hash),
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: Some(metadata.field_initial_hash),
                field_final_hash: None,
                normalized_edit_distance: None,
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            };
            let _ = app.emit("edit-observation-completed", &record);
            return;
        }
        persist_observation_outcome(
            &app,
            &request,
            observation_id,
            started_at,
            metadata,
            outcome,
        );
    });
    true
}

/// Preserve a terminal audit record when insertion succeeded but the target
/// identity needed for field pinning was unavailable.
pub fn schedule_unavailable_post_insert_observation(
    app: AppHandle,
    session_id: Uuid,
    attempt_id: Uuid,
    inserted_text: String,
    reason: &'static str,
) {
    schedule_failed_observation(
        app,
        PostInsertWatchRequest {
            session_id,
            attempt_id,
            inserted_text,
            target: FrontmostTarget {
                name: None,
                bundle_id: None,
                process_id: None,
            },
            pane_target: None,
        },
        Uuid::new_v4(),
        Utc::now(),
        reason.into(),
    );
}

/// Starting another dictation gives every previous observer an explicit terminal
/// reason instead of letting it silently time out against a changed field.
pub fn cancel_post_insert_watches() {
    EDIT_WATCH_GENERATION.fetch_add(1, Ordering::SeqCst);
}

fn anchor_failure_reason(reason: &str) -> String {
    match reason {
        "inserted_text_not_found_in_field" | "inserted_text_not_found_in_pane" => {
            "inserted_text_not_found"
        }
        "inserted_text_not_unique_in_field" | "inserted_text_not_unique_in_pane" => {
            "inserted_text_not_unique"
        }
        "pinned_target_field_unavailable" => "target_field_unavailable",
        _ => "anchor_unavailable",
    }
    .into()
}

fn prepare_edit_watch(request: &PostInsertWatchRequest) -> Result<PreparedEditWatch, String> {
    let mut pane_failure = None;
    if let Some(pane) = request.pane_target.as_ref() {
        match pane.snapshot() {
            Ok(snapshot) => {
                let value = normalize_pane_text(&snapshot.text);
                match TerminalInsertionAnchor::from_snapshot(&value, &request.inserted_text) {
                    Ok(anchor) => {
                        let accessibility_fallback = prepare_accessibility_surface(request).ok();
                        return Ok(PreparedEditWatch {
                            surface: PreparedEditSurface::Pane {
                                target: pane.clone(),
                                anchor,
                                accessibility_fallback,
                            },
                            observer_id: pane.observer_id().into(),
                            edit_source: EditSource::PostPastePane,
                            target_fingerprint_hash: text_hash(&pane.fingerprint_material()),
                            field_before_hash: text_hash(&value),
                        });
                    }
                    Err(reason) => {
                        tracing::debug!(
                            observer = pane.observer_id(),
                            %reason,
                            "pane snapshot could not anchor inserted span; trying accessibility"
                        );
                        pane_failure = Some(reason);
                    }
                }
            }
            Err(reason) => {
                tracing::debug!(
                    observer = pane.observer_id(),
                    %reason,
                    "pane snapshot unavailable after insertion; trying accessibility"
                );
                pane_failure = Some("pane_snapshot_unavailable".to_owned());
            }
        }
    }
    let accessibility =
        prepare_accessibility_surface(request).map_err(|reason| pane_failure.unwrap_or(reason))?;
    Ok(PreparedEditWatch {
        observer_id: accessibility.identity.observer_id.clone(),
        edit_source: accessibility.identity.edit_source,
        target_fingerprint_hash: accessibility.identity.target_fingerprint_hash.clone(),
        field_before_hash: accessibility.identity.field_before_hash.clone(),
        surface: PreparedEditSurface::Accessibility(accessibility),
    })
}

fn prepare_accessibility_surface(
    request: &PostInsertWatchRequest,
) -> Result<AccessibilityEditSurface, String> {
    let initial = read_pinned_field(&request.target)
        .ok_or_else(|| "pinned_target_field_unavailable".to_owned())?;
    let anchor = InsertionAnchor::from_post_insert(&initial.value, &request.inserted_text)?;
    let expected_fingerprint = field_fingerprint(&request.target, &initial);
    Ok(AccessibilityEditSurface {
        target: request.target.clone(),
        anchor,
        expected_fingerprint: expected_fingerprint.clone(),
        identity: ObservationIdentity {
            observer_id: AX_EDIT_OBSERVER_ID.into(),
            edit_source: EditSource::PostPasteAx,
            target_fingerprint_hash: expected_fingerprint,
            field_before_hash: text_hash(&initial.value),
        },
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
    match observe_prepared_edit(
        request,
        remaining.as_secs().max(1),
        prepared,
        EDIT_WATCH_GENERATION.load(Ordering::SeqCst),
    )
    .await
    {
        EditObservationOutcome::Edited(observed) => Some(observed),
        EditObservationOutcome::NoEdit { .. } | EditObservationOutcome::Failed { .. } => None,
    }
}

async fn observe_prepared_edit(
    _request: &PostInsertWatchRequest,
    seconds: u64,
    prepared: PreparedEditWatch,
    watch_generation: u64,
) -> EditObservationOutcome {
    const STABLE_EDIT_DURATION: std::time::Duration = std::time::Duration::from_millis(1_200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let PreparedEditWatch {
        surface,
        observer_id,
        edit_source,
        target_fingerprint_hash,
        field_before_hash,
    } = prepared;
    let mut tracker = EditWatchTracker::new(
        observer_id,
        edit_source,
        target_fingerprint_hash,
        field_before_hash,
        STABLE_EDIT_DURATION,
    );
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if EDIT_WATCH_GENERATION.load(Ordering::SeqCst) != watch_generation {
            return EditObservationOutcome::Failed {
                reason: "next_dictation_started",
                field_final_hash: tracker.last_field_hash(),
            };
        }
        let surface = surface.clone();
        let observation = tokio::task::spawn_blocking(move || read_surface_projection(&surface))
            .await
            .ok()
            .flatten();
        let Some(observation) = observation else {
            match tracker.observe_unavailable() {
                EditWatchDecision::Continue => continue,
                EditWatchDecision::Complete(outcome) => return outcome,
            }
        };
        match tracker.observe(observation, std::time::Instant::now()) {
            EditWatchDecision::Continue => {}
            EditWatchDecision::Complete(outcome) => return outcome,
        }
    }
    tracker.finish()
}

fn read_surface_projection(surface: &PreparedEditSurface) -> Option<PinnedFieldProjection> {
    match surface {
        PreparedEditSurface::Accessibility(accessibility) => {
            read_accessibility_projection(accessibility, None)
        }
        PreparedEditSurface::Pane {
            target,
            anchor,
            accessibility_fallback,
        } => {
            let Some(snapshot) = target.snapshot().ok() else {
                return accessibility_fallback.as_ref().and_then(|accessibility| {
                    read_accessibility_projection(
                        accessibility,
                        Some(accessibility.identity.clone()),
                    )
                });
            };
            let value = normalize_pane_text(&snapshot.text);
            let projection = anchor.project(&value);
            if projection == EditProjection::Unrelated {
                if let Some(fallback) = accessibility_fallback.as_ref().and_then(|accessibility| {
                    read_accessibility_projection(
                        accessibility,
                        Some(accessibility.identity.clone()),
                    )
                }) {
                    return Some(fallback);
                }
            }
            Some(PinnedFieldProjection::Current {
                projection,
                field_hash: text_hash(&value),
                identity: None,
            })
        }
    }
}

fn read_accessibility_projection(
    accessibility: &AccessibilityEditSurface,
    identity: Option<ObservationIdentity>,
) -> Option<PinnedFieldProjection> {
    let current = read_pinned_field(&accessibility.target)?;
    if field_fingerprint(&accessibility.target, &current) != accessibility.expected_fingerprint {
        return Some(PinnedFieldProjection::FieldChanged);
    }
    Some(PinnedFieldProjection::Current {
        projection: project_for_target(
            &accessibility.target,
            &accessibility.anchor,
            &current.value,
        ),
        field_hash: text_hash(&current.value),
        identity,
    })
}

fn normalize_pane_text(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
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
        process_id: None,
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

fn schedule_failed_observation(
    app: AppHandle,
    request: PostInsertWatchRequest,
    observation_id: Uuid,
    started_at: DateTime<Utc>,
    reason: String,
) {
    let observer_id = request
        .pane_target
        .as_ref()
        .map(|pane| pane.observer_id())
        .unwrap_or(AX_EDIT_OBSERVER_ID)
        .to_owned();
    tauri::async_runtime::spawn(async move {
        if !wait_for_session_persistence(&app, request.session_id, 10).await {
            tracing::warn!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                %reason,
                "failed edit observation could not be attached to its session"
            );
            let record = EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: observer_id.clone(),
                status: "failed".into(),
                end_reason: "session_persistence_timeout".into(),
                target_app_name: request.target.name,
                target_bundle_id: request.target.bundle_id,
                target_fingerprint_hash: None,
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: None,
                field_final_hash: None,
                normalized_edit_distance: None,
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            };
            let _ = app.emit("edit-observation-completed", &record);
            return;
        }
        save_observation(
            &app,
            EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: observer_id,
                status: "failed".into(),
                end_reason: reason,
                target_app_name: request.target.name,
                target_bundle_id: request.target.bundle_id,
                target_fingerprint_hash: None,
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: None,
                field_final_hash: None,
                normalized_edit_distance: None,
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            },
        );
    });
}

fn persist_observation_outcome(
    app: &AppHandle,
    request: &PostInsertWatchRequest,
    observation_id: Uuid,
    started_at: DateTime<Utc>,
    metadata: PreparedObservationMetadata,
    outcome: EditObservationOutcome,
) {
    match outcome {
        EditObservationOutcome::Edited(observed) => {
            let observation = EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: observed.observer_id.clone(),
                status: "completed_with_edit".into(),
                end_reason: "stable_edit_captured".into(),
                target_app_name: request.target.name.clone(),
                target_bundle_id: request.target.bundle_id.clone(),
                target_fingerprint_hash: Some(observed.target_fingerprint_hash.clone()),
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: Some(observed.field_before_hash.clone()),
                field_final_hash: Some(observed.field_after_hash.clone()),
                normalized_edit_distance: Some(normalized_edit_distance(
                    &request.inserted_text,
                    &observed.after_text,
                )),
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            };
            persist_and_emit_edit(app, request, &observed, observation);
        }
        EditObservationOutcome::NoEdit {
            reason,
            field_final_hash,
        } => save_observation(
            app,
            EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: metadata.observer_id.clone(),
                status: "completed_no_edit".into(),
                end_reason: reason.into(),
                target_app_name: request.target.name.clone(),
                target_bundle_id: request.target.bundle_id.clone(),
                target_fingerprint_hash: Some(metadata.target_fingerprint_hash.clone()),
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: Some(metadata.field_initial_hash.clone()),
                field_final_hash,
                normalized_edit_distance: Some(0.0),
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            },
        ),
        EditObservationOutcome::Failed {
            reason,
            field_final_hash,
        } => save_observation(
            app,
            EditObservationRecord {
                id: observation_id,
                session_id: request.session_id,
                attempt_id: request.attempt_id,
                source: metadata.observer_id,
                status: "failed".into(),
                end_reason: reason.into(),
                target_app_name: request.target.name.clone(),
                target_bundle_id: request.target.bundle_id.clone(),
                target_fingerprint_hash: Some(metadata.target_fingerprint_hash),
                inserted_text_hash: text_hash(&request.inserted_text),
                field_initial_hash: Some(metadata.field_initial_hash),
                field_final_hash,
                normalized_edit_distance: None,
                started_at,
                completed_at: Utc::now(),
                edit_event_id: None,
            },
        ),
    }
}

fn save_observation(app: &AppHandle, record: EditObservationRecord) {
    let state = app.state::<AppState>();
    let result = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())
        .and_then(|guard| {
            guard
                .as_ref()
                .ok_or_else(|| "database unavailable".to_owned())
                .and_then(|store| {
                    store
                        .save_edit_observation(&record)
                        .map_err(|error| error.to_string())
                })
        });
    match result {
        Ok(()) => {
            let _ = app.emit("edit-observation-completed", &record);
        }
        Err(error) => {
            tracing::warn!(
                session_id = %record.session_id,
                attempt_id = %record.attempt_id,
                status = %record.status,
                reason = %record.end_reason,
                %error,
                "failed to persist edit observation terminal state"
            );
        }
    }
}

fn normalized_edit_distance(before: &str, after: &str) -> f64 {
    let before: Vec<char> = before.chars().collect();
    let after: Vec<char> = after.chars().collect();
    let denominator = before.len().max(after.len());
    if denominator == 0 {
        return 0.0;
    }
    let mut previous: Vec<usize> = (0..=after.len()).collect();
    let mut current = vec![0; after.len() + 1];
    for (row, left) in before.iter().enumerate() {
        current[0] = row + 1;
        for (column, right) in after.iter().enumerate() {
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + usize::from(left != right));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[after.len()] as f64 / denominator as f64
}

fn edit_source_name(source: EditSource) -> &'static str {
    match source {
        EditSource::PreInsertUi => "pre_insert_ui",
        EditSource::PostPasteAx => "post_paste_ax",
        EditSource::PostPastePane => "post_paste_pane",
        EditSource::Manual => "manual",
    }
}

fn persist_and_emit_edit(
    app: &AppHandle,
    request: &PostInsertWatchRequest,
    observed: &ObservedEdit,
    observation: EditObservationRecord,
) {
    let attribution = EditAttribution {
        attempt_id: Some(request.attempt_id),
        target_app_name: request.target.name.clone(),
        target_bundle_id: request.target.bundle_id.clone(),
        observer: Some(observed.observer_id.clone()),
        target_fingerprint_hash: Some(observed.target_fingerprint_hash.clone()),
        field_before_hash: Some(observed.field_before_hash.clone()),
        field_after_hash: Some(observed.field_after_hash.clone()),
        status: "confirmed_same_field_span".into(),
        ..EditAttribution::default()
    };
    let state = app.state::<AppState>();
    let persisted = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())
        .and_then(|guard| {
            guard
                .as_ref()
                .ok_or_else(|| "database unavailable".to_owned())
                .and_then(|store| {
                    store
                        .add_edit_event_with_observation(
                            request.session_id,
                            observed.edit_source,
                            &request.inserted_text,
                            &observed.after_text,
                            &attribution,
                            &observation,
                        )
                        .map_err(|error| error.to_string())
                })
        });
    match persisted {
        Ok(edit_event_id) => {
            let mut linked_observation = observation;
            linked_observation.edit_event_id = Some(edit_event_id);
            let _ = app.emit("edit-observation-completed", &linked_observation);
            let learning_result = process_edit_from_state(
                &state,
                ProcessEditInput {
                    before_text: request.inserted_text.clone(),
                    after_text: observed.after_text.clone(),
                    session_id: Some(request.session_id.to_string()),
                    source: Some(edit_source_name(observed.edit_source).into()),
                    record_event: Some(false),
                },
                None,
            );
            let (candidates, auto_promoted, message) = match learning_result {
                Ok(result) => (result.candidates, result.auto_promoted, result.message),
                Err(error) => {
                    tracing::warn!(
                        session_id = %request.session_id,
                        attempt_id = %request.attempt_id,
                        %error,
                        "edit persisted but dictionary learning failed"
                    );
                    (
                        Vec::new(),
                        Vec::new(),
                        "edit captured; dictionary learning failed".into(),
                    )
                }
            };
            let _ = app.emit(
                "edit-feedback-captured",
                EditFeedbackEvent {
                    edit_event_id: Some(edit_event_id.to_string()),
                    session_id: request.session_id.to_string(),
                    before_text: request.inserted_text.clone(),
                    after_text: observed.after_text.clone(),
                    source: edit_source_name(observed.edit_source).into(),
                    candidates,
                    auto_promoted,
                    message,
                },
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %request.session_id,
                attempt_id = %request.attempt_id,
                error = %error,
                "failed to persist attributed edit feedback"
            );
            let mut failed_observation = observation;
            failed_observation.status = "failed".into();
            failed_observation.end_reason = "edit_event_persistence_failed".into();
            failed_observation.normalized_edit_distance = None;
            failed_observation.edit_event_id = None;
            save_observation(app, failed_observation);
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
        Some("post_paste_pane") => EditSource::PostPastePane,
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

    #[test]
    fn normalized_distance_has_clear_zero_and_full_scale_meaning() {
        assert_eq!(normalized_edit_distance("Codex", "Codex"), 0.0);
        assert_eq!(normalized_edit_distance("", ""), 0.0);
        assert_eq!(normalized_edit_distance("abc", "xyz"), 1.0);
        assert!((normalized_edit_distance("Cortex", "Codex") - (2.0 / 6.0)).abs() < 1e-9);
    }

    #[test]
    fn starting_a_new_dictation_advances_the_observer_generation() {
        let before = EDIT_WATCH_GENERATION.load(Ordering::SeqCst);
        cancel_post_insert_watches();
        let after = EDIT_WATCH_GENERATION.load(Ordering::SeqCst);

        assert!(after > before);
    }

    #[test]
    fn pinned_pane_surface_is_preferred_when_accessibility_has_no_field() {
        let request = PostInsertWatchRequest {
            session_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            inserted_text: "HERDR".into(),
            target: FrontmostTarget {
                name: Some("Ghostty".into()),
                bundle_id: Some("com.mitchellh.ghostty".into()),
                process_id: Some(659),
            },
            pane_target: Some(LockedPane::test_snapshot(
                "herdr_pane_v1",
                "herdr:w7:p2",
                "header\nproject $ HERDR\nfooter",
            )),
        };

        let prepared = prepare_edit_watch(&request).unwrap();

        assert_eq!(prepared.observer_id, "herdr_pane_v1");
        assert_eq!(prepared.edit_source, EditSource::PostPastePane);
        assert!(matches!(prepared.surface, PreparedEditSurface::Pane { .. }));
    }

    #[test]
    fn captured_edit_keeps_the_provider_observer_identity() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            "tmux_pane_v1".into(),
            EditSource::PostPastePane,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );

        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "tmux".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now,
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "tmux".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(120),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                observer_id,
                edit_source: EditSource::PostPastePane,
                ..
            })) if observer_id == "tmux_pane_v1"
        ));
    }

    #[test]
    fn pane_loss_can_complete_with_the_accessibility_fallback_identity() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            "tmux_pane_v1".into(),
            EditSource::PostPastePane,
            "pane-target".into(),
            "pane-before".into(),
            std::time::Duration::from_millis(100),
        );
        let fallback = ObservationIdentity {
            observer_id: AX_EDIT_OBSERVER_ID.into(),
            edit_source: EditSource::PostPasteAx,
            target_fingerprint_hash: "ax-target".into(),
            field_before_hash: "ax-before".into(),
        };

        for observed_at in [now, now + std::time::Duration::from_millis(120)] {
            let decision = tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "ax-after".into(),
                    identity: Some(fallback.clone()),
                },
                observed_at,
            );
            if observed_at == now {
                assert!(matches!(decision, EditWatchDecision::Continue));
            } else {
                assert!(matches!(
                    decision,
                    EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                        observer_id,
                        edit_source: EditSource::PostPasteAx,
                        target_fingerprint_hash,
                        field_before_hash,
                        ..
                    })) if observer_id == AX_EDIT_OBSERVER_ID
                        && target_fingerprint_hash == "ax-target"
                        && field_before_hash == "ax-before"
                ));
            }
        }
    }

    #[tokio::test]
    async fn a_running_observer_reports_next_dictation_cancellation() {
        let inserted = "Codex";
        let request = PostInsertWatchRequest {
            session_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            inserted_text: inserted.into(),
            target: FrontmostTarget {
                name: Some("TextEdit".into()),
                bundle_id: Some("com.apple.TextEdit".into()),
                process_id: None,
            },
            pane_target: None,
        };
        let anchor = InsertionAnchor::from_post_insert("prefix Codex suffix", inserted).unwrap();
        let prepared = PreparedEditWatch {
            surface: PreparedEditSurface::Accessibility(AccessibilityEditSurface {
                target: request.target.clone(),
                anchor,
                expected_fingerprint: "target".into(),
                identity: ObservationIdentity {
                    observer_id: AX_EDIT_OBSERVER_ID.into(),
                    edit_source: EditSource::PostPasteAx,
                    target_fingerprint_hash: "target".into(),
                    field_before_hash: "field".into(),
                },
            }),
            observer_id: AX_EDIT_OBSERVER_ID.into(),
            edit_source: EditSource::PostPasteAx,
            target_fingerprint_hash: "target".into(),
            field_before_hash: "field".into(),
        };
        let generation = EDIT_WATCH_GENERATION.load(Ordering::SeqCst);
        cancel_post_insert_watches();

        let outcome = observe_prepared_edit(&request, 1, prepared, generation).await;

        assert!(matches!(
            outcome,
            EditObservationOutcome::Failed {
                reason: "next_dictation_started",
                ..
            }
        ));
    }

    #[test]
    fn transient_field_mismatch_does_not_terminate_the_observation() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            AX_EDIT_OBSERVER_ID.into(),
            EditSource::PostPasteAx,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );

        assert!(matches!(
            tracker.observe(PinnedFieldProjection::FieldChanged, now),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(10),
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(150),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                after_text,
                ..
            })) if after_text == "Codex"
        ));
    }

    #[test]
    fn persistent_field_mismatch_has_an_explicit_terminal_reason() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            AX_EDIT_OBSERVER_ID.into(),
            EditSource::PostPasteAx,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );
        for offset in 0..(EditWatchTracker::MAX_CONSECUTIVE_MISMATCHES - 1) {
            assert!(matches!(
                tracker.observe(
                    PinnedFieldProjection::FieldChanged,
                    now + std::time::Duration::from_millis(u64::from(offset) * 10),
                ),
                EditWatchDecision::Continue
            ));
        }
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::FieldChanged,
                now + std::time::Duration::from_millis(
                    u64::from(EditWatchTracker::MAX_CONSECUTIVE_MISMATCHES) * 10,
                ),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Failed {
                reason: "focused_field_changed",
                ..
            })
        ));
    }

    #[test]
    fn transient_unavailability_preserves_the_original_text_anchor() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            AX_EDIT_OBSERVER_ID.into(),
            EditSource::PostPasteAx,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );

        assert!(matches!(
            tracker.observe_unavailable(),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Qdrant".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now,
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Qdrant".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(120),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                after_text,
                ..
            })) if after_text == "Qdrant"
        ));
    }

    #[test]
    fn transient_anchor_mismatch_can_recover_to_the_original_field() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            AX_EDIT_OBSERVER_ID.into(),
            EditSource::PostPasteAx,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );

        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Unrelated,
                    field_hash: "other-field".into(),
                    identity: None,
                },
                now,
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Railway".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(10),
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Railway".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(120),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                after_text,
                ..
            })) if after_text == "Railway"
        ));
    }

    #[test]
    fn elapsed_time_while_unavailable_does_not_confirm_a_single_edit_sample() {
        let now = std::time::Instant::now();
        let mut tracker = EditWatchTracker::new(
            AX_EDIT_OBSERVER_ID.into(),
            EditSource::PostPasteAx,
            "target".into(),
            "field-before".into(),
            std::time::Duration::from_millis(100),
        );

        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now,
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe_unavailable(),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_secs(1),
            ),
            EditWatchDecision::Continue
        ));
        assert!(matches!(
            tracker.observe(
                PinnedFieldProjection::Current {
                    projection: EditProjection::Edited {
                        after_text: "Codex".into(),
                    },
                    field_hash: "field-after".into(),
                    identity: None,
                },
                now + std::time::Duration::from_millis(1_120),
            ),
            EditWatchDecision::Complete(EditObservationOutcome::Edited(ObservedEdit {
                after_text,
                ..
            })) if after_text == "Codex"
        ));
    }

    #[test]
    fn anchor_failures_keep_actionable_terminal_reasons() {
        assert_eq!(
            anchor_failure_reason("inserted_text_not_found_in_field"),
            "inserted_text_not_found"
        );
        assert_eq!(
            anchor_failure_reason("inserted_text_not_unique_in_field"),
            "inserted_text_not_unique"
        );
        assert_eq!(
            anchor_failure_reason("pinned_target_field_unavailable"),
            "target_field_unavailable"
        );
    }

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
                process_id: None,
            },
            pane_target: None,
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
    async fn live_textedit_edit_survives_geometry_and_focus_churn() {
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
            process_id: None,
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
            pane_target: None,
        };
        let prepared = prepare_edit_watch(&request).expect("anchor the live TextEdit field");
        let watch_generation = EDIT_WATCH_GENERATION.load(Ordering::SeqCst);
        let replacement = actual_field
            .value
            .replacen(&request.inserted_text, &edited, 1);
        let editor = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let churn_status = Command::new("osascript")
                .args([
                    "-e",
                    "tell application \"System Events\"",
                    "-e",
                    "tell process \"TextEdit\"",
                    "-e",
                    "set position of front window to {180, 140}",
                    "-e",
                    "set size of front window to {720, 520}",
                    "-e",
                    "end tell",
                    "-e",
                    "end tell",
                    "-e",
                    "tell application \"Finder\" to activate",
                    "-e",
                    "delay 0.7",
                    "-e",
                    "tell application \"TextEdit\" to activate",
                ])
                .status()
                .expect("move TextEdit and temporarily change focus");
            assert!(churn_status.success());
            tokio::time::sleep(Duration::from_millis(400)).await;
            let script = format!(
                "tell application \"TextEdit\" to set text of front document to \"{}\"",
                replacement.replace('\\', "\\\\").replace('"', "\\\"")
            );
            Command::new("osascript")
                .args(["-e", &script])
                .status()
                .unwrap()
        };

        let (outcome, edit_status) = tokio::join!(
            observe_prepared_edit(&request, 6, prepared, watch_generation),
            editor
        );
        let _ = Command::new("osascript")
            .args([
                "-e",
                "tell application \"TextEdit\" to close front document saving no",
            ])
            .status();

        assert!(edit_status.success());
        let EditObservationOutcome::Edited(observed) = outcome else {
            panic!(
                "same-field edit was not observed; outcome={outcome:?}; \
                 initial_fingerprint={}; initial_value_hash={}; initial_value_len={}",
                field_fingerprint(&request.target, &actual_field),
                text_hash(&actual_field.value),
                actual_field.value.len(),
            );
        };
        assert_eq!(observed.after_text, edited);
        assert!(!observed.target_fingerprint_hash.is_empty());
        assert_ne!(observed.field_before_hash, observed.field_after_hash);
    }
}
