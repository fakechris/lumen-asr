use anyhow::Result;
use rusqlite::Connection;

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
          version INTEGER PRIMARY KEY
        );

        CREATE TABLE IF NOT EXISTS sessions (
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

        CREATE INDEX IF NOT EXISTS idx_sessions_created_at ON sessions(created_at DESC);

        CREATE TABLE IF NOT EXISTS edit_events (
          id TEXT PRIMARY KEY NOT NULL,
          session_id TEXT NOT NULL,
          source TEXT NOT NULL,
          before_text TEXT NOT NULL,
          after_text TEXT NOT NULL,
          created_at TEXT NOT NULL,
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_edit_events_session ON edit_events(session_id);

        CREATE TABLE IF NOT EXISTS dictionary_entries (
          id TEXT PRIMARY KEY NOT NULL,
          kind TEXT NOT NULL,
          term TEXT,
          from_text TEXT,
          to_text TEXT,
          source TEXT NOT NULL,
          hit_count INTEGER NOT NULL DEFAULT 0,
          confirmed INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS context_snapshots (
          capture_id TEXT NOT NULL,
          session_id TEXT NOT NULL,
          revision INTEGER NOT NULL,
          schema_version INTEGER NOT NULL,
          profile TEXT NOT NULL,
          target_generation INTEGER NOT NULL,
          started_at TEXT NOT NULL,
          frozen_at TEXT NOT NULL,
          completed_at TEXT,
          manifest_path TEXT NOT NULL,
          source_presence_bitmap INTEGER NOT NULL,
          source_status_json TEXT NOT NULL,
          sanitized_hash TEXT NOT NULL,
          encryption TEXT NOT NULL DEFAULT 'none',
          status TEXT NOT NULL,
          PRIMARY KEY(capture_id, revision)
        );

        CREATE INDEX IF NOT EXISTS idx_context_snapshots_session
          ON context_snapshots(session_id, revision);
        "#,
    )?;

    // Record base migration if empty.
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    if count == 0 {
        conn.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (2)",
        [],
    )?;
    let has_encryption = {
        let mut statement = conn.prepare("PRAGMA table_info(context_snapshots)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "encryption")
    };
    if !has_encryption {
        conn.execute(
            "ALTER TABLE context_snapshots ADD COLUMN encryption TEXT NOT NULL DEFAULT 'none'",
            [],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (3)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_two_context_table_is_upgraded_with_encryption_metadata() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE context_snapshots (
                  capture_id TEXT NOT NULL,
                  session_id TEXT NOT NULL,
                  revision INTEGER NOT NULL,
                  schema_version INTEGER NOT NULL,
                  profile TEXT NOT NULL,
                  target_generation INTEGER NOT NULL,
                  started_at TEXT NOT NULL,
                  frozen_at TEXT NOT NULL,
                  completed_at TEXT,
                  manifest_path TEXT NOT NULL,
                  source_presence_bitmap INTEGER NOT NULL,
                  source_status_json TEXT NOT NULL,
                  sanitized_hash TEXT NOT NULL,
                  status TEXT NOT NULL,
                  PRIMARY KEY(capture_id, revision)
                );
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        let mut statement = connection
            .prepare("PRAGMA table_info(context_snapshots)")
            .unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "encryption"));
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 3);
    }
}
