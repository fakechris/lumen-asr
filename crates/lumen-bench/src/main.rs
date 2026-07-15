use anyhow::{bail, Context, Result};
use lumen_bench::config::{
    default_benchmark_dir, default_lumen_config_path, default_lumen_db_path,
    default_reference_db_path, LumenConfig,
};
use lumen_bench::pipeline::LumenPipeline;
use lumen_bench::reference::{CandidateOrder, ReferenceDataset};
use lumen_bench::runner::{run_benchmark, RunOptions};
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".into());
    let parsed = ParsedArgs::parse(args.collect())?;
    match command.as_str() {
        "inventory" => inventory(parsed),
        "run" => run_pipeline(parsed).await,
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => bail!("unknown command `{other}`; run `lumen-bench help`"),
    }
}

fn inventory(args: ParsedArgs) -> Result<()> {
    args.reject_flags()?;
    let db = args.path("reference-db", default_reference_db_path());
    args.reject_unknown(&["reference-db"])?;
    let dataset = ReferenceDataset::open(&db)?;
    println!("{}", serde_json::to_string_pretty(&dataset.inventory()?)?);
    Ok(())
}

async fn run_pipeline(args: ParsedArgs) -> Result<()> {
    let reference_db = args.path("reference-db", default_reference_db_path());
    let config_path = args.path("config", default_lumen_config_path());
    let lumen_db = args.path("lumen-db", default_lumen_db_path());
    let output_dir = args.path("output", default_benchmark_dir());
    let ffmpeg = args.path("ffmpeg", PathBuf::from("ffmpeg"));
    let model_dir = args.optional_path("model-dir");
    let limit = args.usize("limit", 20)?;
    let offset = args.usize("offset", 0)?;
    let order = match args.value("order").unwrap_or("stable") {
        "stable" => CandidateOrder::Stable,
        "newest" => CandidateOrder::Newest,
        "oldest" => CandidateOrder::Oldest,
        value => bail!("--order must be stable, newest, or oldest (got `{value}`)"),
    };
    anyhow::ensure!(
        config_path.is_file(),
        "Lumen config does not exist: {}",
        config_path.display()
    );
    let mut config = LumenConfig::load(&config_path)?;
    if let Some(provider) = args.value("asr") {
        config.asr.provider = provider.to_string();
    }
    if let Some(language) = args.value("language") {
        config.asr.language = language.to_string();
    }
    if args.has_flag("no-corrector") {
        config.corrector.enabled = false;
        config.corrector.provider = "none".into();
    }
    args.reject_unknown(&[
        "reference-db",
        "config",
        "lumen-db",
        "output",
        "ffmpeg",
        "model-dir",
        "limit",
        "offset",
        "order",
        "asr",
        "language",
    ])?;
    args.reject_unknown_flags(&["no-corrector"])?;

    let dataset = ReferenceDataset::open(&reference_db)?;
    let pipeline = LumenPipeline::build(&config, &lumen_db, model_dir)?;
    eprintln!("ASR: {}", pipeline.asr_label);
    eprintln!("Corrector: {}", pipeline.corrector_label);
    eprintln!("Output: {}", output_dir.display());
    let options = RunOptions {
        reference_db,
        config_path,
        output_dir,
        ffmpeg,
        limit,
        offset,
        order,
    };
    let outcome = run_benchmark(&dataset, &pipeline, &options).await?;
    println!(
        "completed: {}/{} samples; raw content CER {:.2}%; repaired content CER {:.2}%",
        outcome.summary.succeeded,
        outcome.summary.total,
        outcome.summary.raw.content.rate * 100.0,
        outcome.summary.repaired.content.rate * 100.0,
    );
    println!("report: {}", outcome.report_path.display());
    println!("results: {}", outcome.results_path.display());
    println!("summary: {}", outcome.summary_path.display());
    Ok(())
}

fn print_usage() {
    println!(
        r#"lumen-bench — replay a private reference dataset through the Lumen pipeline

USAGE
  cargo run -p lumen-bench -- inventory [--reference-db PATH]
  cargo run -p lumen-bench -- run [OPTIONS]

RUN OPTIONS
  --limit N             Number of records (default: 20)
  --offset N            Skip N records in the selected order (default: 0)
  --order ORDER         stable (UUID sample, default), newest, or oldest
  --output PATH         Private result directory under Lumen Application Support by default
  --reference-db PATH    Reference SQLite database (or set LUMEN_BENCH_REFERENCE_DB)
  --config PATH         Lumen config.toml
  --lumen-db PATH       Lumen SQLite database (for confirmed dictionary entries)
  --ffmpeg PATH         ffmpeg executable (default: ffmpeg from PATH)
  --asr PROVIDER        Override ASR provider (local_sensevoice, local_whisper, openai_audio)
  --model-dir PATH      Override local ASR model directory
  --language CODE       Override ASR language (SenseVoice defaults to auto)
  --no-corrector        Run raw ASR plus deterministic preprocess only; makes no LLM calls

Detailed JSONL contains private text and stays outside the repository by default.
Reusing the same --output directory resumes completed UUIDs."#
    );
}

struct ParsedArgs {
    values: HashMap<String, String>,
    flags: Vec<String>,
}

impl ParsedArgs {
    fn parse(tokens: Vec<String>) -> Result<Self> {
        let mut values = HashMap::new();
        let mut flags = Vec::new();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];
            anyhow::ensure!(
                token.starts_with("--"),
                "unexpected positional argument `{token}`"
            );
            let key = token.trim_start_matches("--").to_string();
            if key == "no-corrector" {
                flags.push(key);
                index += 1;
                continue;
            }
            let value = tokens
                .get(index + 1)
                .with_context(|| format!("missing value for --{key}"))?;
            anyhow::ensure!(!value.starts_with("--"), "missing value for --{key}");
            values.insert(key, value.clone());
            index += 2;
        }
        Ok(Self { values, flags })
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn path(&self, key: &str, default: PathBuf) -> PathBuf {
        self.value(key).map(PathBuf::from).unwrap_or(default)
    }

    fn optional_path(&self, key: &str) -> Option<PathBuf> {
        self.value(key).map(PathBuf::from)
    }

    fn usize(&self, key: &str, default: usize) -> Result<usize> {
        self.value(key)
            .map(|value| {
                value
                    .parse()
                    .with_context(|| format!("--{key} must be a non-negative integer"))
            })
            .transpose()
            .map(|value| value.unwrap_or(default))
    }

    fn has_flag(&self, key: &str) -> bool {
        self.flags.iter().any(|flag| flag == key)
    }

    fn reject_unknown(&self, allowed: &[&str]) -> Result<()> {
        for key in self.values.keys() {
            anyhow::ensure!(allowed.contains(&key.as_str()), "unknown option --{key}");
        }
        Ok(())
    }

    fn reject_unknown_flags(&self, allowed: &[&str]) -> Result<()> {
        for flag in &self.flags {
            anyhow::ensure!(allowed.contains(&flag.as_str()), "unknown flag --{flag}");
        }
        Ok(())
    }

    fn reject_flags(&self) -> Result<()> {
        self.reject_unknown_flags(&[])
    }
}
