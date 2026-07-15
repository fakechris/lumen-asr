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
        "#,
    )?;

    // Record base migration if empty.
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    if count == 0 {
        conn.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
    }
    Ok(())
}
