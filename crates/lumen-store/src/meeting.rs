//! Meeting-mode CRUD over the v6 tables.
//!
//! Domain shapes come from `lumen_core` (`Meeting`, `TranscriptSegment`,
//! `Speaker`, `MeetingSummary`); this module is only the SQLite mapping. Word
//! timing is stored as a `words_json` blob so the persisted segment stays
//! aligned with the `lumen-transcript.v1` `Word` shape.

use anyhow::Result;
use lumen_core::transcript::Word;
use lumen_core::{Meeting, MeetingStatus, MeetingSummary, Speaker, SummaryKind, TranscriptSegment};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::{parse_dt, parse_u32_column, parse_uuid_column, Store};

impl Store {
    // ----- meetings ---------------------------------------------------------

    /// Insert a new meeting row.
    pub fn create_meeting(&self, meeting: &Meeting) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO meetings (
              id, created_at, title, audio_path, duration_seconds, status, language
            ) VALUES (?1,?2,?3,?4,?5,?6,?7)
            "#,
            params![
                meeting.id.to_string(),
                meeting.created_at.to_rfc3339(),
                meeting.title,
                meeting.audio_path,
                meeting.duration_seconds,
                meeting.status.as_str(),
                meeting.language,
            ],
        )?;
        Ok(())
    }

    /// Update only the lifecycle status of a meeting. Returns `true` if a row
    /// was updated.
    pub fn update_meeting_status(&self, id: Uuid, status: MeetingStatus) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings SET status=?2 WHERE id=?1",
            params![id.to_string(), status.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Record the finalized recording (audio path + duration) and move the
    /// meeting to a new lifecycle status in one update. Used when a live
    /// recording stops: the WAV is on disk and the meeting moves to
    /// `Processing`. Returns `true` if a row was updated.
    pub fn set_meeting_audio(
        &self,
        id: Uuid,
        audio_path: &str,
        duration_seconds: f64,
        status: MeetingStatus,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings SET audio_path=?2, duration_seconds=?3, status=?4 WHERE id=?1",
            params![
                id.to_string(),
                audio_path,
                duration_seconds,
                status.as_str()
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn get_meeting(&self, id: Uuid) -> Result<Option<Meeting>> {
        self.conn
            .query_row(
                r#"
                SELECT id, created_at, title, audio_path, duration_seconds, status, language
                FROM meetings WHERE id=?1
                "#,
                params![id.to_string()],
                map_meeting,
            )
            .optional()
            .map_err(Into::into)
    }

    /// List meetings newest first.
    pub fn list_meetings(&self, limit: u32) -> Result<Vec<Meeting>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, created_at, title, audio_path, duration_seconds, status, language
            FROM meetings
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], map_meeting)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete a meeting; its segments, speakers, and summaries cascade away via
    /// foreign keys (the store opens connections with `foreign_keys = ON`).
    /// Returns `true` if a meeting was deleted.
    pub fn delete_meeting(&self, id: Uuid) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM meetings WHERE id=?1", params![id.to_string()])?;
        Ok(changed > 0)
    }

    // ----- transcript segments ---------------------------------------------

    /// Insert one transcript segment.
    pub fn add_segment(&self, segment: &TranscriptSegment) -> Result<()> {
        add_segment_on(&self.conn, segment)
    }

    /// Insert many transcript segments atomically.
    pub fn add_segments(&self, segments: &[TranscriptSegment]) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        for segment in segments {
            add_segment_on(&transaction, segment)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// List a meeting's segments in `seq` order.
    pub fn list_segments(&self, meeting_id: Uuid) -> Result<Vec<TranscriptSegment>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, seq, start_seconds, end_seconds, text,
                   speaker_id, confidence, words_json
            FROM transcript_segments
            WHERE meeting_id=?1
            ORDER BY seq ASC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_segment)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ----- speakers ---------------------------------------------------------

    /// Insert or update a speaker by id.
    pub fn upsert_speaker(&self, speaker: &Speaker) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO speakers (id, meeting_id, label, display_name, embedding_ref)
            VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(id) DO UPDATE SET
              label=excluded.label,
              display_name=excluded.display_name,
              embedding_ref=excluded.embedding_ref
            "#,
            params![
                speaker.id.to_string(),
                speaker.meeting_id.to_string(),
                speaker.label,
                speaker.display_name,
                speaker.embedding_ref,
            ],
        )?;
        Ok(())
    }

    /// Set a speaker's user-assigned display name. Returns `true` if updated.
    pub fn rename_speaker(&self, id: Uuid, display_name: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE speakers SET display_name=?2 WHERE id=?1",
            params![id.to_string(), display_name],
        )?;
        Ok(changed > 0)
    }

    /// List a meeting's speakers ordered by label.
    pub fn list_speakers(&self, meeting_id: Uuid) -> Result<Vec<Speaker>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, label, display_name, embedding_ref
            FROM speakers
            WHERE meeting_id=?1
            ORDER BY label ASC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_speaker)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ----- summaries --------------------------------------------------------

    /// Persist a generated summary.
    pub fn save_summary(&self, summary: &MeetingSummary) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO meeting_summaries (id, meeting_id, kind, content, created_at, model)
            VALUES (?1,?2,?3,?4,?5,?6)
            "#,
            params![
                summary.id.to_string(),
                summary.meeting_id.to_string(),
                summary.kind.as_str(),
                summary.content,
                summary.created_at.to_rfc3339(),
                summary.model,
            ],
        )?;
        Ok(())
    }

    /// Get the most recent summary of a given kind for a meeting.
    pub fn get_summary(
        &self,
        meeting_id: Uuid,
        kind: SummaryKind,
    ) -> Result<Option<MeetingSummary>> {
        self.conn
            .query_row(
                r#"
                SELECT id, meeting_id, kind, content, created_at, model
                FROM meeting_summaries
                WHERE meeting_id=?1 AND kind=?2
                ORDER BY created_at DESC
                LIMIT 1
                "#,
                params![meeting_id.to_string(), kind.as_str()],
                map_summary,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn add_segment_on(conn: &Connection, segment: &TranscriptSegment) -> Result<()> {
    let words_json = match &segment.words {
        Some(words) => Some(serde_json::to_string(words)?),
        None => None,
    };
    conn.execute(
        r#"
        INSERT INTO transcript_segments (
          id, meeting_id, seq, start_seconds, end_seconds, text,
          speaker_id, confidence, words_json
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
        "#,
        params![
            segment.id.to_string(),
            segment.meeting_id.to_string(),
            i64::from(segment.seq),
            segment.start_seconds,
            segment.end_seconds,
            segment.text,
            segment.speaker_id.map(|id| id.to_string()),
            segment.confidence,
            words_json,
        ],
    )?;
    Ok(())
}

fn map_meeting(row: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
    Ok(Meeting {
        id: parse_uuid_column(row, 0)?,
        created_at: parse_dt(&row.get::<_, String>(1)?),
        title: row.get(2)?,
        audio_path: row.get(3)?,
        duration_seconds: row.get(4)?,
        status: MeetingStatus::from_str_or_recording(&row.get::<_, String>(5)?),
        language: row.get(6)?,
    })
}

fn map_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptSegment> {
    let speaker_id = match row.get::<_, Option<String>>(6)? {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    let words = match row.get::<_, Option<String>>(8)? {
        Some(json) => Some(serde_json::from_str::<Vec<Word>>(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    Ok(TranscriptSegment {
        id: parse_uuid_column(row, 0)?,
        meeting_id: parse_uuid_column(row, 1)?,
        seq: parse_u32_column(row, 2)?,
        start_seconds: row.get(3)?,
        end_seconds: row.get(4)?,
        text: row.get(5)?,
        speaker_id,
        confidence: row.get(7)?,
        words,
    })
}

fn map_speaker(row: &rusqlite::Row<'_>) -> rusqlite::Result<Speaker> {
    Ok(Speaker {
        id: parse_uuid_column(row, 0)?,
        meeting_id: parse_uuid_column(row, 1)?,
        label: row.get(2)?,
        display_name: row.get(3)?,
        embedding_ref: row.get(4)?,
    })
}

fn map_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MeetingSummary> {
    Ok(MeetingSummary {
        id: parse_uuid_column(row, 0)?,
        meeting_id: parse_uuid_column(row, 1)?,
        kind: SummaryKind::from_str_or_summary(&row.get::<_, String>(2)?),
        content: row.get(3)?,
        created_at: parse_dt(&row.get::<_, String>(4)?),
        model: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use lumen_core::transcript::Word;
    use lumen_core::{
        Meeting, MeetingStatus, MeetingSummary, Speaker, SummaryKind, TranscriptSegment,
    };
    use uuid::Uuid;

    use crate::Store;

    fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("meeting.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn meeting_crud_round_trips() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.title = Some("Standup".into());
        meeting.audio_path = Some("/store/audio.wav".into());
        meeting.duration_seconds = Some(123.5);
        meeting.language = Some("zh-CN".into());
        store.create_meeting(&meeting).unwrap();

        let fetched = store.get_meeting(meeting.id).unwrap().expect("meeting");
        assert_eq!(fetched, meeting);

        assert!(store
            .update_meeting_status(meeting.id, MeetingStatus::Ready)
            .unwrap());
        let updated = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(updated.status, MeetingStatus::Ready);
        // Non-status fields are untouched by the status update.
        assert_eq!(updated.title.as_deref(), Some("Standup"));

        let listed = store.list_meetings(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, meeting.id);
    }

    #[test]
    fn set_meeting_audio_records_path_duration_and_status() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        // Fresh meeting starts Recording with no audio yet.
        assert_eq!(meeting.status, MeetingStatus::Recording);
        assert!(meeting.audio_path.is_none());
        store.create_meeting(&meeting).unwrap();

        assert!(store
            .set_meeting_audio(
                meeting.id,
                "/store/meetings/take.wav",
                61.5,
                MeetingStatus::Processing,
            )
            .unwrap());

        let updated = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(
            updated.audio_path.as_deref(),
            Some("/store/meetings/take.wav")
        );
        assert_eq!(updated.duration_seconds, Some(61.5));
        assert_eq!(updated.status, MeetingStatus::Processing);

        // No row for an unknown id.
        assert!(!store
            .set_meeting_audio(Uuid::new_v4(), "/x.wav", 1.0, MeetingStatus::Failed)
            .unwrap());
    }

    #[test]
    fn segments_persist_words_and_read_back_in_seq_order() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();

        // Insert out of order to prove the query sorts by seq.
        let mut second = TranscriptSegment::new(meeting.id, 1, 2.0, 4.0, "world");
        second.speaker_id = Some(speaker.id);
        second.confidence = Some(0.9);
        second.words = Some(vec![Word::new("world", 2.0, 4.0).with_confidence(0.9)]);
        let first = TranscriptSegment::new(meeting.id, 0, 0.0, 2.0, "hello");
        store
            .add_segments(&[second.clone(), first.clone()])
            .unwrap();

        let segments = store.list_segments(meeting.id).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], first);
        assert_eq!(segments[1], second);
        // Word-level timing survives the JSON round trip.
        let words = segments[1].words.as_ref().unwrap();
        assert_eq!(words[0].word, "world");
        assert_eq!(words[0].confidence, Some(0.9));
    }

    #[test]
    fn upsert_and_rename_speaker() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();

        assert!(store.rename_speaker(speaker.id, "Chris").unwrap());
        let speakers = store.list_speakers(meeting.id).unwrap();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].display_name.as_deref(), Some("Chris"));
        // Label unchanged; embedding_ref still reserved/empty.
        assert_eq!(speakers[0].label, "S1");
        assert_eq!(speakers[0].embedding_ref, None);

        // Upsert on the same id updates rather than duplicating.
        let mut updated = speakers[0].clone();
        updated.embedding_ref = Some("identity/chris".into());
        store.upsert_speaker(&updated).unwrap();
        let speakers = store.list_speakers(meeting.id).unwrap();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].embedding_ref.as_deref(), Some("identity/chris"));
    }

    #[test]
    fn summaries_save_and_get_by_kind() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        let summary = MeetingSummary::new(meeting.id, SummaryKind::Summary, "overall");
        let actions = {
            let mut s = MeetingSummary::new(meeting.id, SummaryKind::ActionItems, "do the thing");
            s.model = Some("qwen2.5".into());
            s
        };
        store.save_summary(&summary).unwrap();
        store.save_summary(&actions).unwrap();

        let got = store
            .get_summary(meeting.id, SummaryKind::Summary)
            .unwrap()
            .unwrap();
        assert_eq!(got.content, "overall");
        let got_actions = store
            .get_summary(meeting.id, SummaryKind::ActionItems)
            .unwrap()
            .unwrap();
        assert_eq!(got_actions.model.as_deref(), Some("qwen2.5"));
        assert!(store
            .get_summary(meeting.id, SummaryKind::Decisions)
            .unwrap()
            .is_none());
    }

    #[test]
    fn delete_meeting_cascades_segments_speakers_and_summaries() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();
        let mut segment = TranscriptSegment::new(meeting.id, 0, 0.0, 1.0, "hi");
        segment.speaker_id = Some(speaker.id);
        store.add_segment(&segment).unwrap();
        store
            .save_summary(&MeetingSummary::new(meeting.id, SummaryKind::Summary, "s"))
            .unwrap();

        assert!(store.delete_meeting(meeting.id).unwrap());

        assert!(store.get_meeting(meeting.id).unwrap().is_none());
        assert!(store.list_segments(meeting.id).unwrap().is_empty());
        assert!(store.list_speakers(meeting.id).unwrap().is_empty());
        assert!(store
            .get_summary(meeting.id, SummaryKind::Summary)
            .unwrap()
            .is_none());
        // Deleting a missing meeting is a no-op.
        assert!(!store.delete_meeting(Uuid::new_v4()).unwrap());
    }
}
