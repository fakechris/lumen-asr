use crate::audio::decode_to_mono_f32;
use crate::metrics::{score_text, TextScore};
use crate::pipeline::LumenPipeline;
use crate::reference::{CandidateOrder, ReferenceCandidate, ReferenceDataset};
use crate::report::{render_markdown, summarize, BenchmarkSummary, ScoredSample};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub reference_db: PathBuf,
    pub config_path: PathBuf,
    pub output_dir: PathBuf,
    pub ffmpeg: PathBuf,
    pub limit: usize,
    pub offset: usize,
    pub order: CandidateOrder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema_version: u32,
    pub started_at: String,
    pub reference_db: PathBuf,
    pub config_path: PathBuf,
    pub asr: String,
    pub corrector: String,
    pub pipeline_fingerprint: String,
    pub source_fingerprint: String,
    pub ffmpeg: PathBuf,
    pub ffmpeg_version: String,
    pub limit: usize,
    pub offset: usize,
    #[serde(default)]
    pub order: CandidateOrder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleResult {
    pub id: String,
    pub source_table: String,
    pub audio_path: PathBuf,
    pub duration_seconds: Option<f64>,
    pub created_at: Option<String>,
    pub reference: String,
    pub raw: String,
    pub repaired: String,
    pub model_applied: bool,
    pub raw_score: Option<TextScore>,
    pub repaired_score: Option<TextScore>,
    pub decode_ms: u64,
    pub pipeline_ms: u64,
    pub error: Option<String>,
}

impl SampleResult {
    fn base(candidate: &ReferenceCandidate) -> Self {
        Self {
            id: candidate.id.clone(),
            source_table: candidate.source_table.clone(),
            audio_path: candidate.audio_path.clone(),
            duration_seconds: candidate.duration_seconds,
            created_at: candidate.created_at.clone(),
            reference: candidate.reference.clone(),
            raw: String::new(),
            repaired: String::new(),
            model_applied: false,
            raw_score: None,
            repaired_score: None,
            decode_ms: 0,
            pipeline_ms: 0,
            error: None,
        }
    }

    fn scored(&self) -> ScoredSample {
        match (&self.raw_score, &self.repaired_score) {
            (Some(raw), Some(repaired)) => ScoredSample::success_with_model(
                self.duration_seconds,
                raw.clone(),
                repaired.clone(),
                self.model_applied,
            ),
            _ => ScoredSample::failure(self.duration_seconds),
        }
    }

    fn is_successful(&self) -> bool {
        self.error.is_none() && self.raw_score.is_some() && self.repaired_score.is_some()
    }
}

pub struct BenchmarkOutcome {
    pub summary: BenchmarkSummary,
    pub results_path: PathBuf,
    pub report_path: PathBuf,
    pub summary_path: PathBuf,
}

pub async fn run_benchmark(
    dataset: &ReferenceDataset,
    pipeline: &LumenPipeline,
    options: &RunOptions,
) -> Result<BenchmarkOutcome> {
    create_private_dir(&options.output_dir)?;
    let candidates = dataset.candidates_ordered(options.limit, options.offset, options.order)?;
    let manifest = RunManifest {
        schema_version: 4,
        started_at: Utc::now().to_rfc3339(),
        reference_db: options.reference_db.clone(),
        config_path: options.config_path.clone(),
        asr: pipeline.asr_label.clone(),
        corrector: pipeline.corrector_label.clone(),
        pipeline_fingerprint: pipeline.fingerprint.clone(),
        source_fingerprint: source_fingerprint(&candidates),
        ffmpeg: options.ffmpeg.clone(),
        ffmpeg_version: ffmpeg_version(&options.ffmpeg)?,
        limit: options.limit,
        offset: options.offset,
        order: options.order,
    };
    ensure_manifest(&options.output_dir.join("run.json"), &manifest)?;

    let results_path = options.output_dir.join("results.jsonl");
    let mut existing = read_results(&results_path)?;
    let previous_count = existing.len();
    existing.retain(SampleResult::is_successful);
    if existing.len() != previous_count {
        write_results(&results_path, &existing)?;
    }
    let completed_ids: HashSet<String> = existing.iter().map(|result| result.id.clone()).collect();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&results_path)
        .with_context(|| format!("open results: {}", results_path.display()))?;
    set_private_file_permissions(&results_path)?;
    let mut writer = BufWriter::new(file);

    for (index, candidate) in candidates.iter().enumerate() {
        if completed_ids.contains(&candidate.id) {
            eprintln!(
                "[{}/{}] {} already complete",
                index + 1,
                candidates.len(),
                candidate.id
            );
            continue;
        }
        eprintln!("[{}/{}] {}", index + 1, candidates.len(), candidate.id);
        let result = process_candidate(candidate, pipeline, &options.ffmpeg).await;
        serde_json::to_writer(&mut writer, &result)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        existing.push(result);
    }

    let scored: Vec<ScoredSample> = existing.iter().map(SampleResult::scored).collect();
    let summary = summarize(&scored);
    let summary_path = options.output_dir.join("summary.json");
    write_json(&summary_path, &summary)?;
    let report_path = options.output_dir.join("report.md");
    write_private(
        &report_path,
        render_markdown(&summary, &pipeline.asr_label, &pipeline.corrector_label),
    )?;
    Ok(BenchmarkOutcome {
        summary,
        results_path,
        report_path,
        summary_path,
    })
}

async fn process_candidate(
    candidate: &ReferenceCandidate,
    pipeline: &LumenPipeline,
    ffmpeg: &Path,
) -> SampleResult {
    let decode_started = Instant::now();
    let samples = match decode_to_mono_f32(&candidate.audio_path, ffmpeg) {
        Ok(samples) => samples,
        Err(error) => {
            return failed_result(
                candidate,
                decode_started.elapsed().as_millis() as u64,
                0,
                format!("decode: {error:#}"),
            )
        }
    };
    let decode_ms = decode_started.elapsed().as_millis() as u64;
    let pipeline_started = Instant::now();
    let processed = match pipeline.process(samples).await {
        Ok(processed) => processed,
        Err(error) => {
            return failed_result(
                candidate,
                decode_ms,
                pipeline_started.elapsed().as_millis() as u64,
                format!("pipeline: {error:#}"),
            )
        }
    };
    let mut result = SampleResult::base(candidate);
    result.raw_score = Some(score_text(&candidate.reference, &processed.raw));
    result.repaired_score = Some(score_text(&candidate.reference, &processed.repaired));
    result.raw = processed.raw;
    result.repaired = processed.repaired;
    result.model_applied = processed.model_applied;
    result.decode_ms = decode_ms;
    result.pipeline_ms = pipeline_started.elapsed().as_millis() as u64;
    result
}

fn failed_result(
    candidate: &ReferenceCandidate,
    decode_ms: u64,
    pipeline_ms: u64,
    error: String,
) -> SampleResult {
    let mut result = SampleResult::base(candidate);
    result.decode_ms = decode_ms;
    result.pipeline_ms = pipeline_ms;
    result.error = Some(error);
    result
}

fn read_results(path: &Path) -> Result<Vec<SampleResult>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    BufReader::new(File::open(path)?)
        .lines()
        .filter(|line| line.as_ref().map_or(true, |line| !line.trim().is_empty()))
        .map(|line| {
            let line = line?;
            serde_json::from_str(&line).context("parse existing benchmark result")
        })
        .collect()
}

fn ensure_manifest(path: &Path, expected: &RunManifest) -> Result<()> {
    if path.is_file() {
        let existing: RunManifest = serde_json::from_reader(File::open(path)?)?;
        anyhow::ensure!(
            existing.schema_version == expected.schema_version
                && existing.reference_db == expected.reference_db
                && existing.asr == expected.asr
                && existing.corrector == expected.corrector
                && existing.pipeline_fingerprint == expected.pipeline_fingerprint
                && existing.source_fingerprint == expected.source_fingerprint
                && existing.ffmpeg == expected.ffmpeg
                && existing.ffmpeg_version == expected.ffmpeg_version
                && existing.limit == expected.limit
                && existing.offset == expected.offset
                && existing.order == expected.order,
            "output directory belongs to a different benchmark configuration"
        );
        return Ok(());
    }
    write_json(path, expected)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn write_results(path: &Path, results: &[SampleResult]) -> Result<()> {
    let temporary = path.with_extension(format!("jsonl.tmp-{}", uuid::Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        set_private_file_permissions(&temporary)?;
        let mut writer = BufWriter::new(file);
        for result in results {
            serde_json::to_writer(&mut writer, result)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replace {} atomically", path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn source_fingerprint(candidates: &[ReferenceCandidate]) -> String {
    let mut fingerprint = 0xcbf29ce484222325_u64;
    let mut update = |value: &str| {
        for byte in value.len().to_le_bytes().into_iter().chain(value.bytes()) {
            fingerprint ^= u64::from(byte);
            fingerprint = fingerprint.wrapping_mul(0x100000001b3);
        }
    };
    for candidate in candidates {
        update(&candidate.id);
        update(&candidate.reference);
        update(&candidate.audio_path.to_string_lossy());
        update(&candidate.source_table);
        update(&format!("{:?}", candidate.duration_seconds));
        match std::fs::metadata(&candidate.audio_path) {
            Ok(metadata) => {
                update(&metadata.len().to_string());
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs());
                update(&modified.to_string());
            }
            Err(_) => update("audio:missing"),
        }
    }
    format!("fnv1a64:{fingerprint:016x}")
}

fn ffmpeg_version(path: &Path) -> Result<String> {
    let output = std::process::Command::new(path)
        .arg("-version")
        .output()
        .with_context(|| format!("run ffmpeg at {}", path.display()))?;
    anyhow::ensure!(output.status.success(), "ffmpeg -version failed");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown")
        .to_string())
}

fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    std::fs::write(path, contents)?;
    set_private_file_permissions(path)
}

fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create benchmark output: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> ReferenceCandidate {
        ReferenceCandidate {
            id: "sample".into(),
            reference: "你好".into(),
            audio_path: PathBuf::from("sample.ogg"),
            duration_seconds: Some(1.0),
            created_at: None,
            app_version: "1.8".into(),
            source_table: "history_v2".into(),
        }
    }

    #[test]
    fn failed_results_are_not_completed_for_resume() {
        let failed = failed_result(&candidate(), 1, 2, "transient".into());
        assert!(!failed.is_successful());

        let mut successful = SampleResult::base(&candidate());
        successful.raw_score = Some(score_text("你好", "你好"));
        successful.repaired_score = Some(score_text("你好", "你好"));
        assert!(successful.is_successful());
    }
}
