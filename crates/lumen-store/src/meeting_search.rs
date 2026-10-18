//! Full-text search over the meeting library (v20 `meeting_search` index).
//!
//! The index itself is maintained by triggers (see `schema.rs`); this module
//! only queries it. Terms of at least three characters go through FTS5 `MATCH`
//! (trigram tokenizer: substring, case-insensitive) and are ranked by BM25. A
//! shorter term cannot be matched by a trigram index, so it falls back to a
//! case-insensitive `LIKE` substring check over the same indexed text.

use anyhow::Result;
use lumen_core::{Meeting, MeetingStatus};
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use serde::Serialize;

use crate::meeting::map_meeting;
use crate::Store;

/// Marks the start of a highlighted match inside [`MeetingSearchHit::snippet`].
/// A control character so it can never collide with transcript text.
pub const SEARCH_SNIPPET_MARK_START: &str = "\u{2}";
/// Marks the end of a highlighted match inside [`MeetingSearchHit::snippet`].
pub const SEARCH_SNIPPET_MARK_END: &str = "\u{3}";

/// Shortest term the trigram tokenizer can match.
const MIN_FTS_TERM_CHARS: usize = 3;
/// Characters of context kept before the highlighted match in a snippet.
const SNIPPET_CONTEXT_BEFORE: usize = 12;
/// Characters of context kept after the highlighted match in a snippet.
const SNIPPET_CONTEXT_AFTER: usize = 24;

/// Which part of a meeting a search hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MeetingSearchField {
    Title,
    Notes,
    Segment,
    Summary,
}

impl MeetingSearchField {
    fn from_token(token: &str) -> Self {
        match token {
            "title" => Self::Title,
            "notes" => Self::Notes,
            "summary" => Self::Summary,
            _ => Self::Segment,
        }
    }
}

/// One meeting matching a library search, with the best-matching excerpt.
#[derive(Debug, Clone, Serialize)]
pub struct MeetingSearchHit {
    pub meeting: Meeting,
    /// Field the snippet was taken from.
    pub field: MeetingSearchField,
    /// Short excerpt around the match; matched text is wrapped in
    /// [`SEARCH_SNIPPET_MARK_START`] / [`SEARCH_SNIPPET_MARK_END`].
    pub snippet: String,
}

/// Columns `map_meeting` expects, prefixed with `m.`.
const MEETING_COLUMNS: &str = "m.id, m.created_at, m.title, m.audio_path, m.duration_seconds, \
     m.status, m.language, m.failure_reason, m.notes, m.system_audio_path";

impl Store {
    /// Search meetings by title, notes, transcript text and final minutes.
    ///
    /// Every whitespace-separated term must occur in the same indexed text
    /// (one title, notes, segment or minutes row), case-insensitively. Terms
    /// of three or more characters use the FTS5 trigram index and rank by
    /// BM25; shorter terms, which a trigram index cannot match, are checked
    /// with `LIKE`. Returns at most `limit` meetings, each once with the
    /// snippet of its best row, ordered by relevance then recency (a query of
    /// only short terms has no relevance score, so it is ordered by recency).
    /// Within a meeting, equally relevant rows prefer the title, then notes,
    /// then minutes, then transcript. A blank query returns nothing — use
    /// [`Store::list_meetings_filtered`] to list without a query.
    pub fn search_meetings(
        &self,
        status: Option<MeetingStatus>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MeetingSearchHit>> {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let (long, short): (Vec<&str>, Vec<&str>) = terms
            .iter()
            .partition(|term| term.chars().count() >= MIN_FTS_TERM_CHARS);

        let mut conditions = Vec::new();
        let mut score = "0.0";
        if !long.is_empty() {
            conditions.push("meeting_search MATCH ?1".to_string());
            score = "bm25(meeting_search)";
        }
        // ?1 match expression, ?2 status, ?3 limit, ?4.. LIKE patterns.
        for index in 0..short.len() {
            conditions.push(format!("s.body LIKE ?{} ESCAPE '\\'", index + 4));
        }
        let sql = format!(
            r#"
            WITH hits AS (
              SELECT d.meeting_id AS meeting_id, d.id AS rid, d.field AS field,
                     {score} AS score
              FROM meeting_search s
              JOIN meeting_search_docs d ON d.id = s.rowid
              WHERE {conditions}
            ), best AS (
              SELECT meeting_id, rid, field, score,
                     ROW_NUMBER() OVER (
                       PARTITION BY meeting_id
                       ORDER BY score,
                                CASE field
                                  WHEN 'title' THEN 0
                                  WHEN 'notes' THEN 1
                                  WHEN 'summary' THEN 2
                                  ELSE 3
                                END,
                                rid
                     ) AS rn
              FROM hits
            )
            SELECT {MEETING_COLUMNS}, b.field,
                   (SELECT body FROM meeting_search WHERE rowid = b.rid)
            FROM best b
            JOIN meetings m ON m.id = b.meeting_id
            WHERE b.rn = 1 AND (?2 IS NULL OR m.status = ?2)
            ORDER BY b.score ASC, m.created_at DESC
            LIMIT ?3
            "#,
            conditions = conditions.join(" AND "),
        );

        let mut values: Vec<Value> = vec![
            Value::Text(fts_match_expression(&long)),
            status.map_or(Value::Null, |s| Value::Text(s.as_str().to_string())),
            Value::Integer(i64::from(limit)),
        ];
        values.extend(
            short
                .iter()
                .map(|term| Value::Text(format!("%{}%", escape_like(term)))),
        );

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok((
                map_meeting(row)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (meeting, field, body) = row?;
            hits.push(MeetingSearchHit {
                meeting,
                field: MeetingSearchField::from_token(&field),
                snippet: match_snippet(&body, &terms),
            });
        }
        Ok(hits)
    }
}

/// Build an FTS5 expression that requires every term (implicit AND). Each term
/// is a quoted string so FTS5 operators and punctuation in user input are
/// matched literally instead of being parsed as query syntax.
fn fts_match_expression(terms: &[&str]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Case folding shared by snippet matching: single-character lowercase, the
/// granularity both the trigram tokenizer and `LIKE` compare at.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Char offset of the first case-insensitive occurrence of `needle`.
fn find_folded(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| fold(*a) == fold(*b))
    })
}

/// Excerpt around the earliest occurrence of any of `terms` in `body`, with
/// that occurrence wrapped in the snippet marks and `…` where text was cut.
/// Falls back to the start of `body` if no term is found verbatim.
fn match_snippet(body: &str, terms: &[&str]) -> String {
    let haystack: Vec<char> = body.chars().collect();
    let found = terms
        .iter()
        .filter_map(|term| {
            let needle: Vec<char> = term.chars().collect();
            find_folded(&haystack, &needle).map(|start| (start, start + needle.len()))
        })
        .min();
    let (start, end) = found.unwrap_or((0, 0));
    let from = start.saturating_sub(SNIPPET_CONTEXT_BEFORE);
    let to = (end.max(from) + SNIPPET_CONTEXT_AFTER).min(haystack.len());
    let mut snippet = String::new();
    if from > 0 {
        snippet.push('…');
    }
    snippet.extend(&haystack[from..start]);
    if end > start {
        snippet.push_str(SEARCH_SNIPPET_MARK_START);
        snippet.extend(&haystack[start..end]);
        snippet.push_str(SEARCH_SNIPPET_MARK_END);
    }
    snippet.extend(&haystack[end..to]);
    if to < haystack.len() {
        snippet.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use lumen_core::{Meeting, MeetingStatus, MeetingSummary, SummaryKind, TranscriptSegment};
    use rusqlite::Connection;
    use uuid::Uuid;

    use super::{match_snippet, MeetingSearchField, MeetingSearchHit};
    use crate::{Store, SEARCH_SNIPPET_MARK_END as E, SEARCH_SNIPPET_MARK_START as S};

    fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("search.sqlite")).unwrap();
        (dir, store)
    }

    /// A meeting created `days_ago` days back, so recency order is explicit.
    fn meeting(store: &Store, title: Option<&str>, days_ago: i64) -> Meeting {
        let mut meeting = Meeting::new();
        meeting.title = title.map(str::to_string);
        meeting.status = MeetingStatus::Ready;
        meeting.created_at = Utc::now() - Duration::days(days_ago);
        store.create_meeting(&meeting).unwrap();
        meeting
    }

    fn segment(store: &Store, meeting_id: Uuid, seq: u32, text: &str) -> TranscriptSegment {
        let segment = TranscriptSegment::new(meeting_id, seq, seq as f64, seq as f64 + 1.0, text);
        store.add_segment(&segment).unwrap();
        segment
    }

    fn ids(hits: &[MeetingSearchHit]) -> Vec<Uuid> {
        hits.iter().map(|hit| hit.meeting.id).collect()
    }

    fn search(store: &Store, query: &str) -> Vec<MeetingSearchHit> {
        store.search_meetings(None, query, 50).unwrap()
    }

    fn index_rows(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM meeting_search", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn bundled_sqlite_provides_fts5_with_the_trigram_tokenizer() {
        let connection = Connection::open_in_memory().unwrap();
        let fts5: bool = connection
            .query_row(
                "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts5, "bundled SQLite must be compiled with FTS5");
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE probe USING fts5(body, tokenize='trigram');
                 INSERT INTO probe(body) VALUES ('季度预算评审会议');",
            )
            .unwrap();
        let hits: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH '\"预算评\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn search_finds_title_segment_minutes_and_notes_and_names_the_field() {
        let (_dir, store) = open_store();
        let by_title = meeting(&store, Some("Roadmap planning"), 4);
        let by_segment = meeting(&store, Some("Weekly sync"), 3);
        segment(
            &store,
            by_segment.id,
            0,
            "we should revisit the roadmap soon",
        );
        let by_minutes = meeting(&store, None, 2);
        store
            .save_summary(&MeetingSummary::new(
                by_minutes.id,
                SummaryKind::Summary,
                r#"{"one_liner":"Agreed the roadmap freeze","decisions":[]}"#,
            ))
            .unwrap();
        let by_notes = meeting(&store, None, 1);
        store
            .set_meeting_notes(by_notes.id, "remember: ROADMAP owners")
            .unwrap();
        let unrelated = meeting(&store, Some("Lunch"), 0);
        segment(&store, unrelated.id, 0, "nothing to see here");

        let hits = search(&store, "roadmap");
        let mut found = ids(&hits);
        found.sort();
        let mut expected = vec![by_title.id, by_segment.id, by_minutes.id, by_notes.id];
        expected.sort();
        assert_eq!(found, expected);
        let field_of = |id: Uuid| hits.iter().find(|h| h.meeting.id == id).unwrap().field;
        assert_eq!(field_of(by_title.id), MeetingSearchField::Title);
        assert_eq!(field_of(by_segment.id), MeetingSearchField::Segment);
        assert_eq!(field_of(by_minutes.id), MeetingSearchField::Summary);
        assert_eq!(field_of(by_notes.id), MeetingSearchField::Notes);
        let segment_hit = hits.iter().find(|h| h.meeting.id == by_segment.id).unwrap();
        assert!(
            segment_hit.snippet.contains(&format!("{S}roadmap{E}")),
            "snippet highlights the match: {:?}",
            segment_hit.snippet
        );
        // JSON keys of the minutes are not indexed.
        assert!(search(&store, "one_liner").is_empty());
    }

    #[test]
    fn chinese_queries_match_substrings_without_word_segmentation() {
        let (_dir, store) = open_store();
        let target = meeting(&store, Some("周会"), 1);
        segment(&store, target.id, 0, "下个季度的预算评审放在周五");
        let other = meeting(&store, Some("周会"), 0);
        segment(&store, other.id, 0, "讨论招聘计划");

        let hits = search(&store, "预算评审");
        assert_eq!(ids(&hits), vec![target.id]);
        assert_eq!(hits[0].field, MeetingSearchField::Segment);
        assert!(hits[0].snippet.contains(&format!("{S}预算评审{E}")));
        // Multiple terms must all match (implicit AND).
        assert_eq!(ids(&search(&store, "预算评审 周五")), vec![target.id]);
        assert!(search(&store, "预算评审 招聘计划").is_empty());
    }

    #[test]
    fn ranks_by_relevance_then_recency() {
        let (_dir, store) = open_store();
        let older = meeting(&store, Some("budget"), 5);
        let newer = meeting(&store, Some("budget"), 1);
        let weak = meeting(&store, Some("misc"), 0);
        segment(
            &store,
            weak.id,
            0,
            "a long rambling segment that eventually mentions the budget once among many \
             other unrelated words about lunch travel hiring and weather",
        );
        let hits = search(&store, "budget");
        // Equal-score title hits: newest first; the diluted segment hit last.
        assert_eq!(ids(&hits), vec![newer.id, older.id, weak.id]);
    }

    #[test]
    fn short_queries_fall_back_to_case_insensitive_substring() {
        let (_dir, store) = open_store();
        let ai = meeting(&store, Some("AI review"), 2);
        let cn = meeting(&store, None, 1);
        segment(&store, cn.id, 0, "今天聊预算");
        meeting(&store, Some("Lunch"), 0);

        assert_eq!(ids(&search(&store, "ai")), vec![ai.id]);
        let cn_hits = search(&store, "预算");
        assert_eq!(ids(&cn_hits), vec![cn.id]);
        assert_eq!(cn_hits[0].snippet, format!("今天聊{S}预算{E}"));
        // Short and long terms combine: both must occur in the same text.
        assert_eq!(ids(&search(&store, "ai review")), vec![ai.id]);
        assert!(search(&store, "ai lunch").is_empty());
        // LIKE wildcards in the query are literal.
        assert!(search(&store, "%").is_empty());
        assert!(search(&store, "_").is_empty());
    }

    #[test]
    fn status_filter_blank_query_and_fts_syntax_are_safe() {
        let (_dir, store) = open_store();
        let ready = meeting(&store, Some("Design review"), 1);
        let mut failed = Meeting::new();
        failed.title = Some("Design crash".into());
        failed.status = MeetingStatus::Failed;
        store.create_meeting(&failed).unwrap();

        let ready_only = store
            .search_meetings(Some(MeetingStatus::Ready), "design", 10)
            .unwrap();
        assert_eq!(ids(&ready_only), vec![ready.id]);
        assert!(search(&store, "   ").is_empty());
        // FTS operators and quotes are matched literally, never a syntax error.
        assert!(search(&store, "design\" OR \"x").is_empty());
        assert!(search(&store, "NEAR(design)").is_empty());
        assert_eq!(search(&store, "design*").len(), 0);
    }

    #[test]
    fn index_follows_every_write_path() {
        let (_dir, store) = open_store();
        let m = meeting(&store, Some("Alpha kickoff"), 0);
        let seg = segment(&store, m.id, 0, "first draft of the charter");

        // Rename: old title gone, new title found.
        store.set_meeting_title(m.id, "Beta kickoff").unwrap();
        assert!(search(&store, "Alpha").is_empty());
        assert_eq!(ids(&search(&store, "Beta")), vec![m.id]);
        store
            .set_meeting_title_if_untitled(m.id, "ignored")
            .unwrap();

        // Segment edit re-indexes the text.
        store.update_segment_text(seg.id, "final charter").unwrap();
        assert!(search(&store, "first draft").is_empty());
        assert_eq!(ids(&search(&store, "final charter")), vec![m.id]);

        // Notes update, then clearing notes removes them.
        store.set_meeting_notes(m.id, "zebra crossing").unwrap();
        assert_eq!(ids(&search(&store, "zebra")), vec![m.id]);
        store.set_meeting_notes(m.id, "").unwrap();
        assert!(search(&store, "zebra").is_empty());

        // Draft minutes are not indexed; final minutes are, and replace it.
        let draft = MeetingSummary::new(
            m.id,
            SummaryKind::MinutesDraft,
            r#"{"one_liner":"provisional walrus"}"#,
        );
        assert!(store.save_minutes_draft_if_no_final(&draft).unwrap());
        assert!(search(&store, "walrus").is_empty());
        store
            .save_summary(&MeetingSummary::new(
                m.id,
                SummaryKind::Summary,
                r#"{"one_liner":"final narwhal"}"#,
            ))
            .unwrap();
        assert_eq!(ids(&search(&store, "narwhal")), vec![m.id]);

        // Malformed summary content is still indexed as plain text.
        store
            .save_summary(&MeetingSummary::new(
                m.id,
                SummaryKind::ActionItems,
                "{not json: platypus",
            ))
            .unwrap();
        assert_eq!(ids(&search(&store, "platypus")), vec![m.id]);

        // Clearing the transcript drops segments and final minutes.
        store.clear_meeting_transcript(m.id).unwrap();
        assert!(search(&store, "charter").is_empty());
        assert!(search(&store, "narwhal").is_empty());
        assert_eq!(ids(&search(&store, "Beta")), vec![m.id]);
    }

    #[test]
    fn deleting_a_meeting_removes_it_from_the_index() {
        let (_dir, store) = open_store();
        let gone = meeting(&store, Some("Quarterly review"), 1);
        segment(&store, gone.id, 0, "quarterly numbers");
        store
            .save_summary(&MeetingSummary::new(
                gone.id,
                SummaryKind::Summary,
                r#"{"one_liner":"quarterly wrap"}"#,
            ))
            .unwrap();
        store.set_meeting_notes(gone.id, "quarterly notes").unwrap();
        let kept = meeting(&store, Some("Quarterly plan"), 0);
        let atomic = meeting(&store, Some("Quarterly extra"), 2);
        segment(&store, atomic.id, 0, "quarterly extra words");
        assert_eq!(index_rows(&store), 7);

        assert!(store.delete_meeting(gone.id).unwrap());
        assert!(store
            .delete_meeting_with_audio_paths(atomic.id)
            .unwrap()
            .is_some());
        assert_eq!(ids(&search(&store, "quarterly")), vec![kept.id]);
        assert_eq!(index_rows(&store), 1);
        let orphan_docs: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM meeting_search_docs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(orphan_docs, 1);
    }

    #[test]
    fn migration_backfills_meetings_that_predate_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let m = {
            let store = Store::open(&path).unwrap();
            let m = meeting(&store, Some("Legacy offsite"), 0);
            segment(&store, m.id, 0, "历史会议的逐字稿");
            store
                .save_summary(&MeetingSummary::new(
                    m.id,
                    SummaryKind::Summary,
                    r#"{"one_liner":"legacy minutes"}"#,
                ))
                .unwrap();
            store
                .save_minutes_draft_if_no_final(&MeetingSummary::new(
                    m.id,
                    SummaryKind::MinutesDraft,
                    "draft",
                ))
                .unwrap();
            store.set_meeting_notes(m.id, "legacy notes").unwrap();
            // Simulate a pre-v20 database: no index, no triggers, no marker.
            store
                .conn
                .execute_batch(
                    r#"
                    DROP TRIGGER meeting_search_docs_ad;
                    DROP TRIGGER meeting_search_meetings_ai;
                    DROP TRIGGER meeting_search_meetings_au_title;
                    DROP TRIGGER meeting_search_meetings_au_notes;
                    DROP TRIGGER meeting_search_meetings_ad;
                    DROP TRIGGER meeting_search_segments_ai;
                    DROP TRIGGER meeting_search_segments_au;
                    DROP TRIGGER meeting_search_segments_ad;
                    DROP TRIGGER meeting_search_summaries_ai;
                    DROP TRIGGER meeting_search_summaries_au;
                    DROP TRIGGER meeting_search_summaries_ad;
                    DROP TABLE meeting_search;
                    DROP TABLE meeting_search_docs;
                    DELETE FROM schema_migrations WHERE version = 20;
                    "#,
                )
                .unwrap();
            m
        };

        let store = Store::open(&path).unwrap();
        // title + notes + segment + final minutes; the draft stays out.
        assert_eq!(index_rows(&store), 4);
        assert_eq!(ids(&search(&store, "offsite")), vec![m.id]);
        assert_eq!(ids(&search(&store, "逐字稿")), vec![m.id]);
        assert_eq!(ids(&search(&store, "legacy minutes")), vec![m.id]);
        assert_eq!(ids(&search(&store, "legacy notes")), vec![m.id]);

        // Reopening (marker present) neither rebuilds nor duplicates rows.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(index_rows(&store), 4);
    }

    #[test]
    fn snippet_trims_context_and_marks_the_earliest_match() {
        let body = "0123456789abcdefghijXYzklmnopqrstuvwxyz0123456789Q";
        assert_eq!(
            match_snippet(body, &["q", "xy"]),
            format!("…89abcdefghij{S}XY{E}zklmnopqrstuvwxyz0123456…")
        );
        assert_eq!(match_snippet("短", &["zz"]), "短");
        assert_eq!(
            match_snippet("Ärger im Büro", &["ärger"]),
            format!("{S}Ärger{E} im Büro")
        );
    }
}
