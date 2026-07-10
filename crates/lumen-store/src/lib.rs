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
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
        let conn = Connection::open(&path).with_context(|| format!("open db {}", path.display()))?;
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
        self.conn
            .execute("DELETE FROM dictionary_entries WHERE id=?1", params![id.to_string()])?;
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
    }
}
