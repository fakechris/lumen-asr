//! Crash-safe journal of a recording's live transcript.
//!
//! The live layer ([`crate::meeting_live`]) keeps its finalized segments in
//! memory and hands them back at stop for draft minutes. A crash mid-meeting
//! would lose that text until the offline pipeline has re-transcribed the
//! salvaged audio, which takes a sizeable fraction of the meeting's length. The
//! journal closes that gap: every finalized live segment, and a periodic
//! checkpoint of each track's unfinished partial, is appended to
//! `<meetings_dir>/<meeting_id>.live.jsonl`, so crash recovery can produce
//! draft minutes from it right away.
//!
//! ## Format
//! One JSON object per line, each carrying a `seq` that strictly increases
//! through the file:
//!
//! - `{"kind":"final","seq":..,"track":..,"segment_id":..,"start_ms":..,"end_ms":..,"text":..}`
//!   — a finalized segment. A later line for the same `segment_id` supersedes
//!   it; an empty `text` retracts it (the live event contract).
//! - `{"kind":"checkpoint","seq":..,"partials":[{track,segment_id,start_ms,end_ms,text}]}`
//!   — the unfinished text of each track at that moment. On recovery a
//!   partial counts as a segment unless a higher-`seq` line replaced it.
//!
//! ## Durability
//! The file is created `0600` (its directory `0700`) and only ever appended.
//! Lines are written by a dedicated thread, never the live decode loop, and
//! each drained batch is `fsync`ed, so at most the batch in flight is lost to
//! a crash. A crash mid-write leaves a truncated last line, which the loader
//! skips.
//!
//! ## Lifecycle
//! A normal stop still uses the in-memory segments; the journal is deleted once
//! the offline pipeline has saved the final transcript, kept when processing
//! fails, and removed with the meeting.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::meeting_draft::LiveLine;

/// How often the live worker checkpoints the per-track partial text.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

/// Journal file of `meeting_id` under the meetings directory.
pub fn journal_path(meetings_dir: &Path, meeting_id: &str) -> PathBuf {
    meetings_dir.join(format!("{meeting_id}.live.jsonl"))
}

/// Create `dir` (and missing parents) readable by the owner only. An existing
/// directory keeps its permissions.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Open `path` for appending, creating it owner-only (`0600`) if missing.
fn open_append_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Best-effort removal of a meeting's journal; a missing file is fine.
pub fn remove_journal(meetings_dir: &Path, meeting_id: &str) {
    let path = journal_path(meetings_dir, meeting_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not remove live transcript journal")
        }
    }
}

fn seconds_to_ms(seconds: f64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * 1000.0).round() as u64
    } else {
        0
    }
}

/// One track's unfinished text at checkpoint time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalPartial {
    pub track: String,
    pub segment_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

impl JournalPartial {
    pub fn new(
        track: &str,
        segment_id: &str,
        start_seconds: f64,
        end_seconds: f64,
        text: &str,
    ) -> Self {
        Self {
            track: track.to_string(),
            segment_id: segment_id.to_string(),
            start_ms: seconds_to_ms(start_seconds),
            end_ms: seconds_to_ms(end_seconds),
            text: text.to_string(),
        }
    }
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalRecord {
    Final {
        seq: u64,
        track: String,
        segment_id: String,
        start_ms: u64,
        end_ms: u64,
        text: String,
    },
    Checkpoint {
        seq: u64,
        partials: Vec<JournalPartial>,
    },
}

/// Sender side of the journal, owned by the live worker. Sending never blocks
/// on disk I/O: lines go over a channel to the writer thread.
pub struct JournalWriter {
    tx: Option<Sender<JournalRecord>>,
    seq: u64,
    last_checkpoint: Vec<JournalPartial>,
    handle: Option<JoinHandle<()>>,
}

impl JournalWriter {
    /// Create (or reopen) the journal at `path` and start its writer thread.
    /// The parent directory is created `0700` if missing.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            create_private_dir(dir)?;
        }
        let file = open_append_private(path)?;
        // Continue numbering after an existing journal, so lines appended to
        // it still supersede the earlier ones.
        let seq = read_records(path).iter().map(record_seq).max().unwrap_or(0);
        let (tx, rx) = channel();
        let display = path.display().to_string();
        let handle = std::thread::Builder::new()
            .name("lumen-meeting-journal".into())
            .spawn(move || run_writer(file, rx, &display))?;
        Ok(Self {
            tx: Some(tx),
            seq,
            last_checkpoint: Vec::new(),
            handle: Some(handle),
        })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn send(&mut self, record: JournalRecord) {
        if let Some(tx) = &self.tx {
            if tx.send(record).is_err() {
                // The writer thread gave up (I/O error, already logged); the
                // live preview itself is unaffected.
                self.tx = None;
            }
        }
    }

    /// Journal a finalized segment (an empty `text` retracts it).
    pub fn record_final(
        &mut self,
        track: &str,
        segment_id: &str,
        start_seconds: f64,
        end_seconds: f64,
        text: &str,
    ) {
        let seq = self.next_seq();
        self.send(JournalRecord::Final {
            seq,
            track: track.to_string(),
            segment_id: segment_id.to_string(),
            start_ms: seconds_to_ms(start_seconds),
            end_ms: seconds_to_ms(end_seconds),
            text: text.trim().to_string(),
        });
    }

    /// Journal the current per-track partials. Skipped when nothing changed
    /// since the previous checkpoint.
    pub fn checkpoint(&mut self, partials: Vec<JournalPartial>) {
        if partials == self.last_checkpoint {
            return;
        }
        self.last_checkpoint = partials.clone();
        let seq = self.next_seq();
        self.send(JournalRecord::Checkpoint { seq, partials });
    }

    /// Flush every queued line to disk and stop the writer thread.
    pub fn finish(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Writer thread: block for the next line, drain whatever else is queued,
/// write the batch and `fsync` it. Exits when the sender is dropped (after
/// writing what was queued) or on the first I/O error.
fn run_writer(mut file: File, rx: Receiver<JournalRecord>, path: &str) {
    while let Ok(first) = rx.recv() {
        let mut batch = String::new();
        for record in std::iter::once(first).chain(rx.try_iter()) {
            match serde_json::to_string(&record) {
                Ok(line) => {
                    batch.push_str(&line);
                    batch.push('\n');
                }
                Err(error) => tracing::warn!(%error, "live journal: could not serialize line"),
            }
        }
        if let Err(error) = file
            .write_all(batch.as_bytes())
            .and_then(|()| file.sync_data())
        {
            tracing::warn!(path, %error, "live journal write failed; journaling stopped");
            return;
        }
    }
}

fn record_seq(record: &JournalRecord) -> u64 {
    match record {
        JournalRecord::Final { seq, .. } | JournalRecord::Checkpoint { seq, .. } => *seq,
    }
}

/// Parse every complete line of the journal. A line that does not parse —
/// normally the last one, cut off by the crash — is skipped.
fn read_records(path: &Path) -> Vec<JournalRecord> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for line in BufReader::new(file).split(b'\n') {
        let Ok(line) = line else {
            break;
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<JournalRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::info!(
            path = %path.display(),
            skipped,
            "live journal: skipped unreadable line(s), typically a truncated tail"
        );
    }
    records
}

fn track_name(track: &str) -> &'static str {
    match track {
        "system" => "system",
        _ => "mic",
    }
}

/// Rebuild the live transcript from a journal, in timeline order. Per
/// `segment_id` the highest-`seq` line wins (a checkpointed partial included);
/// a winning empty text means the segment was retracted. A missing journal
/// yields no lines.
pub fn load_journal(path: &Path) -> Vec<LiveLine> {
    let mut latest: HashMap<String, (u64, LiveLine)> = HashMap::new();
    let mut keep =
        |seq: u64, track: &str, segment_id: String, start: u64, end: u64, text: String| {
            if latest.get(&segment_id).is_some_and(|(s, _)| *s > seq) {
                return;
            }
            let line = LiveLine {
                track: track_name(track),
                segment_id: segment_id.clone(),
                start_seconds: start as f64 / 1000.0,
                end_seconds: end.max(start) as f64 / 1000.0,
                text: text.trim().to_string(),
            };
            latest.insert(segment_id, (seq, line));
        };
    for record in read_records(path) {
        match record {
            JournalRecord::Final {
                seq,
                track,
                segment_id,
                start_ms,
                end_ms,
                text,
            } => keep(seq, &track, segment_id, start_ms, end_ms, text),
            JournalRecord::Checkpoint { seq, partials } => {
                for p in partials {
                    keep(seq, &p.track, p.segment_id, p.start_ms, p.end_ms, p.text);
                }
            }
        }
    }
    let mut lines: Vec<LiveLine> = latest
        .into_values()
        .map(|(_, line)| line)
        .filter(|line| !line.text.is_empty())
        .collect();
    lines.sort_by(|a, b| {
        a.start_seconds
            .total_cmp(&b.start_seconds)
            .then_with(|| a.segment_id.cmp(&b.segment_id))
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_in(dir: &Path) -> PathBuf {
        journal_path(&dir.join("meetings"), "m1")
    }

    #[test]
    fn roundtrips_finals_in_timeline_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("system", "system-0", 2.0, 3.5, "远端发言");
        writer.record_final("mic", "mic-0", 0.25, 1.75, " 现场发言 ");
        writer.finish();

        let lines = load_journal(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].track, "mic");
        assert_eq!(lines[0].segment_id, "mic-0");
        assert_eq!(lines[0].start_seconds, 0.25);
        assert_eq!(lines[0].end_seconds, 1.75);
        assert_eq!(lines[0].text, "现场发言");
        assert_eq!(lines[1].track, "system");
        assert_eq!(lines[1].text, "远端发言");
    }

    #[test]
    fn missing_journal_loads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_journal(&journal_in(tmp.path())).is_empty());
    }

    #[test]
    fn truncated_last_line_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "完整的一句");
        writer.finish();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"kind":"final","seq":2,"track":"mic","segment_id":"mic-1","st"#)
            .unwrap();
        drop(file);

        let lines = load_journal(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "完整的一句");
    }

    #[test]
    fn later_final_supersedes_and_empty_final_retracts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "初稿");
        writer.record_final("mic", "mic-1", 2.0, 3.0, "会被撤回");
        writer.record_final("mic", "mic-0", 0.0, 1.2, "终稿");
        writer.record_final("mic", "mic-1", 2.0, 3.0, "");
        writer.finish();

        let lines = load_journal(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "终稿");
        assert_eq!(lines[0].end_seconds, 1.2);
    }

    #[test]
    fn winner_is_highest_seq_not_line_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        create_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"kind":"final","seq":5,"track":"mic","segment_id":"mic-0","start_ms":0,"end_ms":900,"text":"新"}"#,
                "\n",
                r#"{"kind":"final","seq":3,"track":"mic","segment_id":"mic-0","start_ms":0,"end_ms":800,"text":"旧"}"#,
                "\n",
            ),
        )
        .unwrap();
        let lines = load_journal(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "新");
    }

    #[test]
    fn checkpointed_partials_recover_unless_finalized_later() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "第一句");
        writer.checkpoint(vec![
            JournalPartial::new("mic", "mic-1", 1.0, 2.0, "第二"),
            JournalPartial::new("system", "system-0", 0.5, 2.0, "远端未完"),
        ]);
        // Unchanged partials write nothing new.
        writer.checkpoint(vec![
            JournalPartial::new("mic", "mic-1", 1.0, 2.0, "第二"),
            JournalPartial::new("system", "system-0", 0.5, 2.0, "远端未完"),
        ]);
        writer.record_final("mic", "mic-1", 1.0, 2.5, "第二句");
        writer.checkpoint(vec![JournalPartial::new(
            "system",
            "system-0",
            0.5,
            3.0,
            "远端未完的话",
        )]);
        writer.finish();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 4);
        let lines = load_journal(&path);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["第一句", "远端未完的话", "第二句"]);
        let partial = lines.iter().find(|l| l.segment_id == "system-0").unwrap();
        assert_eq!(partial.track, "system");
        assert_eq!(partial.end_seconds, 3.0);
    }

    #[test]
    fn reopened_journal_continues_the_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "旧");
        writer.finish();
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "新");
        writer.finish();
        assert_eq!(load_journal(&path)[0].text, "新");
    }

    #[test]
    fn remove_journal_deletes_and_tolerates_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let meetings = tmp.path().join("meetings");
        let path = journal_path(&meetings, "m1");
        let mut writer = JournalWriter::open(&path).unwrap();
        writer.record_final("mic", "mic-0", 0.0, 1.0, "内容");
        writer.finish();
        assert!(path.exists());
        remove_journal(&meetings, "m1");
        assert!(!path.exists());
        remove_journal(&meetings, "m1");
    }

    #[cfg(unix)]
    #[test]
    fn journal_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = journal_in(tmp.path());
        let writer = JournalWriter::open(&path).unwrap();
        writer.finish();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }
}
