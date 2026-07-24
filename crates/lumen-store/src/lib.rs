//! SQLite store for Lumen ASR.

mod schema;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lumen_core::{
    AsrRuntimeDiagnostics, DictEntryKind, DictEntrySource, EditSource, FocusInfo, InsertStrategy,
    SessionRecord, SessionStatus,
};
use lumen_dictionary::DictionaryEntry;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_ATTEMPT_PAGE_SIZE: u32 = 100;
pub const MAX_ATTEMPT_PAGE_SIZE: u32 = 500;

/// One user edit associated with a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditEventRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub source: EditSource,
    pub before_text: String,
    pub after_text: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub attribution: EditAttribution,
}

/// Evidence tying an edit to one dictation attempt and one focused control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EditAttribution {
    pub schema_version: u32,
    pub attempt_id: Option<Uuid>,
    pub target_app_name: Option<String>,
    pub target_bundle_id: Option<String>,
    pub observer: Option<String>,
    pub target_fingerprint_hash: Option<String>,
    pub field_before_hash: Option<String>,
    pub field_after_hash: Option<String>,
    pub status: String,
}

impl Default for EditAttribution {
    fn default() -> Self {
        Self {
            schema_version: 1,
            attempt_id: None,
            target_app_name: None,
            target_bundle_id: None,
            observer: None,
            target_fingerprint_hash: None,
            field_before_hash: None,
            field_after_hash: None,
            status: "unattributed".into(),
        }
    }
}

/// Terminal outcome of the post-insert observation protocol. Unlike an edit
/// event, this record also exists when no edit was captured or tracking failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditObservationRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub source: String,
    pub status: String,
    pub end_reason: String,
    pub target_app_name: Option<String>,
    pub target_bundle_id: Option<String>,
    pub target_fingerprint_hash: Option<String>,
    pub inserted_text_hash: String,
    pub field_initial_hash: Option<String>,
    pub field_final_hash: Option<String>,
    pub normalized_edit_distance: Option<f64>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub edit_event_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Unknown,
}

impl AttemptStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

fn parse_attempt_status(value: &str) -> AttemptStatus {
    match value {
        "completed" => AttemptStatus::Completed,
        "failed" => AttemptStatus::Failed,
        "in_progress" => AttemptStatus::InProgress,
        _ => AttemptStatus::Unknown,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    Capture,
    Preprocess,
    Asr,
    Enhancement,
    Corrector,
    Insert,
    #[serde(other)]
    Unknown,
}

impl PipelineStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Preprocess => "preprocess",
            Self::Asr => "asr",
            Self::Enhancement => "enhancement",
            Self::Corrector => "corrector",
            Self::Insert => "insert",
            Self::Unknown => "unknown",
        }
    }
}

fn parse_pipeline_stage(value: &str) -> Option<PipelineStage> {
    match value {
        "capture" => Some(PipelineStage::Capture),
        "preprocess" => Some(PipelineStage::Preprocess),
        "asr" => Some(PipelineStage::Asr),
        "enhancement" => Some(PipelineStage::Enhancement),
        "corrector" => Some(PipelineStage::Corrector),
        "insert" => Some(PipelineStage::Insert),
        _ => Some(PipelineStage::Unknown),
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnhancementMode {
    #[default]
    None,
    QwenShadow,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineIssueKind {
    Fallback,
    InputUnavailable,
    ClipboardFailure,
    InjectionFailure,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InsertionOutcome {
    #[default]
    NotRequested,
    Copied,
    Inserted,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PipelineIdentity {
    pub schema_version: u32,
    pub asr_provider: String,
    pub asr_engine: String,
    pub asr_model: Option<String>,
    pub asr_model_revision: Option<String>,
    pub corrector_provider: String,
    pub corrector_engine: String,
    pub corrector_model: Option<String>,
    pub prompt_hash: Option<String>,
    pub prompt_hash_algorithm: Option<String>,
    pub temperature: Option<f64>,
    pub dictionary_context_hash: Option<String>,
    pub dictionary_context_hash_algorithm: Option<String>,
    pub dictionary_term_count: u32,
    pub dictionary_replacement_count: u32,
    pub enhancement_mode: EnhancementMode,
}

impl Default for PipelineIdentity {
    fn default() -> Self {
        Self {
            schema_version: 2,
            asr_provider: String::new(),
            asr_engine: String::new(),
            asr_model: None,
            asr_model_revision: None,
            corrector_provider: String::new(),
            corrector_engine: String::new(),
            corrector_model: None,
            prompt_hash: None,
            prompt_hash_algorithm: None,
            temperature: None,
            dictionary_context_hash: None,
            dictionary_context_hash_algorithm: None,
            dictionary_term_count: 0,
            dictionary_replacement_count: 0,
            enhancement_mode: EnhancementMode::None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PipelineStageIssue {
    pub stage: PipelineStage,
    pub kind: PipelineIssueKind,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextSnapshotRecord {
    pub capture_id: Uuid,
    pub session_id: Uuid,
    pub revision: u64,
    pub schema_version: u32,
    pub profile: String,
    pub target_generation: u64,
    pub started_at: DateTime<Utc>,
    pub frozen_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub manifest_path: String,
    pub source_presence_bitmap: u64,
    pub source_status_json: String,
    pub sanitized_hash: String,
    pub encryption: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextInputRef {
    pub capture_id: Uuid,
    pub revision: u64,
    pub snapshot_hash: String,
    pub context_schema_version: u32,
    pub capture_profile: String,
    pub source_presence_bitmap: u64,
    pub source_status_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextStageUsage {
    pub stage: PipelineStage,
    pub sources: Vec<String>,
    pub projection_schema_version: u32,
    pub projection_path: Option<String>,
    pub projection_hash: Option<String>,
    pub projection_chars: u32,
    pub captured: bool,
    pub selected: bool,
    pub consumed: bool,
    pub sent: bool,
    pub not_used_reason: Option<String>,
}

impl Default for ContextStageUsage {
    fn default() -> Self {
        Self {
            stage: PipelineStage::Unknown,
            sources: Vec::new(),
            projection_schema_version: 1,
            projection_path: None,
            projection_hash: None,
            projection_chars: 0,
            captured: false,
            selected: false,
            consumed: false,
            sent: false,
            not_used_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PipelineInputs {
    pub schema_version: u32,
    pub context: Option<ContextInputRef>,
    pub stage_usages: Vec<ContextStageUsage>,
}

impl Default for PipelineInputs {
    fn default() -> Self {
        Self {
            schema_version: 1,
            context: None,
            stage_usages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PipelineMetrics {
    pub schema_version: u32,
    pub audio_duration_ms: u64,
    pub preprocess_ms: f64,
    pub asr_ms: f64,
    pub enhancement_ms: f64,
    pub corrector_ms: f64,
    pub insert_ms: f64,
    pub total_ms: f64,
    pub asr_rtf: Option<f64>,
    pub asr_worker_reused: Option<bool>,
    pub asr_runtime: Option<AsrRuntimeDiagnostics>,
    pub corrector_fallback: bool,
    pub insertion_outcome: InsertionOutcome,
    /// Compatibility field derived from `insertion_outcome`.
    pub insert_succeeded: bool,
    pub stage_issues: Vec<PipelineStageIssue>,
}

#[derive(Deserialize)]
#[serde(default)]
struct PipelineMetricsDeserialize {
    schema_version: u32,
    audio_duration_ms: u64,
    preprocess_ms: f64,
    asr_ms: f64,
    enhancement_ms: f64,
    corrector_ms: f64,
    insert_ms: f64,
    total_ms: f64,
    asr_rtf: Option<f64>,
    asr_worker_reused: Option<bool>,
    asr_runtime: Option<AsrRuntimeDiagnostics>,
    corrector_fallback: bool,
    insertion_outcome: Option<InsertionOutcome>,
    insert_succeeded: bool,
    stage_issues: Vec<PipelineStageIssue>,
}

impl Default for PipelineMetricsDeserialize {
    fn default() -> Self {
        let current = PipelineMetrics::default();
        Self {
            schema_version: current.schema_version,
            audio_duration_ms: current.audio_duration_ms,
            preprocess_ms: current.preprocess_ms,
            asr_ms: current.asr_ms,
            enhancement_ms: current.enhancement_ms,
            corrector_ms: current.corrector_ms,
            insert_ms: current.insert_ms,
            total_ms: current.total_ms,
            asr_rtf: current.asr_rtf,
            asr_worker_reused: current.asr_worker_reused,
            asr_runtime: current.asr_runtime,
            corrector_fallback: current.corrector_fallback,
            insertion_outcome: None,
            insert_succeeded: current.insert_succeeded,
            stage_issues: current.stage_issues,
        }
    }
}

impl<'de> Deserialize<'de> for PipelineMetrics {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = PipelineMetricsDeserialize::deserialize(deserializer)?;
        let insertion_outcome = value.insertion_outcome.unwrap_or_else(|| {
            if value.insert_succeeded {
                InsertionOutcome::Inserted
            } else {
                InsertionOutcome::Unknown
            }
        });
        Ok(Self {
            schema_version: value.schema_version,
            audio_duration_ms: value.audio_duration_ms,
            preprocess_ms: value.preprocess_ms,
            asr_ms: value.asr_ms,
            enhancement_ms: value.enhancement_ms,
            corrector_ms: value.corrector_ms,
            insert_ms: value.insert_ms,
            total_ms: value.total_ms,
            asr_rtf: value.asr_rtf,
            asr_worker_reused: value.asr_worker_reused,
            asr_runtime: value.asr_runtime,
            corrector_fallback: value.corrector_fallback,
            insertion_outcome,
            insert_succeeded: insertion_outcome == InsertionOutcome::Inserted,
            stage_issues: value.stage_issues,
        })
    }
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self {
            schema_version: 3,
            audio_duration_ms: 0,
            preprocess_ms: 0.0,
            asr_ms: 0.0,
            enhancement_ms: 0.0,
            corrector_ms: 0.0,
            insert_ms: 0.0,
            total_ms: 0.0,
            asr_rtf: None,
            asr_worker_reused: None,
            asr_runtime: None,
            corrector_fallback: false,
            insertion_outcome: InsertionOutcome::NotRequested,
            insert_succeeded: false,
            stage_issues: Vec::new(),
        }
    }
}

impl PipelineMetrics {
    pub fn set_asr_rtf(&mut self) {
        self.asr_rtf =
            (self.audio_duration_ms > 0).then(|| self.asr_ms / self.audio_duration_ms as f64);
    }

    pub fn set_insertion_outcome(&mut self, outcome: InsertionOutcome) {
        self.insertion_outcome = outcome;
        self.insert_succeeded = outcome == InsertionOutcome::Inserted;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DictationAttemptRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub attempt_ordinal: u32,
    pub created_at: DateTime<Utc>,
    pub asr_raw: Option<String>,
    pub asr_enhanced: Option<String>,
    pub corrected: Option<String>,
    pub inserted: Option<String>,
    pub pipeline_identity: PipelineIdentity,
    pub pipeline_metrics: PipelineMetrics,
    pub pipeline_inputs: PipelineInputs,
    pub status: AttemptStatus,
    pub failed_stage: Option<PipelineStage>,
    pub failure_message: Option<String>,
    pub supersedes_attempt_id: Option<Uuid>,
}

impl DictationAttemptRecord {
    pub fn new(session_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            attempt_ordinal: 0,
            created_at: Utc::now(),
            asr_raw: None,
            asr_enhanced: None,
            corrected: None,
            inserted: None,
            pipeline_identity: PipelineIdentity::default(),
            pipeline_metrics: PipelineMetrics::default(),
            pipeline_inputs: PipelineInputs::default(),
            status: AttemptStatus::InProgress,
            failed_stage: None,
            failure_message: None,
            supersedes_attempt_id: None,
        }
    }
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("open db {}", path.display()))?;
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        schema::migrate(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save_session(&self, rec: &SessionRecord) -> Result<()> {
        save_session_on(&self.conn, rec)?;
        Ok(())
    }

    /// Append one immutable pipeline attempt and link it to the prior attempt.
    pub fn append_dictation_attempt(
        &self,
        mut record: DictationAttemptRecord,
    ) -> Result<DictationAttemptRecord> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        record = append_dictation_attempt_on(&transaction, record)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Persist the mutable session snapshot and its immutable attempt atomically.
    pub fn save_session_and_append_attempt(
        &self,
        session: &SessionRecord,
        record: DictationAttemptRecord,
    ) -> Result<DictationAttemptRecord> {
        if session.id != record.session_id {
            anyhow::bail!(
                "attempt session mismatch: session_id={}, attempt_session_id={}",
                session.id,
                record.session_id
            );
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        save_session_on(&transaction, session)?;
        let record = append_dictation_attempt_on(&transaction, record)?;
        transaction.commit()?;
        Ok(record)
    }

    pub fn list_dictation_attempts(
        &self,
        session_id: Uuid,
        limit: u32,
        before_ordinal: Option<u32>,
    ) -> Result<Vec<DictationAttemptRecord>> {
        let limit = limit.clamp(1, MAX_ATTEMPT_PAGE_SIZE);
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, session_id, attempt_ordinal, created_at,
                   asr_raw, asr_enhanced, corrected, inserted,
                   pipeline_identity_json, pipeline_metrics_json, pipeline_inputs_json,
                   status, failed_stage, failure_message, supersedes_attempt_id
            FROM dictation_attempts
            WHERE session_id=?1
              AND (?2 IS NULL OR attempt_ordinal < ?2)
            ORDER BY attempt_ordinal DESC
            LIMIT ?3
            "#,
        )?;
        let rows = statement.query_map(
            params![
                session_id.to_string(),
                before_ordinal.map(i64::from),
                i64::from(limit)
            ],
            map_attempt,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn save_context_snapshot(&self, record: &ContextSnapshotRecord) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO context_snapshots (
              capture_id, session_id, revision, schema_version, profile,
              target_generation, started_at, frozen_at, completed_at,
              manifest_path, source_presence_bitmap, source_status_json,
              sanitized_hash, encryption, status
            ) VALUES (
              ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15
            )
            "#,
            params![
                record.capture_id.to_string(),
                record.session_id.to_string(),
                i64::try_from(record.revision)?,
                i64::from(record.schema_version),
                record.profile,
                i64::try_from(record.target_generation)?,
                record.started_at.to_rfc3339(),
                record.frozen_at.to_rfc3339(),
                record.completed_at.map(|value| value.to_rfc3339()),
                record.manifest_path,
                i64::try_from(record.source_presence_bitmap)?,
                record.source_status_json,
                record.sanitized_hash,
                record.encryption,
                record.status,
            ],
        )?;
        Ok(())
    }

    pub fn list_context_snapshots(&self, session_id: Uuid) -> Result<Vec<ContextSnapshotRecord>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT capture_id, session_id, revision, schema_version, profile,
                   target_generation, started_at, frozen_at, completed_at,
                   manifest_path, source_presence_bitmap, source_status_json,
                   sanitized_hash, encryption, status
            FROM context_snapshots
            WHERE session_id=?1
            ORDER BY revision ASC
            "#,
        )?;
        let rows = statement.query_map(params![session_id.to_string()], map_context_snapshot)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, created_at, focused_app, focused_bundle_id,
                   asr_raw, corrected, pasted, asr_engine, corrector_engine,
                   insert_strategy, audio_path, status
            FROM sessions
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = stmt.query_map(params![limit], map_session)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_session(&self, id: Uuid) -> Result<Option<SessionRecord>> {
        self.conn
            .query_row(
                r#"
                SELECT id, created_at, focused_app, focused_bundle_id,
                       asr_raw, corrected, pasted, asr_engine, corrector_engine,
                       insert_strategy, audio_path, status
                FROM sessions WHERE id=?1
                "#,
                params![id.to_string()],
                map_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_session(&self, id: Uuid) -> Result<bool> {
        // edit_events cascade via FK
        let n = self
            .conn
            .execute("DELETE FROM sessions WHERE id=?1", params![id.to_string()])?;
        Ok(n > 0)
    }

    pub fn add_edit_event(
        &self,
        session_id: Uuid,
        source: EditSource,
        before: &str,
        after: &str,
    ) -> Result<Uuid> {
        self.add_edit_event_with_attribution(
            session_id,
            source,
            before,
            after,
            &EditAttribution::default(),
        )
    }

    /// Persists an edit together with evidence linking it to its insertion attempt.
    pub fn add_edit_event_with_attribution(
        &self,
        session_id: Uuid,
        source: EditSource,
        before: &str,
        after: &str,
        attribution: &EditAttribution,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let attribution_json = serde_json::to_string(attribution)?;
        self.conn.execute(
            r#"
            INSERT INTO edit_events (
              id, session_id, source, before_text, after_text, created_at, attribution_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                id.to_string(),
                session_id.to_string(),
                edit_source_str(source),
                before,
                after,
                Utc::now().to_rfc3339(),
                attribution_json,
            ],
        )?;
        Ok(id)
    }

    /// Atomically persists an attributed edit and its terminal observation.
    /// Dictionary learning runs after this transaction so it cannot hide or
    /// split the primary audit record.
    pub fn add_edit_event_with_observation(
        &self,
        session_id: Uuid,
        source: EditSource,
        before: &str,
        after: &str,
        attribution: &EditAttribution,
        observation: &EditObservationRecord,
    ) -> Result<Uuid> {
        anyhow::ensure!(
            observation.session_id == session_id
                && attribution.attempt_id == Some(observation.attempt_id),
            "edit event and observation attribution must identify the same attempt"
        );
        let transaction = self.conn.unchecked_transaction()?;
        let edit_event_id = Uuid::new_v4();
        transaction.execute(
            r#"
            INSERT INTO edit_events (
              id, session_id, source, before_text, after_text, created_at, attribution_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                edit_event_id.to_string(),
                session_id.to_string(),
                edit_source_str(source),
                before,
                after,
                Utc::now().to_rfc3339(),
                serde_json::to_string(attribution)?,
            ],
        )?;
        let mut linked_observation = observation.clone();
        linked_observation.edit_event_id = Some(edit_event_id);
        save_edit_observation_on(&transaction, &linked_observation)?;
        transaction.commit()?;
        Ok(edit_event_id)
    }

    pub fn list_edit_events(&self, session_id: Uuid) -> Result<Vec<EditEventRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, session_id, source, before_text, after_text, created_at, attribution_json
            FROM edit_events
            WHERE session_id=?1
            ORDER BY created_at ASC
            "#,
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], map_edit)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn save_edit_observation(&self, record: &EditObservationRecord) -> Result<()> {
        save_edit_observation_on(&self.conn, record)
    }

    pub fn list_edit_observations(&self, session_id: Uuid) -> Result<Vec<EditObservationRecord>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, session_id, attempt_id, source, status, end_reason,
                   target_app_name, target_bundle_id, target_fingerprint_hash,
                   inserted_text_hash, field_initial_hash, field_final_hash,
                   normalized_edit_distance, started_at, completed_at, edit_event_id
            FROM edit_observations
            WHERE session_id=?1
            ORDER BY completed_at ASC
            "#,
        )?;
        let rows = statement.query_map(params![session_id.to_string()], map_edit_observation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_dictionary_entry(&self, e: &DictionaryEntry) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO dictionary_entries (
              id, kind, term, from_text, to_text, source, hit_count, confirmed, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ON CONFLICT(id) DO UPDATE SET
              kind=excluded.kind,
              term=excluded.term,
              from_text=excluded.from_text,
              to_text=excluded.to_text,
              source=excluded.source,
              hit_count=excluded.hit_count,
              confirmed=excluded.confirmed,
              updated_at=excluded.updated_at
            "#,
            params![
                e.id.to_string(),
                dict_kind_str(e.kind),
                e.term,
                e.from_text,
                e.to_text,
                dict_source_str(e.source),
                e.hit_count,
                e.confirmed as i32,
                e.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_dictionary(&self) -> Result<Vec<DictionaryEntry>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, kind, term, from_text, to_text, source, hit_count, confirmed, updated_at
            FROM dictionary_entries
            ORDER BY updated_at DESC
            "#,
        )?;
        let rows = stmt.query_map([], map_dict)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_dictionary_entry(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM dictionary_entries WHERE id=?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn get_dictionary_entry(&self, id: Uuid) -> Result<Option<DictionaryEntry>> {
        self.conn
            .query_row(
                r#"
                SELECT id, kind, term, from_text, to_text, source, hit_count, confirmed, updated_at
                FROM dictionary_entries WHERE id=?1
                "#,
                params![id.to_string()],
                map_dict,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Find a replacement entry by exact from/to pair.
    pub fn find_replacement(&self, from: &str, to: &str) -> Result<Option<DictionaryEntry>> {
        self.conn
            .query_row(
                r#"
                SELECT id, kind, term, from_text, to_text, source, hit_count, confirmed, updated_at
                FROM dictionary_entries
                WHERE kind='replacement' AND from_text=?1 AND to_text=?2
                LIMIT 1
                "#,
                params![from, to],
                map_dict,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn find_term(&self, term: &str) -> Result<Option<DictionaryEntry>> {
        self.conn
            .query_row(
                r#"
                SELECT id, kind, term, from_text, to_text, source, hit_count, confirmed, updated_at
                FROM dictionary_entries
                WHERE kind='term' AND term=?1
                LIMIT 1
                "#,
                params![term],
                map_dict,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Count edit_events whose before/after contain the same replacement middle
    /// (exact match on before_text/after_text pair, or exact from→to as full strings).
    pub fn count_identical_edits(&self, before: &str, after: &str) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*) FROM edit_events
            WHERE before_text=?1 AND after_text=?2
            "#,
            params![before, after],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn list_recent_edit_events(&self, limit: u32) -> Result<Vec<EditEventRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, session_id, source, before_text, after_text, created_at, attribution_json
            FROM edit_events
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = stmt.query_map(params![limit], map_edit)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn save_edit_observation_on(connection: &Connection, record: &EditObservationRecord) -> Result<()> {
    let changed = connection.execute(
        r#"
        INSERT INTO edit_observations (
          id, session_id, attempt_id, source, status, end_reason,
          target_app_name, target_bundle_id, target_fingerprint_hash,
          inserted_text_hash, field_initial_hash, field_final_hash,
          normalized_edit_distance, started_at, completed_at, edit_event_id
        )
        SELECT
          ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16
        FROM dictation_attempts AS attempt
        WHERE attempt.id=?3
          AND attempt.session_id=?2
          AND (
            ?16 IS NULL OR EXISTS (
              SELECT 1
              FROM edit_events AS edit
              WHERE edit.id=?16 AND edit.session_id=?2
            )
          )
        ON CONFLICT(attempt_id) DO UPDATE SET
          status=excluded.status,
          end_reason=excluded.end_reason,
          target_app_name=excluded.target_app_name,
          target_bundle_id=excluded.target_bundle_id,
          target_fingerprint_hash=excluded.target_fingerprint_hash,
          inserted_text_hash=excluded.inserted_text_hash,
          field_initial_hash=excluded.field_initial_hash,
          field_final_hash=excluded.field_final_hash,
          normalized_edit_distance=excluded.normalized_edit_distance,
          completed_at=excluded.completed_at,
          edit_event_id=excluded.edit_event_id
        "#,
        params![
            record.id.to_string(),
            record.session_id.to_string(),
            record.attempt_id.to_string(),
            record.source,
            record.status,
            record.end_reason,
            record.target_app_name,
            record.target_bundle_id,
            record.target_fingerprint_hash,
            record.inserted_text_hash,
            record.field_initial_hash,
            record.field_final_hash,
            record.normalized_edit_distance,
            record.started_at.to_rfc3339(),
            record.completed_at.to_rfc3339(),
            record.edit_event_id.map(|value| value.to_string()),
        ],
    )?;
    if changed == 0 {
        anyhow::bail!(
            "edit observation must reference an attempt and edit event from the same session"
        );
    }
    Ok(())
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
        created_at: parse_dt(&row.get::<_, String>(1)?),
        focus: FocusInfo {
            app_name: row.get(2)?,
            bundle_id: row.get(3)?,
            window_title: None,
        },
        asr_raw: row.get(4)?,
        corrected: row.get(5)?,
        pasted: row.get(6)?,
        asr_engine: row.get(7)?,
        corrector_engine: row.get(8)?,
        insert_strategy: parse_strategy(&row.get::<_, String>(9)?),
        audio_path: row.get(10)?,
        status: parse_status(&row.get::<_, String>(11)?),
    })
}

fn map_attempt(row: &rusqlite::Row<'_>) -> rusqlite::Result<DictationAttemptRecord> {
    let identity_json: String = row.get(8)?;
    let metrics_json: String = row.get(9)?;
    let inputs_json: String = row.get(10)?;
    let pipeline_identity = serde_json::from_str(&identity_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let pipeline_metrics = serde_json::from_str(&metrics_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let pipeline_inputs = serde_json::from_str(&inputs_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let id = parse_uuid_column(row, 0)?;
    let session_id = parse_uuid_column(row, 1)?;
    let attempt_ordinal = parse_u32_column(row, 2)?;
    let supersedes_attempt_id = match row.get::<_, Option<String>>(14)? {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                14,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    Ok(DictationAttemptRecord {
        id,
        session_id,
        attempt_ordinal,
        created_at: parse_dt(&row.get::<_, String>(3)?),
        asr_raw: row.get(4)?,
        asr_enhanced: row.get(5)?,
        corrected: row.get(6)?,
        inserted: row.get(7)?,
        pipeline_identity,
        pipeline_metrics,
        pipeline_inputs,
        status: parse_attempt_status(&row.get::<_, String>(11)?),
        failed_stage: row
            .get::<_, Option<String>>(12)?
            .and_then(|value| parse_pipeline_stage(&value)),
        failure_message: row.get(13)?,
        supersedes_attempt_id,
    })
}

fn map_context_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextSnapshotRecord> {
    Ok(ContextSnapshotRecord {
        capture_id: parse_uuid_column(row, 0)?,
        session_id: parse_uuid_column(row, 1)?,
        revision: parse_u64_column(row, 2)?,
        schema_version: parse_u32_column(row, 3)?,
        profile: row.get(4)?,
        target_generation: parse_u64_column(row, 5)?,
        started_at: parse_dt(&row.get::<_, String>(6)?),
        frozen_at: parse_dt(&row.get::<_, String>(7)?),
        completed_at: row
            .get::<_, Option<String>>(8)?
            .map(|value| parse_dt(&value)),
        manifest_path: row.get(9)?,
        source_presence_bitmap: parse_u64_column(row, 10)?,
        source_status_json: row.get(11)?,
        sanitized_hash: row.get(12)?,
        encryption: row.get(13)?,
        status: row.get(14)?,
    })
}

fn map_edit_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<EditObservationRecord> {
    Ok(EditObservationRecord {
        id: parse_uuid_column(row, 0)?,
        session_id: parse_uuid_column(row, 1)?,
        attempt_id: parse_uuid_column(row, 2)?,
        source: row.get(3)?,
        status: row.get(4)?,
        end_reason: row.get(5)?,
        target_app_name: row.get(6)?,
        target_bundle_id: row.get(7)?,
        target_fingerprint_hash: row.get(8)?,
        inserted_text_hash: row.get(9)?,
        field_initial_hash: row.get(10)?,
        field_final_hash: row.get(11)?,
        normalized_edit_distance: row.get(12)?,
        started_at: parse_dt(&row.get::<_, String>(13)?),
        completed_at: parse_dt(&row.get::<_, String>(14)?),
        edit_event_id: row
            .get::<_, Option<String>>(15)?
            .map(|value| Uuid::parse_str(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    15,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
    })
}

fn parse_uuid_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let value: String = row.get(index)?;
    Uuid::parse_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_u32_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value: i64 = row.get(index)?;
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn parse_u64_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn save_session_on(conn: &Connection, rec: &SessionRecord) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO sessions (
          id, created_at, focused_app, focused_bundle_id,
          asr_raw, corrected, pasted, asr_engine, corrector_engine,
          insert_strategy, audio_path, status
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
        ON CONFLICT(id) DO UPDATE SET
          asr_raw=excluded.asr_raw,
          corrected=excluded.corrected,
          pasted=excluded.pasted,
          asr_engine=excluded.asr_engine,
          corrector_engine=excluded.corrector_engine,
          insert_strategy=excluded.insert_strategy,
          audio_path=excluded.audio_path,
          status=excluded.status
        "#,
        params![
            rec.id.to_string(),
            rec.created_at.to_rfc3339(),
            rec.focus.app_name,
            rec.focus.bundle_id,
            rec.asr_raw,
            rec.corrected,
            rec.pasted,
            rec.asr_engine,
            rec.corrector_engine,
            strategy_str(rec.insert_strategy),
            rec.audio_path,
            status_str(rec.status),
        ],
    )?;
    Ok(())
}

fn append_dictation_attempt_on(
    transaction: &Transaction<'_>,
    mut record: DictationAttemptRecord,
) -> Result<DictationAttemptRecord> {
    let latest: Option<(Uuid, u32)> = transaction
        .query_row(
            r#"
            SELECT id, attempt_ordinal
            FROM dictation_attempts
            WHERE session_id=?1
            ORDER BY attempt_ordinal DESC
            LIMIT 1
            "#,
            params![record.session_id.to_string()],
            |row| Ok((parse_uuid_column(row, 0)?, parse_u32_column(row, 1)?)),
        )
        .optional()?;
    record.attempt_ordinal = match latest.as_ref() {
        Some((_, ordinal)) => ordinal.checked_add(1).context("attempt ordinal overflow")?,
        None => 1,
    };
    record.supersedes_attempt_id = latest.map(|(id, _)| id);
    let identity_json = serde_json::to_string(&record.pipeline_identity)?;
    let metrics_json = serde_json::to_string(&record.pipeline_metrics)?;
    let inputs_json = serde_json::to_string(&record.pipeline_inputs)?;
    transaction.execute(
        r#"
        INSERT INTO dictation_attempts (
          id, session_id, attempt_ordinal, created_at,
          asr_raw, asr_enhanced, corrected, inserted,
          pipeline_identity_json, pipeline_metrics_json, pipeline_inputs_json,
          status, failed_stage, failure_message, supersedes_attempt_id
        ) VALUES (
          ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15
        )
        "#,
        params![
            record.id.to_string(),
            record.session_id.to_string(),
            record.attempt_ordinal as i64,
            record.created_at.to_rfc3339(),
            record.asr_raw,
            record.asr_enhanced,
            record.corrected,
            record.inserted,
            identity_json,
            metrics_json,
            inputs_json,
            record.status.as_str(),
            record.failed_stage.map(PipelineStage::as_str),
            record.failure_message,
            record.supersedes_attempt_id.map(|value| value.to_string()),
        ],
    )?;
    Ok(record)
}

fn map_dict(row: &rusqlite::Row<'_>) -> rusqlite::Result<DictionaryEntry> {
    Ok(DictionaryEntry {
        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
        kind: parse_dict_kind(&row.get::<_, String>(1)?),
        term: row.get(2)?,
        from_text: row.get(3)?,
        to_text: row.get(4)?,
        source: parse_dict_source(&row.get::<_, String>(5)?),
        hit_count: row.get::<_, i64>(6)? as u32,
        confirmed: row.get::<_, i64>(7)? != 0,
        updated_at: parse_dt(&row.get::<_, String>(8)?),
    })
}

fn map_edit(row: &rusqlite::Row<'_>) -> rusqlite::Result<EditEventRecord> {
    let attribution_json: String = row.get(6)?;
    let attribution = serde_json::from_str(&attribution_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(EditEventRecord {
        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
        session_id: Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_default(),
        source: parse_edit_source(&row.get::<_, String>(2)?),
        before_text: row.get(3)?,
        after_text: row.get(4)?,
        created_at: parse_dt(&row.get::<_, String>(5)?),
        attribution,
    })
}

fn parse_edit_source(s: &str) -> EditSource {
    match s {
        "post_paste_ax" => EditSource::PostPasteAx,
        "post_paste_pane" => EditSource::PostPastePane,
        "manual" => EditSource::Manual,
        _ => EditSource::PreInsertUi,
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn strategy_str(s: InsertStrategy) -> &'static str {
    match s {
        InsertStrategy::Paste => "paste",
        InsertStrategy::Ax => "ax",
        InsertStrategy::Type => "type",
        InsertStrategy::CopyOnly => "copy_only",
        InsertStrategy::None => "none",
    }
}

fn parse_strategy(s: &str) -> InsertStrategy {
    match s {
        "paste" => InsertStrategy::Paste,
        "ax" => InsertStrategy::Ax,
        "type" => InsertStrategy::Type,
        "copy_only" => InsertStrategy::CopyOnly,
        _ => InsertStrategy::None,
    }
}

fn status_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::InProgress => "in_progress",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> SessionStatus {
    match s {
        "completed" => SessionStatus::Completed,
        "cancelled" => SessionStatus::Cancelled,
        "failed" => SessionStatus::Failed,
        _ => SessionStatus::InProgress,
    }
}

fn edit_source_str(s: EditSource) -> &'static str {
    match s {
        EditSource::PreInsertUi => "pre_insert_ui",
        EditSource::PostPasteAx => "post_paste_ax",
        EditSource::PostPastePane => "post_paste_pane",
        EditSource::Manual => "manual",
    }
}

fn dict_kind_str(k: DictEntryKind) -> &'static str {
    match k {
        DictEntryKind::Term => "term",
        DictEntryKind::Replacement => "replacement",
    }
}

fn parse_dict_kind(s: &str) -> DictEntryKind {
    match s {
        "replacement" => DictEntryKind::Replacement,
        _ => DictEntryKind::Term,
    }
}

fn dict_source_str(s: DictEntrySource) -> &'static str {
    match s {
        DictEntrySource::Manual => "manual",
        DictEntrySource::Learned => "learned",
    }
}

fn parse_dict_source(s: &str) -> DictEntrySource {
    match s {
        "learned" => DictEntrySource::Learned,
        _ => DictEntrySource::Manual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_dictionary::DictionaryEntry;

    #[test]
    fn session_and_dict_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let store = Store::open(&db).unwrap();

        let mut rec = SessionRecord::new();
        rec.asr_raw = Some("hello".into());
        rec.status = SessionStatus::Completed;
        rec.insert_strategy = InsertStrategy::Paste;
        store.save_session(&rec).unwrap();

        let list = store.list_sessions(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].asr_raw.as_deref(), Some("hello"));

        let entry = DictionaryEntry::term("Morpho");
        store.upsert_dictionary_entry(&entry).unwrap();
        let d = store.list_dictionary().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].term.as_deref(), Some("Morpho"));

        store
            .add_edit_event(rec.id, EditSource::PreInsertUi, "a", "b")
            .unwrap();

        let got = store.get_session(rec.id).unwrap().expect("session");
        assert_eq!(got.asr_raw.as_deref(), Some("hello"));

        let edits = store.list_edit_events(rec.id).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].before_text, "a");
        assert_eq!(edits[0].after_text, "b");

        assert!(store.delete_session(rec.id).unwrap());
        assert!(store.get_session(rec.id).unwrap().is_none());
        assert!(store.list_edit_events(rec.id).unwrap().is_empty());
    }

    #[test]
    fn attributed_edit_round_trips_attempt_target_and_field_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("attributed-edit.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        let attempt_id = Uuid::new_v4();
        let attribution = EditAttribution {
            attempt_id: Some(attempt_id),
            target_app_name: Some("TextEdit".into()),
            target_bundle_id: Some("com.apple.TextEdit".into()),
            observer: Some("focused_field_poll_v2".into()),
            target_fingerprint_hash: Some("target-hash".into()),
            field_before_hash: Some("before-hash".into()),
            field_after_hash: Some("after-hash".into()),
            status: "confirmed_same_field_span".into(),
            ..EditAttribution::default()
        };

        store
            .add_edit_event_with_attribution(
                session.id,
                EditSource::PostPasteAx,
                "Lumen Asr",
                "Lumen ASR",
                &attribution,
            )
            .unwrap();

        let edits = store.list_edit_events(session.id).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].before_text, "Lumen Asr");
        assert_eq!(edits[0].after_text, "Lumen ASR");
        assert_eq!(edits[0].attribution, attribution);
        let recent = store.list_recent_edit_events(1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].attribution, attribution);
    }

    #[test]
    fn pane_edit_source_round_trips_without_collapsing_to_ax() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("pane-edit.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();

        store
            .add_edit_event(
                session.id,
                EditSource::PostPastePane,
                "Lumen Asr",
                "Lumen ASR",
            )
            .unwrap();

        let edits = store.list_edit_events(session.id).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].source, EditSource::PostPastePane);
    }

    #[test]
    fn edit_observation_records_both_terminal_reason_and_linked_edit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("edit-observation.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        let attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
            .unwrap();
        let edit_id = store
            .add_edit_event(session.id, EditSource::PostPasteAx, "Cortex", "Codex")
            .unwrap();
        let now = Utc::now();
        let observation = EditObservationRecord {
            id: Uuid::new_v4(),
            session_id: session.id,
            attempt_id: attempt.id,
            source: "focused_field_poll_v3".into(),
            status: "completed_with_edit".into(),
            end_reason: "stable_edit_captured".into(),
            target_app_name: Some("TextEdit".into()),
            target_bundle_id: Some("com.apple.TextEdit".into()),
            target_fingerprint_hash: Some("target".into()),
            inserted_text_hash: "inserted".into(),
            field_initial_hash: Some("initial".into()),
            field_final_hash: Some("final".into()),
            normalized_edit_distance: Some(0.33),
            started_at: now,
            completed_at: now,
            edit_event_id: Some(edit_id),
        };

        store.save_edit_observation(&observation).unwrap();
        let records = store.list_edit_observations(session.id).unwrap();

        assert_eq!(records, vec![observation]);
    }

    #[test]
    fn edit_observation_rejects_cross_session_attempts_and_edit_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("edit-observation-integrity.sqlite")).unwrap();
        let first = SessionRecord::new();
        let second = SessionRecord::new();
        store.save_session(&first).unwrap();
        store.save_session(&second).unwrap();
        let first_attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(first.id))
            .unwrap();
        let second_attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(second.id))
            .unwrap();
        let second_edit = store
            .add_edit_event(second.id, EditSource::PostPasteAx, "before", "after")
            .unwrap();
        let now = Utc::now();
        let base = EditObservationRecord {
            id: Uuid::new_v4(),
            session_id: first.id,
            attempt_id: first_attempt.id,
            source: "focused_field_poll_v3".into(),
            status: "completed_no_edit".into(),
            end_reason: "observation_window_elapsed".into(),
            target_app_name: None,
            target_bundle_id: None,
            target_fingerprint_hash: None,
            inserted_text_hash: "inserted".into(),
            field_initial_hash: None,
            field_final_hash: None,
            normalized_edit_distance: Some(0.0),
            started_at: now,
            completed_at: now,
            edit_event_id: None,
        };

        let mut wrong_attempt = base.clone();
        wrong_attempt.attempt_id = second_attempt.id;
        assert!(store.save_edit_observation(&wrong_attempt).is_err());

        let mut wrong_edit = base;
        wrong_edit.edit_event_id = Some(second_edit);
        assert!(store.save_edit_observation(&wrong_edit).is_err());
        assert!(store.list_edit_observations(first.id).unwrap().is_empty());
    }

    #[test]
    fn attributed_edit_and_terminal_observation_commit_or_roll_back_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("atomic-edit-observation.sqlite")).unwrap();
        let first = SessionRecord::new();
        let second = SessionRecord::new();
        store.save_session(&first).unwrap();
        store.save_session(&second).unwrap();
        let first_attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(first.id))
            .unwrap();
        let second_attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(second.id))
            .unwrap();
        let now = Utc::now();
        let observation = EditObservationRecord {
            id: Uuid::new_v4(),
            session_id: first.id,
            attempt_id: first_attempt.id,
            source: "focused_field_poll_v3".into(),
            status: "completed_with_edit".into(),
            end_reason: "stable_edit_captured".into(),
            target_app_name: Some("TextEdit".into()),
            target_bundle_id: Some("com.apple.TextEdit".into()),
            target_fingerprint_hash: Some("target".into()),
            inserted_text_hash: "inserted".into(),
            field_initial_hash: Some("before".into()),
            field_final_hash: Some("after".into()),
            normalized_edit_distance: Some(0.25),
            started_at: now,
            completed_at: now,
            edit_event_id: None,
        };
        let attribution = EditAttribution {
            attempt_id: Some(first_attempt.id),
            status: "confirmed_same_field_span".into(),
            ..EditAttribution::default()
        };
        let edit_event_id = store
            .add_edit_event_with_observation(
                first.id,
                EditSource::PostPasteAx,
                "Cortex",
                "Codex",
                &attribution,
                &observation,
            )
            .unwrap();
        assert_eq!(
            store.list_edit_observations(first.id).unwrap()[0].edit_event_id,
            Some(edit_event_id)
        );
        assert_eq!(store.list_edit_events(first.id).unwrap().len(), 1);

        let mut invalid_observation = observation;
        invalid_observation.id = Uuid::new_v4();
        invalid_observation.attempt_id = second_attempt.id;
        let invalid_attribution = EditAttribution {
            attempt_id: Some(second_attempt.id),
            status: "confirmed_same_field_span".into(),
            ..EditAttribution::default()
        };
        assert!(store
            .add_edit_event_with_observation(
                first.id,
                EditSource::PostPasteAx,
                "bad",
                "data",
                &invalid_attribution,
                &invalid_observation,
            )
            .is_err());
        assert_eq!(store.list_edit_events(first.id).unwrap().len(), 1);
    }

    #[test]
    fn migrated_legacy_edit_defaults_deserialize_through_all_store_views() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("legacy-edit.sqlite");
        let session_id = Uuid::new_v4();
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                    INSERT INTO schema_migrations (version) VALUES (3);
                    CREATE TABLE sessions (
                      id TEXT PRIMARY KEY NOT NULL,
                      created_at TEXT NOT NULL,
                      focused_app TEXT,
                      focused_bundle_id TEXT,
                      asr_raw TEXT,
                      corrected TEXT,
                      pasted TEXT,
                      asr_engine TEXT,
                      corrector_engine TEXT,
                      insert_strategy TEXT NOT NULL DEFAULT 'none',
                      audio_path TEXT,
                      status TEXT NOT NULL DEFAULT 'in_progress'
                    );
                    CREATE TABLE edit_events (
                      id TEXT PRIMARY KEY NOT NULL,
                      session_id TEXT NOT NULL,
                      source TEXT NOT NULL,
                      before_text TEXT NOT NULL,
                      after_text TEXT NOT NULL,
                      created_at TEXT NOT NULL
                    );
                    "#,
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO sessions (id, created_at, status) VALUES (?1, ?2, ?3)",
                    params![session_id.to_string(), "2026-07-23T00:00:00Z", "completed"],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO edit_events (
                       id, session_id, source, before_text, after_text, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        Uuid::new_v4().to_string(),
                        session_id.to_string(),
                        "post_paste_ax",
                        "旧",
                        "新",
                        "2026-07-23T00:00:01Z"
                    ],
                )
                .unwrap();
        }

        let store = Store::open(&database).unwrap();
        let by_session = store.list_edit_events(session_id).unwrap();
        let recent = store.list_recent_edit_events(1).unwrap();

        assert_eq!(by_session.len(), 1);
        assert_eq!(by_session[0].attribution, EditAttribution::default());
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].attribution, EditAttribution::default());
    }

    #[test]
    fn attempts_are_append_only_and_preserve_pipeline_stages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("attempts.sqlite")).unwrap();
        let mut session = SessionRecord::new();
        session.status = SessionStatus::Completed;
        store.save_session(&session).unwrap();

        let mut first = DictationAttemptRecord::new(session.id);
        first.status = AttemptStatus::Completed;
        first.asr_raw = Some("cotex".into());
        first.asr_enhanced = Some("cotex".into());
        first.corrected = Some("Codex".into());
        first.inserted = Some("Codex".into());
        first.pipeline_identity.asr_engine = "qwen".into();
        first.pipeline_identity.enhancement_mode = EnhancementMode::None;
        first.pipeline_metrics.audio_duration_ms = 1_000;
        first.pipeline_metrics.asr_ms = 80.0;
        first
            .pipeline_metrics
            .stage_issues
            .push(PipelineStageIssue {
                stage: PipelineStage::Corrector,
                kind: PipelineIssueKind::Fallback,
                message: "timeout".into(),
            });
        first.pipeline_metrics.set_asr_rtf();
        let first = store.append_dictation_attempt(first).unwrap();

        let mut retry = DictationAttemptRecord::new(session.id);
        retry.status = AttemptStatus::Failed;
        retry.failed_stage = Some(PipelineStage::Asr);
        retry.failure_message = Some("worker exited".into());
        let retry = store.append_dictation_attempt(retry).unwrap();

        let attempts = store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap();
        let stored_first = attempts
            .iter()
            .find(|attempt| attempt.attempt_ordinal == 1)
            .unwrap();
        let stored_retry = attempts
            .iter()
            .find(|attempt| attempt.attempt_ordinal == 2)
            .unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(stored_first.id, first.id);
        assert_eq!(stored_first.asr_raw.as_deref(), Some("cotex"));
        assert_eq!(stored_first.asr_enhanced.as_deref(), Some("cotex"));
        assert_eq!(stored_first.corrected.as_deref(), Some("Codex"));
        assert_eq!(stored_first.inserted.as_deref(), Some("Codex"));
        assert_eq!(stored_first.pipeline_metrics.asr_rtf, Some(0.08));
        assert_eq!(
            stored_first.pipeline_metrics.stage_issues,
            vec![PipelineStageIssue {
                stage: PipelineStage::Corrector,
                kind: PipelineIssueKind::Fallback,
                message: "timeout".into(),
            }]
        );
        assert_eq!(stored_retry.supersedes_attempt_id, Some(first.id));
        assert_eq!(stored_retry.failed_stage, Some(PipelineStage::Asr));
        assert_eq!(retry.supersedes_attempt_id, Some(first.id));
    }

    #[test]
    fn attempt_round_trips_asr_runtime_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runtime-diagnostics.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();

        let mut attempt = DictationAttemptRecord::new(session.id);
        attempt.pipeline_metrics.asr_runtime = Some(AsrRuntimeDiagnostics {
            worker_reused: Some(true),
            model: Some("Qwen3-ASR-0.6B-8bit".into()),
            token_evidence: vec![lumen_core::AsrTokenEvidence {
                token_index: 3,
                token_id: 42,
                selected_logprob: -0.5,
                ..lumen_core::AsrTokenEvidence::default()
            }],
            qwen: Some(lumen_core::QwenRuntimeMetrics {
                chunk_count: Some(1),
                audio_encode_count: Some(1),
                prompt_prefill_count: Some(1),
                ..lumen_core::QwenRuntimeMetrics::default()
            }),
            ..AsrRuntimeDiagnostics::default()
        });
        store.append_dictation_attempt(attempt).unwrap();

        let stored = store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap();
        let diagnostics = stored[0].pipeline_metrics.asr_runtime.as_ref().unwrap();
        assert_eq!(stored[0].pipeline_metrics.schema_version, 3);
        assert_eq!(diagnostics.worker_reused, Some(true));
        assert_eq!(diagnostics.token_evidence[0].token_index, 3);
        assert_eq!(
            diagnostics.qwen.as_ref().unwrap().audio_encode_count,
            Some(1)
        );
        assert_eq!(
            diagnostics.qwen.as_ref().unwrap().prompt_prefill_count,
            Some(1)
        );
    }

    #[test]
    fn deleting_session_cascades_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cascade.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        store
            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
            .unwrap();

        assert!(store.delete_session(session.id).unwrap());
        assert!(store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn zero_duration_does_not_produce_invalid_rtf() {
        let mut metrics = PipelineMetrics {
            audio_duration_ms: 0,
            asr_ms: 42.0,
            ..PipelineMetrics::default()
        };
        metrics.set_asr_rtf();
        assert_eq!(metrics.asr_rtf, None);
    }

    #[test]
    fn insertion_success_is_derived_without_collapsing_other_outcomes() {
        let mut metrics = PipelineMetrics::default();
        for outcome in [
            InsertionOutcome::NotRequested,
            InsertionOutcome::Copied,
            InsertionOutcome::Failed,
        ] {
            metrics.set_insertion_outcome(outcome);
            assert_eq!(metrics.insertion_outcome, outcome);
            assert!(!metrics.insert_succeeded);
        }

        metrics.set_insertion_outcome(InsertionOutcome::Inserted);
        assert_eq!(metrics.insertion_outcome, InsertionOutcome::Inserted);
        assert!(metrics.insert_succeeded);
    }

    #[test]
    fn future_enum_values_degrade_to_unknown() {
        let identity: PipelineIdentity = serde_json::from_str(
            r#"{
                "schema_version": 2,
                "enhancement_mode": "future_context_repair"
            }"#,
        )
        .unwrap();
        assert_eq!(identity.enhancement_mode, EnhancementMode::Unknown);

        let metrics: PipelineMetrics = serde_json::from_str(
            r#"{
                "schema_version": 2,
                "stage_issues": [{
                    "stage": "future_stage",
                    "kind": "future_issue",
                    "message": "newer writer"
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(metrics.stage_issues[0].stage, PipelineStage::Unknown);
        assert_eq!(metrics.stage_issues[0].kind, PipelineIssueKind::Unknown);
        assert_eq!(metrics.insertion_outcome, InsertionOutcome::Unknown);
    }

    #[test]
    fn legacy_insertion_boolean_migrates_without_inventing_a_denominator() {
        let inserted: PipelineMetrics = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "insert_succeeded": true
            }"#,
        )
        .unwrap();
        assert_eq!(inserted.insertion_outcome, InsertionOutcome::Inserted);
        assert!(inserted.insert_succeeded);

        let ambiguous_false: PipelineMetrics = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "insert_succeeded": false
            }"#,
        )
        .unwrap();
        assert_eq!(ambiguous_false.insertion_outcome, InsertionOutcome::Unknown);
        assert!(!ambiguous_false.insert_succeeded);
    }

    #[test]
    fn session_snapshot_and_attempt_commit_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("atomic.sqlite")).unwrap();
        let mut session = SessionRecord::new();
        session.asr_raw = Some("before".into());
        store.save_session(&session).unwrap();

        let attempt = DictationAttemptRecord::new(session.id);
        store
            .save_session_and_append_attempt(&session, attempt.clone())
            .unwrap();

        session.asr_raw = Some("must roll back".into());
        assert!(store
            .save_session_and_append_attempt(&session, attempt)
            .is_err());

        let stored = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(stored.asr_raw.as_deref(), Some("before"));
        assert_eq!(
            store
                .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn session_snapshot_rejects_an_attempt_for_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("session-mismatch.sqlite")).unwrap();
        let session = SessionRecord::new();
        let other_session = SessionRecord::new();
        store.save_session(&session).unwrap();

        let result = store.save_session_and_append_attempt(
            &session,
            DictationAttemptRecord::new(other_session.id),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains(&session.id.to_string()));
        assert!(error.contains(&other_session.id.to_string()));
        assert!(store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_attempt_identifiers_are_rejected_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("malformed-read.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        let attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE dictation_attempts SET id='not-a-uuid' WHERE id=?1",
                params![attempt.id.to_string()],
            )
            .unwrap();

        assert!(store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .is_err());
    }

    #[test]
    fn malformed_latest_ordinal_is_rejected_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("malformed-latest.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        let attempt = store
            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE dictation_attempts SET attempt_ordinal=-1 WHERE id=?1",
                params![attempt.id.to_string()],
            )
            .unwrap();

        assert!(store
            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
            .is_err());
    }

    #[test]
    fn concurrent_connections_append_without_losing_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.sqlite");
        let setup = Store::open(&path).unwrap();
        let session = SessionRecord::new();
        setup.save_session(&session).unwrap();
        drop(setup);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = Store::open(path).unwrap();
                    barrier.wait();
                    for _ in 0..25 {
                        store
                            .append_dictation_attempt(DictationAttemptRecord::new(session.id))
                            .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let store = Store::open(path).unwrap();
        let attempts = store
            .list_dictation_attempts(session.id, MAX_ATTEMPT_PAGE_SIZE, None)
            .unwrap();
        assert_eq!(attempts.len(), 50);
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.attempt_ordinal)
                .collect::<Vec<_>>(),
            (1..=50).rev().collect::<Vec<_>>()
        );
        for pair in attempts.windows(2) {
            assert_eq!(pair[0].supersedes_attempt_id, Some(pair[1].id));
        }
        assert_eq!(attempts.last().unwrap().supersedes_attempt_id, None);
    }

    #[test]
    fn attempts_are_returned_in_bounded_cursor_pages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("pages.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        for _ in 0..5 {
            store
                .append_dictation_attempt(DictationAttemptRecord::new(session.id))
                .unwrap();
        }

        let newest = store.list_dictation_attempts(session.id, 2, None).unwrap();
        assert_eq!(
            newest
                .iter()
                .map(|attempt| attempt.attempt_ordinal)
                .collect::<Vec<_>>(),
            vec![5, 4]
        );

        let older = store
            .list_dictation_attempts(session.id, 2, Some(4))
            .unwrap();
        assert_eq!(
            older
                .iter()
                .map(|attempt| attempt.attempt_ordinal)
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
    }

    #[test]
    fn context_snapshot_and_exact_stage_usage_round_trip_with_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("context-round-trip.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();

        let capture_id = Uuid::new_v4();
        let frozen_at = Utc::now();
        let snapshot = ContextSnapshotRecord {
            capture_id,
            session_id: session.id,
            revision: 1,
            schema_version: 1,
            profile: "visible".into(),
            target_generation: 7,
            started_at: frozen_at,
            frozen_at,
            completed_at: Some(frozen_at),
            manifest_path: "context/example/revision-1.envelope.json".into(),
            source_presence_bitmap: 0b111,
            source_status_json: r#"{"target":"succeeded","editor_ax":"succeeded"}"#.into(),
            sanitized_hash: "snapshot-hash".into(),
            encryption: "xchacha20poly1305".into(),
            status: "complete".into(),
        };
        store.save_context_snapshot(&snapshot).unwrap();

        let mut attempt = DictationAttemptRecord::new(session.id);
        attempt.pipeline_inputs = PipelineInputs {
            schema_version: 1,
            context: Some(ContextInputRef {
                capture_id,
                revision: 1,
                snapshot_hash: "snapshot-hash".into(),
                context_schema_version: 1,
                capture_profile: "visible".into(),
                source_presence_bitmap: 0b111,
                source_status_summary: "complete".into(),
            }),
            stage_usages: vec![
                ContextStageUsage {
                    stage: PipelineStage::Asr,
                    sources: vec!["captured_context".into()],
                    projection_schema_version: 1,
                    projection_path: None,
                    projection_hash: None,
                    projection_chars: 0,
                    captured: true,
                    selected: false,
                    consumed: false,
                    sent: false,
                    not_used_reason: Some("engine_context_disabled".into()),
                },
                ContextStageUsage {
                    stage: PipelineStage::Corrector,
                    sources: vec!["personal_dictionary".into()],
                    projection_schema_version: 1,
                    projection_path: Some("context/example/attempt/corrector.envelope.json".into()),
                    projection_hash: Some("corrector-input-hash".into()),
                    projection_chars: 42,
                    captured: true,
                    selected: true,
                    consumed: true,
                    sent: true,
                    not_used_reason: None,
                },
            ],
        };
        attempt.status = AttemptStatus::Completed;
        let saved = store.append_dictation_attempt(attempt).unwrap();

        let snapshots = store.list_context_snapshots(session.id).unwrap();
        assert_eq!(snapshots, vec![snapshot]);
        let attempts = store.list_dictation_attempts(session.id, 10, None).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, saved.id);
        assert_eq!(
            attempts[0]
                .pipeline_inputs
                .context
                .as_ref()
                .unwrap()
                .capture_id,
            capture_id
        );
        assert_eq!(attempts[0].pipeline_inputs.stage_usages.len(), 2);
        assert!(!attempts[0].pipeline_inputs.stage_usages[0].sent);
        assert_eq!(
            attempts[0].pipeline_inputs.stage_usages[1]
                .projection_hash
                .as_deref(),
            Some("corrector-input-hash")
        );
    }

    #[test]
    fn context_snapshot_revisions_are_append_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("context-revisions.sqlite")).unwrap();
        let session = SessionRecord::new();
        store.save_session(&session).unwrap();
        let capture_id = Uuid::new_v4();
        let frozen_at = Utc::now();
        let snapshot = ContextSnapshotRecord {
            capture_id,
            session_id: session.id,
            revision: 1,
            schema_version: 1,
            profile: "visible".into(),
            target_generation: 1,
            started_at: frozen_at,
            frozen_at,
            completed_at: None,
            manifest_path: "context/revision-1.envelope.json".into(),
            source_presence_bitmap: 1,
            source_status_json: "{}".into(),
            sanitized_hash: "first".into(),
            encryption: "xchacha20poly1305".into(),
            status: "partial".into(),
        };
        store.save_context_snapshot(&snapshot).unwrap();

        let mut conflicting = snapshot.clone();
        conflicting.sanitized_hash = "mutated".into();
        assert!(store.save_context_snapshot(&conflicting).is_err());

        let mut revision_two = snapshot;
        revision_two.revision = 2;
        revision_two.sanitized_hash = "second".into();
        revision_two.status = "complete".into();
        store.save_context_snapshot(&revision_two).unwrap();

        let snapshots = store.list_context_snapshots(session.id).unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].sanitized_hash, "first");
        assert_eq!(snapshots[1].sanitized_hash, "second");
    }
}
