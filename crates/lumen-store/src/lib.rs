//! SQLite store for Lumen ASR.

mod schema;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lumen_core::{
    DictEntryKind, DictEntrySource, EditSource, FocusInfo, InsertStrategy, SessionRecord,
    SessionStatus,
};
use lumen_dictionary::DictionaryEntry;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        schema::migrate(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save_session(&self, rec: &SessionRecord) -> Result<()> {
        self.conn.execute(
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

    pub fn save_context_snapshot(&self, rec: &ContextSnapshotRecord) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO context_snapshots (
              capture_id, session_id, revision, schema_version, profile,
              target_generation, started_at, frozen_at, completed_at,
              manifest_path, source_presence_bitmap, source_status_json,
              sanitized_hash, encryption, status
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
            "#,
            params![
                rec.capture_id.to_string(),
                rec.session_id.to_string(),
                rec.revision as i64,
                rec.schema_version as i64,
                rec.profile,
                rec.target_generation as i64,
                rec.started_at.to_rfc3339(),
                rec.frozen_at.to_rfc3339(),
                rec.completed_at.map(|value| value.to_rfc3339()),
                rec.manifest_path,
                rec.source_presence_bitmap as i64,
                rec.source_status_json,
                rec.sanitized_hash,
                rec.encryption,
                rec.status,
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
        let rows = statement.query_map(params![session_id.to_string()], |row| {
            Ok(ContextSnapshotRecord {
                capture_id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                session_id: Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_default(),
                revision: row.get::<_, i64>(2)? as u64,
                schema_version: row.get::<_, i64>(3)? as u32,
                profile: row.get(4)?,
                target_generation: row.get::<_, i64>(5)? as u64,
                started_at: parse_dt(&row.get::<_, String>(6)?),
                frozen_at: parse_dt(&row.get::<_, String>(7)?),
                completed_at: row
                    .get::<_, Option<String>>(8)?
                    .map(|value| parse_dt(&value)),
                manifest_path: row.get(9)?,
                source_presence_bitmap: row.get::<_, i64>(10)? as u64,
                source_status_json: row.get(11)?,
                sanitized_hash: row.get(12)?,
                encryption: row.get(13)?,
                status: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn prune_context_captures_before(&self, cutoff: DateTime<Utc>) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT capture_id
            FROM context_snapshots
            GROUP BY capture_id
            HAVING MAX(frozen_at) < ?1
            "#,
        )?;
        let capture_ids = statement
            .query_map(params![cutoff.to_rfc3339()], |row| {
                Ok(Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default())
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for capture_id in &capture_ids {
            self.conn.execute(
                "DELETE FROM context_snapshots WHERE capture_id=?1",
                params![capture_id.to_string()],
            )?;
        }
        Ok(capture_ids)
    }

    pub fn clear_context_snapshots(&self) -> Result<usize> {
        self.conn
            .execute("DELETE FROM context_snapshots", [])
            .map_err(Into::into)
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
    fn context_revisions_are_append_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("context.sqlite")).unwrap();
        let capture_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let now = Utc::now();
        let record = ContextSnapshotRecord {
            capture_id,
            session_id,
            revision: 4,
            schema_version: 1,
            profile: "metadata".to_owned(),
            target_generation: 1,
            started_at: now,
            frozen_at: now,
            completed_at: Some(now),
            manifest_path: "manifest.r0004.v1.json.zst".to_owned(),
            source_presence_bitmap: 1,
            source_status_json: "{}".to_owned(),
            sanitized_hash: "first".to_owned(),
            encryption: "chacha20_poly1305".to_owned(),
            status: "complete".to_owned(),
        };

        store.save_context_snapshot(&record).unwrap();
        let mut conflicting = record.clone();
        conflicting.sanitized_hash = "replacement".to_owned();
        assert!(store.save_context_snapshot(&conflicting).is_err());

        let records = store.list_context_snapshots(session_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sanitized_hash, "first");
    }

    #[test]
    fn retention_prunes_only_fully_expired_captures() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("retention.sqlite")).unwrap();
        let session_id = Uuid::new_v4();
        let now = Utc::now();
        let make_record = |capture_id, revision, frozen_at| ContextSnapshotRecord {
            capture_id,
            session_id,
            revision,
            schema_version: 1,
            profile: "metadata".to_owned(),
            target_generation: 1,
            started_at: frozen_at,
            frozen_at,
            completed_at: Some(frozen_at),
            manifest_path: format!("{capture_id}-{revision}.sealed.json"),
            source_presence_bitmap: 1,
            source_status_json: "{}".to_owned(),
            sanitized_hash: format!("hash-{revision}"),
            encryption: "chacha20_poly1305".to_owned(),
            status: "complete".to_owned(),
        };
        let expired = Uuid::new_v4();
        let current = Uuid::new_v4();
        store
            .save_context_snapshot(&make_record(expired, 1, now - chrono::Duration::days(8)))
            .unwrap();
        store
            .save_context_snapshot(&make_record(current, 1, now))
            .unwrap();

        let pruned = store
            .prune_context_captures_before(now - chrono::Duration::days(7))
            .unwrap();
        assert_eq!(pruned, vec![expired]);
        let records = store.list_context_snapshots(session_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].capture_id, current);
    }
}
