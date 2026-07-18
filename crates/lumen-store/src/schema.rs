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

        CREATE TABLE IF NOT EXISTS dictation_attempts (
          id TEXT PRIMARY KEY NOT NULL,
          session_id TEXT NOT NULL,
          attempt_ordinal INTEGER NOT NULL,
          created_at TEXT NOT NULL,
          asr_raw TEXT,
          asr_enhanced TEXT,
          corrected TEXT,
          inserted TEXT,
          pipeline_identity_json TEXT NOT NULL,
          pipeline_metrics_json TEXT NOT NULL,
          status TEXT NOT NULL,
          failed_stage TEXT,
          failure_message TEXT,
          supersedes_attempt_id TEXT,
          UNIQUE(session_id, attempt_ordinal),
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
          FOREIGN KEY(supersedes_attempt_id) REFERENCES dictation_attempts(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_dictation_attempts_session
          ON dictation_attempts(session_id, attempt_ordinal);
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_two_is_additive_and_preserves_legacy_sessions() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
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
                INSERT INTO sessions (id, created_at, asr_raw, status)
                VALUES ('legacy', '2026-07-18T00:00:00Z', '旧结果', 'completed');
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1);
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        let raw: String = connection
            .query_row(
                "SELECT asr_raw FROM sessions WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, "旧结果");
        let attempts_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dictation_attempts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts_table, 1);
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 2);
    }
}
