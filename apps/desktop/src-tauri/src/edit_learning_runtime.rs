use crate::config::InjectConfig;
use crate::pane_observer::LockedPane;
use crate::AppState;
use async_trait::async_trait;
use chrono::Utc;
use lumen_edit_learning::{
    EditLearningEngine, EditLearningRepository, EditRevisionRecord, EditSessionRecord,
    EngineConfig, EngineError, FeedbackNotice, FeedbackSink, InsertionExecutor,
    LearningProposalRecord, ObservabilitySnapshot, ObservedInsertion, RepositoryError,
    SurfaceDescriptor, SurfaceError, SurfaceErrorKind, SurfaceReservation, SurfaceSnapshot,
    TargetHint,
};
use lumen_inject::InsertOutcome;
use lumen_platform_macos::{FrontmostTarget, MacAccessibilitySurfaceAdapter};
use lumen_store::Store;
use std::sync::{Arc, Mutex, RwLock};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

const ACKNOWLEDGED_FEEDBACK_RETENTION_DAYS: i64 = 30;

pub(crate) struct DesktopEditLearning {
    engine: Arc<EditLearningEngine>,
    store: Arc<Mutex<Option<Store>>>,
    pending: Mutex<Option<Arc<dyn SurfaceReservation>>>,
    feedback: Arc<TauriFeedbackSink>,
}

#[derive(Debug)]
pub(crate) struct DesktopObservedInsertionOutcome {
    pub insertion: InsertOutcome,
    pub observation_started: bool,
}

impl DesktopEditLearning {
    pub fn new(store: Arc<Mutex<Option<Store>>>, persist_evidence_text: bool) -> Self {
        if let Ok(guard) = store.lock() {
            if let Some(database) = guard.as_ref() {
                match database.finalize_incomplete_edit_learning_sessions("application_restarted") {
                    Ok(count) if count > 0 => tracing::info!(
                        finalized_sessions = count,
                        "finalized edit-learning sessions left active by previous process"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        error = %error,
                        "could not finalize stale edit-learning sessions"
                    ),
                }
                if !persist_evidence_text {
                    match database.redact_edit_learning_evidence_text() {
                        Ok((sessions, revisions)) if sessions + revisions > 0 => tracing::info!(
                            session_records = sessions,
                            revision_records = revisions,
                            "redacted stored edit-learning plaintext evidence at startup"
                        ),
                        Ok(_) => {}
                        Err(error) => tracing::warn!(
                            error = %error,
                            "could not redact stored edit-learning plaintext evidence"
                        ),
                    }
                }
                let feedback_cutoff =
                    Utc::now() - chrono::Duration::days(ACKNOWLEDGED_FEEDBACK_RETENTION_DAYS);
                match database.purge_acknowledged_edit_learning_feedback_before(feedback_cutoff) {
                    Ok(count) if count > 0 => tracing::info!(
                        purged_feedback_records = count,
                        retention_days = ACKNOWLEDGED_FEEDBACK_RETENTION_DAYS,
                        "purged acknowledged edit-learning feedback"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        error = %error,
                        retention_days = ACKNOWLEDGED_FEEDBACK_RETENTION_DAYS,
                        "could not purge acknowledged edit-learning feedback"
                    ),
                }
            }
        }
        let repository = Arc::new(DesktopEditLearningRepository {
            store: store.clone(),
        });
        let feedback = Arc::new(TauriFeedbackSink::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository,
            feedback.clone(),
            EngineConfig {
                persist_evidence_text,
                ..EngineConfig::default()
            },
        ));
        Self {
            engine,
            store,
            pending: Mutex::new(None),
            feedback,
        }
    }

    pub fn attach_app_handle(&self, app: AppHandle) {
        self.feedback.attach(app);
    }

    pub fn reserve_target(&self, target: Option<&FrontmostTarget>) -> bool {
        let Some(target) = target else {
            self.clear_pending("target_metadata_unavailable");
            return false;
        };
        let hint = target_hint(Some(target));
        match self.engine.reserve(&MacAccessibilitySurfaceAdapter, &hint) {
            Ok(reservation) => match self.pending.lock() {
                Ok(mut pending) => {
                    *pending = Some(reservation);
                    true
                }
                Err(_) => {
                    tracing::error!("edit-learning pending-reservation lock poisoned");
                    false
                }
            },
            Err(error) => {
                tracing::warn!(
                    target_bundle_id = ?hint.bundle_id,
                    error_kind = ?error.kind,
                    error_code = %error.code,
                    "edit-learning native target reservation failed"
                );
                self.clear_pending("native_surface_reservation_failed");
                false
            }
        }
    }

    pub fn clear_pending(&self, reason: &'static str) {
        match self.pending.lock() {
            Ok(mut pending) => {
                let had_pending = pending.take().is_some();
                tracing::debug!(had_pending, reason, "cleared pending edit-learning target");
            }
            Err(_) => tracing::error!(reason, "edit-learning pending-reservation lock poisoned"),
        }
    }

    fn take_pending(&self) -> Option<Arc<dyn SurfaceReservation>> {
        match self.pending.lock() {
            Ok(mut pending) => pending.take(),
            Err(_) => {
                tracing::error!("edit-learning pending-reservation lock poisoned");
                None
            }
        }
    }

    fn reserve_current_target(
        &self,
        target: Option<&FrontmostTarget>,
        reason: &'static str,
    ) -> Option<Arc<dyn SurfaceReservation>> {
        let hint = target_hint(target);
        hint.process_id?;
        match self.engine.reserve(&MacAccessibilitySurfaceAdapter, &hint) {
            Ok(reservation) => {
                tracing::info!(
                    target_bundle_id = ?hint.bundle_id,
                    reason,
                    "edit-learning target reacquired"
                );
                Some(reservation)
            }
            Err(error) => {
                tracing::warn!(
                    target_bundle_id = ?hint.bundle_id,
                    reason,
                    error_kind = ?error.kind,
                    error_code = %error.code,
                    "edit-learning target reacquisition failed"
                );
                None
            }
        }
    }

    pub async fn insert(
        &self,
        config: &InjectConfig,
        dictation_session_id: Uuid,
        attempt_id: Uuid,
        text: &str,
        target: Option<&FrontmostTarget>,
        pane: Option<LockedPane>,
    ) -> Result<DesktopObservedInsertionOutcome, String> {
        let executor = Arc::new(DesktopInsertionExecutor {
            config: config.clone(),
            outcome: Mutex::new(None),
        });
        let pending = self.take_pending();
        let reservation = pane
            .map(|pane| {
                Arc::new(PaneSurfaceReservation::new(pane, target)) as Arc<dyn SurfaceReservation>
            })
            .or(pending)
            .or_else(|| self.reserve_current_target(target, "insert_time_reacquire"));

        let Some(reservation) = reservation else {
            tracing::warn!(
                dictation_session_id = %dictation_session_id,
                attempt_id = %attempt_id,
                target_bundle_id = ?target.and_then(|value| value.bundle_id.as_deref()),
                "edit-learning target unavailable; inserting without observation"
            );
            let insertion = crate::inject_cmd::insert_with_config(config, text).await?;
            if is_observable_insertion(insertion.strategy) {
                self.engine.report_observation_failure(
                    ObservedInsertion {
                        dictation_session_id,
                        attempt_id,
                        text: text.to_owned(),
                    },
                    &target_hint(target),
                    "surface_reservation_unavailable",
                );
            }
            return Ok(DesktopObservedInsertionOutcome {
                insertion,
                observation_started: false,
            });
        };

        let result = self
            .engine
            .insert(
                reservation,
                executor.clone(),
                ObservedInsertion {
                    dictation_session_id,
                    attempt_id,
                    text: text.to_owned(),
                },
            )
            .await;
        let insertion = executor
            .outcome
            .lock()
            .map_err(|_| "edit-learning insertion outcome lock poisoned".to_owned())?
            .clone();
        match result {
            Ok(receipt) => {
                let insertion = insertion.ok_or_else(|| {
                    "edit-learning engine completed without insertion outcome".to_owned()
                })?;
                tracing::info!(
                    edit_session_id = %receipt.edit_session_id,
                    dictation_session_id = %dictation_session_id,
                    attempt_id = %attempt_id,
                    surface_key_hash = %receipt.surface_key_hash,
                    insertion_strategy = %receipt.insertion_strategy,
                    "observed insertion registered"
                );
                Ok(DesktopObservedInsertionOutcome {
                    insertion,
                    observation_started: true,
                })
            }
            Err(error) => {
                if let Some(insertion) = insertion {
                    if is_observable_insertion(insertion.strategy) {
                        self.engine.report_observation_failure(
                            ObservedInsertion {
                                dictation_session_id,
                                attempt_id,
                                text: text.to_owned(),
                            },
                            &target_hint(target),
                            observation_failure_reason(&error),
                        );
                    }
                    tracing::warn!(
                        dictation_session_id = %dictation_session_id,
                        attempt_id = %attempt_id,
                        error = %error,
                        "text inserted but edit-learning observation could not start"
                    );
                    Ok(DesktopObservedInsertionOutcome {
                        insertion,
                        observation_started: false,
                    })
                } else if matches!(error, EngineError::Prepare(_)) {
                    crate::inject_cmd::copy_only(text).await?;
                    tracing::warn!(
                        dictation_session_id = %dictation_session_id,
                        attempt_id = %attempt_id,
                        target_bundle_id = ?target.and_then(|value| value.bundle_id.as_deref()),
                        "reserved insertion target changed; copied output instead of risking wrong-field insertion"
                    );
                    Ok(DesktopObservedInsertionOutcome {
                        insertion: InsertOutcome {
                            strategy: lumen_core::InsertStrategy::CopyOnly,
                            restored_clipboard: false,
                        },
                        observation_started: false,
                    })
                } else {
                    Err(error.to_string())
                }
            }
        }
    }

    pub fn observability(&self) -> ObservabilitySnapshot {
        self.engine.observability()
    }

    pub fn set_persist_evidence_text(&self, enabled: bool) -> Result<(), String> {
        self.engine.set_persist_evidence_text(enabled);
        if enabled {
            return Ok(());
        }
        // Disabling retention must redact existing plaintext; a failure here
        // means sensitive evidence is still stored, so surface it to the
        // caller instead of reporting success.
        let guard = self
            .store
            .lock()
            .map_err(|_| "store lock poisoned while redacting edit-learning evidence".to_owned())?;
        let Some(store) = guard.as_ref() else {
            return Ok(());
        };
        let (sessions, revisions) = store
            .redact_edit_learning_evidence_text()
            .map_err(|error| format!("edit-learning evidence redaction failed: {error}"))?;
        tracing::info!(
            session_records = sessions,
            revision_records = revisions,
            "redacted stored edit-learning plaintext evidence"
        );
        Ok(())
    }
}

fn observation_failure_reason(error: &EngineError) -> &'static str {
    match error {
        EngineError::Snapshot(_) => "post_insert_snapshot_failed",
        EngineError::InsertedTextNotFound => "post_insert_anchor_not_found",
        EngineError::Prepare(_) => "reserved_target_changed",
        EngineError::SurfaceTransitionTimeout => "surface_transition_timeout",
        EngineError::Insert(_) | EngineError::Repository(_) => {
            "post_insert_observation_start_failed"
        }
    }
}

struct DesktopInsertionExecutor {
    config: InjectConfig,
    outcome: Mutex<Option<InsertOutcome>>,
}

#[async_trait]
impl InsertionExecutor for DesktopInsertionExecutor {
    async fn insert(&self, text: &str) -> Result<lumen_edit_learning::InsertionOutcome, String> {
        let outcome = crate::inject_cmd::insert_with_config(&self.config, text).await?;
        let strategy = strategy_name(outcome.strategy).to_owned();
        *self
            .outcome
            .lock()
            .map_err(|_| "edit-learning insertion outcome lock poisoned".to_owned())? =
            Some(outcome);
        Ok(lumen_edit_learning::InsertionOutcome { strategy })
    }
}

struct PaneSurfaceReservation {
    descriptor: SurfaceDescriptor,
    pane: LockedPane,
}

impl PaneSurfaceReservation {
    fn new(pane: LockedPane, target: Option<&FrontmostTarget>) -> Self {
        let descriptor = SurfaceDescriptor {
            adapter_kind: pane.observer_id().into(),
            surface_key: pane.fingerprint_material(),
            target_app_name: target.and_then(|value| value.name.clone()),
            target_bundle_id: target.and_then(|value| value.bundle_id.clone()),
            target_fingerprint: pane.fingerprint_material(),
        };
        Self { descriptor, pane }
    }
}

#[async_trait]
impl SurfaceReservation for PaneSurfaceReservation {
    fn descriptor(&self) -> &SurfaceDescriptor {
        &self.descriptor
    }

    async fn prepare_insertion(&self) -> Result<(), SurfaceError> {
        let pane = self.pane.clone();
        tokio::task::spawn_blocking(move || pane.snapshot())
            .await
            .map_err(|_| SurfaceError {
                kind: SurfaceErrorKind::Internal,
                code: "pane_prepare_blocking_task_failed".into(),
            })?
            .map(|_| ())
            .map_err(|code| SurfaceError {
                kind: SurfaceErrorKind::TargetRemoved,
                code,
            })
    }

    async fn snapshot(&self) -> Result<SurfaceSnapshot, SurfaceError> {
        let pane = self.pane.clone();
        tokio::task::spawn_blocking(move || pane.snapshot())
            .await
            .map_err(|_| SurfaceError {
                kind: SurfaceErrorKind::Internal,
                code: "pane_snapshot_blocking_task_failed".into(),
            })?
            .map(|snapshot| SurfaceSnapshot {
                text: snapshot
                    .text
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n"),
                selection: None,
                observed_at: Utc::now(),
            })
            .map_err(|code| SurfaceError {
                kind: SurfaceErrorKind::TemporarilyUnavailable,
                code,
            })
    }
}

struct DesktopEditLearningRepository {
    store: Arc<Mutex<Option<Store>>>,
}

impl DesktopEditLearningRepository {
    fn with_store<T>(
        &self,
        operation: impl FnOnce(&Store) -> anyhow::Result<T>,
    ) -> Result<T, RepositoryError> {
        let guard = self
            .store
            .lock()
            .map_err(|_| RepositoryError::Unavailable("store lock poisoned".into()))?;
        let store = guard
            .as_ref()
            .ok_or_else(|| RepositoryError::Unavailable("database unavailable".into()))?;
        operation(store).map_err(|error| RepositoryError::Failure(error.to_string()))
    }
}

fn parent_not_ready(result: Result<(), RepositoryError>) -> Result<(), RepositoryError> {
    match result {
        Err(RepositoryError::Failure(message))
            if message.contains(lumen_store::EDIT_LEARNING_PARENT_NOT_READY) =>
        {
            Err(RepositoryError::ParentNotReady)
        }
        result => result,
    }
}

impl EditLearningRepository for DesktopEditLearningRepository {
    fn create_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError> {
        parent_not_ready(self.with_store(|store| store.create_edit_learning_session(record)))
    }

    fn update_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError> {
        self.with_store(|store| store.update_edit_learning_session(record))
    }

    fn append_revision_and_supersede(
        &self,
        record: &EditRevisionRecord,
    ) -> Result<Vec<Uuid>, RepositoryError> {
        self.with_store(|store| store.append_edit_learning_revision_and_supersede(record))
    }

    fn save_proposals_with_feedback(
        &self,
        records: &[LearningProposalRecord],
        notice: &FeedbackNotice,
    ) -> Result<(), RepositoryError> {
        self.with_store(|store| store.save_edit_learning_proposals_with_feedback(records, notice))
    }

    fn save_observation_failure_with_feedback(
        &self,
        record: &EditSessionRecord,
        notice: &FeedbackNotice,
    ) -> Result<(), RepositoryError> {
        parent_not_ready(self.with_store(|store| {
            store.save_edit_learning_observation_failure_with_feedback(record, notice)
        }))
    }

    fn enqueue_feedback(&self, notice: &FeedbackNotice) -> Result<(), RepositoryError> {
        self.with_store(|store| store.enqueue_edit_learning_feedback(notice))
    }
}

#[derive(Default)]
struct TauriFeedbackSink {
    app: RwLock<Option<AppHandle>>,
}

impl TauriFeedbackSink {
    fn attach(&self, app: AppHandle) {
        match self.app.write() {
            Ok(mut handle) => *handle = Some(app),
            Err(_) => tracing::error!("edit-learning feedback handle lock poisoned"),
        }
    }
}

impl FeedbackSink for TauriFeedbackSink {
    fn publish(&self, notice: &FeedbackNotice) {
        let app = self.app.read().ok().and_then(|handle| handle.clone());
        match app {
            Some(app) => {
                if let Err(error) = app.emit("edit-learning-feedback", notice) {
                    tracing::warn!(
                        notice_id = %notice.id,
                        edit_session_id = %notice.edit_session_id,
                        error = %error,
                        "edit-learning live feedback delivery failed; outbox remains durable"
                    );
                }
                let capsule_presented = crate::dictation::show_transient_background_notice(
                    &app,
                    notice.message.clone(),
                    notice.kind == "observation_unavailable",
                );
                tracing::debug!(
                    notice_id = %notice.id,
                    edit_session_id = %notice.edit_session_id,
                    capsule_presented,
                    "edit-learning live feedback presentation attempted"
                );
            }
            None => tracing::debug!(
                notice_id = %notice.id,
                edit_session_id = %notice.edit_session_id,
                "edit-learning UI not attached; outbox will deliver later"
            ),
        }
    }
}

#[tauri::command]
pub fn get_edit_learning_observability(state: State<'_, AppState>) -> ObservabilitySnapshot {
    state.edit_learning.observability()
}

#[tauri::command]
pub fn list_edit_learning_feedback(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<FeedbackNotice>, String> {
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database unavailable".to_owned())?;
    let notices = store
        .list_pending_edit_learning_feedback(limit.unwrap_or(100))
        .map_err(|error| error.to_string())?;
    for notice in &notices {
        if let Err(error) = store.mark_edit_learning_feedback_delivered(notice.id) {
            tracing::warn!(
                notice_id = %notice.id,
                error = %error,
                "could not mark edit-learning feedback delivered"
            );
        }
    }
    Ok(notices)
}

#[tauri::command]
pub fn acknowledge_edit_learning_feedback(
    state: State<'_, AppState>,
    notice_id: String,
) -> Result<(), String> {
    let notice_id = Uuid::parse_str(&notice_id).map_err(|error| error.to_string())?;
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database unavailable".to_owned())?;
    store
        .acknowledge_edit_learning_feedback(notice_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn list_edit_learning_proposals(
    state: State<'_, AppState>,
    edit_session_id: String,
) -> Result<Vec<LearningProposalRecord>, String> {
    let edit_session_id = Uuid::parse_str(&edit_session_id).map_err(|error| error.to_string())?;
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database unavailable".to_owned())?;
    store
        .list_edit_learning_proposals(edit_session_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn decide_edit_learning_proposal(
    state: State<'_, AppState>,
    proposal_id: String,
    decision: String,
) -> Result<(), String> {
    if decision != "rejected" {
        return Err("only proposal rejection is supported by this command".into());
    }
    let proposal_id = Uuid::parse_str(&proposal_id).map_err(|error| error.to_string())?;
    let guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_owned())?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "database unavailable".to_owned())?;
    let changed = store
        .decide_edit_learning_proposal(proposal_id, &decision)
        .map_err(|error| error.to_string())?;
    tracing::info!(
        proposal_id = %proposal_id,
        decision,
        changed,
        "edit-learning proposal decision processed"
    );
    changed
        .then_some(())
        .ok_or_else(|| "proposal is no longer pending for this decision".to_owned())
}

fn strategy_name(strategy: lumen_core::InsertStrategy) -> &'static str {
    match strategy {
        lumen_core::InsertStrategy::Paste => "paste",
        lumen_core::InsertStrategy::Ax => "ax",
        lumen_core::InsertStrategy::Type => "type",
        lumen_core::InsertStrategy::CopyOnly => "copy_only",
        lumen_core::InsertStrategy::None => "none",
    }
}

fn is_observable_insertion(strategy: lumen_core::InsertStrategy) -> bool {
    matches!(
        strategy,
        lumen_core::InsertStrategy::Paste
            | lumen_core::InsertStrategy::Ax
            | lumen_core::InsertStrategy::Type
    )
}

fn target_hint(target: Option<&FrontmostTarget>) -> TargetHint {
    TargetHint {
        app_name: target.and_then(|value| value.name.clone()),
        bundle_id: target.and_then(|value| value.bundle_id.clone()),
        process_id: target.and_then(|value| value.process_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_hint_keeps_the_reserved_native_process() {
        let target = FrontmostTarget {
            name: Some("Safari".into()),
            bundle_id: Some("com.apple.Safari".into()),
            process_id: Some(42),
        };

        let hint = target_hint(Some(&target));

        assert_eq!(hint.app_name.as_deref(), Some("Safari"));
        assert_eq!(hint.bundle_id.as_deref(), Some("com.apple.Safari"));
        assert_eq!(hint.process_id, Some(42));
    }

    #[test]
    fn surface_transition_timeout_has_a_distinct_failure_reason() {
        assert_eq!(
            observation_failure_reason(&EngineError::SurfaceTransitionTimeout),
            "surface_transition_timeout"
        );
    }
}
