use anyhow::Result;
use rusqlite::Connection;

pub(crate) const DEFAULT_EDIT_ATTRIBUTION_JSON: &str = r#"{"schema_version":1,"attempt_id":null,"target_app_name":null,"target_bundle_id":null,"observer":null,"target_fingerprint_hash":null,"field_before_hash":null,"field_after_hash":null,"status":"unattributed"}"#;

/// Current storage schema version. v6 added the meeting data model
/// (`meetings`, `speakers`, `transcript_segments`, `meeting_summaries`); v7
/// adds the additive `meetings.failure_reason` column; v8 adds the additive
/// `meetings.notes` column (user notes taken during the meeting); v9 adds the
/// additive `speakers.embedding` column (per-speaker voiceprint centroid,
/// f32 little-endian bytes) for cross-meeting speaker enrollment.
pub(crate) const SCHEMA_VERSION: i64 = 9;

/// Additive v6 migration: the meeting-mode tables. These sit alongside the
/// dictation tables and never touch them. `speakers` is created before
/// `transcript_segments` so the segment→speaker foreign key resolves cleanly.
const MEETING_SCHEMA_V6: &str = r#"
        CREATE TABLE IF NOT EXISTS meetings (
          id TEXT PRIMARY KEY NOT NULL,
          created_at TEXT NOT NULL,
          title TEXT,
          audio_path TEXT,
          duration_seconds REAL,
          status TEXT NOT NULL DEFAULT 'recording',
          language TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_meetings_created_at ON meetings(created_at DESC);

        CREATE TABLE IF NOT EXISTS speakers (
          id TEXT PRIMARY KEY NOT NULL,
          meeting_id TEXT NOT NULL,
          label TEXT NOT NULL,
          display_name TEXT,
          embedding_ref TEXT,
          FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_speakers_meeting ON speakers(meeting_id);

        CREATE TABLE IF NOT EXISTS transcript_segments (
          id TEXT PRIMARY KEY NOT NULL,
          meeting_id TEXT NOT NULL,
          seq INTEGER NOT NULL,
          start_seconds REAL NOT NULL,
          end_seconds REAL NOT NULL,
          text TEXT NOT NULL,
          speaker_id TEXT,
          confidence REAL,
          words_json TEXT,
          FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE,
          FOREIGN KEY(speaker_id) REFERENCES speakers(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_transcript_segments_meeting_seq
          ON transcript_segments(meeting_id, seq);

        CREATE TABLE IF NOT EXISTS meeting_summaries (
          id TEXT PRIMARY KEY NOT NULL,
          meeting_id TEXT NOT NULL,
          kind TEXT NOT NULL,
          content TEXT NOT NULL,
          created_at TEXT NOT NULL,
          model TEXT,
          FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_meeting_summaries_meeting
          ON meeting_summaries(meeting_id);
    "#;

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

        CREATE TABLE IF NOT EXISTS edit_observations (
          id TEXT PRIMARY KEY NOT NULL,
          session_id TEXT NOT NULL,
          attempt_id TEXT NOT NULL,
          source TEXT NOT NULL,
          status TEXT NOT NULL,
          end_reason TEXT NOT NULL,
          target_app_name TEXT,
          target_bundle_id TEXT,
          target_fingerprint_hash TEXT,
          inserted_text_hash TEXT NOT NULL,
          field_initial_hash TEXT,
          field_final_hash TEXT,
          normalized_edit_distance REAL,
          started_at TEXT NOT NULL,
          completed_at TEXT NOT NULL,
          edit_event_id TEXT,
          UNIQUE(attempt_id),
          FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
          FOREIGN KEY(attempt_id) REFERENCES dictation_attempts(id) ON DELETE CASCADE,
          FOREIGN KEY(edit_event_id) REFERENCES edit_events(id) ON DELETE SET NULL
        );

        CREATE INDEX IF NOT EXISTS idx_edit_observations_session
          ON edit_observations(session_id, completed_at);

        CREATE INDEX IF NOT EXISTS idx_edit_observations_edit_event
          ON edit_observations(edit_event_id);

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
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (5)",
        [],
    )?;
    // v6: meeting-mode tables. Additive — the CREATE ... IF NOT EXISTS block
    // leaves every existing table and row untouched.
    conn.execute_batch(MEETING_SCHEMA_V6)?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (6)",
        [],
    )?;
    // v7: additive `failure_reason` column on `meetings`. Guarded by a column
    // check (SQLite has no `ADD COLUMN IF NOT EXISTS`) so re-running is a no-op.
    let has_failure_reason = {
        let mut statement = conn.prepare("PRAGMA table_info(meetings)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "failure_reason")
    };
    if !has_failure_reason {
        conn.execute("ALTER TABLE meetings ADD COLUMN failure_reason TEXT", [])?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (7)",
        [],
    )?;
    // v8: additive `notes` column on `meetings` (free-form user notes taken
    // during the meeting). Guarded by a column check (SQLite has no `ADD COLUMN
    // IF NOT EXISTS`) so re-running is a no-op. The `NOT NULL DEFAULT ''` back-
    // fills every existing row with an empty string.
    let has_notes = {
        let mut statement = conn.prepare("PRAGMA table_info(meetings)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "notes")
    };
    if !has_notes {
        conn.execute(
            "ALTER TABLE meetings ADD COLUMN notes TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (8)",
        [],
    )?;
    // v9: additive `embedding` column on `speakers` — the diarization
    // pipeline's per-speaker centroid voiceprint (256 × f32, little-endian
    // bytes). Powers cross-meeting speaker enrollment; NULL for speakers from
    // meetings transcribed before this version (they simply cannot be
    // enrolled until re-processed). Guarded by a column check so re-running is
    // a no-op.
    let has_embedding = {
        let mut statement = conn.prepare("PRAGMA table_info(speakers)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "embedding")
    };
    if !has_embedding {
        conn.execute("ALTER TABLE speakers ADD COLUMN embedding BLOB", [])?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (?1)",
        [SCHEMA_VERSION],
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
        assert_eq!(version, SCHEMA_VERSION);
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
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_five_adds_edit_observation_terminal_records() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();

        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(edit_observations)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(columns.contains(&"status".to_owned()));
        assert!(columns.contains(&"end_reason".to_owned()));
        assert!(columns.contains(&"normalized_edit_distance".to_owned()));
        assert!(columns.contains(&"edit_event_id".to_owned()));

        let indexes: Vec<String> = connection
            .prepare("PRAGMA index_list(edit_observations)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(indexes.contains(&"idx_edit_observations_edit_event".to_owned()));
    }

    #[test]
    fn version_six_adds_meeting_tables_without_disturbing_existing_v5_data() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a realistic v5 database with one legacy dictation session.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5);
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
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // Existing v5 data survives untouched.
        let raw: String = connection
            .query_row(
                "SELECT asr_raw FROM sessions WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, "旧结果");

        // All four meeting tables now exist.
        for table in [
            "meetings",
            "speakers",
            "transcript_segments",
            "meeting_summaries",
        ] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} should exist after v6");
        }

        // The segment ordering index is present.
        let indexes: Vec<String> = connection
            .prepare("PRAGMA index_list(transcript_segments)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(indexes.contains(&"idx_transcript_segments_meeting_seq".to_owned()));

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_seven_adds_failure_reason_column_without_touching_v6_meetings() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v6 database (schema recorded up to 6, meetings table without
        // the failure_reason column) holding one meeting row.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT
                );
                INSERT INTO meetings (id, created_at, status)
                VALUES ('m1', '2026-07-25T00:00:00Z', 'ready');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // The new column exists and existing rows default it to NULL.
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(meetings)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"failure_reason".to_owned()));

        let reason: Option<String> = connection
            .query_row(
                "SELECT failure_reason FROM meetings WHERE id='m1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, None);
        // The pre-existing meeting row is otherwise untouched.
        let status: String = connection
            .query_row("SELECT status FROM meetings WHERE id='m1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "ready");

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_eight_adds_notes_column_defaulting_empty_without_touching_v7_meetings() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v7 database (schema recorded up to 7, meetings table with
        // failure_reason but without notes) holding one meeting row.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT
                );
                INSERT INTO meetings (id, created_at, status, title)
                VALUES ('m1', '2026-07-25T00:00:00Z', 'ready', '旧会议');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // The new column exists and every pre-existing row defaults to ''.
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(meetings)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"notes".to_owned()));

        let notes: String = connection
            .query_row("SELECT notes FROM meetings WHERE id='m1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(notes, "");
        // The pre-existing meeting row is otherwise untouched.
        let title: String = connection
            .query_row("SELECT title FROM meetings WHERE id='m1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "旧会议");

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_nine_adds_speaker_embedding_column_without_touching_v8_speakers() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v8 database (schema recorded up to 8, speakers table
        // without the embedding column) holding one speaker row.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT,
                  notes TEXT NOT NULL DEFAULT ''
                );
                CREATE TABLE speakers (
                  id TEXT PRIMARY KEY NOT NULL,
                  meeting_id TEXT NOT NULL,
                  label TEXT NOT NULL,
                  display_name TEXT,
                  embedding_ref TEXT,
                  FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
                );
                INSERT INTO meetings (id, created_at, status) VALUES ('m1', '2026-07-29T00:00:00Z', 'ready');
                INSERT INTO speakers (id, meeting_id, label, display_name) VALUES ('s1', 'm1', 'S1', '李明');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // The new column exists and pre-existing rows default it to NULL.
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(speakers)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"embedding".to_owned()));

        let embedding: Option<Vec<u8>> = connection
            .query_row("SELECT embedding FROM speakers WHERE id='s1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(embedding, None);
        // The pre-existing speaker row is otherwise untouched.
        let name: Option<String> = connection
            .query_row(
                "SELECT display_name FROM speakers WHERE id='s1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name.as_deref(), Some("李明"));

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_is_idempotent_across_repeated_runs() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        // Running again must not fail (CREATE IF NOT EXISTS / INSERT OR IGNORE).
        migrate(&connection).unwrap();
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
