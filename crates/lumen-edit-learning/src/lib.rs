//! Persistent observed-insertion sessions for learning from post-dictation edits.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditSessionState {
    Inserted,
    Observing,
    Editing,
    Quiescent,
    Suspended,
    Finalized,
    Failed,
}

impl EditSessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Observing => "observing",
            Self::Editing => "editing",
            Self::Quiescent => "quiescent",
            Self::Suspended => "suspended",
            Self::Finalized => "finalized",
            Self::Failed => "failed",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "inserted" => Self::Inserted,
            "editing" => Self::Editing,
            "quiescent" => Self::Quiescent,
            "suspended" => Self::Suspended,
            "finalized" => Self::Finalized,
            "failed" => Self::Failed,
            _ => Self::Observing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceErrorKind {
    TemporarilyUnavailable,
    TargetRemoved,
    PermissionDenied,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, Error)]
#[error("surface {kind:?}: {code}")]
pub struct SurfaceError {
    pub kind: SurfaceErrorKind,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub location_utf16: usize,
    pub length_utf16: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSnapshot {
    pub text: String,
    pub selection: Option<TextRange>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceDescriptor {
    pub adapter_kind: String,
    pub surface_key: String,
    pub target_app_name: Option<String>,
    pub target_bundle_id: Option<String>,
    pub target_fingerprint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetHint {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub process_id: Option<u32>,
}

pub trait SurfaceAdapter: Send + Sync {
    fn reserve(&self, target: &TargetHint) -> Result<Arc<dyn SurfaceReservation>, SurfaceError>;
}

#[async_trait]
pub trait SurfaceReservation: Send + Sync {
    fn descriptor(&self) -> &SurfaceDescriptor;
    /// Verify that an upcoming insertion will still target this exact reserved
    /// surface. Native adapters should fail closed when focus moved.
    async fn prepare_insertion(&self) -> Result<(), SurfaceError> {
        Ok(())
    }
    async fn snapshot(&self) -> Result<SurfaceSnapshot, SurfaceError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertionOutcome {
    pub strategy: String,
}

#[async_trait]
pub trait InsertionExecutor: Send + Sync {
    async fn insert(&self, text: &str) -> Result<InsertionOutcome, String>;
}

#[derive(Debug, Clone)]
pub struct ObservedInsertion {
    pub dictation_session_id: Uuid,
    pub attempt_id: Uuid,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertionReceipt {
    pub edit_session_id: Uuid,
    pub dictation_session_id: Uuid,
    pub attempt_id: Uuid,
    pub surface_key_hash: String,
    pub insertion_strategy: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditSessionRecord {
    pub id: Uuid,
    pub dictation_session_id: Uuid,
    pub attempt_id: Uuid,
    pub surface_key_hash: String,
    pub adapter_kind: String,
    pub state: EditSessionState,
    pub target_app_name: Option<String>,
    pub target_bundle_id: Option<String>,
    pub target_fingerprint_hash: String,
    pub original_text: String,
    pub original_text_hash: String,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub last_edit_at: Option<DateTime<Utc>>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub end_reason: Option<String>,
    pub relocation_attempts: u32,
    pub revision_count: u32,
    pub final_edit_distance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditRevisionRecord {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub ordinal: u32,
    pub observed_at: DateTime<Utc>,
    pub trigger: String,
    pub after_text: String,
    pub after_text_hash: String,
    pub normalized_edit_distance: f64,
    pub locator_confidence: f64,
    pub bounded: bool,
    pub quiescent: bool,
    pub final_revision: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningProposalRecord {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub revision_id: Uuid,
    pub kind: String,
    pub payload_json: String,
    pub confidence: f64,
    pub risk: String,
    pub status: String,
    pub policy_version: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackNotice {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub kind: String,
    pub message: String,
    pub proposal_ids: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("parent dictation attempt is not persisted yet")]
    ParentNotReady,
    #[error("repository unavailable: {0}")]
    Unavailable(String),
    #[error("repository failure: {0}")]
    Failure(String),
}

pub trait EditLearningRepository: Send + Sync {
    fn create_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError>;
    fn update_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError>;
    /// Atomically append a revision, supersede older candidates, and retire their feedback.
    fn append_revision_and_supersede(
        &self,
        record: &EditRevisionRecord,
    ) -> Result<Vec<Uuid>, RepositoryError>;
    /// Persist a proposal batch and its user-facing notice atomically.
    fn save_proposals_with_feedback(
        &self,
        records: &[LearningProposalRecord],
        notice: &FeedbackNotice,
    ) -> Result<(), RepositoryError>;
    /// Persist an observation failure and its user-facing notice atomically.
    fn save_observation_failure_with_feedback(
        &self,
        record: &EditSessionRecord,
        notice: &FeedbackNotice,
    ) -> Result<(), RepositoryError>;
    fn enqueue_feedback(&self, notice: &FeedbackNotice) -> Result<(), RepositoryError>;
}

pub trait FeedbackSink: Send + Sync {
    fn publish(&self, notice: &FeedbackNotice);
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub poll_interval: Duration,
    pub burst_quiescence: Duration,
    pub learning_quiescence: Duration,
    pub retention: Duration,
    pub parent_persistence_timeout: Duration,
    pub context_chars: usize,
    /// Store plaintext dictated/edited evidence in the local database. Hashes
    /// and reviewable proposal payloads remain available when disabled.
    pub persist_evidence_text: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(500),
            burst_quiescence: Duration::from_millis(1_200),
            learning_quiescence: Duration::from_secs(10),
            retention: Duration::from_secs(24 * 60 * 60),
            parent_persistence_timeout: Duration::from_secs(30),
            context_chars: 64,
            persist_evidence_text: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservabilitySnapshot {
    pub active_sessions: u64,
    pub reservations_started: u64,
    pub reservations_succeeded: u64,
    pub reservations_failed: u64,
    pub sessions_started: u64,
    pub sessions_failed_to_start: u64,
    pub snapshots_observed: u64,
    pub snapshots_unavailable: u64,
    pub suspensions: u64,
    pub recoveries: u64,
    pub revisions_recorded: u64,
    pub proposals_created: u64,
    pub proposals_superseded: u64,
    pub proposal_persistence_retries: u64,
    pub feedback_enqueued: u64,
    pub parent_persistence_retries: u64,
    pub persistence_failures: u64,
    pub sessions_evicted: u64,
    pub same_surface_sessions_finalized: u64,
    pub evidence_records_redacted: u64,
    pub insertion_target_mismatches: u64,
    pub surface_transition_timeouts: u64,
    pub content_boundary_finalizations: u64,
    pub snapshot_latency_ms_total: u64,
    pub snapshot_latency_ms_max: u64,
    pub poll_backoffs: u64,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("reserved insertion target is no longer valid: {0}")]
    Prepare(SurfaceError),
    #[error("insert failed: {0}")]
    Insert(String),
    #[error("post-insert surface snapshot failed: {0}")]
    Snapshot(#[from] SurfaceError),
    #[error("could not locate inserted text in target surface")]
    InsertedTextNotFound,
    #[error("prior edit-learning sessions did not stop before insertion")]
    SurfaceTransitionTimeout,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

pub struct EditLearningEngine {
    repository: Arc<dyn EditLearningRepository>,
    feedback: Arc<dyn FeedbackSink>,
    config: EngineConfig,
    active: Mutex<HashMap<Uuid, ActiveSession>>,
    transition: tokio::sync::Mutex<()>,
    metrics: EngineMetrics,
    persist_evidence_text: std::sync::atomic::AtomicBool,
}

struct ActiveSession {
    _surface: Arc<dyn SurfaceReservation>,
    surface_key_hash: String,
    stop: StopControl,
    started_at: Instant,
}

#[derive(Clone)]
struct StopControl {
    signal: Arc<std::sync::atomic::AtomicU8>,
    wake: Arc<tokio::sync::Notify>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

const STOP_RUNNING: u8 = 0;
const STOP_NEW_INSERTION_SAME_SURFACE: u8 = 1;
const STOP_ACTIVE_SESSION_LIMIT: u8 = 2;
const STOP_ALREADY_INSERTED_SAME_SURFACE: u8 = 3;
const MAX_ACTIVE_SESSIONS: usize = 64;
const MAX_ACTIVE_SESSIONS_PER_SURFACE: usize = 8;
const MAX_TRACKED_TEXT_CHARS: usize = 4_096;

#[derive(Default)]
struct EngineMetrics {
    reservations_started: AtomicU64,
    reservations_succeeded: AtomicU64,
    reservations_failed: AtomicU64,
    sessions_started: AtomicU64,
    sessions_failed_to_start: AtomicU64,
    snapshots_observed: AtomicU64,
    snapshots_unavailable: AtomicU64,
    suspensions: AtomicU64,
    recoveries: AtomicU64,
    revisions_recorded: AtomicU64,
    proposals_created: AtomicU64,
    proposals_superseded: AtomicU64,
    proposal_persistence_retries: AtomicU64,
    feedback_enqueued: AtomicU64,
    parent_persistence_retries: AtomicU64,
    persistence_failures: AtomicU64,
    sessions_evicted: AtomicU64,
    same_surface_sessions_finalized: AtomicU64,
    evidence_records_redacted: AtomicU64,
    insertion_target_mismatches: AtomicU64,
    surface_transition_timeouts: AtomicU64,
    content_boundary_finalizations: AtomicU64,
    snapshot_latency_ms_total: AtomicU64,
    snapshot_latency_ms_max: AtomicU64,
    poll_backoffs: AtomicU64,
}

#[derive(Debug, Clone)]
struct RangeLocator {
    original_text: String,
    left_context: String,
    right_context: String,
}

#[derive(Debug)]
struct PendingEdit {
    text: String,
    stable_since: Instant,
    observed_at: DateTime<Utc>,
}

#[derive(Debug)]
struct PendingLearning {
    revision: EditRevisionRecord,
    stable_since: Instant,
}

struct StopCompletion(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl EditLearningEngine {
    pub fn new(
        repository: Arc<dyn EditLearningRepository>,
        feedback: Arc<dyn FeedbackSink>,
        config: EngineConfig,
    ) -> Self {
        let persist_evidence_text = config.persist_evidence_text;
        Self {
            repository,
            feedback,
            config,
            active: Mutex::new(HashMap::new()),
            transition: tokio::sync::Mutex::new(()),
            metrics: EngineMetrics::default(),
            persist_evidence_text: std::sync::atomic::AtomicBool::new(persist_evidence_text),
        }
    }

    pub fn set_persist_evidence_text(&self, enabled: bool) {
        self.persist_evidence_text.store(enabled, Ordering::Release);
        tracing::info!(
            enabled,
            "edit-learning plaintext evidence persistence setting changed"
        );
    }

    async fn prepare_surface(
        &self,
        surface: &Arc<dyn SurfaceReservation>,
    ) -> Result<(), EngineError> {
        surface.prepare_insertion().await.map_err(|error| {
            self.metrics
                .insertion_target_mismatches
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                adapter = %surface.descriptor().adapter_kind,
                surface_key_hash = %hash_text(&surface.descriptor().surface_key),
                error_kind = ?error.kind,
                error_code = %error.code,
                "edit-learning insertion refused because reserved target changed"
            );
            EngineError::Prepare(error)
        })
    }

    pub async fn insert(
        self: &Arc<Self>,
        surface: Arc<dyn SurfaceReservation>,
        executor: Arc<dyn InsertionExecutor>,
        insertion: ObservedInsertion,
    ) -> Result<InsertionReceipt, EngineError> {
        let _transition = self.transition.lock().await;
        self.prepare_surface(&surface).await?;
        if !self
            .stop_sessions_for_surface(surface.descriptor(), STOP_NEW_INSERTION_SAME_SURFACE)
            .await
        {
            self.metrics
                .surface_transition_timeouts
                .fetch_add(1, Ordering::Relaxed);
            return Err(EngineError::SurfaceTransitionTimeout);
        }
        self.prepare_surface(&surface).await?;
        let outcome = executor
            .insert(&insertion.text)
            .await
            .map_err(EngineError::Insert)?;
        self.observe_inserted_locked(surface, insertion, outcome)
            .await
    }

    /// Attaches observation to text that has already been inserted. This is
    /// used when a retained native element became stale during insertion and a
    /// fresh post-insert reservation can recover the actual target without
    /// inserting the text a second time.
    pub async fn observe_inserted(
        self: &Arc<Self>,
        surface: Arc<dyn SurfaceReservation>,
        insertion: ObservedInsertion,
        outcome: InsertionOutcome,
    ) -> Result<InsertionReceipt, EngineError> {
        let _transition = self.transition.lock().await;
        if !self
            .stop_sessions_for_surface(surface.descriptor(), STOP_ALREADY_INSERTED_SAME_SURFACE)
            .await
        {
            self.metrics
                .surface_transition_timeouts
                .fetch_add(1, Ordering::Relaxed);
            return Err(EngineError::SurfaceTransitionTimeout);
        }
        self.observe_inserted_locked(surface, insertion, outcome)
            .await
    }

    async fn observe_inserted_locked(
        self: &Arc<Self>,
        surface: Arc<dyn SurfaceReservation>,
        insertion: ObservedInsertion,
        outcome: InsertionOutcome,
    ) -> Result<InsertionReceipt, EngineError> {
        let snapshot = surface.snapshot().await?;
        let locator = RangeLocator::from_post_insert(
            &snapshot.text,
            &insertion.text,
            snapshot.selection.as_ref(),
            self.config.context_chars,
        )
        .ok_or(EngineError::InsertedTextNotFound)?;

        let descriptor = surface.descriptor();
        let now = Utc::now();
        let edit_session_id = Uuid::new_v4();
        let record = EditSessionRecord {
            id: edit_session_id,
            dictation_session_id: insertion.dictation_session_id,
            attempt_id: insertion.attempt_id,
            surface_key_hash: hash_text(&descriptor.surface_key),
            adapter_kind: descriptor.adapter_kind.clone(),
            state: EditSessionState::Observing,
            target_app_name: descriptor.target_app_name.clone(),
            target_bundle_id: descriptor.target_bundle_id.clone(),
            target_fingerprint_hash: hash_text(&descriptor.target_fingerprint),
            original_text_hash: hash_text(&insertion.text),
            original_text: insertion.text,
            started_at: now,
            last_seen_at: snapshot.observed_at,
            last_edit_at: None,
            finalized_at: None,
            end_reason: None,
            relocation_attempts: 0,
            revision_count: 0,
            final_edit_distance: None,
        };
        let persisted = match self.create_persisted_session(&record) {
            Ok(()) => true,
            Err(RepositoryError::ParentNotReady) => {
                self.metrics
                    .parent_persistence_retries
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    edit_session_id = %record.id,
                    dictation_session_id = %record.dictation_session_id,
                    attempt_id = %record.attempt_id,
                    "edit-learning session waiting for parent attempt persistence"
                );
                false
            }
            Err(error) => {
                self.metrics
                    .persistence_failures
                    .fetch_add(1, Ordering::Relaxed);
                return Err(error.into());
            }
        };
        let stop = StopControl {
            signal: Arc::new(std::sync::atomic::AtomicU8::new(STOP_RUNNING)),
            wake: Arc::new(tokio::sync::Notify::new()),
            stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        {
            let mut active = self
                .active
                .lock()
                .expect("edit-learning active-session lock poisoned");
            let running_on_surface = active
                .values()
                .filter(|existing| {
                    existing.surface_key_hash == record.surface_key_hash
                        && existing.stop.signal.load(Ordering::Acquire) == STOP_RUNNING
                })
                .count();
            let running_total = active
                .values()
                .filter(|existing| existing.stop.signal.load(Ordering::Acquire) == STOP_RUNNING)
                .count();
            if running_on_surface >= MAX_ACTIVE_SESSIONS_PER_SURFACE
                || running_total >= MAX_ACTIVE_SESSIONS
            {
                if let Some(oldest) = active
                    .values()
                    .filter(|existing| {
                        existing.stop.signal.load(Ordering::Acquire) == STOP_RUNNING
                            && (running_total >= MAX_ACTIVE_SESSIONS
                                || existing.surface_key_hash == record.surface_key_hash)
                    })
                    .min_by_key(|existing| existing.started_at)
                {
                    if oldest
                        .stop
                        .signal
                        .compare_exchange(
                            STOP_RUNNING,
                            STOP_ACTIVE_SESSION_LIMIT,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        oldest.stop.wake.notify_one();
                        self.metrics
                            .sessions_evicted
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            active.insert(
                edit_session_id,
                ActiveSession {
                    _surface: surface,
                    surface_key_hash: record.surface_key_hash.clone(),
                    stop: stop.clone(),
                    started_at: Instant::now(),
                },
            );
        }
        self.metrics
            .sessions_started
            .fetch_add(1, Ordering::Relaxed);

        tracing::info!(
            edit_session_id = %edit_session_id,
            dictation_session_id = %record.dictation_session_id,
            attempt_id = %record.attempt_id,
            adapter = %record.adapter_kind,
            surface_key_hash = %record.surface_key_hash,
            insertion_strategy = %outcome.strategy,
            active_sessions = self.active_len(),
            "edit-learning session started"
        );

        let worker_surface = self
            .active
            .lock()
            .expect("edit-learning active-session lock poisoned")
            .get(&edit_session_id)
            .expect("new edit-learning session missing")
            ._surface
            .clone();
        let engine = Arc::clone(self);
        let worker_record = record.clone();
        tokio::spawn(async move {
            engine
                .observe_session(worker_record, worker_surface, locator, persisted, stop)
                .await;
        });

        Ok(InsertionReceipt {
            edit_session_id,
            dictation_session_id: record.dictation_session_id,
            attempt_id: record.attempt_id,
            surface_key_hash: record.surface_key_hash,
            insertion_strategy: outcome.strategy,
        })
    }

    /// Records an insertion that succeeded but could not be bound to an
    /// observable surface. The terminal session and user-facing notice are
    /// persisted together after the parent dictation attempt becomes visible.
    pub fn report_observation_failure(
        self: &Arc<Self>,
        insertion: ObservedInsertion,
        target: &TargetHint,
        reason: impl Into<String>,
    ) -> Uuid {
        let reason = reason.into();
        let now = Utc::now();
        let edit_session_id = Uuid::new_v4();
        let target_key = format!(
            "{}:{}",
            target.bundle_id.as_deref().unwrap_or("unknown"),
            target
                .process_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        let record = EditSessionRecord {
            id: edit_session_id,
            dictation_session_id: insertion.dictation_session_id,
            attempt_id: insertion.attempt_id,
            surface_key_hash: hash_text(&target_key),
            adapter_kind: "unavailable".into(),
            state: EditSessionState::Failed,
            target_app_name: target.app_name.clone(),
            target_bundle_id: target.bundle_id.clone(),
            target_fingerprint_hash: hash_text(&target_key),
            original_text_hash: hash_text(&insertion.text),
            original_text: insertion.text,
            started_at: now,
            last_seen_at: now,
            last_edit_at: None,
            finalized_at: Some(now),
            end_reason: Some(reason.clone()),
            relocation_attempts: 0,
            revision_count: 0,
            final_edit_distance: None,
        };
        self.metrics
            .sessions_started
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .sessions_failed_to_start
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            edit_session_id = %edit_session_id,
            dictation_session_id = %record.dictation_session_id,
            attempt_id = %record.attempt_id,
            target_bundle_id = ?record.target_bundle_id,
            surface_key_hash = %record.surface_key_hash,
            reason,
            "edit-learning observation failure registered"
        );

        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine.persist_observation_failure(record).await;
        });
        edit_session_id
    }

    pub fn reserve(
        &self,
        adapter: &dyn SurfaceAdapter,
        target: &TargetHint,
    ) -> Result<Arc<dyn SurfaceReservation>, SurfaceError> {
        self.metrics
            .reservations_started
            .fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        match adapter.reserve(target) {
            Ok(reservation) => {
                self.metrics
                    .reservations_succeeded
                    .fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    adapter = %reservation.descriptor().adapter_kind,
                    surface_key_hash = %hash_text(&reservation.descriptor().surface_key),
                    target_bundle_id = ?target.bundle_id,
                    acquisition_latency_ms = started.elapsed().as_millis() as u64,
                    "edit-learning target reserved"
                );
                Ok(reservation)
            }
            Err(error) => {
                self.metrics
                    .reservations_failed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target_bundle_id = ?target.bundle_id,
                    acquisition_latency_ms = started.elapsed().as_millis() as u64,
                    error_kind = ?error.kind,
                    error_code = %error.code,
                    "edit-learning target reservation failed"
                );
                Err(error)
            }
        }
    }

    pub fn observability(&self) -> ObservabilitySnapshot {
        ObservabilitySnapshot {
            active_sessions: self.active_len() as u64,
            reservations_started: self.metrics.reservations_started.load(Ordering::Relaxed),
            reservations_succeeded: self.metrics.reservations_succeeded.load(Ordering::Relaxed),
            reservations_failed: self.metrics.reservations_failed.load(Ordering::Relaxed),
            sessions_started: self.metrics.sessions_started.load(Ordering::Relaxed),
            sessions_failed_to_start: self
                .metrics
                .sessions_failed_to_start
                .load(Ordering::Relaxed),
            snapshots_observed: self.metrics.snapshots_observed.load(Ordering::Relaxed),
            snapshots_unavailable: self.metrics.snapshots_unavailable.load(Ordering::Relaxed),
            suspensions: self.metrics.suspensions.load(Ordering::Relaxed),
            recoveries: self.metrics.recoveries.load(Ordering::Relaxed),
            revisions_recorded: self.metrics.revisions_recorded.load(Ordering::Relaxed),
            proposals_created: self.metrics.proposals_created.load(Ordering::Relaxed),
            proposals_superseded: self.metrics.proposals_superseded.load(Ordering::Relaxed),
            proposal_persistence_retries: self
                .metrics
                .proposal_persistence_retries
                .load(Ordering::Relaxed),
            feedback_enqueued: self.metrics.feedback_enqueued.load(Ordering::Relaxed),
            parent_persistence_retries: self
                .metrics
                .parent_persistence_retries
                .load(Ordering::Relaxed),
            persistence_failures: self.metrics.persistence_failures.load(Ordering::Relaxed),
            sessions_evicted: self.metrics.sessions_evicted.load(Ordering::Relaxed),
            same_surface_sessions_finalized: self
                .metrics
                .same_surface_sessions_finalized
                .load(Ordering::Relaxed),
            evidence_records_redacted: self
                .metrics
                .evidence_records_redacted
                .load(Ordering::Relaxed),
            insertion_target_mismatches: self
                .metrics
                .insertion_target_mismatches
                .load(Ordering::Relaxed),
            surface_transition_timeouts: self
                .metrics
                .surface_transition_timeouts
                .load(Ordering::Relaxed),
            content_boundary_finalizations: self
                .metrics
                .content_boundary_finalizations
                .load(Ordering::Relaxed),
            snapshot_latency_ms_total: self
                .metrics
                .snapshot_latency_ms_total
                .load(Ordering::Relaxed),
            snapshot_latency_ms_max: self.metrics.snapshot_latency_ms_max.load(Ordering::Relaxed),
            poll_backoffs: self.metrics.poll_backoffs.load(Ordering::Relaxed),
        }
    }

    fn active_len(&self) -> usize {
        self.active
            .lock()
            .expect("edit-learning active-session lock poisoned")
            .len()
    }

    async fn stop_sessions_for_surface(&self, descriptor: &SurfaceDescriptor, reason: u8) -> bool {
        let surface_key_hash = hash_text(&descriptor.surface_key);
        let stopped = self
            .active
            .lock()
            .expect("edit-learning active-session lock poisoned")
            .values()
            .filter(|existing| existing.surface_key_hash == surface_key_hash)
            .map(|existing| {
                let signalled = existing
                    .stop
                    .signal
                    .compare_exchange(STOP_RUNNING, reason, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
                if signalled {
                    existing.stop.wake.notify_one();
                }
                (signalled, existing.stop.stopped.clone())
            })
            .collect::<Vec<_>>();
        let signalled = stopped.iter().filter(|(signalled, _)| *signalled).count();
        let wait_started = Instant::now();
        while stopped
            .iter()
            .any(|(_, stopped)| !stopped.load(Ordering::Acquire))
            && wait_started.elapsed() < Duration::from_secs(2)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let completed = stopped
            .iter()
            .all(|(_, stopped)| stopped.load(Ordering::Acquire));
        if completed && signalled > 0 {
            self.metrics
                .same_surface_sessions_finalized
                .fetch_add(signalled as u64, Ordering::Relaxed);
            tracing::info!(
                adapter = %descriptor.adapter_kind,
                surface_key_hash,
                sessions_signalled = signalled,
                wait_ms = wait_started.elapsed().as_millis() as u64,
                "edit-learning stopped prior sessions before same-surface insertion"
            );
        }
        if !completed {
            tracing::error!(
                adapter = %descriptor.adapter_kind,
                surface_key_hash,
                sessions_waited = stopped.len(),
                "edit-learning same-surface transition timed out"
            );
        }
        completed
    }

    async fn observe_session(
        self: Arc<Self>,
        mut record: EditSessionRecord,
        surface: Arc<dyn SurfaceReservation>,
        locator: RangeLocator,
        mut persisted: bool,
        stop: StopControl,
    ) {
        let _completion = StopCompletion(stop.stopped.clone());
        let started = Instant::now();
        let mut next_poll = Duration::ZERO;
        let mut unavailable_streak = 0_u32;
        let mut pending: Option<PendingEdit> = None;
        let mut pending_learning: Option<PendingLearning> = None;
        let mut last_revision_text = record.original_text.clone();

        loop {
            if !next_poll.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(next_poll) => {}
                    _ = stop.wake.notified() => {}
                }
            }
            next_poll = self.config.poll_interval;
            if started.elapsed() >= self.config.retention {
                if persisted {
                    if let Some(candidate) = pending.take() {
                        self.persist_terminal_revision(
                            &mut record,
                            &locator,
                            candidate,
                            "retention_expired",
                        );
                    } else if let Some(candidate) = pending_learning.take() {
                        self.persist_proposals_for_revision(&record, &candidate.revision);
                    }
                }
                record.state = EditSessionState::Finalized;
                record.finalized_at = Some(Utc::now());
                record.end_reason = Some("retention_expired".into());
                if persisted {
                    self.persist_session_update(&record);
                }
                self.active
                    .lock()
                    .expect("edit-learning active-session lock poisoned")
                    .remove(&record.id);
                tracing::info!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    revision_count = record.revision_count,
                    end_reason = "retention_expired",
                    persisted,
                    active_sessions = self.active_len(),
                    "edit-learning session finalized"
                );
                return;
            }
            let stop_reason = stop.signal.load(Ordering::Acquire);
            if stop_reason != STOP_RUNNING {
                if stop_reason != STOP_ALREADY_INSERTED_SAME_SURFACE {
                    if let Ok(snapshot) = surface.snapshot().await {
                        if let Some(current_text) = locator.project(&snapshot.text) {
                            if current_text != last_revision_text {
                                pending = Some(PendingEdit {
                                    text: current_text,
                                    stable_since: Instant::now(),
                                    observed_at: snapshot.observed_at,
                                });
                            }
                        }
                    }
                }
                if persisted {
                    if let Some(candidate) = pending.take() {
                        self.persist_terminal_revision(
                            &mut record,
                            &locator,
                            candidate,
                            "session_lifecycle_boundary",
                        );
                    } else if let Some(candidate) = pending_learning.take() {
                        self.persist_proposals_for_revision(&record, &candidate.revision);
                    }
                }
                let reason = match stop_reason {
                    STOP_NEW_INSERTION_SAME_SURFACE | STOP_ALREADY_INSERTED_SAME_SURFACE => {
                        "new_insertion_same_surface"
                    }
                    STOP_ACTIVE_SESSION_LIMIT => "active_session_limit",
                    _ => "engine_stop_requested",
                };
                record.state = EditSessionState::Finalized;
                record.finalized_at = Some(Utc::now());
                record.end_reason = Some(reason.into());
                if persisted {
                    self.persist_session_update(&record);
                }
                self.active
                    .lock()
                    .expect("edit-learning active-session lock poisoned")
                    .remove(&record.id);
                drop(surface);
                stop.stopped.store(true, Ordering::Release);
                if !persisted {
                    self.persist_finalized_session(record.clone(), &locator, pending.take())
                        .await;
                }
                tracing::info!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    surface_key_hash = %record.surface_key_hash,
                    revision_count = record.revision_count,
                    end_reason = reason,
                    active_sessions = self.active_len(),
                    "edit-learning session finalized by lifecycle boundary"
                );
                return;
            }
            if !persisted {
                match self.create_persisted_session(&record) {
                    Ok(()) => {
                        persisted = true;
                        tracing::info!(
                            edit_session_id = %record.id,
                            dictation_session_id = %record.dictation_session_id,
                            attempt_id = %record.attempt_id,
                            parent_persistence_retries = self
                                .metrics
                                .parent_persistence_retries
                                .load(Ordering::Relaxed),
                            "edit-learning session persistence attached"
                        );
                    }
                    Err(RepositoryError::ParentNotReady)
                        if started.elapsed() < self.config.parent_persistence_timeout =>
                    {
                        let retry = self
                            .metrics
                            .parent_persistence_retries
                            .fetch_add(1, Ordering::Relaxed)
                            .saturating_add(1);
                        tracing::debug!(
                            edit_session_id = %record.id,
                            dictation_session_id = %record.dictation_session_id,
                            attempt_id = %record.attempt_id,
                            retry,
                            "edit-learning parent attempt still unavailable"
                        );
                        continue;
                    }
                    Err(error) => {
                        self.metrics
                            .persistence_failures
                            .fetch_add(1, Ordering::Relaxed);
                        self.active
                            .lock()
                            .expect("edit-learning active-session lock poisoned")
                            .remove(&record.id);
                        tracing::error!(
                            edit_session_id = %record.id,
                            dictation_session_id = %record.dictation_session_id,
                            attempt_id = %record.attempt_id,
                            error = %error,
                            "edit-learning session persistence failed permanently"
                        );
                        return;
                    }
                }
            }
            let snapshot_started = Instant::now();
            let snapshot = match surface.snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    if stop.signal.load(Ordering::Acquire) != STOP_RUNNING {
                        next_poll = Duration::ZERO;
                        continue;
                    }
                    if error.kind != SurfaceErrorKind::TemporarilyUnavailable {
                        self.metrics
                            .snapshots_unavailable
                            .fetch_add(1, Ordering::Relaxed);
                        if let Some(candidate) = pending.take() {
                            self.persist_terminal_revision(
                                &mut record,
                                &locator,
                                candidate,
                                "surface_unavailable",
                            );
                        } else if let Some(candidate) = pending_learning.take() {
                            self.persist_proposals_for_revision(&record, &candidate.revision);
                        }
                        let reason = match error.kind {
                            SurfaceErrorKind::TargetRemoved => "surface_target_removed",
                            SurfaceErrorKind::PermissionDenied => "surface_permission_denied",
                            SurfaceErrorKind::Unsupported => "surface_unsupported",
                            _ => "surface_internal_error",
                        };
                        record.state = EditSessionState::Finalized;
                        record.finalized_at = Some(Utc::now());
                        record.end_reason = Some(reason.into());
                        self.persist_session_update(&record);
                        self.active
                            .lock()
                            .expect("edit-learning active-session lock poisoned")
                            .remove(&record.id);
                        tracing::warn!(
                            edit_session_id = %record.id,
                            attempt_id = %record.attempt_id,
                            error_kind = ?error.kind,
                            error_code = %error.code,
                            end_reason = reason,
                            "edit-learning surface failed permanently"
                        );
                        return;
                    }
                    unavailable_streak = unavailable_streak.saturating_add(1).min(8);
                    let multiplier = 1_u32 << unavailable_streak;
                    next_poll = self
                        .config
                        .poll_interval
                        .saturating_mul(multiplier)
                        .min(Duration::from_secs(30));
                    self.metrics.poll_backoffs.fetch_add(1, Ordering::Relaxed);
                    self.metrics
                        .snapshots_unavailable
                        .fetch_add(1, Ordering::Relaxed);
                    if record.state != EditSessionState::Suspended {
                        record.state = EditSessionState::Suspended;
                        record.relocation_attempts = record.relocation_attempts.saturating_add(1);
                        self.metrics.suspensions.fetch_add(1, Ordering::Relaxed);
                        self.persist_session_update(&record);
                        tracing::warn!(
                            edit_session_id = %record.id,
                            attempt_id = %record.attempt_id,
                            adapter = %record.adapter_kind,
                            surface_key_hash = %record.surface_key_hash,
                            error_kind = ?error.kind,
                            error_code = %error.code,
                            "edit-learning surface suspended"
                        );
                    }
                    pending = None;
                    continue;
                }
            };
            if stop.signal.load(Ordering::Acquire) != STOP_RUNNING {
                next_poll = Duration::ZERO;
                continue;
            }
            unavailable_streak = 0;
            self.metrics
                .snapshots_observed
                .fetch_add(1, Ordering::Relaxed);
            let snapshot_latency_ms = snapshot_started.elapsed().as_millis() as u64;
            self.metrics
                .snapshot_latency_ms_total
                .fetch_add(snapshot_latency_ms, Ordering::Relaxed);
            self.metrics
                .snapshot_latency_ms_max
                .fetch_max(snapshot_latency_ms, Ordering::Relaxed);
            record.last_seen_at = snapshot.observed_at;
            let Some(current_text) = locator.project(&snapshot.text) else {
                if let Some(candidate) = pending.take() {
                    self.persist_terminal_revision(
                        &mut record,
                        &locator,
                        candidate,
                        "surface_content_removed",
                    );
                }
                if let Some(candidate) = pending_learning.take() {
                    self.persist_proposals_for_revision(&record, &candidate.revision);
                }
                record.state = EditSessionState::Finalized;
                record.finalized_at = Some(Utc::now());
                record.end_reason = Some("surface_content_removed".into());
                self.metrics
                    .content_boundary_finalizations
                    .fetch_add(1, Ordering::Relaxed);
                self.persist_session_update(&record);
                self.active
                    .lock()
                    .expect("edit-learning active-session lock poisoned")
                    .remove(&record.id);
                tracing::info!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    surface_key_hash = %record.surface_key_hash,
                    revision_count = record.revision_count,
                    end_reason = "surface_content_removed",
                    active_sessions = self.active_len(),
                    "edit-learning session finalized after tracked content disappeared"
                );
                return;
            };

            // A successful snapshot alone does not mean a suspended locator
            // recovered. Only report recovery after the tracked range has also
            // been projected successfully, otherwise an ambiguous locator would
            // oscillate between recovered/suspended on every poll.
            if record.state == EditSessionState::Suspended {
                record.state = EditSessionState::Observing;
                self.metrics.recoveries.fetch_add(1, Ordering::Relaxed);
                self.persist_session_update(&record);
                tracing::info!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    relocation_attempts = record.relocation_attempts,
                    snapshot_latency_ms = snapshot_started.elapsed().as_millis() as u64,
                    "edit-learning surface recovered"
                );
            };

            if current_text != last_revision_text {
                if let Some(candidate) = pending_learning.as_mut() {
                    candidate.stable_since = Instant::now();
                }
            }
            if current_text == record.original_text {
                pending = None;
                pending_learning = None;
                if last_revision_text != record.original_text {
                    let ordinal = record.revision_count.saturating_add(1);
                    let revision = EditRevisionRecord {
                        id: Uuid::new_v4(),
                        edit_session_id: record.id,
                        ordinal,
                        observed_at: snapshot.observed_at,
                        trigger: "reverted_to_original".into(),
                        after_text_hash: record.original_text_hash.clone(),
                        after_text: record.original_text.clone(),
                        normalized_edit_distance: 0.0,
                        locator_confidence: locator.confidence(),
                        bounded: locator.is_fully_bounded(),
                        quiescent: true,
                        final_revision: false,
                    };
                    let superseded = match self.append_persisted_revision(&revision) {
                        Ok(superseded) => superseded,
                        Err(error) => {
                            self.metrics
                                .persistence_failures
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::error!(
                                edit_session_id = %record.id,
                                attempt_id = %record.attempt_id,
                                revision_ordinal = ordinal,
                                error = %error,
                                "edit-learning revert transition persistence failed"
                            );
                            continue;
                        }
                    };
                    self.publish_superseded_feedback(&record, &revision, superseded);
                    last_revision_text = record.original_text.clone();
                    record.revision_count = ordinal;
                    record.final_edit_distance = Some(0.0);
                    self.metrics
                        .revisions_recorded
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        edit_session_id = %record.id,
                        attempt_id = %record.attempt_id,
                        revision_ordinal = ordinal,
                        "edit-learning edit reverted to original"
                    );
                }
                if record.state != EditSessionState::Observing {
                    record.state = EditSessionState::Observing;
                    self.persist_session_update(&record);
                }
                continue;
            }
            if current_text == last_revision_text {
                pending = None;
                let learning_ready = pending_learning.as_ref().is_some_and(|candidate| {
                    candidate.stable_since.elapsed() >= self.config.learning_quiescence
                });
                if learning_ready {
                    let candidate = pending_learning
                        .as_ref()
                        .expect("ready learning candidate missing");
                    if self.persist_proposals_for_revision(&record, &candidate.revision) {
                        pending_learning = None;
                    } else if let Some(candidate) = pending_learning.as_mut() {
                        candidate.stable_since = Instant::now();
                        self.metrics
                            .proposal_persistence_retries
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                continue;
            }

            let ready = pending.as_ref().is_some_and(|candidate| {
                candidate.text == current_text
                    && candidate.stable_since.elapsed() >= self.config.burst_quiescence
            });
            if !ready {
                if pending
                    .as_ref()
                    .is_none_or(|candidate| candidate.text != current_text)
                {
                    pending = Some(PendingEdit {
                        text: current_text,
                        stable_since: Instant::now(),
                        observed_at: snapshot.observed_at,
                    });
                    record.state = EditSessionState::Editing;
                    record.last_edit_at = Some(snapshot.observed_at);
                    self.persist_session_update(&record);
                }
                continue;
            }

            let candidate = pending.take().expect("ready pending edit missing");
            let ordinal = record.revision_count.saturating_add(1);
            let edit_distance = normalized_edit_distance(&record.original_text, &candidate.text);
            let revision = EditRevisionRecord {
                id: Uuid::new_v4(),
                edit_session_id: record.id,
                ordinal,
                observed_at: candidate.observed_at,
                trigger: "burst_quiescence".into(),
                after_text_hash: hash_text(&candidate.text),
                after_text: candidate.text.clone(),
                normalized_edit_distance: edit_distance,
                locator_confidence: locator.confidence(),
                bounded: locator.is_fully_bounded(),
                quiescent: true,
                final_revision: false,
            };
            let superseded = match self.append_persisted_revision(&revision) {
                Ok(superseded) => superseded,
                Err(error) => {
                    self.metrics
                        .persistence_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        edit_session_id = %record.id,
                        attempt_id = %record.attempt_id,
                        revision_ordinal = ordinal,
                        error = %error,
                        "edit-learning revision transition persistence failed"
                    );
                    continue;
                }
            };
            self.publish_superseded_feedback(&record, &revision, superseded);
            last_revision_text = candidate.text;
            record.state = EditSessionState::Quiescent;
            record.revision_count = ordinal;
            record.final_edit_distance = Some(edit_distance);
            self.persist_session_update(&record);
            self.metrics
                .revisions_recorded
                .fetch_add(1, Ordering::Relaxed);
            pending_learning = Some(PendingLearning {
                revision: revision.clone(),
                stable_since: Instant::now(),
            });
            tracing::info!(
                edit_session_id = %record.id,
                dictation_session_id = %record.dictation_session_id,
                attempt_id = %record.attempt_id,
                adapter = %record.adapter_kind,
                surface_key_hash = %record.surface_key_hash,
                revision_ordinal = ordinal,
                normalized_edit_distance = edit_distance,
                locator_confidence = revision.locator_confidence,
                snapshot_latency_ms = snapshot_started.elapsed().as_millis() as u64,
                "edit-learning revision recorded"
            );
        }
    }

    fn persist_proposals_for_revision(
        &self,
        record: &EditSessionRecord,
        revision: &EditRevisionRecord,
    ) -> bool {
        let proposals = proposals_from_revision(&record.original_text, revision);
        if proposals.is_empty() {
            return true;
        }
        let term_count = proposals
            .iter()
            .filter(|proposal| proposal.kind == "term")
            .count();
        let replacement_count = proposals.len().saturating_sub(term_count);
        let notice = FeedbackNotice {
            id: Uuid::new_v4(),
            edit_session_id: record.id,
            kind: if replacement_count > 0 {
                "learning_confirmation_required".into()
            } else {
                "learning_candidates_ready".into()
            },
            message: format!(
                "已记录这次修改，发现 {} 个词汇候选和 {} 个替换候选。",
                term_count, replacement_count
            ),
            proposal_ids: proposals.iter().map(|proposal| proposal.id).collect(),
            created_at: Utc::now(),
            delivered_at: None,
            acknowledged_at: None,
        };
        if let Err(error) = self
            .repository
            .save_proposals_with_feedback(&proposals, &notice)
        {
            self.metrics
                .persistence_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                edit_session_id = %record.id,
                attempt_id = %record.attempt_id,
                notice_id = %notice.id,
                proposal_count = proposals.len(),
                error = %error,
                "edit-learning proposal and feedback transaction failed"
            );
            return false;
        }
        self.metrics
            .proposals_created
            .fetch_add(proposals.len() as u64, Ordering::Relaxed);
        self.metrics
            .feedback_enqueued
            .fetch_add(1, Ordering::Relaxed);
        self.feedback.publish(&notice);
        tracing::info!(
            edit_session_id = %record.id,
            attempt_id = %record.attempt_id,
            revision_ordinal = revision.ordinal,
            proposal_count = proposals.len(),
            term_count,
            replacement_count,
            learning_quiescence_ms = self.config.learning_quiescence.as_millis() as u64,
            policy_version = 1,
            "edit-learning proposals created after learning quiescence"
        );
        true
    }

    fn persist_terminal_revision(
        &self,
        record: &mut EditSessionRecord,
        locator: &RangeLocator,
        candidate: PendingEdit,
        trigger: &'static str,
    ) {
        let ordinal = record.revision_count.saturating_add(1);
        let edit_distance = normalized_edit_distance(&record.original_text, &candidate.text);
        let revision = EditRevisionRecord {
            id: Uuid::new_v4(),
            edit_session_id: record.id,
            ordinal,
            observed_at: candidate.observed_at,
            trigger: trigger.into(),
            after_text_hash: hash_text(&candidate.text),
            after_text: candidate.text,
            normalized_edit_distance: edit_distance,
            locator_confidence: locator.confidence(),
            bounded: locator.is_fully_bounded(),
            quiescent: false,
            final_revision: true,
        };
        match self.append_persisted_revision(&revision) {
            Ok(superseded) => {
                self.publish_superseded_feedback(record, &revision, superseded);
                record.revision_count = ordinal;
                record.final_edit_distance = Some(edit_distance);
                self.metrics
                    .revisions_recorded
                    .fetch_add(1, Ordering::Relaxed);
                self.persist_proposals_for_revision(record, &revision);
                tracing::info!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    revision_ordinal = ordinal,
                    normalized_edit_distance = edit_distance,
                    locator_confidence = revision.locator_confidence,
                    trigger,
                    "edit-learning pending edit flushed at terminal boundary"
                );
            }
            Err(error) => {
                self.metrics
                    .persistence_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    edit_session_id = %record.id,
                    attempt_id = %record.attempt_id,
                    revision_ordinal = ordinal,
                    trigger,
                    error = %error,
                    "edit-learning terminal revision persistence failed"
                );
            }
        }
    }

    fn publish_superseded_feedback(
        &self,
        record: &EditSessionRecord,
        revision: &EditRevisionRecord,
        superseded: Vec<Uuid>,
    ) {
        if superseded.is_empty() {
            return;
        }
        self.metrics
            .proposals_superseded
            .fetch_add(superseded.len() as u64, Ordering::Relaxed);
        let notice = FeedbackNotice {
            id: Uuid::new_v4(),
            edit_session_id: record.id,
            kind: "learning_candidates_superseded".into(),
            message: "检测到后续修改，之前的学习候选已撤回；正在等待最终版本。".into(),
            proposal_ids: Vec::new(),
            created_at: Utc::now(),
            delivered_at: None,
            acknowledged_at: None,
        };
        if let Err(error) = self.repository.enqueue_feedback(&notice) {
            self.metrics
                .persistence_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                edit_session_id = %record.id,
                attempt_id = %record.attempt_id,
                notice_id = %notice.id,
                error = %error,
                "edit-learning supersede feedback enqueue failed"
            );
        } else {
            self.metrics
                .feedback_enqueued
                .fetch_add(1, Ordering::Relaxed);
            self.feedback.publish(&notice);
        }
        tracing::info!(
            edit_session_id = %record.id,
            attempt_id = %record.attempt_id,
            revision_ordinal = revision.ordinal,
            superseded_proposal_count = superseded.len(),
            "edit-learning prior proposals superseded"
        );
    }

    async fn persist_observation_failure(&self, record: EditSessionRecord) {
        let notice = FeedbackNotice {
            id: Uuid::new_v4(),
            edit_session_id: record.id,
            kind: "observation_unavailable".into(),
            message: "文字已插入，但未能持续监听这次编辑；后续修改不会自动学习。请检查辅助功能权限或目标输入框。".into(),
            proposal_ids: Vec::new(),
            created_at: Utc::now(),
            delivered_at: None,
            acknowledged_at: None,
        };
        let persisted = self.session_for_persistence(&record);
        let started = Instant::now();
        loop {
            match self
                .repository
                .save_observation_failure_with_feedback(&persisted, &notice)
            {
                Ok(()) => break,
                Err(RepositoryError::ParentNotReady)
                    if started.elapsed() < self.config.parent_persistence_timeout =>
                {
                    let retry = self
                        .metrics
                        .parent_persistence_retries
                        .fetch_add(1, Ordering::Relaxed)
                        .saturating_add(1);
                    tracing::debug!(
                        edit_session_id = %record.id,
                        dictation_session_id = %record.dictation_session_id,
                        attempt_id = %record.attempt_id,
                        retry,
                        "failed edit-learning session waiting for parent attempt persistence"
                    );
                    tokio::time::sleep(self.config.poll_interval).await;
                }
                Err(error) => {
                    self.metrics
                        .persistence_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        edit_session_id = %record.id,
                        dictation_session_id = %record.dictation_session_id,
                        attempt_id = %record.attempt_id,
                        error = %error,
                        "failed edit-learning session could not be persisted"
                    );
                    return;
                }
            }
        }
        if persisted.original_text.is_empty() && !record.original_text.is_empty() {
            self.metrics
                .evidence_records_redacted
                .fetch_add(1, Ordering::Relaxed);
        }
        self.metrics
            .feedback_enqueued
            .fetch_add(1, Ordering::Relaxed);
        self.feedback.publish(&notice);
        tracing::info!(
            edit_session_id = %record.id,
            attempt_id = %record.attempt_id,
            notice_id = %notice.id,
            end_reason = ?record.end_reason,
            "edit-learning observation failure feedback enqueued"
        );
    }

    async fn persist_finalized_session(
        &self,
        mut record: EditSessionRecord,
        locator: &RangeLocator,
        pending: Option<PendingEdit>,
    ) {
        let started = Instant::now();
        loop {
            match self.create_persisted_session(&record) {
                Ok(()) => {
                    if let Some(candidate) = pending {
                        self.persist_terminal_revision(
                            &mut record,
                            locator,
                            candidate,
                            "session_lifecycle_boundary",
                        );
                        self.persist_session_update(&record);
                    }
                    return;
                }
                Err(RepositoryError::ParentNotReady)
                    if started.elapsed() < self.config.parent_persistence_timeout =>
                {
                    self.metrics
                        .parent_persistence_retries
                        .fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(self.config.poll_interval).await;
                }
                Err(error) => {
                    self.metrics
                        .persistence_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        edit_session_id = %record.id,
                        attempt_id = %record.attempt_id,
                        error = %error,
                        "finalized edit-learning session could not be persisted"
                    );
                    return;
                }
            }
        }
    }

    fn persist_session_update(&self, record: &EditSessionRecord) {
        let persisted = self.session_for_persistence(record);
        if let Err(error) = self.repository.update_session(&persisted) {
            self.metrics
                .persistence_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                edit_session_id = %record.id,
                attempt_id = %record.attempt_id,
                state = ?record.state,
                error = %error,
                "edit-learning session state persistence failed"
            );
        }
    }

    fn create_persisted_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError> {
        let persisted = self.session_for_persistence(record);
        let result = self.repository.create_session(&persisted);
        if result.is_ok() && persisted.original_text.is_empty() && !record.original_text.is_empty()
        {
            self.metrics
                .evidence_records_redacted
                .fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn append_persisted_revision(
        &self,
        record: &EditRevisionRecord,
    ) -> Result<Vec<Uuid>, RepositoryError> {
        let persisted = self.revision_for_persistence(record);
        let result = self.repository.append_revision_and_supersede(&persisted);
        if result.is_ok() && persisted.after_text.is_empty() && !record.after_text.is_empty() {
            self.metrics
                .evidence_records_redacted
                .fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn session_for_persistence(&self, record: &EditSessionRecord) -> EditSessionRecord {
        let mut persisted = record.clone();
        if !self.persist_evidence_text.load(Ordering::Acquire) {
            persisted.original_text.clear();
        }
        persisted
    }

    fn revision_for_persistence(&self, record: &EditRevisionRecord) -> EditRevisionRecord {
        let mut persisted = record.clone();
        if !self.persist_evidence_text.load(Ordering::Acquire) {
            persisted.after_text.clear();
        }
        persisted
    }
}

fn hash_text(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

impl RangeLocator {
    fn max_candidate_chars(&self) -> usize {
        self.original_text
            .chars()
            .count()
            .saturating_mul(8)
            .saturating_add(1_024)
            .min(MAX_TRACKED_TEXT_CHARS)
    }

    fn is_fully_bounded(&self) -> bool {
        !self.left_context.is_empty() && !self.right_context.is_empty()
    }

    fn confidence(&self) -> f64 {
        match (self.left_context.is_empty(), self.right_context.is_empty()) {
            (false, false) => 1.0,
            (true, true) => 0.6,
            _ => 0.75,
        }
    }

    fn from_post_insert(
        field_text: &str,
        inserted_text: &str,
        selection: Option<&TextRange>,
        context_chars: usize,
    ) -> Option<Self> {
        if inserted_text.is_empty() || inserted_text.chars().count() > MAX_TRACKED_TEXT_CHARS {
            return None;
        }
        let mut starts = field_text
            .match_indices(inserted_text)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        let start = if starts.len() == 1 {
            starts.pop()?
        } else if let Some(selection) = selection {
            let caret = selection
                .location_utf16
                .saturating_add(selection.length_utf16);
            starts.into_iter().find(|start| {
                utf16_len(&field_text[..*start]) + utf16_len(inserted_text) == caret
            })?
        } else {
            return None;
        };
        let end = start + inserted_text.len();
        let left_context = take_last_chars(&field_text[..start], context_chars);
        let right_context = take_first_chars(&field_text[end..], context_chars);
        Some(Self {
            original_text: inserted_text.to_owned(),
            left_context,
            right_context,
        })
    }

    fn project(&self, field_text: &str) -> Option<String> {
        if self.left_context.is_empty() && self.right_context.is_empty() {
            // The insertion occupied the whole field. An empty value is the
            // semantic boundary used by chat/search inputs after submit; never
            // let a later reuse of that field attach to this session. Polling
            // can miss the transient empty value, so an unanchored replacement
            // must also retain meaningful prefix/suffix evidence from the
            // dictated text. Ambiguous complete replacements fail closed.
            return (!field_text.is_empty()
                && field_text.chars().count() <= self.max_candidate_chars()
                && shares_edit_anchor(&self.original_text, field_text))
            .then(|| field_text.to_owned());
        }

        let starts = if self.left_context.is_empty() {
            vec![0]
        } else {
            field_text
                .match_indices(&self.left_context)
                .map(|(index, _)| index + self.left_context.len())
                .collect::<Vec<_>>()
        };
        let mut candidates = Vec::new();
        for start in starts {
            if self.right_context.is_empty() {
                let candidate = &field_text[start..];
                if candidate.chars().count() <= self.max_candidate_chars() {
                    candidates.push(candidate.to_owned());
                }
                continue;
            }
            for (offset, _) in field_text[start..].match_indices(&self.right_context) {
                let end = start + offset;
                let candidate = &field_text[start..end];
                if candidate.chars().count() <= self.max_candidate_chars() {
                    candidates.push(candidate.to_owned());
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        (candidates.len() == 1).then(|| candidates.remove(0))
    }
}

fn shares_edit_anchor(before: &str, after: &str) -> bool {
    if before == after {
        return true;
    }
    let before = before.chars().collect::<Vec<_>>();
    let after = after.chars().collect::<Vec<_>>();
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(left, right)| left == right)
        .count();
    let remaining = before.len().min(after.len()).saturating_sub(prefix);
    let suffix = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take(remaining)
        .take_while(|(left, right)| left == right)
        .count();
    let shared = prefix.saturating_add(suffix);
    let shorter = before.len().min(after.len());
    shared > 0 && shared.saturating_mul(3) >= shorter.max(1)
}

fn take_last_chars(value: &str, count: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    chars[chars.len().saturating_sub(count)..].iter().collect()
}

fn take_first_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn normalized_edit_distance(before: &str, after: &str) -> f64 {
    let before = before.chars().collect::<Vec<_>>();
    let after = after.chars().collect::<Vec<_>>();
    let denominator = before.len().max(after.len()).max(1) as f64;
    let mut previous = (0..=after.len()).collect::<Vec<_>>();
    for (before_index, before_char) in before.iter().enumerate() {
        let mut current = vec![before_index + 1; after.len() + 1];
        for (after_index, after_char) in after.iter().enumerate() {
            let substitution = previous[after_index] + usize::from(before_char != after_char);
            let deletion = previous[after_index + 1] + 1;
            let insertion = current[after_index] + 1;
            current[after_index + 1] = substitution.min(deletion).min(insertion);
        }
        previous = current;
    }
    previous[after.len()] as f64 / denominator
}

fn proposals_from_revision(
    original_text: &str,
    revision: &EditRevisionRecord,
) -> Vec<LearningProposalRecord> {
    let before = tokens(original_text);
    let after = tokens(&revision.after_text);
    if before == after || after.is_empty() {
        return Vec::new();
    }

    let now = Utc::now();
    let mut proposals = Vec::new();
    let added = after
        .iter()
        .filter(|token| !before.contains(token))
        .cloned()
        .collect::<Vec<_>>();
    let removed = before
        .iter()
        .filter(|token| !after.contains(token))
        .cloned()
        .collect::<Vec<_>>();

    // A deletion that preserves a specialized token is usually a correction
    // toward that token, not a phrase replacement (for example, removing an
    // ASR duplicate beside the correctly spelled acronym).
    if added.is_empty() && !removed.is_empty() {
        for token in &after {
            if before.contains(token) && looks_specialized(token) {
                proposals.push(term_proposal(token, revision, 0.68, now));
            }
        }
        if !proposals.is_empty() {
            proposals.sort_by(|left, right| left.payload_json.cmp(&right.payload_json));
            proposals.dedup_by(|left, right| {
                left.kind == right.kind && left.payload_json == right.payload_json
            });
            return proposals;
        }
    }

    if let Some((from_text, to_text)) = single_ascii_token_replacement(&before, &after) {
        let source_still_present = before
            .iter()
            .filter(|token| token.as_str() == from_text)
            .count()
            > after
                .iter()
                .filter(|token| token.as_str() == from_text)
                .count();
        if source_still_present
            && after.iter().any(|token| token == from_text)
            && looks_specialized(to_text)
        {
            proposals.push(term_proposal(to_text, revision, 0.85, now));
        } else {
            proposals.push(replacement_proposal(
                from_text, to_text, revision, 0.86, now,
            ));
        }
        return proposals;
    }

    let candidates = lumen_dictionary::candidates_from_edit(original_text, &revision.after_text);
    let learnable_terms = candidates
        .iter()
        .filter_map(|candidate| candidate.term.as_deref())
        .filter(|term| looks_learnable_term(term))
        .collect::<Vec<_>>();
    if learnable_terms.is_empty() {
        for candidate in candidates {
            if let (Some(from_text), Some(to_text)) = (candidate.from_text, candidate.to_text) {
                proposals.push(replacement_proposal(
                    &from_text, &to_text, revision, 0.82, now,
                ));
            }
        }
    } else {
        for term in learnable_terms {
            proposals.push(term_proposal(term, revision, 0.85, now));
        }
    }
    proposals.sort_by(|left, right| left.payload_json.cmp(&right.payload_json));
    proposals
        .dedup_by(|left, right| left.kind == right.kind && left.payload_json == right.payload_json);
    proposals
}

fn single_ascii_token_replacement<'a>(
    before: &'a [String],
    after: &'a [String],
) -> Option<(&'a str, &'a str)> {
    let prefix = before
        .iter()
        .zip(after)
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = before.len().min(after.len()).saturating_sub(prefix);
    let suffix = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take(max_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    let before_end = before.len().saturating_sub(suffix);
    let after_end = after.len().saturating_sub(suffix);
    let from = (before_end == prefix + 1).then(|| before[prefix].as_str())?;
    let to = (after_end == prefix + 1).then(|| after[prefix].as_str())?;
    let is_ascii_token = |value: &str| {
        !value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
    };
    (is_ascii_token(from) && is_ascii_token(to)).then_some((from, to))
}

fn looks_learnable_term(value: &str) -> bool {
    let value = value.trim();
    let count = value.chars().count();
    if value.is_empty() || count > 24 || value.contains(char::is_whitespace) {
        return false;
    }
    let has_cjk = value.chars().any(|character| {
        ('\u{3400}'..='\u{4dbf}').contains(&character)
            || ('\u{4e00}'..='\u{9fff}').contains(&character)
            || ('\u{f900}'..='\u{faff}').contains(&character)
    });
    if has_cjk {
        return true;
    }
    if count < 2
        || !value
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '.'))
    {
        return false;
    }
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "was"
            | "with"
    )
}

fn replacement_proposal(
    from_text: &str,
    to_text: &str,
    revision: &EditRevisionRecord,
    confidence: f64,
    created_at: DateTime<Utc>,
) -> LearningProposalRecord {
    LearningProposalRecord {
        id: Uuid::new_v4(),
        edit_session_id: revision.edit_session_id,
        revision_id: revision.id,
        kind: "replacement".into(),
        payload_json: serde_json::json!({
            "fromText": from_text,
            "toText": to_text,
            "evidenceRevisionId": revision.id,
        })
        .to_string(),
        confidence,
        risk: "confirmation_required".into(),
        status: "proposed".into(),
        policy_version: 1,
        created_at,
    }
}

fn term_proposal(
    term: &str,
    revision: &EditRevisionRecord,
    confidence: f64,
    created_at: DateTime<Utc>,
) -> LearningProposalRecord {
    LearningProposalRecord {
        id: Uuid::new_v4(),
        edit_session_id: revision.edit_session_id,
        revision_id: revision.id,
        kind: "term".into(),
        payload_json: serde_json::json!({
            "term": term,
            "evidenceRevisionId": revision.id,
        })
        .to_string(),
        confidence,
        risk: "reviewable".into(),
        status: "proposed".into(),
        policy_version: 1,
        created_at,
    }
}

fn tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in value.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '-' | '.') {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn looks_specialized(token: &str) -> bool {
    let count = token.chars().count();
    if !(2..=64).contains(&count) {
        return false;
    }
    let letters = token.chars().filter(|character| character.is_alphabetic());
    let letter_count = letters.clone().count();
    let uppercase_count = letters.filter(|character| character.is_uppercase()).count();
    let has_cjk = token.chars().any(|character| {
        ('\u{3400}'..='\u{4dbf}').contains(&character)
            || ('\u{4e00}'..='\u{9fff}').contains(&character)
            || ('\u{f900}'..='\u{faff}').contains(&character)
    });
    has_cjk
        || (letter_count >= 2 && uppercase_count == letter_count)
        || (uppercase_count > 0
            && token
                .chars()
                .any(|character| character.is_ascii_digit() || matches!(character, '_' | '-')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct MemoryRepository {
        sessions: Mutex<Vec<EditSessionRecord>>,
        revisions: Mutex<Vec<EditRevisionRecord>>,
        proposals: Mutex<Vec<LearningProposalRecord>>,
        notices: Mutex<Vec<FeedbackNotice>>,
        parent_failures_remaining: std::sync::atomic::AtomicUsize,
        fail_proposal_batch: AtomicBool,
    }

    impl EditLearningRepository for MemoryRepository {
        fn create_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError> {
            if self
                .parent_failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(RepositoryError::ParentNotReady);
            }
            self.sessions.lock().push(record.clone());
            Ok(())
        }

        fn update_session(&self, record: &EditSessionRecord) -> Result<(), RepositoryError> {
            if let Some(existing) = self
                .sessions
                .lock()
                .iter_mut()
                .find(|existing| existing.id == record.id)
            {
                *existing = record.clone();
            }
            Ok(())
        }

        fn append_revision_and_supersede(
            &self,
            record: &EditRevisionRecord,
        ) -> Result<Vec<Uuid>, RepositoryError> {
            self.revisions.lock().push(record.clone());
            let mut proposal_ids = Vec::new();
            for proposal in self.proposals.lock().iter_mut().filter(|proposal| {
                proposal.edit_session_id == record.edit_session_id
                    && proposal.revision_id != record.id
                    && proposal.status == "proposed"
            }) {
                proposal.status = "superseded".into();
                proposal_ids.push(proposal.id);
            }
            Ok(proposal_ids)
        }

        fn save_proposals_with_feedback(
            &self,
            records: &[LearningProposalRecord],
            notice: &FeedbackNotice,
        ) -> Result<(), RepositoryError> {
            if self.fail_proposal_batch.load(Ordering::Relaxed) {
                return Err(RepositoryError::Unavailable("test batch failure".into()));
            }
            self.proposals.lock().extend_from_slice(records);
            self.notices.lock().push(notice.clone());
            Ok(())
        }

        fn save_observation_failure_with_feedback(
            &self,
            record: &EditSessionRecord,
            notice: &FeedbackNotice,
        ) -> Result<(), RepositoryError> {
            if self
                .parent_failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(RepositoryError::ParentNotReady);
            }
            self.sessions.lock().push(record.clone());
            self.notices.lock().push(notice.clone());
            Ok(())
        }

        fn enqueue_feedback(&self, notice: &FeedbackNotice) -> Result<(), RepositoryError> {
            self.notices.lock().push(notice.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingFeedback {
        notices: Mutex<Vec<FeedbackNotice>>,
    }

    impl FeedbackSink for RecordingFeedback {
        fn publish(&self, notice: &FeedbackNotice) {
            self.notices.lock().push(notice.clone());
        }
    }

    struct StaticSurface {
        descriptor: SurfaceDescriptor,
        text: Mutex<String>,
        available: AtomicBool,
        failure_kind: Mutex<SurfaceErrorKind>,
        snapshots: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl SurfaceReservation for StaticSurface {
        fn descriptor(&self) -> &SurfaceDescriptor {
            &self.descriptor
        }

        async fn snapshot(&self) -> Result<SurfaceSnapshot, SurfaceError> {
            self.snapshots.fetch_add(1, Ordering::Relaxed);
            if !self.available.load(Ordering::Relaxed) {
                return Err(SurfaceError {
                    kind: *self.failure_kind.lock(),
                    code: "test_unavailable".into(),
                });
            }
            Ok(SurfaceSnapshot {
                text: self.text.lock().clone(),
                selection: None,
                observed_at: Utc::now(),
            })
        }
    }

    struct AppendExecutor {
        surface: Arc<StaticSurface>,
    }

    #[async_trait]
    impl InsertionExecutor for AppendExecutor {
        async fn insert(&self, text: &str) -> Result<InsertionOutcome, String> {
            self.surface.text.lock().push_str(text);
            Ok(InsertionOutcome {
                strategy: "test".into(),
            })
        }
    }

    struct PausingAppendExecutor {
        surface: Arc<StaticSurface>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl InsertionExecutor for PausingAppendExecutor {
        async fn insert(&self, text: &str) -> Result<InsertionOutcome, String> {
            self.surface.text.lock().push_str(text);
            self.started.notify_one();
            self.release.notified().await;
            Ok(InsertionOutcome {
                strategy: "test".into(),
            })
        }
    }

    fn surface(key: &str) -> Arc<StaticSurface> {
        Arc::new(StaticSurface {
            descriptor: SurfaceDescriptor {
                adapter_kind: "test".into(),
                surface_key: key.into(),
                target_app_name: Some("Editor".into()),
                target_bundle_id: Some("test.editor".into()),
                target_fingerprint: key.into(),
            },
            text: Mutex::new(String::new()),
            available: AtomicBool::new(true),
            failure_kind: Mutex::new(SurfaceErrorKind::TemporarilyUnavailable),
            snapshots: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn fast_config() -> EngineConfig {
        EngineConfig {
            poll_interval: Duration::from_millis(5),
            burst_quiescence: Duration::from_millis(10),
            learning_quiescence: Duration::from_millis(20),
            retention: Duration::from_secs(2),
            parent_persistence_timeout: Duration::from_millis(200),
            context_chars: 16,
            persist_evidence_text: true,
        }
    }

    #[tokio::test]
    async fn old_unbounded_observer_stops_before_the_next_insertion_executes() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            fast_config(),
        ));
        let surface = surface("same-surface-race");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "first".into(),
                },
            )
            .await
            .unwrap();

        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = {
            let engine = engine.clone();
            let surface = surface.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                engine
                    .insert(
                        surface.clone(),
                        Arc::new(PausingAppendExecutor {
                            surface,
                            started,
                            release,
                        }),
                        ObservedInsertion {
                            dictation_session_id: Uuid::new_v4(),
                            attempt_id: Uuid::new_v4(),
                            text: " second".into(),
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert!(repository.revisions.lock().is_empty());
        release.notify_one();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_new_insertion_flushes_the_last_observed_edit_before_stopping_the_old_session() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                burst_quiescence: Duration::from_secs(1),
                learning_quiescence: Duration::from_secs(1),
                ..fast_config()
            },
        ));
        let surface = surface("same-surface-pending-edit");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "serber".into(),
                },
            )
            .await
            .unwrap();

        wait_until(|| surface.snapshots.load(Ordering::Relaxed) >= 2).await;
        *surface.text.lock() = "server".into();
        let snapshots_before_edit = surface.snapshots.load(Ordering::Relaxed);
        wait_until(|| surface.snapshots.load(Ordering::Relaxed) > snapshots_before_edit).await;

        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: " next".into(),
                },
            )
            .await
            .unwrap();

        wait_until(|| !repository.proposals.lock().is_empty()).await;
        let revisions = repository.revisions.lock();
        assert!(revisions.iter().any(|revision| {
            revision.trigger == "session_lifecycle_boundary"
                && revision.after_text == "server"
                && revision.final_revision
        }));
        let proposals = repository.proposals.lock();
        assert!(proposals.iter().any(|proposal| {
            proposal.kind == "replacement"
                && proposal.payload_json.contains("serber")
                && proposal.payload_json.contains("server")
        }));
    }

    #[tokio::test]
    async fn clearing_a_reused_input_finalizes_before_unrelated_text_can_be_learned() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            fast_config(),
        ));
        let surface = surface("reused-chat-input");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();

        // Chat inputs are commonly cleared on Enter and then immediately reused.
        *surface.text.lock() = String::new();
        wait_until(|| engine.observability().active_sessions == 0).await;
        *surface.text.lock() = "unrelated secret".into();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(repository.revisions.lock().is_empty());
        let sessions = repository.sessions.lock();
        assert_eq!(sessions[0].state, EditSessionState::Finalized);
        assert_eq!(
            sessions[0].end_reason.as_deref(),
            Some("surface_content_removed")
        );
    }

    #[tokio::test]
    async fn rapid_clear_and_retype_between_polls_is_not_treated_as_an_edit() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                poll_interval: Duration::from_millis(10),
                burst_quiescence: Duration::from_millis(20),
                ..fast_config()
            },
        ));
        let surface = surface("rapidly-reused-chat-input");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "hello world".into(),
                },
            )
            .await
            .unwrap();

        // The observer can miss the transient empty value when the user sends
        // and immediately starts a new message inside one polling interval.
        *surface.text.lock() = "unrelated secret".into();
        wait_until(|| engine.observability().active_sessions == 0).await;

        assert!(repository.revisions.lock().is_empty());
        assert_eq!(
            repository.sessions.lock()[0].end_reason.as_deref(),
            Some("surface_content_removed")
        );
    }

    #[tokio::test]
    async fn submit_flushes_the_last_observed_edit_without_waiting_for_quiescence() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                burst_quiescence: Duration::from_millis(200),
                learning_quiescence: Duration::from_millis(200),
                ..fast_config()
            },
        ));
        let surface = surface("submit-flush");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "use serber".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "use server".into();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(repository.revisions.lock().is_empty());
        *surface.text.lock() = String::new();
        wait_until(|| engine.observability().active_sessions == 0).await;

        let revisions = repository.revisions.lock();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].after_text, "use server");
        assert_eq!(revisions[0].trigger, "surface_content_removed");
        assert!(revisions[0].final_revision);
        drop(revisions);
        assert_eq!(repository.proposals.lock().len(), 1);
        assert_eq!(repository.proposals.lock()[0].kind, "replacement");
    }

    #[tokio::test]
    async fn repeated_edit_bursts_append_revisions_without_finalizing_the_session() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                poll_interval: Duration::from_millis(10),
                burst_quiescence: Duration::from_millis(20),
                ..fast_config()
            },
        ));
        let surface = surface("long-edit");
        let executor = Arc::new(AppendExecutor {
            surface: surface.clone(),
        });
        let receipt = engine
            .insert(
                surface.clone(),
                executor,
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "hullo".into();
        wait_until(|| repository.revisions.lock().len() == 1).await;
        *surface.text.lock() = "hello world".into();
        wait_until(|| repository.revisions.lock().len() == 2).await;

        let revisions = repository.revisions.lock();
        assert_eq!(revisions[0].after_text, "hullo");
        assert_eq!(revisions[1].after_text, "hello world");
        assert_eq!(engine.observability().active_sessions, 1);
        let sessions = repository.sessions.lock();
        let session = sessions
            .iter()
            .find(|session| session.id == receipt.edit_session_id)
            .unwrap();
        assert_eq!(session.state, EditSessionState::Quiescent);
        assert_eq!(session.revision_count, 2);
        assert!(session.finalized_at.is_none());
    }

    #[tokio::test]
    async fn plaintext_edit_evidence_is_redacted_when_local_retention_is_disabled() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                persist_evidence_text: false,
                ..fast_config()
            },
        ));
        let surface = surface("private-edit");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "confidential draft".into(),
                },
            )
            .await
            .unwrap();
        *surface.text.lock() = "confidential corrected".into();
        wait_until(|| !repository.proposals.lock().is_empty()).await;

        assert_eq!(repository.sessions.lock()[0].original_text, "");
        assert_eq!(repository.revisions.lock()[0].after_text, "");
        assert_ne!(repository.sessions.lock()[0].original_text_hash, "");
        assert_ne!(repository.revisions.lock()[0].after_text_hash, "");
        assert_eq!(engine.observability().evidence_records_redacted, 2);
    }

    #[tokio::test]
    async fn temporarily_unavailable_surface_suspends_then_recovers_observation() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                poll_interval: Duration::from_millis(10),
                burst_quiescence: Duration::from_millis(20),
                ..fast_config()
            },
        ));
        let surface = surface("recoverable");
        let executor = Arc::new(AppendExecutor {
            surface: surface.clone(),
        });
        let receipt = engine
            .insert(
                surface.clone(),
                executor,
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "original".into(),
                },
            )
            .await
            .unwrap();

        surface.available.store(false, Ordering::Relaxed);
        wait_until(|| engine.observability().suspensions == 1).await;
        surface.available.store(true, Ordering::Relaxed);
        *surface.text.lock() = "original corrected".into();
        wait_until(|| engine.observability().recoveries == 1).await;
        wait_until(|| repository.revisions.lock().len() == 1).await;

        assert_eq!(
            repository.revisions.lock()[0].after_text,
            "original corrected"
        );
        assert_eq!(engine.observability().active_sessions, 1);
        let sessions = repository.sessions.lock();
        let session = sessions
            .iter()
            .find(|session| session.id == receipt.edit_session_id)
            .unwrap();
        assert_eq!(session.state, EditSessionState::Quiescent);
        assert_eq!(session.relocation_attempts, 1);
    }

    #[tokio::test]
    async fn removed_surface_finalizes_without_temporary_retry_state() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            fast_config(),
        ));
        let surface = surface("removed");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "original".into(),
                },
            )
            .await
            .unwrap();
        *surface.failure_kind.lock() = SurfaceErrorKind::TargetRemoved;
        surface.available.store(false, Ordering::Relaxed);

        wait_until(|| engine.observability().active_sessions == 0).await;
        assert_eq!(engine.observability().suspensions, 0);
        assert_eq!(
            repository.sessions.lock()[0].end_reason.as_deref(),
            Some("surface_target_removed")
        );
    }

    #[tokio::test]
    async fn deletion_with_retained_specialized_token_creates_term_proposal_and_durable_feedback() {
        let repository = Arc::new(MemoryRepository::default());
        let feedback = Arc::new(RecordingFeedback::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            feedback.clone(),
            EngineConfig {
                poll_interval: Duration::from_millis(10),
                burst_quiescence: Duration::from_millis(20),
                ..fast_config()
            },
        ));
        let surface = surface("specialized-term");
        let executor = Arc::new(AppendExecutor {
            surface: surface.clone(),
        });
        engine
            .insert(
                surface.clone(),
                executor,
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "DSS DSH".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "DSH".into();
        wait_until(|| !repository.proposals.lock().is_empty()).await;

        let proposals = repository.proposals.lock();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].kind, "term");
        let payload: serde_json::Value = serde_json::from_str(&proposals[0].payload_json).unwrap();
        assert_eq!(payload["term"], "DSH");
        drop(proposals);
        assert_eq!(repository.notices.lock().len(), 1);
        assert_eq!(feedback.notices.lock().len(), 1);
        assert_eq!(engine.observability().proposals_created, 1);
        assert_eq!(engine.observability().feedback_enqueued, 1);
    }

    #[tokio::test]
    async fn failed_proposal_batch_retries_without_publishing_false_success() {
        let repository = Arc::new(MemoryRepository::default());
        repository
            .fail_proposal_batch
            .store(true, Ordering::Relaxed);
        let feedback = Arc::new(RecordingFeedback::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            feedback.clone(),
            EngineConfig {
                learning_quiescence: Duration::from_millis(10),
                ..fast_config()
            },
        ));
        let surface = surface("failed-proposal-batch");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "DSS DSH".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "DSH".into();
        wait_until(|| engine.observability().persistence_failures >= 1).await;

        assert!(repository.proposals.lock().is_empty());
        assert!(repository.notices.lock().is_empty());
        assert!(feedback.notices.lock().is_empty());
        assert_eq!(engine.observability().proposals_created, 0);
        assert_eq!(engine.observability().feedback_enqueued, 0);
        repository
            .fail_proposal_batch
            .store(false, Ordering::Relaxed);
        wait_until(|| repository.notices.lock().len() == 1).await;
        assert_eq!(repository.proposals.lock().len(), 1);
        assert!(engine.observability().proposal_persistence_retries >= 1);
    }

    #[tokio::test]
    async fn failed_observation_is_persisted_and_reported_after_parent_attempt_arrives() {
        let repository = Arc::new(MemoryRepository {
            parent_failures_remaining: std::sync::atomic::AtomicUsize::new(1),
            ..MemoryRepository::default()
        });
        let feedback = Arc::new(RecordingFeedback::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            feedback.clone(),
            EngineConfig {
                poll_interval: Duration::from_millis(10),
                burst_quiescence: Duration::from_millis(20),
                ..fast_config()
            },
        ));

        let edit_session_id = engine.report_observation_failure(
            ObservedInsertion {
                dictation_session_id: Uuid::new_v4(),
                attempt_id: Uuid::new_v4(),
                text: "inserted text".into(),
            },
            &TargetHint {
                app_name: Some("Safari".into()),
                bundle_id: Some("com.apple.Safari".into()),
                process_id: Some(42),
            },
            "post_insert_anchor_unavailable",
        );

        wait_until(|| repository.sessions.lock().len() == 1).await;
        wait_until(|| repository.notices.lock().len() == 1).await;

        let sessions = repository.sessions.lock();
        assert_eq!(sessions[0].id, edit_session_id);
        assert_eq!(sessions[0].state, EditSessionState::Failed);
        assert_eq!(
            sessions[0].end_reason.as_deref(),
            Some("post_insert_anchor_unavailable")
        );
        drop(sessions);
        assert_eq!(repository.notices.lock()[0].kind, "observation_unavailable");
        assert_eq!(feedback.notices.lock().len(), 1);
        assert_eq!(engine.observability().sessions_failed_to_start, 1);
        assert_eq!(engine.observability().feedback_enqueued, 1);
    }

    #[tokio::test]
    async fn rapid_edit_bursts_only_publish_proposals_for_the_latest_revision() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                learning_quiescence: Duration::from_millis(60),
                ..fast_config()
            },
        ));
        let surface = surface("latest-net-edit");
        let executor = Arc::new(AppendExecutor {
            surface: surface.clone(),
        });
        engine
            .insert(
                surface.clone(),
                executor,
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "draft".into(),
                },
            )
            .await
            .unwrap();

        for (index, edited) in ["draft firstTerm", "draft secondTerm", "draft FinalTerm"]
            .into_iter()
            .enumerate()
        {
            *surface.text.lock() = edited.into();
            wait_until(|| repository.revisions.lock().len() == index + 1).await;
        }
        wait_until(|| repository.notices.lock().len() == 1).await;

        let revisions = repository.revisions.lock();
        let final_revision_id = revisions.last().unwrap().id;
        drop(revisions);
        assert!(repository
            .proposals
            .lock()
            .iter()
            .all(|proposal| proposal.revision_id == final_revision_id));
        assert_eq!(repository.notices.lock().len(), 1);
    }

    #[tokio::test]
    async fn transient_typing_then_returning_to_latest_revision_restarts_learning_quiescence() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            EngineConfig {
                burst_quiescence: Duration::from_millis(100),
                learning_quiescence: Duration::from_millis(60),
                ..fast_config()
            },
        ));
        let surface = surface("transient-return");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "serber".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "server".into();
        wait_until(|| repository.revisions.lock().len() == 1).await;
        let snapshots = surface.snapshots.load(Ordering::Relaxed);
        *surface.text.lock() = "serverx".into();
        wait_until(|| surface.snapshots.load(Ordering::Relaxed) > snapshots).await;
        *surface.text.lock() = "server".into();

        wait_until(|| repository.notices.lock().len() == 1).await;
        assert_eq!(repository.revisions.lock().len(), 1);
        assert_eq!(repository.proposals.lock()[0].kind, "replacement");
    }

    #[tokio::test]
    async fn later_revision_supersedes_already_published_candidates() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            fast_config(),
        ));
        let surface = surface("superseded-edit");
        let executor = Arc::new(AppendExecutor {
            surface: surface.clone(),
        });
        engine
            .insert(
                surface.clone(),
                executor,
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "draft".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "draft FirstTerm".into();
        wait_until(|| repository.notices.lock().len() == 1).await;
        *surface.text.lock() = "draft FinalTerm".into();
        wait_until(|| repository.revisions.lock().len() == 2).await;
        wait_until(|| {
            repository
                .proposals
                .lock()
                .iter()
                .any(|proposal| proposal.status == "superseded")
        })
        .await;
        wait_until(|| repository.notices.lock().len() == 3).await;

        let revisions = repository.revisions.lock();
        let final_revision_id = revisions.last().unwrap().id;
        drop(revisions);
        let proposals = repository.proposals.lock();
        assert!(proposals.iter().any(|proposal| {
            proposal.revision_id == final_revision_id && proposal.status == "proposed"
        }));
        assert!(proposals.iter().all(|proposal| {
            proposal.revision_id == final_revision_id || proposal.status == "superseded"
        }));
        assert_eq!(engine.observability().proposals_superseded, 1);
    }

    #[tokio::test]
    async fn undoing_to_the_original_supersedes_an_already_published_candidate() {
        let repository = Arc::new(MemoryRepository::default());
        let engine = Arc::new(EditLearningEngine::new(
            repository.clone(),
            Arc::new(RecordingFeedback::default()),
            fast_config(),
        ));
        let surface = surface("undo-edit");
        engine
            .insert(
                surface.clone(),
                Arc::new(AppendExecutor {
                    surface: surface.clone(),
                }),
                ObservedInsertion {
                    dictation_session_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                    text: "draft".into(),
                },
            )
            .await
            .unwrap();

        *surface.text.lock() = "draftTerm".into();
        wait_until(|| !repository.proposals.lock().is_empty()).await;
        *surface.text.lock() = "draft".into();
        wait_until(|| {
            repository
                .proposals
                .lock()
                .iter()
                .all(|proposal| proposal.status == "superseded")
        })
        .await;

        let revisions = repository.revisions.lock();
        assert_eq!(revisions.last().unwrap().trigger, "reverted_to_original");
        assert_eq!(revisions.last().unwrap().after_text, "draft");
        assert_eq!(engine.observability().proposals_superseded, 1);
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..100 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition was not reached before test timeout");
    }
}
