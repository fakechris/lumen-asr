//! One-shot migration of legacy PCM WAV meeting recordings to Ogg-Opus.
//!
//! Meetings recorded before the Opus default (Ogg-Opus recording landed with
//! [`lumen_audio::OpusSink`]) sit on disk as raw PCM16 WAVs — roughly 10× the
//! size of their Opus twin at 16 kHz mono. [`compact_meetings`] walks the
//! meeting store, re-encodes each remaining `.wav` track (mic and system) in
//! place to `.opus`, verifies the result decodes back to (almost) the same
//! number of samples, and only then updates the DB path and deletes the WAV.
//!
//! Safety contract:
//!
//! - **Verify-then-delete.** A track's WAV is deleted only after the new Opus
//!   file has been decoded back and its sample count matches the source
//!   within [`duration_matches`] tolerance (±1% or ±50 ms, whichever is
//!   looser — Opus trims codec pre-skip and pads the final frame).
//! - **Failure keeps the original.** Any read/encode/verify/store error
//!   leaves the WAV and the DB row untouched; the meeting is reported as
//!   failed and the run continues with the next meeting.
//! - **Idempotent.** Tracks already stored as `.opus` are skipped, so a
//!   re-run after an interruption only redoes what never finished. A meeting
//!   whose conversion crashed between the DB update and the WAV delete shows
//!   up as "already opus" with an orphan WAV next to it; the orphan is
//!   removed on the next run (it is no longer referenced by the DB).
//! - **Never touch a live recording.** Meetings in a non-terminal lifecycle
//!   status (`recording`, `processing`, `transcribing`, `summarizing`) are
//!   skipped outright — their audio may still be written or read by the
//!   running pipeline.
//! - **Stale headers are repaired, not trusted.** A crash-interrupted WAV can
//!   claim a fraction of the audio its file actually holds; such files are
//!   detected by comparing the header's data size with the file length and
//!   repaired ([`lumen_audio::repair_wav_header`]) before encoding, so the
//!   unclaimed tail is converted rather than silently dropped.
//!
//! Sidecar files (`<id>.timeline.json`, `<id>.echo_suppression.json`) key off
//! the meeting-id stem, not the audio extension, so they keep matching the
//! new `.opus` path without any rename.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lumen_core::{Meeting, MeetingStatus};
use lumen_store::Store;
use uuid::Uuid;

/// Knobs for one [`compact_meetings`] run.
#[derive(Debug, Clone, Default)]
pub struct CompactOptions {
    /// Report what would change without writing anything.
    pub dry_run: bool,
    /// Restrict the run to a single meeting id.
    pub meeting: Option<Uuid>,
}

/// Which of a meeting's two audio tracks a [`TrackReport`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Mic,
    System,
}

impl TrackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }
}

/// Outcome for one audio track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackStatus {
    /// WAV → Opus conversion done (or, with `dry_run`, what it would save).
    Converted { before_bytes: u64, after_bytes: u64 },
    /// Nothing to do (already Opus, file missing, meeting still live, …).
    Skipped(String),
    /// Conversion failed; the WAV and the DB row are untouched.
    Failed(String),
}

impl TrackStatus {
    fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// Per-track report: the source path the DB pointed at and what happened.
#[derive(Debug, Clone)]
pub struct TrackReport {
    pub kind: TrackKind,
    pub source: PathBuf,
    pub status: TrackStatus,
}

/// All track outcomes for one meeting.
#[derive(Debug, Clone)]
pub struct MeetingReport {
    pub id: Uuid,
    pub title: Option<String>,
    pub tracks: Vec<TrackReport>,
}

/// Whole-run totals. `bytes_before`/`bytes_after` cover converted tracks
/// only (measured sizes, or projected sizes for a dry run).
#[derive(Debug, Clone, Default)]
pub struct CompactSummary {
    /// Meetings where at least one track converted and none failed.
    pub converted: usize,
    /// Meetings with nothing converted and nothing failed.
    pub skipped: usize,
    /// Meetings where at least one track failed.
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub reports: Vec<MeetingReport>,
}

impl CompactSummary {
    pub fn projected_savings_bytes(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Encode target bitrate of [`lumen_audio::OpusSink`] (24 kbps VBR); used to
/// project the Opus size of a not-yet-converted track in dry-run mode.
const OPUS_PROJECTED_BYTES_PER_SECOND: f64 = 24_000.0 / 8.0;

/// Do two sample counts describe the same recording? Compared as durations
/// (the Opus decoder reports 16 kHz samples while the WAV is at its native
/// rate). Opus trims the codec pre-skip and pads the final frame, so exact
/// equality is impossible; ±1% or ±50 ms, whichever is looser, catches real
/// truncation without flapping on codec edge effects.
fn duration_matches(
    source_samples: usize,
    source_rate: u32,
    decoded_samples: usize,
    decoded_rate: u32,
) -> bool {
    if source_rate == 0 || decoded_rate == 0 {
        return false;
    }
    let source = source_samples as f64 / f64::from(source_rate);
    let decoded = decoded_samples as f64 / f64::from(decoded_rate);
    let tolerance = (source * 0.01).max(0.05);
    (source - decoded).abs() <= tolerance
}

/// Read a WAV recording as mono f32 samples at the file's native rate.
/// Integer PCM of any depth ≤ 32 bit and 32-bit float are accepted;
/// multi-channel files are down-mixed by averaging.
fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        anyhow::bail!("wav {} has zero channels", path.display());
    }
    let channels = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<hound::Result<Vec<f32>>>()?,
        hound::SampleFormat::Int => {
            // hound widens without scaling: a 16-bit file read as i32 yields
            // raw i16 values. Normalize against the depth's full-scale value.
            let full_scale = (1_i64 << (spec.bits_per_sample.saturating_sub(1))) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / full_scale))
                .collect::<hound::Result<Vec<f32>>>()?
        }
    };
    let mono = if channels == 1 {
        samples
    } else {
        samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };
    Ok((mono, spec.sample_rate))
}

/// Where the Opus twin of a WAV track lives: `<id>.wav` → `<id>.opus`,
/// `<id>.system.wav` → `<id>.system.opus`.
fn opus_path_for(wav_path: &Path) -> PathBuf {
    wav_path.with_extension("opus")
}

/// Staging path the encoder writes before verification: keeps a crashed or
/// failed run from ever leaving a half-written file at the final name.
fn staging_path_for(opus_path: &Path) -> PathBuf {
    let mut name = opus_path.file_name().unwrap_or_default().to_owned();
    name.push(".compact-tmp");
    opus_path.with_file_name(name)
}

/// Remove a WAV no longer referenced by the DB (left behind by an
/// interrupted earlier run whose DB update already pointed at the Opus).
fn remove_orphan_wav(wav_path: &Path, report: &mut TrackReport) {
    if wav_path.is_file() {
        match std::fs::remove_file(wav_path) {
            Ok(()) => {
                report.status = TrackStatus::Skipped(
                    "already opus (removed orphan wav from an interrupted run)".into(),
                );
            }
            Err(error) => {
                report.status = TrackStatus::Failed(format!(
                    "remove orphan wav {}: {error}",
                    wav_path.display()
                ));
            }
        }
    }
}

/// Convert one WAV track to Opus in place. On success the DB path has been
/// updated and the WAV deleted; on failure both are untouched.
fn convert_track(store: &Store, meeting_id: Uuid, kind: TrackKind, wav_path: &Path) -> TrackReport {
    let fail = |reason: String| TrackReport {
        kind,
        source: wav_path.to_path_buf(),
        status: TrackStatus::Failed(reason),
    };
    let result = convert_track_inner(store, meeting_id, kind, wav_path);
    match result {
        Ok(status) => TrackReport {
            kind,
            source: wav_path.to_path_buf(),
            status,
        },
        Err(reason) => fail(reason),
    }
}

/// Header-level probe of a WAV track: no samples are decoded.
struct WavProbe {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    /// Data bytes the header claims.
    claimed_data_bytes: u64,
    /// Actual file length on disk.
    file_len: u64,
}

impl WavProbe {
    /// Duration implied by the header, or — when the header is stale — by the
    /// actual file length (the recorder's fixed 44-byte header layout).
    fn duration_seconds(&self) -> f64 {
        let bytes = if self.is_stale() {
            self.file_len.saturating_sub(44)
        } else {
            self.claimed_data_bytes
        };
        let bytes_per_second =
            u64::from(self.sample_rate) * u64::from(self.channels) * self.bytes_per_sample();
        bytes as f64 / bytes_per_second.max(1) as f64
    }

    fn bytes_per_sample(&self) -> u64 {
        u64::from(self.bits_per_sample / 8).max(1)
    }

    /// The header claims materially less data than the file holds: the classic
    /// crash-interrupted recording whose RIFF/data sizes were never patched.
    /// Converting as-is would silently drop the unclaimed tail, so such files
    /// must be repaired ([`lumen_audio::repair_wav_header`]) before encoding.
    fn is_stale(&self) -> bool {
        let claimed_total = 44 + self.claimed_data_bytes;
        let slack = (self.file_len / 100).max(4096);
        self.file_len > claimed_total + slack
    }
}

fn probe_wav(path: &Path) -> Result<WavProbe> {
    let file_len = std::fs::metadata(path)
        .with_context(|| format!("stat wav {}", path.display()))?
        .len();
    let reader =
        hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        anyhow::bail!("wav {} has zero channels", path.display());
    }
    Ok(WavProbe {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        bits_per_sample: spec.bits_per_sample,
        claimed_data_bytes: u64::from(reader.duration())
            * u64::from(spec.channels)
            * u64::from(spec.bits_per_sample / 8),
        file_len,
    })
}

fn convert_track_inner(
    store: &Store,
    meeting_id: Uuid,
    kind: TrackKind,
    wav_path: &Path,
) -> std::result::Result<TrackStatus, String> {
    let opus_path = opus_path_for(wav_path);
    if opus_path.exists() {
        return Err(format!(
            "target {} already exists; refusing to clobber",
            opus_path.display()
        ));
    }
    let probe = probe_wav(wav_path).map_err(|e| e.to_string())?;
    if probe.is_stale() {
        // Crash-interrupted recording: the audio bytes are all there, only
        // the header sizes were never patched. The repair is idempotent and
        // only makes the header honest — the same salvage the app itself
        // runs during crash recovery.
        lumen_audio::repair_wav_header(wav_path).map_err(|e| {
            format!(
                "wav header is stale ({} claims {} B of data but the file is {} B) and repair failed: {e}",
                wav_path.display(),
                probe.claimed_data_bytes,
                probe.file_len
            )
        })?;
    }
    let (samples, sample_rate) = read_wav_mono(wav_path).map_err(|e| e.to_string())?;
    let before_bytes = probe.file_len;

    let staging = staging_path_for(&opus_path);
    let _ = std::fs::remove_file(&staging);
    let encode = || -> std::io::Result<()> {
        let mut sink = lumen_audio::OpusSink::create(&staging, sample_rate)?;
        sink.write_samples(&samples)?;
        sink.finalize()?;
        Ok(())
    };
    if let Err(error) = encode() {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("encode {}: {error}", staging.display()));
    }

    // Verify before anything becomes irreversible: the Opus must decode back
    // to (almost) the source duration.
    let verified = lumen_audio::decode_opus_to_pcm(&staging).map_err(|e| e.to_string());
    let (decoded, decoded_rate) = match verified {
        Ok(ok) => ok,
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            return Err(format!("verify decode {}: {error}", staging.display()));
        }
    };
    if !duration_matches(samples.len(), sample_rate, decoded.len(), decoded_rate) {
        let _ = std::fs::remove_file(&staging);
        return Err(format!(
            "verify failed: wav has {} samples @ {sample_rate} Hz but opus decodes to {} @ {decoded_rate} Hz",
            samples.len(),
            decoded.len()
        ));
    }
    if let Err(error) = std::fs::rename(&staging, &opus_path) {
        let _ = std::fs::remove_file(&staging);
        return Err(format!(
            "rename {} → {}: {error}",
            staging.display(),
            opus_path.display()
        ));
    }

    // DB first, then the delete: if the DB update fails we remove the new
    // Opus and keep the WAV, leaving the meeting exactly as found.
    let path_string = opus_path.to_string_lossy().into_owned();
    let updated = match kind {
        TrackKind::Mic => store.set_meeting_audio_path(meeting_id, &path_string),
        TrackKind::System => store.set_meeting_system_audio_path(meeting_id, Some(&path_string)),
    };
    match updated {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(&opus_path);
            return Err(format!("meeting {meeting_id} vanished from the store"));
        }
        Err(error) => {
            let _ = std::fs::remove_file(&opus_path);
            return Err(format!("update store path: {error}"));
        }
    }
    if let Err(error) = std::fs::remove_file(wav_path) {
        // The DB already points at the Opus; the next run's orphan cleanup
        // removes this WAV. Report success with a warning, not a failure.
        tracing::warn!(path = %wav_path.display(), %error, "converted but could not remove wav");
    }
    let after_bytes = std::fs::metadata(&opus_path).map(|m| m.len()).unwrap_or(0);
    Ok(TrackStatus::Converted {
        before_bytes,
        after_bytes,
    })
}

/// Project the Opus size of a WAV track for dry-run reporting. Header-only
/// probe (no sample decoding); a stale header is projected from the actual
/// file length, exactly as the real run would after repairing it.
fn dry_run_status(wav_path: &Path) -> TrackStatus {
    match probe_wav(wav_path) {
        Ok(probe) => {
            let after_bytes =
                (probe.duration_seconds() * OPUS_PROJECTED_BYTES_PER_SECOND) as u64 + 1024;
            TrackStatus::Converted {
                before_bytes: probe.file_len,
                after_bytes,
            }
        }
        Err(error) => TrackStatus::Failed(format!("{error:#}")),
    }
}

/// A meeting's lifecycle status where the audio may still be written or read
/// by the live recorder / processing pipeline.
fn is_live_status(status: MeetingStatus) -> bool {
    matches!(
        status,
        MeetingStatus::Recording
            | MeetingStatus::Processing
            | MeetingStatus::Transcribing
            | MeetingStatus::Summarizing
    )
}

fn is_wav_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
}

fn is_opus_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("opus"))
}

/// Compact one meeting's tracks. `progress` is invoked with each finished
/// meeting report so a CLI can stream lines for a long backlog.
fn compact_one_meeting(
    store: &Store,
    meeting: &Meeting,
    options: &CompactOptions,
) -> MeetingReport {
    let mut report = MeetingReport {
        id: meeting.id,
        title: meeting.title.clone(),
        tracks: Vec::new(),
    };
    if is_live_status(meeting.status) {
        report.tracks.push(TrackReport {
            kind: TrackKind::Mic,
            source: meeting
                .audio_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_default(),
            status: TrackStatus::Skipped(format!(
                "meeting is {} — audio may still be in use",
                meeting.status.as_str()
            )),
        });
        return report;
    }
    for (kind, stored) in [
        (TrackKind::Mic, meeting.audio_path.as_deref()),
        (TrackKind::System, meeting.system_audio_path.as_deref()),
    ] {
        let Some(stored) = stored else { continue };
        let path = PathBuf::from(stored);
        if is_opus_path(&path) {
            let mut track = TrackReport {
                kind,
                source: path.clone(),
                status: TrackStatus::Skipped("already opus".into()),
            };
            // Interrupted earlier run: DB moved to Opus but the WAV delete
            // never happened. Clean the orphan up (not in dry-run).
            if !options.dry_run {
                remove_orphan_wav(path.with_extension("wav").as_path(), &mut track);
            }
            report.tracks.push(track);
            continue;
        }
        if !is_wav_path(&path) {
            report.tracks.push(TrackReport {
                kind,
                source: path,
                status: TrackStatus::Skipped("not a wav path".into()),
            });
            continue;
        }
        if !path.is_file() {
            report.tracks.push(TrackReport {
                kind,
                source: path,
                status: TrackStatus::Skipped("file not found".into()),
            });
            continue;
        }
        if options.dry_run {
            report.tracks.push(TrackReport {
                kind,
                status: dry_run_status(&path),
                source: path,
            });
            continue;
        }
        report
            .tracks
            .push(convert_track(store, meeting.id, kind, &path));
    }
    report
}

/// Migrate every stored meeting's WAV tracks to Ogg-Opus in place.
///
/// Meetings are processed newest first; `progress` fires after each meeting.
/// Returns an error only when the store itself cannot be read — individual
/// meeting failures are reported inside the summary.
pub fn compact_meetings(
    store: &Store,
    options: &CompactOptions,
    mut progress: impl FnMut(&MeetingReport),
) -> Result<CompactSummary> {
    let meetings = match options.meeting {
        Some(id) => vec![store
            .get_meeting(id)?
            .with_context(|| format!("no meeting with id {id}"))?],
        None => store.list_meetings(u32::MAX)?,
    };
    let mut summary = CompactSummary::default();
    for meeting in &meetings {
        let report = compact_one_meeting(store, meeting, options);
        let failed = report.tracks.iter().any(|t| t.status.is_failure());
        let converted = report
            .tracks
            .iter()
            .any(|t| matches!(t.status, TrackStatus::Converted { .. }));
        for track in &report.tracks {
            if let TrackStatus::Converted {
                before_bytes,
                after_bytes,
            } = track.status
            {
                summary.bytes_before += before_bytes;
                summary.bytes_after += after_bytes;
            }
        }
        if failed {
            summary.failed += 1;
        } else if converted {
            summary.converted += 1;
        } else {
            summary.skipped += 1;
        }
        progress(&report);
        summary.reports.push(report);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::Meeting;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        meetings_dir: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let meetings_dir = dir.path().join("meetings");
        std::fs::create_dir_all(&meetings_dir).unwrap();
        let store = Store::open(dir.path().join("test.sqlite")).unwrap();
        Fixture {
            _dir: dir,
            store,
            meetings_dir,
        }
    }

    /// Write a 16-bit mono PCM WAV of `seconds` of a 440 Hz sine.
    fn write_test_wav(path: &Path, sample_rate: u32, seconds: f32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let count = (sample_rate as f32 * seconds) as usize;
        for i in 0..count {
            let t = i as f32 / sample_rate as f32;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.4;
            writer.write_sample((v * 32_767.0) as i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    /// Create a stored meeting in `status` with a mic WAV (and optionally a
    /// system WAV) on disk; returns (meeting_id, mic_path, system_path).
    fn add_meeting(
        fixture: &Fixture,
        status: MeetingStatus,
        seconds: f32,
        with_system: bool,
    ) -> (Uuid, PathBuf, Option<PathBuf>) {
        let mut meeting = Meeting::new();
        meeting.title = Some(format!("meeting {}", meeting.id));
        meeting.status = status;
        let id = meeting.id;
        fixture.store.create_meeting(&meeting).unwrap();
        let mic = fixture.meetings_dir.join(format!("{id}.wav"));
        write_test_wav(&mic, 16_000, seconds);
        fixture
            .store
            .set_meeting_audio_path(id, &mic.to_string_lossy())
            .unwrap();
        let system = with_system.then(|| {
            let path = fixture.meetings_dir.join(format!("{id}.system.wav"));
            write_test_wav(&path, 16_000, seconds);
            fixture
                .store
                .set_meeting_system_audio_path(id, Some(&path.to_string_lossy()))
                .unwrap();
            path
        });
        (id, mic, system)
    }

    fn run(store: &Store, options: &CompactOptions) -> CompactSummary {
        compact_meetings(store, options, |_| {}).unwrap()
    }

    #[test]
    fn converts_mic_and_system_tracks() {
        let fixture = fixture();
        let (id, mic, system) = add_meeting(&fixture, MeetingStatus::Ready, 2.0, true);
        let system = system.unwrap();
        let mic_before = std::fs::metadata(&mic).unwrap().len();

        let summary = run(&fixture.store, &CompactOptions::default());

        assert_eq!(summary.converted, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.bytes_before, mic_before * 2);
        assert!(summary.bytes_after < summary.bytes_before / 5);
        assert!(!mic.exists());
        assert!(!system.exists());
        let mic_opus = mic.with_extension("opus");
        let system_opus = system.with_extension("opus");
        assert!(mic_opus.is_file());
        assert!(system_opus.is_file());

        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert_eq!(
            stored.audio_path.as_deref(),
            Some(mic_opus.to_string_lossy().as_ref())
        );
        assert_eq!(
            stored.system_audio_path.as_deref(),
            Some(system_opus.to_string_lossy().as_ref())
        );

        // The Opus twin decodes back to ~the same duration.
        let (samples, rate) = lumen_audio::decode_opus_to_pcm(&mic_opus).unwrap();
        assert!(duration_matches(32_000, 16_000, samples.len(), rate));
    }

    #[test]
    fn dry_run_changes_nothing_but_projects_savings() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 2.0, false);
        let options = CompactOptions {
            dry_run: true,
            meeting: None,
        };
        let summary = run(&fixture.store, &options);

        assert_eq!(summary.converted, 1);
        assert!(summary.bytes_before > 0);
        assert!(summary.projected_savings_bytes() > 0);
        // Nothing on disk or in the DB changed.
        assert!(mic.is_file());
        assert!(!mic.with_extension("opus").exists());
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert_eq!(
            stored.audio_path.as_deref(),
            Some(mic.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn rerun_after_conversion_is_a_noop() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        let first = run(&fixture.store, &CompactOptions::default());
        assert_eq!(first.converted, 1);

        let second = run(&fixture.store, &CompactOptions::default());
        assert_eq!(second.converted, 0);
        assert_eq!(second.failed, 0);
        assert_eq!(second.skipped, 1);
        assert_eq!(second.bytes_before, 0);
        assert!(matches!(
            second.reports[0].tracks[0].status,
            TrackStatus::Skipped(_)
        ));
        // DB still points at the Opus.
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert!(stored.audio_path.as_deref().unwrap().ends_with(".opus"));
        assert!(!mic.exists());
    }

    #[test]
    fn rerun_cleans_orphan_wav_left_by_interrupted_run() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        let first = run(&fixture.store, &CompactOptions::default());
        assert_eq!(first.converted, 1);

        // Simulate a crash between the DB update and the WAV delete.
        write_test_wav(&mic, 16_000, 1.0);
        assert!(mic.is_file());
        let second = run(&fixture.store, &CompactOptions::default());
        assert_eq!(second.failed, 0);
        assert_eq!(second.skipped, 1);
        assert!(!mic.exists());
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert!(stored.audio_path.as_deref().unwrap().ends_with(".opus"));
    }

    #[test]
    fn corrupt_wav_keeps_original_and_reports_failure() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        std::fs::write(&mic, b"not a real wav file at all").unwrap();

        let summary = run(&fixture.store, &CompactOptions::default());

        assert_eq!(summary.converted, 0);
        assert_eq!(summary.failed, 1);
        assert!(matches!(
            summary.reports[0].tracks[0].status,
            TrackStatus::Failed(_)
        ));
        // The corrupt file and the DB path are untouched.
        assert_eq!(std::fs::read(&mic).unwrap(), b"not a real wav file at all");
        assert!(!mic.with_extension("opus").exists());
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert_eq!(
            stored.audio_path.as_deref(),
            Some(mic.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn live_meetings_are_skipped() {
        let fixture = fixture();
        for status in [
            MeetingStatus::Recording,
            MeetingStatus::Processing,
            MeetingStatus::Transcribing,
            MeetingStatus::Summarizing,
        ] {
            let (id, mic, _system) = add_meeting(&fixture, status, 1.0, false);
            let options = CompactOptions {
                dry_run: false,
                meeting: Some(id),
            };
            let summary = run(&fixture.store, &options);
            assert_eq!(summary.converted, 0, "status {status:?} must not convert");
            assert_eq!(summary.skipped, 1);
            assert!(mic.is_file());
        }
    }

    #[test]
    fn meeting_filter_only_touches_that_meeting() {
        let fixture = fixture();
        let (keep_id, keep_mic, _s) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        let (_other_id, other_mic, _s2) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        let options = CompactOptions {
            dry_run: false,
            meeting: Some(keep_id),
        };
        let summary = run(&fixture.store, &options);

        assert_eq!(summary.reports.len(), 1);
        assert_eq!(summary.converted, 1);
        assert!(!keep_mic.exists());
        assert!(other_mic.is_file());
    }

    #[test]
    fn unknown_meeting_id_is_an_error() {
        let fixture = fixture();
        let options = CompactOptions {
            dry_run: false,
            meeting: Some(Uuid::new_v4()),
        };
        assert!(compact_meetings(&fixture.store, &options, |_| {}).is_err());
    }

    #[test]
    fn missing_wav_file_is_skipped_not_failed() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        std::fs::remove_file(&mic).unwrap();

        let summary = run(&fixture.store, &CompactOptions::default());
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 1);
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert_eq!(
            stored.audio_path.as_deref(),
            Some(mic.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn converts_non_16khz_wav_at_its_native_rate() {
        let fixture = fixture();
        let meeting = Meeting::new();
        let id = meeting.id;
        fixture.store.create_meeting(&meeting).unwrap();
        fixture
            .store
            .update_meeting_status(id, MeetingStatus::Ready)
            .unwrap();
        let mic = fixture.meetings_dir.join(format!("{id}.wav"));
        write_test_wav(&mic, 44_100, 1.0);
        fixture
            .store
            .set_meeting_audio_path(id, &mic.to_string_lossy())
            .unwrap();

        let summary = run(&fixture.store, &CompactOptions::default());
        assert_eq!(summary.converted, 1);
        assert!(!mic.exists());
        let opus = mic.with_extension("opus");
        let (samples, rate) = lumen_audio::decode_opus_to_pcm(&opus).unwrap();
        assert_eq!(rate, lumen_audio::OPUS_SAMPLE_RATE);
        assert!(duration_matches(44_100, 44_100, samples.len(), rate));
    }

    /// Append `seconds` of raw PCM16 samples to a finished WAV without
    /// updating its header — the shape of a crash-interrupted recording.
    fn append_unclaimed_audio(path: &Path, sample_rate: u32, seconds: f32) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let count = (sample_rate as f32 * seconds) as usize;
        for i in 0..count {
            let t = i as f32 / sample_rate as f32;
            let v = ((2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.4 * 32_767.0) as i16;
            file.write_all(&v.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn stale_header_wav_is_repaired_and_converted_fully() {
        let fixture = fixture();
        let (id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        // Header claims 1 s; the file actually holds 2 s.
        append_unclaimed_audio(&mic, 16_000, 1.0);
        assert!(probe_wav(&mic).unwrap().is_stale());

        let summary = run(&fixture.store, &CompactOptions::default());

        assert_eq!(summary.converted, 1);
        assert_eq!(summary.failed, 0);
        assert!(!mic.exists());
        let opus = mic.with_extension("opus");
        let (samples, rate) = lumen_audio::decode_opus_to_pcm(&opus).unwrap();
        // The unclaimed second survived: ~2 s, not the header-claimed 1 s.
        assert!(duration_matches(32_000, 16_000, samples.len(), rate));
        let stored = fixture.store.get_meeting(id).unwrap().unwrap();
        assert!(stored.audio_path.as_deref().unwrap().ends_with(".opus"));
    }

    #[test]
    fn dry_run_projects_stale_header_from_file_length_without_mutating() {
        let fixture = fixture();
        let (_id, mic, _system) = add_meeting(&fixture, MeetingStatus::Ready, 1.0, false);
        append_unclaimed_audio(&mic, 16_000, 1.0);
        let options = CompactOptions {
            dry_run: true,
            meeting: None,
        };
        let summary = run(&fixture.store, &options);

        assert_eq!(summary.converted, 1);
        let TrackStatus::Converted { after_bytes, .. } = summary.reports[0].tracks[0].status else {
            panic!("expected a conversion projection");
        };
        // ~2 s at 24 kbps ≈ 6 KB, not the header-claimed ~3 KB.
        assert!(after_bytes > 6_000);
        // Dry-run must not repair the header either: the file is untouched.
        assert!(probe_wav(&mic).unwrap().is_stale());
    }

    #[test]
    fn duration_tolerance_accepts_codec_edge_effects() {
        // Exact match.
        assert!(duration_matches(16_000, 16_000, 16_000, 16_000));
        // Well within 1%.
        assert!(duration_matches(16_000, 16_000, 16_080, 16_000));
        // A one-second loss on a one-minute recording (1.7%) is rejected.
        assert!(!duration_matches(60 * 16_000, 16_000, 59 * 16_000, 16_000));
        // …but the 50 ms floor keeps short clips from flapping on pre-skip.
        assert!(duration_matches(3_200, 16_000, 3_000, 16_000));
        // Cross-rate comparison compares durations, not raw counts.
        assert!(duration_matches(44_100, 44_100, 16_000, 16_000));
        // Zero rates never match.
        assert!(!duration_matches(1, 0, 1, 16_000));
    }
}
