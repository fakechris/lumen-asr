use anyhow::Result;
use rusqlite::Connection;

pub(crate) const DEFAULT_EDIT_ATTRIBUTION_JSON: &str = r#"{"schema_version":1,"attempt_id":null,"target_app_name":null,"target_bundle_id":null,"observer":null,"target_fingerprint_hash":null,"field_before_hash":null,"field_after_hash":null,"status":"unattributed"}"#;

pub fn migrate(conn: &Connection) -> Result<()> {
    let base_schema = r#"
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
          attribution_json TEXT NOT NULL DEFAULT '__DEFAULT_EDIT_ATTRIBUTION_JSON__',
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
          pipeline_inputs_json TEXT NOT NULL DEFAULT '{"schema_version":1,"context":null,"stage_usages":[]}',
          status TEXT NOT NULL,
          failed_stage TEXT,
          failure_message TEXT,
          supersedes_attempt_id TEXT,
          UNIQUE(session_id, attempt_ordinal),
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
          FOREIGN KEY(supersedes_attempt_id) REFERENCES dictation_attempts(id) ON DELETE SET NULL
        );

        DROP INDEX IF EXISTS idx_dictation_attempts_session;

        CREATE INDEX IF NOT EXISTS idx_dictation_attempts_supersedes
          ON dictation_attempts(supersedes_attempt_id);

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
        "#;
    conn.execute_batch(&base_schema.replace(
        "__DEFAULT_EDIT_ATTRIBUTION_JSON__",
        DEFAULT_EDIT_ATTRIBUTION_JSON,
    ))?;

    // Record base migration if empty.
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    if count == 0 {
        conn.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (2)",
        [],
    )?;
    let has_pipeline_inputs = {
        let mut statement = conn.prepare("PRAGMA table_info(dictation_attempts)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "pipeline_inputs_json")
    };
    if !has_pipeline_inputs {
        conn.execute(
            r#"ALTER TABLE dictation_attempts
               ADD COLUMN pipeline_inputs_json TEXT NOT NULL
               DEFAULT '{"schema_version":1,"context":null,"stage_usages":[]}'"#,
            [],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (3)",
        [],
    )?;
    let has_edit_attribution = {
        let mut statement = conn.prepare("PRAGMA table_info(edit_events)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "attribution_json")
    };
    if !has_edit_attribution {
        let migration = format!(
            "ALTER TABLE edit_events
             ADD COLUMN attribution_json TEXT NOT NULL
             DEFAULT '{}'",
            DEFAULT_EDIT_ATTRIBUTION_JSON
        );
        conn.execute(&migration, [])?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (4)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_remain_additive_and_preserve_legacy_sessions() {
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
        assert_eq!(version, 4);
    }

    #[test]
    fn version_three_adds_context_storage_without_changing_existing_attempts() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (2);
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
                CREATE TABLE dictation_attempts (
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
                  UNIQUE(session_id, attempt_ordinal)
                );
                INSERT INTO dictation_attempts (
                  id, session_id, attempt_ordinal, created_at,
                  pipeline_identity_json, pipeline_metrics_json, status
                ) VALUES (
                  'attempt', 'session', 1, '2026-07-23T00:00:00Z',
                  '{}', '{}', 'completed'
                );
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        let inputs: String = connection
            .query_row(
                "SELECT pipeline_inputs_json FROM dictation_attempts WHERE id='attempt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            inputs,
            r#"{"schema_version":1,"context":null,"stage_usages":[]}"#
        );
        let context_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='context_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(context_table, 1);
    }

    #[test]
    fn version_four_preserves_existing_edits_and_marks_them_unattributed() {
        let connection = Connection::open_in_memory().unwrap();
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
                INSERT INTO edit_events (
                  id, session_id, source, before_text, after_text, created_at
                ) VALUES (
                  'edit', 'session', 'post_paste_ax', '旧', '新', '2026-07-23T00:00:00Z'
                );
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        let attribution: String = connection
            .query_row(
                "SELECT attribution_json FROM edit_events WHERE id='edit'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attribution, DEFAULT_EDIT_ATTRIBUTION_JSON);
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 4);
    }
}
