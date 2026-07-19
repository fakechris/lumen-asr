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
                   pipeline_identity_json, pipeline_metrics_json,
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
        let id = Uuid::new_v4();
        self.conn.execute(
            r#"
            INSERT INTO edit_events (id, session_id, source, before_text, after_text, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                id.to_string(),
                session_id.to_string(),
                edit_source_str(source),
                before,
                after,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(id)
    }

    pub fn list_edit_events(&self, session_id: Uuid) -> Result<Vec<EditEventRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, session_id, source, before_text, after_text, created_at
            FROM edit_events
            WHERE session_id=?1
            ORDER BY created_at ASC
            "#,
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], map_edit)?;
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
            SELECT id, session_id, source, before_text, after_text, created_at
            FROM edit_events
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = stmt.query_map(params![limit], map_edit)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
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
    let pipeline_identity = serde_json::from_str(&identity_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let pipeline_metrics = serde_json::from_str(&metrics_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let id = parse_uuid_column(row, 0)?;
    let session_id = parse_uuid_column(row, 1)?;
    let attempt_ordinal = parse_u32_column(row, 2)?;
    let supersedes_attempt_id = match row.get::<_, Option<String>>(13)? {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
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
        status: parse_attempt_status(&row.get::<_, String>(10)?),
        failed_stage: row
            .get::<_, Option<String>>(11)?
            .and_then(|value| parse_pipeline_stage(&value)),
        failure_message: row.get(12)?,
        supersedes_attempt_id,
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
    transaction.execute(
        r#"
        INSERT INTO dictation_attempts (
          id, session_id, attempt_ordinal, created_at,
          asr_raw, asr_enhanced, corrected, inserted,
          pipeline_identity_json, pipeline_metrics_json,
          status, failed_stage, failure_message, supersedes_attempt_id
        ) VALUES (
          ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14
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
    Ok(EditEventRecord {
        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
        session_id: Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_default(),
        source: parse_edit_source(&row.get::<_, String>(2)?),
        before_text: row.get(3)?,
        after_text: row.get(4)?,
        created_at: parse_dt(&row.get::<_, String>(5)?),
    })
}

fn parse_edit_source(s: &str) -> EditSource {
    match s {
        "post_paste_ax" => EditSource::PostPasteAx,
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
}
