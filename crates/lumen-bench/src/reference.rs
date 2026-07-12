use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCandidate {
    pub id: String,
    pub reference: String,
    pub audio_path: PathBuf,
    pub duration_seconds: Option<f64>,
    pub created_at: Option<String>,
    pub app_version: String,
    pub source_table: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceInventory {
    pub history_rows: usize,
    pub history_v2_rows: usize,
    pub duplicate_ids: usize,
    pub candidate_rows: usize,
    pub recording_files: usize,
}

pub struct ReferenceDataset {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrder {
    #[default]
    Stable,
    Newest,
    Oldest,
}

impl ReferenceDataset {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = open_read_only(&path)?;
        for table in ["history", "history_v2"] {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            anyhow::ensure!(exists, "reference database is missing table {table}");
        }
        Ok(Self { path })
    }

    pub fn candidates(&self, limit: usize, offset: usize) -> Result<Vec<ReferenceCandidate>> {
        self.candidates_ordered(limit, offset, CandidateOrder::Newest)
    }

    pub fn candidates_ordered(
        &self,
        limit: usize,
        offset: usize,
        order: CandidateOrder,
    ) -> Result<Vec<ReferenceCandidate>> {
        let conn = open_read_only(&self.path)?;
        let order_by = match order {
            CandidateOrder::Stable => "id ASC",
            CandidateOrder::Newest => "created_at DESC, id ASC",
            CandidateOrder::Oldest => "created_at ASC, id ASC",
        };
        let sql = format!(
            r#"
            WITH candidates AS (
              SELECT id, refined_text, audio_local_path, duration, created_at,
                     app_version, 'history_v2' AS source_table
              FROM history_v2
              WHERE status = 'completed'
                AND mode = 'voice_transcript'
                AND length(trim(refined_text)) > 0
                AND length(trim(audio_local_path)) > 0
              UNION ALL
              SELECT h.id, h.refined_text, h.audio_local_path, h.duration, h.created_at,
                     h.app_version, 'history' AS source_table
              FROM history h
              WHERE h.status = 'transcript'
                AND h.mode = 'voice_transcript'
                AND length(trim(h.refined_text)) > 0
                AND length(trim(h.audio_local_path)) > 0
                AND NOT EXISTS (SELECT 1 FROM history_v2 v WHERE v.id = h.id)
            )
            SELECT id, refined_text, audio_local_path, duration, created_at,
                   app_version, source_table
            FROM candidates
            ORDER BY {order_by}
            LIMIT ?1 OFFSET ?2
            "#
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([limit as i64, offset as i64], |row| {
            Ok(ReferenceCandidate {
                id: row.get(0)?,
                reference: row.get(1)?,
                audio_path: PathBuf::from(row.get::<_, String>(2)?),
                duration_seconds: row.get(3)?,
                created_at: row.get(4)?,
                app_version: row.get(5)?,
                source_table: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read reference candidates")
    }

    pub fn inventory(&self) -> Result<ReferenceInventory> {
        let conn = open_read_only(&self.path)?;
        let history_rows = count(&conn, "SELECT count(*) FROM history")?;
        let history_v2_rows = count(&conn, "SELECT count(*) FROM history_v2")?;
        let duplicate_ids = count(
            &conn,
            "SELECT count(*) FROM history h JOIN history_v2 v USING(id)",
        )?;
        let candidate_rows = count(
            &conn,
            r#"
            SELECT count(*) FROM (
              SELECT id FROM history_v2
              WHERE status = 'completed' AND mode = 'voice_transcript'
                AND length(trim(refined_text)) > 0 AND length(trim(audio_local_path)) > 0
              UNION ALL
              SELECT h.id FROM history h
              WHERE h.status = 'transcript' AND h.mode = 'voice_transcript'
                AND length(trim(h.refined_text)) > 0 AND length(trim(h.audio_local_path)) > 0
                AND NOT EXISTS (SELECT 1 FROM history_v2 v WHERE v.id = h.id)
            )
            "#,
        )?;
        let recordings = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("Recordings");
        let recording_files = std::fs::read_dir(recordings)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ogg"))
                    .count()
            })
            .unwrap_or(0);
        Ok(ReferenceInventory {
            history_rows,
            history_v2_rows,
            duplicate_ids,
            candidate_rows,
            recording_files,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn count(conn: &Connection, sql: &str) -> Result<usize> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|value| value as usize)
        .context("count reference rows")
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open reference database read-only: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::ReferenceDataset;
    use rusqlite::Connection;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn candidates_prefer_v2_and_only_include_usable_voice_transcripts() {
        let root = std::env::temp_dir().join(format!("lumen-bench-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("Recordings")).unwrap();
        let db_path = root.join("reference.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE history (
              id TEXT PRIMARY KEY, refined_text TEXT, audio_local_path TEXT,
              status TEXT, mode TEXT, duration REAL, created_at TEXT, app_version TEXT
            );
            CREATE TABLE history_v2 (
              id TEXT PRIMARY KEY, refined_text TEXT, audio_local_path TEXT,
              status TEXT, mode TEXT, duration REAL, created_at TEXT, app_version TEXT
            );
            "#,
        )
        .unwrap();
        for id in ["duplicate", "old-only", "dismissed", "command"] {
            fs::write(root.join("Recordings").join(format!("{id}.ogg")), b"ogg").unwrap();
        }
        let path = |id: &str| root.join("Recordings").join(format!("{id}.ogg"));
        conn.execute(
            "INSERT INTO history VALUES (?1, 'old reference', ?2, 'transcript', 'voice_transcript', 1.0, '2026-01-01', '1.7')",
            ("duplicate", path("duplicate").to_string_lossy().as_ref()),
        ).unwrap();
        conn.execute(
            "INSERT INTO history_v2 VALUES (?1, 'new reference', ?2, 'completed', 'voice_transcript', 1.0, '2026-02-01', '1.8')",
            ("duplicate", path("duplicate").to_string_lossy().as_ref()),
        ).unwrap();
        conn.execute(
            "INSERT INTO history VALUES (?1, 'old only', ?2, 'transcript', 'voice_transcript', 1.0, '2026-01-02', '1.7')",
            ("old-only", path("old-only").to_string_lossy().as_ref()),
        ).unwrap();
        conn.execute(
            "INSERT INTO history_v2 VALUES (?1, 'ignore me', ?2, 'dismissed', 'voice_transcript', 1.0, '2026-02-02', '1.8')",
            ("dismissed", path("dismissed").to_string_lossy().as_ref()),
        ).unwrap();
        conn.execute(
            "INSERT INTO history_v2 VALUES (?1, 'run command', ?2, 'completed', 'voice_command', 1.0, '2026-02-03', '1.8')",
            ("command", path("command").to_string_lossy().as_ref()),
        ).unwrap();
        drop(conn);

        let dataset = ReferenceDataset::open(&db_path).unwrap();
        let candidates = dataset.candidates(10, 0).unwrap();
        let inventory = dataset.inventory().unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].id, "duplicate");
        assert_eq!(candidates[0].reference, "new reference");
        assert_eq!(candidates[0].source_table, "history_v2");
        assert_eq!(candidates[1].id, "old-only");
        assert_eq!(inventory.history_rows, 2);
        assert_eq!(inventory.history_v2_rows, 3);
        assert_eq!(inventory.duplicate_ids, 1);
        assert_eq!(inventory.candidate_rows, 2);
        assert_eq!(inventory.recording_files, 4);

        fs::remove_dir_all(root).unwrap();
    }
}
