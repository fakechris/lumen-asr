//! Meeting-mode CRUD over the v6 tables.
//!
//! Domain shapes come from `lumen_core` (`Meeting`, `TranscriptSegment`,
//! `Speaker`, `MeetingSummary`); this module is only the SQLite mapping. Word
//! timing is stored as a `words_json` blob so the persisted segment stays
//! aligned with the `lumen-transcript.v1` `Word` shape.

use anyhow::Result;
use lumen_core::transcript::Word;
use lumen_core::{
    LiveAnnotation, Meeting, MeetingDetail, MeetingStatus, MeetingSummary, SegmentChannel, Speaker,
    SummaryKind, TranscriptSegment,
};
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
              id, created_at, title, audio_path, duration_seconds, status,
              language, failure_reason, notes, system_audio_path
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
            "#,
            params![
                meeting.id.to_string(),
                meeting.created_at.to_rfc3339(),
                meeting.title,
                meeting.audio_path,
                meeting.duration_seconds,
                meeting.status.as_str(),
                meeting.language,
                meeting.failure_reason,
                meeting.notes,
                meeting.system_audio_path,
            ],
        )?;
        Ok(())
    }

    /// Update only the lifecycle status of a meeting. Returns `true` if a row
    /// was updated.
    ///
    /// Moving a meeting to any **non-failed** status clears any previously
    /// recorded [`failure_reason`](lumen_core::Meeting::failure_reason) — so a
    /// meeting that is re-processed (advancing back through
    /// `transcribing → … → ready`) does not keep a stale reason from an earlier
    /// failed run. To move a meeting *to* failed **with** a reason, use
    /// [`fail_meeting`](Self::fail_meeting).
    pub fn update_meeting_status(&self, id: Uuid, status: MeetingStatus) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings \
             SET status=?2, \
                 failure_reason = CASE WHEN ?2 = 'failed' THEN failure_reason ELSE NULL END \
             WHERE id=?1",
            params![id.to_string(), status.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Move a meeting to [`MeetingStatus::Failed`] and record why. The `reason`
    /// is surfaced to the UI (`get_meeting` / `get_meeting_detail` / list) so a
    /// failure is actionable instead of a bare "失败" badge. Returns `true` if a
    /// row was updated.
    pub fn fail_meeting(&self, id: Uuid, reason: Option<&str>) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings SET status='failed', failure_reason=?2 WHERE id=?1",
            params![id.to_string(), reason],
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
            "UPDATE meetings \
             SET audio_path=?2, duration_seconds=?3, status=?4, \
                 failure_reason = CASE WHEN ?4 = 'failed' THEN failure_reason ELSE NULL END \
             WHERE id=?1",
            params![
                id.to_string(),
                audio_path,
                duration_seconds,
                status.as_str()
            ],
        )?;
        Ok(changed > 0)
    }

    /// Record (or clear, with `None`) the path of the meeting's second,
    /// synchronized system-audio WAV. Written at recording start so crash
    /// recovery can find the file, and cleared at stop when the system track
    /// turned out empty/unusable. Only this column changes. Returns `true` if a
    /// row was updated.
    pub fn set_meeting_system_audio_path(&self, id: Uuid, path: Option<&str>) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings SET system_audio_path=?2 WHERE id=?1",
            params![id.to_string(), path],
        )?;
        Ok(changed > 0)
    }

    /// Rename a meeting: update only its `title`, leaving every other field
    /// untouched. A blank (empty/whitespace) `title` is stored as `NULL` so the
    /// meeting reads back as untitled — matching how untitled meetings are
    /// created and how the title search treats them. Returns `true` if a row was
    /// updated.
    pub fn set_meeting_title(&self, id: Uuid, title: &str) -> Result<bool> {
        let trimmed = title.trim();
        let value = (!trimmed.is_empty()).then_some(trimmed);
        let changed = self.conn.execute(
            "UPDATE meetings SET title=?2 WHERE id=?1",
            params![id.to_string(), value],
        )?;
        Ok(changed > 0)
    }

    /// Overwrite a meeting's free-form user notes. The caller (front-end)
    /// debounces; this is a plain last-write-wins update of just the `notes`
    /// column, leaving every other field untouched. Returns `true` if a row was
    /// updated.
    pub fn set_meeting_notes(&self, id: Uuid, notes: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE meetings SET notes=?2 WHERE id=?1",
            params![id.to_string(), notes],
        )?;
        Ok(changed > 0)
    }

    pub fn get_meeting(&self, id: Uuid) -> Result<Option<Meeting>> {
        self.conn
            .query_row(
                r#"
                SELECT id, created_at, title, audio_path, duration_seconds, status,
                       language, failure_reason, notes, system_audio_path
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
            SELECT id, created_at, title, audio_path, duration_seconds, status,
                   language, failure_reason, notes, system_audio_path
            FROM meetings
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], map_meeting)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// List meetings newest first, optionally filtered by lifecycle `status`
    /// and/or a case-sensitive title substring `query`. Both filters are
    /// optional and independent; an empty/whitespace query is treated as no
    /// query. Untitled meetings never match a non-empty `query` (their title is
    /// `NULL`, and `NULL LIKE …` is never true) — which is the intended
    /// behavior for a title search.
    pub fn list_meetings_filtered(
        &self,
        status: Option<MeetingStatus>,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Meeting>> {
        let status_token = status.map(|s| s.as_str());
        // Raw substring; matched with `instr()` below so the (case-sensitive)
        // contract is query-local and does not depend on the connection's
        // `case_sensitive_like` setting.
        let needle = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string);
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, created_at, title, audio_path, duration_seconds, status,
                   language, failure_reason, notes, system_audio_path
            FROM meetings
            WHERE (?1 IS NULL OR status = ?1)
              AND (?2 IS NULL OR instr(title, ?2) > 0)
            ORDER BY created_at DESC
            LIMIT ?3
            "#,
        )?;
        let rows = statement.query_map(params![status_token, needle, limit], map_meeting)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// List every meeting in a given lifecycle `status`, newest first. Thin
    /// convenience over [`list_meetings_filtered`](Self::list_meetings_filtered)
    /// with no title query and no practical limit.
    ///
    /// Used by crash recovery on launch to find meetings left in
    /// [`MeetingStatus::Recording`] by a previous run that was killed mid-capture
    /// (the stop path never ran, so the row never advanced past `recording`).
    pub fn list_meetings_by_status(&self, status: MeetingStatus) -> Result<Vec<Meeting>> {
        self.list_meetings_filtered(Some(status), None, u32::MAX)
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

    /// Overwrite only the `text` of one transcript segment. Its timing
    /// (`start_seconds`/`end_seconds`), ordering (`seq`), speaker attribution
    /// (`speaker_id`), and word timing (`words_json`) are all left untouched.
    ///
    /// This is the manual "the ASR mis-heard this sentence, fix the words" edit
    /// from the review page — it never moves a segment in time or reassigns it.
    /// Returns `true` if a row was updated (`false` for an unknown segment id).
    pub fn update_segment_text(&self, segment_id: Uuid, text: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE transcript_segments SET text=?2 WHERE id=?1",
            params![segment_id.to_string(), text],
        )?;
        Ok(changed > 0)
    }

    /// List a meeting's segments in `seq` order.
    pub fn list_segments(&self, meeting_id: Uuid) -> Result<Vec<TranscriptSegment>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, seq, start_seconds, end_seconds, text,
                   speaker_id, confidence, words_json, channel
            FROM transcript_segments
            WHERE meeting_id=?1
            ORDER BY seq ASC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_segment)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ----- speakers ---------------------------------------------------------

    /// Insert or update a speaker by id (including its v13 provenance columns:
    /// identity link, attribution origin, and verification confidence).
    pub fn upsert_speaker(&self, speaker: &Speaker) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO speakers (
              id, meeting_id, label, display_name, embedding_ref,
              identity_id, attribution_origin, attribution_confidence
            )
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
            ON CONFLICT(id) DO UPDATE SET
              label=excluded.label,
              display_name=excluded.display_name,
              embedding_ref=excluded.embedding_ref,
              identity_id=excluded.identity_id,
              attribution_origin=excluded.attribution_origin,
              attribution_confidence=excluded.attribution_confidence
            "#,
            params![
                speaker.id.to_string(),
                speaker.meeting_id.to_string(),
                speaker.label,
                speaker.display_name,
                speaker.embedding_ref,
                speaker.identity_id.map(|id| id.to_string()),
                speaker.attribution_origin,
                speaker.attribution_confidence,
            ],
        )?;
        Ok(())
    }

    /// Set a speaker's user-assigned display name — the "this cluster is really
    /// 李明" edit from the review page. This is the single "name + confirm" path:
    /// a speaker with a non-empty `display_name` reads back as **confirmed**, and
    /// a blank (empty/whitespace) name is stored as `NULL` so the speaker reverts
    /// to **unconfirmed** — matching how a freshly diarized speaker starts, how
    /// [`MeetingDetail::speaker_name`](lumen_core::MeetingDetail::speaker_name)
    /// falls back to the engine label, and how `set_meeting_title` blanks an
    /// untitled meeting. Confirmation is derived from the name rather than a
    /// separate column, so there is a single source of truth. Returns `true` if a
    /// row was updated.
    /// Provenance (v13): a human typed this name, so the attribution origin is
    /// recorded as `manual` (the top of the manual > verification >
    /// offline_diarization priority) with no confidence score; blanking the
    /// name clears the provenance too, since an unnamed speaker has none. Any
    /// previous identity link is cleared either way — a typed name is an
    /// ad-hoc string, not a library pick.
    pub fn rename_speaker(&self, id: Uuid, display_name: &str) -> Result<bool> {
        let trimmed = display_name.trim();
        let value = (!trimmed.is_empty()).then_some(trimmed);
        let origin = value.map(|_| lumen_core::attribution_origin::MANUAL);
        let changed = self.conn.execute(
            "UPDATE speakers SET display_name=?2, identity_id=NULL,
             attribution_origin=?3, attribution_confidence=NULL WHERE id=?1",
            params![id.to_string(), value, origin],
        )?;
        Ok(changed > 0)
    }

    /// List a meeting's speakers ordered by label.
    pub fn list_speakers(&self, meeting_id: Uuid) -> Result<Vec<Speaker>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, label, display_name, embedding_ref,
                   identity_id, attribution_origin, attribution_confidence
            FROM speakers
            WHERE meeting_id=?1
            ORDER BY label ASC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_speaker)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Store a speaker's centroid voiceprint embedding (v9 `embedding` BLOB,
    /// f32 little-endian). Written by the diarization pipeline right after the
    /// speaker rows; read back by voiceprint enrollment. Returns `true` if a
    /// row was updated.
    pub fn set_speaker_embedding(&self, id: Uuid, embedding: &[f32]) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE speakers SET embedding=?2 WHERE id=?1",
            params![id.to_string(), embedding_to_bytes(embedding)],
        )?;
        Ok(changed > 0)
    }

    /// Read back a speaker's centroid voiceprint embedding, or `None` when the
    /// speaker has none (pre-v9 meetings, or non-diarized builds). A stored
    /// blob whose length is not a multiple of 4 is rejected as corrupt.
    pub fn get_speaker_embedding(&self, id: Uuid) -> Result<Option<Vec<f32>>> {
        let blob: Option<Option<Vec<u8>>> = self
            .conn
            .query_row(
                "SELECT embedding FROM speakers WHERE id=?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match blob.flatten() {
            Some(bytes) => Ok(Some(embedding_from_bytes(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Reassign one transcript segment to a different speaker. This is the
    /// "the model split this one line onto the wrong person" fix — it moves a
    /// single segment without touching the rest of the cluster. Returns `true`
    /// if the segment was updated.
    ///
    /// The target speaker must belong to the same meeting as the segment;
    /// otherwise the update is a no-op (returns `false`) rather than attaching
    /// the segment to a speaker from another meeting.
    pub fn reassign_segment_speaker(&self, segment_id: Uuid, speaker_id: Uuid) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE transcript_segments SET speaker_id=?2 \
             WHERE id=?1 \
               AND EXISTS ( \
                 SELECT 1 FROM speakers \
                 WHERE speakers.id=?2 AND speakers.meeting_id=transcript_segments.meeting_id \
               )",
            params![segment_id.to_string(), speaker_id.to_string()],
        )?;
        Ok(changed > 0)
    }

    /// Merge speaker `from` into speaker `into` within one meeting: every
    /// segment attributed to `from` is re-pointed at `into`, then the now-empty
    /// `from` speaker row is deleted. This is the "these two clusters are
    /// actually the same person" fix. Runs in a single transaction so the
    /// re-point and the delete are atomic. Returns the number of segments moved.
    ///
    /// A no-op (`from == into`) returns `0` without touching anything. The
    /// `meeting_id` scopes both statements defensively even though speaker ids
    /// are globally unique.
    pub fn merge_speakers(&self, meeting_id: Uuid, from: Uuid, into: Uuid) -> Result<u64> {
        if from == into {
            return Ok(0);
        }
        let transaction = self.conn.unchecked_transaction()?;
        // Refuse to merge into a speaker that doesn't belong to this meeting —
        // otherwise the meeting's segments would be attached to a foreign
        // speaker row.
        let into_owned: bool = transaction
            .query_row(
                "SELECT 1 FROM speakers WHERE id=?1 AND meeting_id=?2",
                params![into.to_string(), meeting_id.to_string()],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !into_owned {
            return Err(anyhow::anyhow!(
                "merge target speaker does not belong to meeting {meeting_id}"
            ));
        }
        let moved = transaction.execute(
            "UPDATE transcript_segments SET speaker_id=?3 WHERE meeting_id=?1 AND speaker_id=?2",
            params![meeting_id.to_string(), from.to_string(), into.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM speakers WHERE id=?1 AND meeting_id=?2",
            params![from.to_string(), meeting_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(moved as u64)
    }

    // ----- live annotations -------------------------------------------------

    /// Append one recording-time "who is speaking" annotation (v12
    /// `live_annotations`). Annotations are append-only while recording;
    /// overlapping ranges are resolved last-write-wins (`created_at`) by the
    /// offline reconciliation, so no dedup happens here.
    pub fn add_live_annotation(&self, annotation: &LiveAnnotation) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO live_annotations (
              id, meeting_id, start_seconds, end_seconds, channel,
              identity_id, display_name, unassigned, created_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            "#,
            params![
                annotation.id.to_string(),
                annotation.meeting_id.to_string(),
                annotation.start_seconds,
                annotation.end_seconds,
                annotation.channel.as_str(),
                annotation.identity_id.map(|id| id.to_string()),
                annotation.display_name,
                annotation.unassigned as i64,
                annotation.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// List a meeting's live annotations, oldest first (`created_at`, then id
    /// as a stable tiebreak) — the order reconciliation's last-write-wins
    /// resolution expects.
    pub fn list_live_annotations(&self, meeting_id: Uuid) -> Result<Vec<LiveAnnotation>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, start_seconds, end_seconds, channel,
                   identity_id, display_name, unassigned, created_at
            FROM live_annotations
            WHERE meeting_id=?1
            ORDER BY created_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_live_annotation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read one live annotation by id, or `None` when it does not exist.
    /// Used by the delete command to know which annotation (track + name) is
    /// about to go away, so the live worker can retract the matching
    /// session-voiceprint samples.
    pub fn get_live_annotation(&self, id: Uuid) -> Result<Option<LiveAnnotation>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, start_seconds, end_seconds, channel,
                   identity_id, display_name, unassigned, created_at
            FROM live_annotations
            WHERE id=?1
            "#,
        )?;
        let mut rows = statement.query_map(params![id.to_string()], map_live_annotation)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Delete one live annotation (the "清除" action on an annotated caption
    /// line). Returns `true` if a row was deleted.
    pub fn delete_live_annotation(&self, id: Uuid) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM live_annotations WHERE id=?1",
            params![id.to_string()],
        )?;
        Ok(changed > 0)
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

    /// List every stored summary for a meeting, newest first (all kinds).
    pub fn list_summaries(&self, meeting_id: Uuid) -> Result<Vec<MeetingSummary>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, meeting_id, kind, content, created_at, model
            FROM meeting_summaries
            WHERE meeting_id=?1
            ORDER BY created_at DESC
            "#,
        )?;
        let rows = statement.query_map(params![meeting_id.to_string()], map_summary)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ----- aggregate read ---------------------------------------------------

    /// Read a meeting and everything attached to it (speakers, `seq`-ordered
    /// segments, all summaries) in one call. Returns `None` if the meeting does
    /// not exist. This is the single query the detail view and the export
    /// functions consume.
    pub fn get_meeting_detail(&self, meeting_id: Uuid) -> Result<Option<MeetingDetail>> {
        let Some(meeting) = self.get_meeting(meeting_id)? else {
            return Ok(None);
        };
        Ok(Some(MeetingDetail {
            meeting,
            speakers: self.list_speakers(meeting_id)?,
            segments: self.list_segments(meeting_id)?,
            summaries: self.list_summaries(meeting_id)?,
        }))
    }
}

/// Serialize an embedding as f32 little-endian bytes (the v9 BLOB contract).
fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for value in embedding {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Inverse of [`embedding_to_bytes`]; errors on a length that is not a
/// multiple of 4 (a corrupt blob) instead of silently truncating.
fn embedding_from_bytes(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        anyhow::bail!("speaker embedding blob has invalid length {}", bytes.len());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
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
          speaker_id, confidence, words_json, channel
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
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
            segment.channel.map(|c| c.as_str()),
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
        failure_reason: row.get(7)?,
        notes: row.get(8)?,
        system_audio_path: row.get(9)?,
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
        channel: row
            .get::<_, Option<String>>(9)?
            .map(|token| SegmentChannel::from_str_or_mic(&token)),
    })
}

fn map_speaker(row: &rusqlite::Row<'_>) -> rusqlite::Result<Speaker> {
    // `identity_id` is best-effort provenance: a corrupt uuid reads as None
    // rather than failing the whole speaker list.
    let identity_id = row
        .get::<_, Option<String>>(5)?
        .and_then(|value| Uuid::parse_str(&value).ok());
    Ok(Speaker {
        id: parse_uuid_column(row, 0)?,
        meeting_id: parse_uuid_column(row, 1)?,
        label: row.get(2)?,
        display_name: row.get(3)?,
        embedding_ref: row.get(4)?,
        identity_id,
        attribution_origin: row.get(6)?,
        attribution_confidence: row.get(7)?,
    })
}

fn map_live_annotation(row: &rusqlite::Row<'_>) -> rusqlite::Result<LiveAnnotation> {
    let identity_id = match row.get::<_, Option<String>>(5)? {
        Some(value) => Some(Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    Ok(LiveAnnotation {
        id: parse_uuid_column(row, 0)?,
        meeting_id: parse_uuid_column(row, 1)?,
        start_seconds: row.get(2)?,
        end_seconds: row.get(3)?,
        channel: SegmentChannel::from_str_or_mic(&row.get::<_, String>(4)?),
        identity_id,
        display_name: row.get(6)?,
        unassigned: row.get::<_, i64>(7)? != 0,
        created_at: parse_dt(&row.get::<_, String>(8)?),
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
    fn speaker_embedding_round_trips_and_defaults_none() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();

        // A fresh speaker has no embedding; an unknown id reads back None too.
        assert_eq!(store.get_speaker_embedding(speaker.id).unwrap(), None);
        assert_eq!(store.get_speaker_embedding(Uuid::new_v4()).unwrap(), None);

        // f32 LE roundtrip is exact, including negatives and non-round values.
        let embedding: Vec<f32> = (0..256).map(|i| (i as f32 - 128.0) * 0.017).collect();
        assert!(store.set_speaker_embedding(speaker.id, &embedding).unwrap());
        assert_eq!(
            store.get_speaker_embedding(speaker.id).unwrap(),
            Some(embedding.clone())
        );

        // Overwrite wins; other speaker fields are untouched.
        let other: Vec<f32> = vec![1.5; 256];
        assert!(store.set_speaker_embedding(speaker.id, &other).unwrap());
        assert_eq!(
            store.get_speaker_embedding(speaker.id).unwrap(),
            Some(other)
        );
        let listed = store.list_speakers(meeting.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "S1");

        // Setting on an unknown speaker updates nothing.
        assert!(!store
            .set_speaker_embedding(Uuid::new_v4(), &embedding)
            .unwrap());
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
    fn system_audio_path_round_trips_and_clears() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.audio_path = Some("/store/m.wav".into());
        // Fresh meetings have no system track.
        assert_eq!(meeting.system_audio_path, None);
        store.create_meeting(&meeting).unwrap();
        assert_eq!(
            store
                .get_meeting(meeting.id)
                .unwrap()
                .unwrap()
                .system_audio_path,
            None
        );

        // Recording start writes the path; only that column changes.
        assert!(store
            .set_meeting_system_audio_path(meeting.id, Some("/store/m.system.wav"))
            .unwrap());
        let updated = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(
            updated.system_audio_path.as_deref(),
            Some("/store/m.system.wav")
        );
        assert_eq!(updated.audio_path.as_deref(), Some("/store/m.wav"));
        assert_eq!(updated.status, MeetingStatus::Recording);

        // An empty/unusable system track clears back to NULL at stop.
        assert!(store
            .set_meeting_system_audio_path(meeting.id, None)
            .unwrap());
        assert_eq!(
            store
                .get_meeting(meeting.id)
                .unwrap()
                .unwrap()
                .system_audio_path,
            None
        );

        // No row for an unknown id.
        assert!(!store
            .set_meeting_system_audio_path(Uuid::new_v4(), Some("/x.wav"))
            .unwrap());
    }

    #[test]
    fn segment_channel_round_trips_and_legacy_rows_read_none() {
        use lumen_core::SegmentChannel;

        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        let mut mic = TranscriptSegment::new(meeting.id, 0, 0.0, 1.0, "我这边说");
        mic.channel = Some(SegmentChannel::Mic);
        let mut system = TranscriptSegment::new(meeting.id, 1, 1.0, 2.0, "对方在说");
        system.channel = Some(SegmentChannel::System);
        let legacy = TranscriptSegment::new(meeting.id, 2, 2.0, 3.0, "旧单轨");
        store
            .add_segments(&[mic.clone(), system.clone(), legacy.clone()])
            .unwrap();

        let segments = store.list_segments(meeting.id).unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0], mic);
        assert_eq!(segments[0].channel, Some(SegmentChannel::Mic));
        assert_eq!(segments[1], system);
        assert_eq!(segments[1].channel, Some(SegmentChannel::System));
        // A segment stored without a channel (legacy single-track) reads None.
        assert_eq!(segments[2], legacy);
        assert_eq!(segments[2].channel, None);
    }

    #[test]
    fn set_meeting_notes_updates_only_notes_and_defaults_empty() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.title = Some("Standup".into());
        // A fresh meeting starts with empty notes.
        assert_eq!(meeting.notes, "");
        store.create_meeting(&meeting).unwrap();

        // The persisted row reads back empty notes.
        let fresh = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(fresh.notes, "");

        // Writing notes updates only that field.
        assert!(store
            .set_meeting_notes(meeting.id, "记得跟进预算\n- 张三负责上线")
            .unwrap());
        let updated = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(updated.notes, "记得跟进预算\n- 张三负责上线");
        assert_eq!(updated.title.as_deref(), Some("Standup"));
        assert_eq!(updated.status, MeetingStatus::Recording);

        // Last-write-wins overwrite, including clearing back to empty.
        assert!(store.set_meeting_notes(meeting.id, "").unwrap());
        assert_eq!(store.get_meeting(meeting.id).unwrap().unwrap().notes, "");

        // Notes come through the aggregate detail read as well.
        assert!(store.set_meeting_notes(meeting.id, "重点A").unwrap());
        let detail = store.get_meeting_detail(meeting.id).unwrap().unwrap();
        assert_eq!(detail.meeting.notes, "重点A");

        // No row for an unknown id.
        assert!(!store.set_meeting_notes(Uuid::new_v4(), "x").unwrap());
    }

    #[test]
    fn set_meeting_title_updates_only_title_and_blanks_to_untitled() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.title = Some("Kickoff".into());
        meeting.notes = "开场要点".into();
        meeting.language = Some("zh-CN".into());
        store.create_meeting(&meeting).unwrap();

        // Rename updates only the title; notes/language/status are untouched.
        assert!(store.set_meeting_title(meeting.id, "季度复盘").unwrap());
        let renamed = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(renamed.title.as_deref(), Some("季度复盘"));
        assert_eq!(renamed.notes, "开场要点");
        assert_eq!(renamed.language.as_deref(), Some("zh-CN"));
        assert_eq!(renamed.status, MeetingStatus::Recording);

        // Surrounding whitespace is trimmed.
        assert!(store.set_meeting_title(meeting.id, "  周会  ").unwrap());
        assert_eq!(
            store
                .get_meeting(meeting.id)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("周会")
        );

        // A blank title clears back to untitled (NULL) so the UI shows the
        // "未命名会议" fallback and the title search excludes it.
        assert!(store.set_meeting_title(meeting.id, "   ").unwrap());
        assert_eq!(store.get_meeting(meeting.id).unwrap().unwrap().title, None);

        // No row for an unknown id.
        assert!(!store.set_meeting_title(Uuid::new_v4(), "x").unwrap());
    }

    #[test]
    fn create_meeting_persists_initial_notes() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.notes = "开场要点".into();
        store.create_meeting(&meeting).unwrap();
        let fetched = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(fetched, meeting);
        assert_eq!(fetched.notes, "开场要点");
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
    fn fail_meeting_records_reason_and_non_failed_transition_clears_it() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        // Failing records both the status and a human-readable reason.
        assert!(store
            .fail_meeting(
                meeting.id,
                Some("diar models not found: missing segmentation")
            )
            .unwrap());
        let failed = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(failed.status, MeetingStatus::Failed);
        assert_eq!(
            failed.failure_reason.as_deref(),
            Some("diar models not found: missing segmentation")
        );

        // Re-processing (a non-failed transition) clears the stale reason so a
        // meeting that recovers does not keep an old failure message.
        assert!(store
            .update_meeting_status(meeting.id, MeetingStatus::Transcribing)
            .unwrap());
        let recovering = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(recovering.status, MeetingStatus::Transcribing);
        assert_eq!(recovering.failure_reason, None);

        // Failing without a specific reason is allowed (reason stays NULL).
        assert!(store.fail_meeting(meeting.id, None).unwrap());
        let failed_again = store.get_meeting(meeting.id).unwrap().unwrap();
        assert_eq!(failed_again.status, MeetingStatus::Failed);
        assert_eq!(failed_again.failure_reason, None);
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
    fn update_segment_text_changes_only_text() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();

        let mut seg = TranscriptSegment::new(meeting.id, 3, 12.5, 15.0, "wrong words");
        seg.speaker_id = Some(speaker.id);
        seg.confidence = Some(0.7);
        seg.words = Some(vec![Word::new("wrong", 12.5, 15.0).with_confidence(0.7)]);
        store.add_segment(&seg).unwrap();

        // Editing the text updates only that column.
        assert!(store
            .update_segment_text(seg.id, "corrected words")
            .unwrap());
        let updated = store.list_segments(meeting.id).unwrap();
        assert_eq!(updated.len(), 1);
        let got = &updated[0];
        assert_eq!(got.text, "corrected words");
        // Timing, ordering, speaker, confidence, and word timing are untouched.
        assert_eq!(got.start_seconds, 12.5);
        assert_eq!(got.end_seconds, 15.0);
        assert_eq!(got.seq, 3);
        assert_eq!(got.speaker_id, Some(speaker.id));
        assert_eq!(got.confidence, Some(0.7));
        assert_eq!(got.words.as_ref().unwrap()[0].word, "wrong");

        // Empty text is allowed (last-write-wins, e.g. the user cleared a line).
        assert!(store.update_segment_text(seg.id, "").unwrap());
        assert_eq!(store.list_segments(meeting.id).unwrap()[0].text, "");

        // Unknown segment id is a no-op.
        assert!(!store.update_segment_text(Uuid::new_v4(), "x").unwrap());
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
    fn speaker_provenance_round_trips_and_rename_marks_manual() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        // A verification-attributed speaker persists its full provenance.
        let identity = Uuid::new_v4();
        let mut speaker = Speaker::new(meeting.id, "S1");
        speaker.display_name = Some("李明".into());
        speaker.identity_id = Some(identity);
        speaker.attribution_origin = Some(lumen_core::attribution_origin::VERIFICATION.into());
        speaker.attribution_confidence = Some(0.82);
        store.upsert_speaker(&speaker).unwrap();

        let read = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(read.identity_id, Some(identity));
        assert_eq!(
            read.attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::VERIFICATION)
        );
        assert_eq!(read.attribution_confidence, Some(0.82));

        // A manual rename (#67 path) overrides: origin becomes manual, the
        // identity link and confidence are cleared (typed name ≠ library pick).
        assert!(store.rename_speaker(speaker.id, "张三").unwrap());
        let renamed = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(renamed.display_name.as_deref(), Some("张三"));
        assert_eq!(
            renamed.attribution_origin.as_deref(),
            Some(lumen_core::attribution_origin::MANUAL)
        );
        assert_eq!(renamed.identity_id, None);
        assert_eq!(renamed.attribution_confidence, None);

        // Blanking the name clears the provenance with it.
        assert!(store.rename_speaker(speaker.id, " ").unwrap());
        let cleared = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(cleared.display_name, None);
        assert_eq!(cleared.attribution_origin, None);
    }

    #[test]
    fn rename_speaker_confirms_by_name_and_blank_clears_to_unconfirmed() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let speaker = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&speaker).unwrap();

        // A freshly diarized speaker has no name → unconfirmed.
        let fresh = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(fresh.display_name, None);

        // Setting a real name confirms the speaker (surrounding whitespace is
        // trimmed) and the aggregate read surfaces the display name.
        assert!(store.rename_speaker(speaker.id, "  李明  ").unwrap());
        let named = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(named.display_name.as_deref(), Some("李明"));
        assert_eq!(named.label, "S1");
        let detail = store.get_meeting_detail(meeting.id).unwrap().unwrap();
        assert_eq!(detail.speaker_name(Some(speaker.id)), "李明");

        // Clearing the name (blank/whitespace) stores NULL, reverting the speaker
        // to unconfirmed so it falls back to the engine label again.
        assert!(store.rename_speaker(speaker.id, "   ").unwrap());
        let cleared = &store.list_speakers(meeting.id).unwrap()[0];
        assert_eq!(cleared.display_name, None);
        let detail = store.get_meeting_detail(meeting.id).unwrap().unwrap();
        assert_eq!(detail.speaker_name(Some(speaker.id)), "S1");

        // Unknown speaker id is a no-op.
        assert!(!store.rename_speaker(Uuid::new_v4(), "X").unwrap());
    }

    #[test]
    fn live_annotations_round_trip_ordered_and_delete() {
        use lumen_core::{LiveAnnotation, SegmentChannel};

        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        // Fresh meetings have no annotations.
        assert!(store.list_live_annotations(meeting.id).unwrap().is_empty());

        // One enrolled-identity annotation and one ad-hoc typed name (open
        // ended: the live segment had not finalized yet).
        let identity = Uuid::new_v4();
        let mut first = LiveAnnotation::new(
            meeting.id,
            3.0,
            Some(6.5),
            SegmentChannel::Mic,
            Some(identity),
            "李明",
        );
        first.created_at = crate::parse_dt("2026-07-30T00:00:01Z");
        let mut second =
            LiveAnnotation::new(meeting.id, 8.0, None, SegmentChannel::System, None, "客户A");
        second.created_at = crate::parse_dt("2026-07-30T00:00:02Z");
        // Insert newest first to prove the list orders by created_at.
        store.add_live_annotation(&second).unwrap();
        store.add_live_annotation(&first).unwrap();

        let listed = store.list_live_annotations(meeting.id).unwrap();
        assert_eq!(listed, vec![first.clone(), second.clone()]);
        assert_eq!(listed[0].identity_id, Some(identity));
        assert_eq!(listed[0].end_seconds, Some(6.5));
        assert_eq!(listed[1].identity_id, None);
        assert_eq!(listed[1].end_seconds, None);
        assert_eq!(listed[1].channel, SegmentChannel::System);

        // Clearing one annotation removes only that row.
        assert!(store.delete_live_annotation(second.id).unwrap());
        assert_eq!(
            store.list_live_annotations(meeting.id).unwrap(),
            vec![first]
        );
        // Deleting a missing annotation is a no-op.
        assert!(!store.delete_live_annotation(Uuid::new_v4()).unwrap());
    }

    #[test]
    fn unassigned_none_boundary_round_trips() {
        use lumen_core::{LiveAnnotation, SegmentChannel};

        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();

        // A "无" boundary: no name, no identity, unassigned set.
        let mut none = LiveAnnotation::none_boundary(meeting.id, 42.0, SegmentChannel::Mic);
        none.created_at = crate::parse_dt("2026-07-30T00:00:05Z");
        // …and a normal named boundary alongside it.
        let mut named =
            LiveAnnotation::new(meeting.id, 10.0, None, SegmentChannel::Mic, None, "张三");
        named.created_at = crate::parse_dt("2026-07-30T00:00:04Z");
        store.add_live_annotation(&named).unwrap();
        store.add_live_annotation(&none).unwrap();

        let listed = store.list_live_annotations(meeting.id).unwrap();
        assert_eq!(listed, vec![named, none.clone()]);
        assert!(listed[1].unassigned);
        assert_eq!(listed[1].display_name, "");
        assert_eq!(listed[1].identity_id, None);
        // Named boundaries default to not-unassigned.
        assert!(!listed[0].unassigned);
    }

    #[test]
    fn get_live_annotation_reads_one_row_by_id() {
        use lumen_core::{LiveAnnotation, SegmentChannel};

        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let mut annotation =
            LiveAnnotation::new(meeting.id, 1.0, None, SegmentChannel::System, None, "客户A");
        annotation.created_at = crate::parse_dt("2026-07-30T00:00:03Z");
        store.add_live_annotation(&annotation).unwrap();

        assert_eq!(
            store.get_live_annotation(annotation.id).unwrap(),
            Some(annotation.clone())
        );
        // Missing id reads as None (not an error).
        assert_eq!(store.get_live_annotation(Uuid::new_v4()).unwrap(), None);
        // And a deleted row is gone.
        assert!(store.delete_live_annotation(annotation.id).unwrap());
        assert_eq!(store.get_live_annotation(annotation.id).unwrap(), None);
    }

    #[test]
    fn delete_meeting_cascades_live_annotations() {
        use lumen_core::{LiveAnnotation, SegmentChannel};

        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        store
            .add_live_annotation(&LiveAnnotation::new(
                meeting.id,
                0.0,
                Some(2.0),
                SegmentChannel::Mic,
                None,
                "张三",
            ))
            .unwrap();

        assert!(store.delete_meeting(meeting.id).unwrap());
        assert!(store.list_live_annotations(meeting.id).unwrap().is_empty());
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

    #[test]
    fn reassign_segment_moves_one_line_without_touching_the_cluster() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let s1 = Speaker::new(meeting.id, "S1");
        let s2 = Speaker::new(meeting.id, "S2");
        store.upsert_speaker(&s1).unwrap();
        store.upsert_speaker(&s2).unwrap();

        let mut seg0 = TranscriptSegment::new(meeting.id, 0, 0.0, 1.0, "a");
        seg0.speaker_id = Some(s1.id);
        let mut seg1 = TranscriptSegment::new(meeting.id, 1, 1.0, 2.0, "b");
        seg1.speaker_id = Some(s1.id);
        store.add_segments(&[seg0.clone(), seg1.clone()]).unwrap();

        // Move only the second line to S2.
        assert!(store.reassign_segment_speaker(seg1.id, s2.id).unwrap());
        let segments = store.list_segments(meeting.id).unwrap();
        assert_eq!(segments[0].speaker_id, Some(s1.id));
        assert_eq!(segments[1].speaker_id, Some(s2.id));

        // Unknown segment id is a no-op.
        assert!(!store
            .reassign_segment_speaker(Uuid::new_v4(), s2.id)
            .unwrap());
    }

    #[test]
    fn merge_speakers_repoints_segments_and_deletes_the_source() {
        let (_dir, store) = open_store();
        let meeting = Meeting::new();
        store.create_meeting(&meeting).unwrap();
        let keep = Speaker::new(meeting.id, "S1");
        let dupe = Speaker::new(meeting.id, "S2");
        store.upsert_speaker(&keep).unwrap();
        store.upsert_speaker(&dupe).unwrap();

        let mut a = TranscriptSegment::new(meeting.id, 0, 0.0, 1.0, "a");
        a.speaker_id = Some(keep.id);
        let mut b = TranscriptSegment::new(meeting.id, 1, 1.0, 2.0, "b");
        b.speaker_id = Some(dupe.id);
        let mut c = TranscriptSegment::new(meeting.id, 2, 2.0, 3.0, "c");
        c.speaker_id = Some(dupe.id);
        store.add_segments(&[a.clone(), b, c]).unwrap();

        // Merge S2 into S1: both of S2's segments move, S2 disappears.
        let moved = store.merge_speakers(meeting.id, dupe.id, keep.id).unwrap();
        assert_eq!(moved, 2);
        let speakers = store.list_speakers(meeting.id).unwrap();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].id, keep.id);
        for seg in store.list_segments(meeting.id).unwrap() {
            assert_eq!(seg.speaker_id, Some(keep.id));
        }

        // Merging a speaker into itself is a no-op.
        assert_eq!(
            store.merge_speakers(meeting.id, keep.id, keep.id).unwrap(),
            0
        );
    }

    #[test]
    fn speaker_ops_reject_speakers_from_another_meeting() {
        let (_dir, store) = open_store();
        let m1 = Meeting::new();
        // m2 must not also be `recording`: the v11 single-active index allows
        // at most one active recording at a time.
        let mut m2 = Meeting::new();
        m2.status = MeetingStatus::Ready;
        store.create_meeting(&m1).unwrap();
        store.create_meeting(&m2).unwrap();
        let m1_spk = Speaker::new(m1.id, "S1");
        let m2_spk = Speaker::new(m2.id, "X1");
        store.upsert_speaker(&m1_spk).unwrap();
        store.upsert_speaker(&m2_spk).unwrap();

        let mut seg = TranscriptSegment::new(m1.id, 0, 0.0, 1.0, "a");
        seg.speaker_id = Some(m1_spk.id);
        store.add_segments(&[seg.clone()]).unwrap();

        // Reassigning a m1 segment to a m2 speaker is rejected (no-op, false);
        // the segment keeps its original speaker.
        assert!(!store.reassign_segment_speaker(seg.id, m2_spk.id).unwrap());
        assert_eq!(
            store.list_segments(m1.id).unwrap()[0].speaker_id,
            Some(m1_spk.id)
        );

        // Merging within m1 into a m2 speaker is rejected, and m1's segment is
        // left untouched.
        assert!(store.merge_speakers(m1.id, m1_spk.id, m2_spk.id).is_err());
        assert_eq!(
            store.list_segments(m1.id).unwrap()[0].speaker_id,
            Some(m1_spk.id)
        );
    }

    #[test]
    fn get_meeting_detail_aggregates_meeting_speakers_segments_and_summaries() {
        let (_dir, store) = open_store();
        let mut meeting = Meeting::new();
        meeting.title = Some("Sync".into());
        store.create_meeting(&meeting).unwrap();
        let s1 = Speaker::new(meeting.id, "S1");
        store.upsert_speaker(&s1).unwrap();
        // Insert out of order; detail must return segments in seq order.
        let seg1 = TranscriptSegment::new(meeting.id, 1, 1.0, 2.0, "second");
        let seg0 = TranscriptSegment::new(meeting.id, 0, 0.0, 1.0, "first");
        store.add_segments(&[seg1, seg0]).unwrap();
        store
            .save_summary(&MeetingSummary::new(meeting.id, SummaryKind::Summary, "{}"))
            .unwrap();
        store
            .save_summary(&MeetingSummary::new(
                meeting.id,
                SummaryKind::Decisions,
                "[]",
            ))
            .unwrap();

        let detail = store.get_meeting_detail(meeting.id).unwrap().unwrap();
        assert_eq!(detail.meeting.id, meeting.id);
        assert_eq!(detail.speakers.len(), 1);
        assert_eq!(detail.segments.len(), 2);
        assert_eq!(detail.segments[0].text, "first");
        assert_eq!(detail.segments[1].text, "second");
        assert_eq!(detail.summaries.len(), 2);
        assert_eq!(detail.speaker_name(Some(s1.id)), "S1");
        assert_eq!(detail.speaker_name(None), "未知说话人");

        assert!(store.get_meeting_detail(Uuid::new_v4()).unwrap().is_none());
    }

    #[test]
    fn list_meetings_by_status_finds_interrupted_recordings() {
        let (_dir, store) = open_store();
        // One meeting left mid-recording by a crashed run (the v11
        // single-active index guarantees there is never more than one), plus
        // meetings that advanced normally.
        let crashed = Meeting::new(); // status = Recording
        let mut processing = Meeting::new();
        processing.status = MeetingStatus::Processing;
        let mut done = Meeting::new();
        done.status = MeetingStatus::Ready;
        store.create_meeting(&crashed).unwrap();
        store.create_meeting(&processing).unwrap();
        store.create_meeting(&done).unwrap();

        let stale = store
            .list_meetings_by_status(MeetingStatus::Recording)
            .unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, crashed.id);

        // A status with no meetings returns empty.
        assert!(store
            .list_meetings_by_status(MeetingStatus::Failed)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn single_active_recording_invariant_is_enforced() {
        let (_dir, store) = open_store();
        let first = Meeting::new(); // status = Recording
        store.create_meeting(&first).unwrap();

        // A second concurrent recording is rejected by the v11 unique index.
        let second = Meeting::new();
        let err = store
            .create_meeting(&second)
            .expect_err("second active recording must be rejected");
        assert!(
            err.to_string().contains("ux_meetings_single_active"),
            "unexpected error: {err}"
        );

        // Once the first advances out of `recording`, a new recording starts
        // fine — and flipping an old meeting back into `recording` while one
        // is active is rejected too.
        assert!(store
            .update_meeting_status(first.id, MeetingStatus::Processing)
            .unwrap());
        store.create_meeting(&second).unwrap();
        assert!(store
            .update_meeting_status(first.id, MeetingStatus::Recording)
            .is_err());

        // Multiple meetings in the post-recording pipeline states may coexist
        // (background processing of one meeting while another records).
        let mut third = Meeting::new();
        third.status = MeetingStatus::Transcribing;
        store.create_meeting(&third).unwrap();
    }

    #[test]
    fn list_meetings_filtered_by_status_and_title_query() {
        let (_dir, store) = open_store();
        let mut ready = Meeting::new();
        ready.title = Some("Weekly Planning".into());
        ready.status = MeetingStatus::Ready;
        let mut failed = Meeting::new();
        failed.title = Some("Broken Recording".into());
        failed.status = MeetingStatus::Failed;
        let untitled = Meeting::new(); // Recording, no title
        store.create_meeting(&ready).unwrap();
        store.create_meeting(&failed).unwrap();
        store.create_meeting(&untitled).unwrap();

        // Status filter only.
        let ready_only = store
            .list_meetings_filtered(Some(MeetingStatus::Ready), None, 10)
            .unwrap();
        assert_eq!(ready_only.len(), 1);
        assert_eq!(ready_only[0].id, ready.id);

        // Title substring only (case-sensitive), untitled excluded.
        let planning = store
            .list_meetings_filtered(None, Some("Planning"), 10)
            .unwrap();
        assert_eq!(planning.len(), 1);
        assert_eq!(planning[0].id, ready.id);

        // No filters returns everything.
        assert_eq!(
            store.list_meetings_filtered(None, None, 10).unwrap().len(),
            3
        );

        // Blank query behaves like no query.
        assert_eq!(
            store
                .list_meetings_filtered(None, Some("   "), 10)
                .unwrap()
                .len(),
            3
        );

        // Combined filters with no match.
        assert!(store
            .list_meetings_filtered(Some(MeetingStatus::Ready), Some("Broken"), 10)
            .unwrap()
            .is_empty());
    }
}
