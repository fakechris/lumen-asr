use anyhow::Result;
use rusqlite::{params, Connection};

pub(crate) const DEFAULT_EDIT_ATTRIBUTION_JSON: &str = r#"{"schema_version":1,"attempt_id":null,"target_app_name":null,"target_bundle_id":null,"observer":null,"target_fingerprint_hash":null,"field_before_hash":null,"field_after_hash":null,"status":"unattributed"}"#;

/// Current storage schema version. v6 added the meeting data model
/// (`meetings`, `speakers`, `transcript_segments`, `meeting_summaries`); v7
/// adds the additive `meetings.failure_reason` column; v8 adds the additive
/// `meetings.notes` column (user notes taken during the meeting); v9 adds the
/// additive `meetings.system_audio_path` and `transcript_segments.channel`
/// columns for dual-track (mic + system audio) meetings; v10 adds the additive
/// `speakers.embedding` column (per-speaker voiceprint centroid, f32
/// little-endian bytes) for cross-meeting speaker enrollment; v11 adds the
/// `ux_meetings_single_active` partial unique index enforcing the
/// single-active-recording invariant (at most one `meetings` row in status
/// `recording` at any time); v12 adds the additive `live_annotations` table
/// (manual "who is speaking" marks made on the live captions while recording,
/// reconciled into speaker attribution by the offline pipeline); v13 adds the
/// additive speaker-provenance columns `speakers.identity_id` (enrolled
/// identity behind the name), `speakers.attribution_origin`
/// ('manual' | 'verification' | 'offline_diarization'), and
/// `speakers.attribution_confidence` (verification match score) — the ground
/// for conflict handling between manual/verified/offline attribution; v14 adds
/// indexed history visibility for short absolute-silence captures.
pub(crate) const SCHEMA_VERSION: i64 = 14;

pub(crate) const HISTORY_TEXT_WHITESPACE: &str =
    "\u{0009}\u{000A}\u{000B}\u{000C}\u{000D}\u{0020}\u{00A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
const LEGACY_EMPTY_CAPTURE_MESSAGE: &str =
    "no audio captured (0 samples) — hold longer or check mic";
const HISTORY_MIGRATION_BATCH_SIZE: i64 = 128;
const HISTORY_MIGRATION_MAX_METRICS_JSON_BYTES: i64 = 64 * 1024;

/// Failure reason written by the v11 migration onto surplus stale `recording`
/// rows (older duplicates that would violate the new single-active index).
pub(crate) const STALE_DUPLICATE_RECORDING_REASON: &str =
    "recording interrupted (stale duplicate active recording)";

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
          status TEXT NOT NULL DEFAULT 'in_progress',
          history_visible INTEGER NOT NULL DEFAULT 1
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
    // v9: additive dual-track columns — `meetings.system_audio_path` (path of
    // the optional second, synchronized system-audio WAV) and
    // `transcript_segments.channel` ('mic' / 'system'; NULL for legacy
    // single-track meetings, which reads as mic). Guarded by column checks
    // (SQLite has no `ADD COLUMN IF NOT EXISTS`) so re-running is a no-op.
    let has_system_audio_path = {
        let mut statement = conn.prepare("PRAGMA table_info(meetings)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "system_audio_path")
    };
    if !has_system_audio_path {
        conn.execute("ALTER TABLE meetings ADD COLUMN system_audio_path TEXT", [])?;
    }
    let has_channel = {
        let mut statement = conn.prepare("PRAGMA table_info(transcript_segments)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "channel")
    };
    if !has_channel {
        conn.execute(
            "ALTER TABLE transcript_segments ADD COLUMN channel TEXT",
            [],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (9)",
        [],
    )?;
    // v10: additive `embedding` column on `speakers` — the diarization
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
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (10)",
        [],
    )?;
    // v11: single-active-recording invariant. Audio capture is exclusive (the
    // runtime arbiter rejects a second concurrent recording), so at most one
    // `meetings` row may be in status `recording` at any time — this partial
    // unique index turns that runtime rule into a database guarantee. Only
    // `recording` is "active" in this sense: `processing` / `transcribing` /
    // `summarizing` meetings legitimately coexist (background transcription of
    // an earlier meeting runs while a new one records, and crash recovery can
    // queue several salvaged meetings for processing at once).
    //
    // Dirty-data repair first, so the index build cannot fail on databases
    // where earlier crashes accumulated several rows stuck in `recording`:
    // keep the newest such row (still salvageable by launch crash recovery)
    // and mark the older duplicates failed with an explicit reason. Re-running
    // is a no-op — with at most one `recording` row left, nothing matches.
    conn.execute(
        "UPDATE meetings SET status='failed', failure_reason=?1
         WHERE status='recording'
           AND id NOT IN (
             SELECT id FROM meetings WHERE status='recording'
             ORDER BY created_at DESC, id DESC LIMIT 1)",
        [STALE_DUPLICATE_RECORDING_REASON],
    )?;
    // Unique over the constant expression (1) restricted to active rows:
    // every `recording` row indexes the same key, so a second one is a
    // constraint violation.
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS ux_meetings_single_active
         ON meetings((1)) WHERE status = 'recording'",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (11)",
        [],
    )?;
    // v12: additive `live_annotations` table — manual "who is speaking" marks
    // made on the live caption view while a meeting is still recording. Speaker
    // rows do not exist at that point (the offline pipeline creates them after
    // stop), so each annotation is anchored to a time range on the meeting's
    // unified timeline plus its capture track; the offline pipeline reconciles
    // them into speaker attribution (manual wins). `identity_id` points into
    // the local voiceprint library for enrolled people (NULL for ad-hoc typed
    // names); `display_name` is the name snapshot at annotate time. Re-running
    // is a no-op (CREATE ... IF NOT EXISTS).
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS live_annotations (
          id TEXT PRIMARY KEY NOT NULL,
          meeting_id TEXT NOT NULL,
          start_seconds REAL NOT NULL,
          end_seconds REAL,
          channel TEXT NOT NULL,
          identity_id TEXT,
          display_name TEXT NOT NULL,
          created_at TEXT NOT NULL,
          FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_live_annotations_meeting
          ON live_annotations(meeting_id, created_at);
        "#,
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (12)",
        [],
    )?;
    // v13: additive speaker-provenance columns on `speakers` — who/what named
    // this speaker. `identity_id` links the enrolled voiceprint identity,
    // `attribution_origin` records the source ('manual' | 'verification' |
    // 'offline_diarization'), `attribution_confidence` records the match score
    // behind a verification hit. All NULL for pre-v13 rows and unnamed
    // speakers. Guarded by column checks (SQLite has no `ADD COLUMN IF NOT
    // EXISTS`) so re-running is a no-op.
    let speaker_columns: Vec<String> = {
        let mut statement = conn.prepare("PRAGMA table_info(speakers)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns.collect::<Result<Vec<_>, _>>()?
    };
    if !speaker_columns.iter().any(|c| c == "identity_id") {
        conn.execute("ALTER TABLE speakers ADD COLUMN identity_id TEXT", [])?;
    }
    if !speaker_columns.iter().any(|c| c == "attribution_origin") {
        conn.execute(
            "ALTER TABLE speakers ADD COLUMN attribution_origin TEXT",
            [],
        )?;
    }
    if !speaker_columns
        .iter()
        .any(|c| c == "attribution_confidence")
    {
        conn.execute(
            "ALTER TABLE speakers ADD COLUMN attribution_confidence REAL",
            [],
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES (13)",
        [],
    )?;

    // v14: persist history visibility so list queries remain index-bounded even
    // if a database contains a very large run of accidental silent captures.
    // Existing rows default visible; the one-time backfill hides only rows with
    // complete, internally consistent silence evidence. Corrupt/unknown rows
    // fail open and remain visible.
    let has_history_visible = {
        let mut statement = conn.prepare("PRAGMA table_info(sessions)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "history_visible")
    };
    if !has_history_visible {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN history_visible INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    let has_v14: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version=14)",
        [],
        |row| row.get(0),
    )?;
    if !has_v14 {
        let mut after_session_id = String::new();
        loop {
            let candidates = {
                let mut statement = conn.prepare(
                    r#"
                SELECT sessions.id, first_attempt.pipeline_metrics_json,
                       first_attempt.failure_message
                FROM sessions
                JOIN dictation_attempts AS first_attempt
                  ON first_attempt.id = (
                    SELECT id
                    FROM dictation_attempts
                    WHERE session_id = sessions.id
                    ORDER BY attempt_ordinal ASC
                    LIMIT 1
                  )
                WHERE sessions.id > ?2
                  AND sessions.status IN ('failed', 'cancelled')
                  AND trim(COALESCE(sessions.corrected, ''), ?1) = ''
                  AND trim(COALESCE(sessions.pasted, ''), ?1) = ''
                  AND trim(COALESCE(sessions.asr_raw, ''), ?1) = ''
                  AND first_attempt.status = 'failed'
                  AND first_attempt.failed_stage = 'capture'
                  AND typeof(first_attempt.pipeline_metrics_json) = 'text'
                  AND length(CAST(first_attempt.pipeline_metrics_json AS BLOB)) <= ?3
                ORDER BY sessions.id ASC
                LIMIT ?4
                "#,
                )?;
                let rows = statement.query_map(
                    params![
                        HISTORY_TEXT_WHITESPACE,
                        after_session_id,
                        HISTORY_MIGRATION_MAX_METRICS_JSON_BYTES,
                        HISTORY_MIGRATION_BATCH_SIZE,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let Some((last_session_id, _, _)) = candidates.last() else {
                break;
            };
            after_session_id.clone_from(last_session_id);

            for (session_id, metrics_json, failure_message) in candidates {
                let Ok(metrics) = serde_json::from_str::<super::PipelineMetrics>(&metrics_json)
                else {
                    continue;
                };
                if metrics.audio_duration_ms >= 2_000 {
                    continue;
                }
                let structured_silence = metrics.stage_issues.iter().any(|issue| {
                    issue.stage == super::PipelineStage::Capture
                        && issue.kind == super::PipelineIssueKind::AbsoluteSilence
                });
                let legacy_zero_samples = metrics.audio_duration_ms == 0
                    && failure_message.as_deref() == Some(LEGACY_EMPTY_CAPTURE_MESSAGE);
                if structured_silence || legacy_zero_samples {
                    conn.execute(
                        "UPDATE sessions SET history_visible=0 WHERE id=?1",
                        params![session_id],
                    )?;
                }
            }
        }
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_history_visible_created_at
         ON sessions(history_visible, created_at DESC)",
        [],
    )?;
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
    fn version_nine_adds_dual_track_columns_without_touching_v8_rows() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v8 database (schema recorded up to 8: meetings with
        // failure_reason + notes, segments without channel) holding one meeting
        // and one segment.
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
                INSERT INTO meetings (id, created_at, status, title, audio_path)
                VALUES ('m1', '2026-07-25T00:00:00Z', 'ready', '旧会议', '/tmp/m1.wav');
                CREATE TABLE transcript_segments (
                  id TEXT PRIMARY KEY NOT NULL,
                  meeting_id TEXT NOT NULL,
                  seq INTEGER NOT NULL,
                  start_seconds REAL NOT NULL,
                  end_seconds REAL NOT NULL,
                  text TEXT NOT NULL,
                  speaker_id TEXT,
                  confidence REAL,
                  words_json TEXT
                );
                INSERT INTO transcript_segments (id, meeting_id, seq, start_seconds, end_seconds, text)
                VALUES ('seg1', 'm1', 0, 0.0, 1.0, '旧句子');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // Both new columns exist; pre-existing rows default them to NULL.
        let meeting_columns: Vec<String> = connection
            .prepare("PRAGMA table_info(meetings)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(meeting_columns.contains(&"system_audio_path".to_owned()));
        let segment_columns: Vec<String> = connection
            .prepare("PRAGMA table_info(transcript_segments)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(segment_columns.contains(&"channel".to_owned()));

        let system_path: Option<String> = connection
            .query_row(
                "SELECT system_audio_path FROM meetings WHERE id='m1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(system_path, None);
        let channel: Option<String> = connection
            .query_row(
                "SELECT channel FROM transcript_segments WHERE id='seg1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(channel, None);
        // The pre-existing rows are otherwise untouched.
        let title: String = connection
            .query_row("SELECT title FROM meetings WHERE id='m1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "旧会议");
        let text: String = connection
            .query_row(
                "SELECT text FROM transcript_segments WHERE id='seg1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text, "旧句子");

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_ten_adds_speaker_embedding_column_without_touching_v9_speakers() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v9 database (schema recorded up to 9: meetings with the
        // dual-track system_audio_path, speakers without embedding) holding
        // one speaker row.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT,
                  notes TEXT NOT NULL DEFAULT '',
                  system_audio_path TEXT
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
    fn version_eleven_repairs_stale_duplicate_recordings_and_enforces_single_active() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v10 database with dirty data: two rows stuck in
        // `recording` (accumulated across earlier crashes), plus one finished
        // meeting.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT,
                  notes TEXT NOT NULL DEFAULT '',
                  system_audio_path TEXT
                );
                CREATE TABLE speakers (
                  id TEXT PRIMARY KEY NOT NULL,
                  meeting_id TEXT NOT NULL,
                  label TEXT NOT NULL,
                  display_name TEXT,
                  embedding_ref TEXT,
                  embedding BLOB,
                  FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
                );
                INSERT INTO meetings (id, created_at, status) VALUES
                  ('older', '2026-07-27T00:00:00Z', 'recording'),
                  ('newest', '2026-07-29T00:00:00Z', 'recording'),
                  ('done',  '2026-07-26T00:00:00Z', 'ready');
                "#,
            )
            .unwrap();

        // The dirty v10 database migrates cleanly (index build cannot fail).
        migrate(&connection).unwrap();

        // The newest stale recording survives (launch crash recovery will
        // salvage it); the older duplicate was repaired to failed with an
        // explicit reason; unrelated rows are untouched.
        let status_of = |id: &str| -> String {
            connection
                .query_row("SELECT status FROM meetings WHERE id=?1", [id], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(status_of("newest"), "recording");
        assert_eq!(status_of("older"), "failed");
        assert_eq!(status_of("done"), "ready");
        let reason: Option<String> = connection
            .query_row(
                "SELECT failure_reason FROM meetings WHERE id='older'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason.as_deref(), Some(STALE_DUPLICATE_RECORDING_REASON));

        // The invariant index exists…
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='ux_meetings_single_active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);

        // …and enforces at most one active recording: a second `recording`
        // row is rejected, while non-active statuses insert freely.
        let violation = connection.execute(
            "INSERT INTO meetings (id, created_at, status) VALUES ('x', '2026-07-30T00:00:00Z', 'recording')",
            [],
        );
        let message = violation
            .expect_err("second recording row must be rejected")
            .to_string();
        assert!(
            message.contains("ux_meetings_single_active"),
            "unexpected error: {message}"
        );
        connection
            .execute(
                "INSERT INTO meetings (id, created_at, status) VALUES ('y', '2026-07-30T00:00:00Z', 'processing')",
                [],
            )
            .unwrap();

        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn version_eleven_repair_is_a_noop_on_a_single_healthy_recording() {
        // A lone stale `recording` row (the normal crashed-once case) must
        // survive migration untouched so launch crash recovery can salvage it.
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO meetings (id, created_at, status) VALUES ('m1', '2026-07-29T00:00:00Z', 'recording')",
                [],
            )
            .unwrap();

        migrate(&connection).unwrap();

        let (status, reason): (String, Option<String>) = connection
            .query_row(
                "SELECT status, failure_reason FROM meetings WHERE id='m1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "recording");
        assert_eq!(reason, None);
    }

    #[test]
    fn version_twelve_adds_live_annotations_without_touching_v11_rows() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v11 database (schema recorded up to 11, no
        // live_annotations table) holding one finished meeting.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),(11);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT,
                  notes TEXT NOT NULL DEFAULT '',
                  system_audio_path TEXT
                );
                INSERT INTO meetings (id, created_at, status, title)
                VALUES ('m1', '2026-07-29T00:00:00Z', 'ready', '旧会议');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // The table and its lookup index now exist.
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='live_annotations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1);
        let indexes: Vec<String> = connection
            .prepare("PRAGMA index_list(live_annotations)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(indexes.contains(&"idx_live_annotations_meeting".to_owned()));

        // All expected columns are present.
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(live_annotations)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in [
            "id",
            "meeting_id",
            "start_seconds",
            "end_seconds",
            "channel",
            "identity_id",
            "display_name",
            "created_at",
        ] {
            assert!(columns.contains(&column.to_owned()), "missing {column}");
        }

        // The pre-existing meeting row is untouched.
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
    fn version_thirteen_adds_speaker_provenance_without_touching_v12_speakers() {
        let connection = Connection::open_in_memory().unwrap();
        // Stand up a v12 database (speakers table through v10's embedding
        // column, no provenance columns) holding one confirmed speaker.
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),(11),(12);
                CREATE TABLE meetings (
                  id TEXT PRIMARY KEY NOT NULL,
                  created_at TEXT NOT NULL,
                  title TEXT,
                  audio_path TEXT,
                  duration_seconds REAL,
                  status TEXT NOT NULL DEFAULT 'recording',
                  language TEXT,
                  failure_reason TEXT,
                  notes TEXT NOT NULL DEFAULT '',
                  system_audio_path TEXT
                );
                CREATE TABLE speakers (
                  id TEXT PRIMARY KEY NOT NULL,
                  meeting_id TEXT NOT NULL,
                  label TEXT NOT NULL,
                  display_name TEXT,
                  embedding_ref TEXT,
                  embedding BLOB,
                  FOREIGN KEY(meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
                );
                INSERT INTO meetings (id, created_at, status) VALUES ('m1', '2026-07-30T00:00:00Z', 'ready');
                INSERT INTO speakers (id, meeting_id, label, display_name) VALUES ('s1', 'm1', 'S1', '李明');
                "#,
            )
            .unwrap();

        migrate(&connection).unwrap();

        // All three provenance columns exist and pre-existing rows default to
        // NULL (provenance unknown for pre-v13 attributions).
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(speakers)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in [
            "identity_id",
            "attribution_origin",
            "attribution_confidence",
        ] {
            assert!(columns.contains(&column.to_owned()), "missing {column}");
        }
        let (identity, origin, confidence): (Option<String>, Option<String>, Option<f64>) =
            connection
                .query_row(
                    "SELECT identity_id, attribution_origin, attribution_confidence FROM speakers WHERE id='s1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
        assert_eq!(identity, None);
        assert_eq!(origin, None);
        assert_eq!(confidence, None);
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
    fn version_fourteen_backfills_only_consistent_silent_capture_evidence() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version)
                VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),(11),(12),(13);
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
                  pipeline_inputs_json TEXT NOT NULL DEFAULT '{"schema_version":1,"context":null,"stage_usages":[]}',
                  status TEXT NOT NULL,
                  failed_stage TEXT,
                  failure_message TEXT,
                  supersedes_attempt_id TEXT,
                  UNIQUE(session_id, attempt_ordinal)
                );

                INSERT INTO sessions (id, created_at, status) VALUES
                  ('structured', '2026-07-31T00:00:07Z', 'failed'),
                  ('legacy',     '2026-07-31T00:00:06Z', 'cancelled'),
                  ('other',      '2026-07-31T00:00:05Z', 'failed'),
                  ('boundary',   '2026-07-31T00:00:04Z', 'failed'),
                  ('corrupt',    '2026-07-31T00:00:03Z', 'failed'),
                  ('mismatch',   '2026-07-31T00:00:02Z', 'failed'),
                  ('ambiguous-legacy', '2026-07-31T00:00:00Z', 'failed'),
                  ('fractional', '2026-07-30T23:59:59Z', 'failed'),
                  ('object-issues', '2026-07-30T23:59:58Z', 'failed'),
                  ('missing-message', '2026-07-30T23:59:57Z', 'failed'),
                  ('oversized', '2026-07-30T23:59:56Z', 'failed');
                INSERT INTO sessions (id, created_at, corrected, status)
                VALUES ('with-text', '2026-07-31T00:00:01Z', '保留', 'failed');

                INSERT INTO dictation_attempts (
                  id, session_id, attempt_ordinal, created_at,
                  pipeline_identity_json, pipeline_metrics_json,
                  status, failed_stage, failure_message
                ) VALUES
                  ('a-structured', 'structured', 1, '2026-07-31T00:00:07Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'failed', 'capture', NULL),
                  ('a-legacy', 'legacy', 1, '2026-07-31T00:00:06Z', '{}',
                   '{"audio_duration_ms":0,"stage_issues":[]}',
                   'failed', 'capture', 'no audio captured (0 samples) — hold longer or check mic'),
                  ('a-other', 'other', 1, '2026-07-31T00:00:05Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"input_unavailable","message":"device unavailable"}]}',
                   'failed', 'capture', NULL),
                  ('a-boundary', 'boundary', 1, '2026-07-31T00:00:04Z', '{}',
                   '{"audio_duration_ms":2000,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'failed', 'capture', NULL),
                  ('a-corrupt', 'corrupt', 1, '2026-07-31T00:00:03Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":["{\"stage\":\"capture\",\"kind\":\"absolute_silence\"}"]}',
                   'failed', 'capture', NULL),
                  ('a-mismatch', 'mismatch', 1, '2026-07-31T00:00:02Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'completed', 'capture', NULL),
                  ('a-with-text', 'with-text', 1, '2026-07-31T00:00:01Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'failed', 'capture', NULL),
                  ('a-ambiguous-legacy', 'ambiguous-legacy', 1, '2026-07-31T00:00:00Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[]}',
                   'failed', 'capture', '未检测到麦克风信号。请检查麦克风权限、输入设备或静音状态后重试。'),
                  ('a-fractional', 'fractional', 1, '2026-07-30T23:59:59Z', '{}',
                   '{"audio_duration_ms":500.5,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'failed', 'capture', NULL),
                  ('a-object-issues', 'object-issues', 1, '2026-07-30T23:59:58Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}}',
                   'failed', 'capture', NULL),
                  ('a-missing-message', 'missing-message', 1, '2026-07-30T23:59:57Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence"}]}',
                   'failed', 'capture', NULL),
                  ('a-oversized', 'oversized', 1, '2026-07-30T23:59:56Z', '{}',
                   '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                   'failed', 'capture', NULL);
                "#,
            )
            .unwrap();

        let oversized_metrics = format!(
            r#"{{"audio_duration_ms":500,"stage_issues":[{{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}}],"padding":"{}"}}"#,
            "x".repeat(HISTORY_MIGRATION_MAX_METRICS_JSON_BYTES as usize),
        );
        connection
            .execute(
                "UPDATE dictation_attempts SET pipeline_metrics_json=?1 WHERE id='a-oversized'",
                params![oversized_metrics],
            )
            .unwrap();
        for index in 0..=HISTORY_MIGRATION_BATCH_SIZE {
            let session_id = format!("batch-{index:03}");
            connection
                .execute(
                    "INSERT INTO sessions (id, created_at, status) VALUES (?1, '2026-07-30T23:00:00Z', 'failed')",
                    params![session_id],
                )
                .unwrap();
            connection
                .execute(
                    r#"
                    INSERT INTO dictation_attempts (
                      id, session_id, attempt_ordinal, created_at,
                      pipeline_identity_json, pipeline_metrics_json,
                      status, failed_stage
                    ) VALUES (?1, ?2, 1, '2026-07-30T23:00:00Z', '{}',
                      '{"audio_duration_ms":500,"stage_issues":[{"stage":"capture","kind":"absolute_silence","message":"absolute_silence"}]}',
                      'failed', 'capture')
                    "#,
                    params![format!("attempt-{index:03}"), session_id],
                )
                .unwrap();
        }

        migrate(&connection).unwrap();

        let visibility = |id: &str| -> i64 {
            connection
                .query_row(
                    "SELECT history_visible FROM sessions WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(visibility("structured"), 0);
        assert_eq!(visibility("legacy"), 0);
        for visible in [
            "other",
            "boundary",
            "corrupt",
            "mismatch",
            "with-text",
            "ambiguous-legacy",
            "fractional",
            "object-issues",
            "missing-message",
            "oversized",
        ] {
            assert_eq!(visibility(visible), 1, "{visible} must fail open");
        }
        assert_eq!(visibility("batch-000"), 0);
        assert_eq!(visibility("batch-128"), 0);

        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_sessions_history_visible_created_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
        let version_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version=14",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version_count, 1);

        migrate(&connection).unwrap();
        assert_eq!(visibility("structured"), 0);
        assert_eq!(visibility("other"), 1);
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
