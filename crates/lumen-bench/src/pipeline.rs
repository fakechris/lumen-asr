use crate::audio::BENCHMARK_SAMPLE_RATE;
use crate::config::LumenConfig;
use anyhow::{bail, Context, Result};
use lumen_asr::{
    default_sensevoice_dir, default_whisper_dir, AsrEngine, AsrRequest, OpenAiAudioAsr,
    OpenAiAudioConfig, SenseVoiceSherpaAsr, WhisperAsr,
};
use lumen_core::CorrectorEngineId;
use lumen_corrector::{
    correct_or_fallback_with, preprocess_only, Corrector, DictionaryContext, OpenAiCompatConfig,
    OpenAiCompatCorrector,
};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct ProcessedText {
    pub raw: String,
    pub repaired: String,
    pub model_applied: bool,
}

pub struct LumenPipeline {
    asr: Box<dyn AsrEngine>,
    corrector: Option<Box<dyn Corrector>>,
    dictionary: DictionaryContext,
    system_prompt: String,
    temperature: f32,
    pub asr_label: String,
    pub corrector_label: String,
    pub fingerprint: String,
}

struct BuiltCorrector {
    engine: Option<Box<dyn Corrector>>,
    label: String,
    identity: String,
}

impl LumenPipeline {
    pub fn build(
        config: &LumenConfig,
        lumen_db: &Path,
        model_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let (asr, asr_identity) = build_asr(config, model_dir)?;
        let dictionary = load_dictionary(lumen_db)?;
        let prompt_input = config.output.prompt_input()?;
        let cleanup = lumen_prompts::effective_cleanup(&prompt_input);
        let system_prompt = lumen_prompts::build_system_prompt_from(&prompt_input);
        let built_corrector = build_corrector(config, &system_prompt)?;
        let fingerprint = pipeline_fingerprint(
            config,
            lumen_db,
            &asr_identity,
            &built_corrector.identity,
            &system_prompt,
            &dictionary,
        );
        Ok(Self {
            asr,
            corrector: built_corrector.engine,
            dictionary,
            system_prompt,
            temperature: cleanup.temperature(),
            asr_label: config.asr_label(),
            corrector_label: built_corrector.label,
            fingerprint,
        })
    }

    pub async fn process(&self, samples: Vec<f32>) -> Result<ProcessedText> {
        let result = self
            .asr
            .transcribe(AsrRequest {
                samples,
                sample_rate: BENCHMARK_SAMPLE_RATE,
                hotwords: Vec::new(),
            })
            .await
            .context("ASR inference")?;
        let raw = result.text.trim().to_string();
        let corrected = match self.corrector.as_deref() {
            Some(corrector) => {
                correct_or_fallback_with(
                    corrector,
                    &raw,
                    self.dictionary.clone(),
                    self.system_prompt.clone(),
                    self.temperature,
                )
                .await
            }
            None => preprocess_only(&raw, &self.dictionary),
        };
        Ok(ProcessedText {
            raw,
            repaired: corrected.text.trim().to_string(),
            model_applied: corrected.model_applied,
        })
    }
}

fn build_asr(
    config: &LumenConfig,
    model_dir: Option<PathBuf>,
) -> Result<(Box<dyn AsrEngine>, String)> {
    match config.asr.provider.as_str() {
        "local_sensevoice" | "sensevoice" => {
            let directory = model_dir.unwrap_or_else(default_sensevoice_dir);
            let language = if config.asr.language.trim().is_empty() {
                "auto"
            } else {
                config.asr.language.trim()
            };
            let identity = local_model_identity(&directory)?;
            let engine = SenseVoiceSherpaAsr::new(directory).with_language(language);
            anyhow::ensure!(engine.is_ready(), "SenseVoice model is not ready");
            Ok((Box::new(engine), identity))
        }
        "local_whisper" | "whisper" => {
            let directory = model_dir.unwrap_or_else(default_whisper_dir);
            let language = if config.asr.language.trim().is_empty() {
                "en"
            } else {
                config.asr.language.trim()
            };
            let identity = local_model_identity(&directory)?;
            let engine = WhisperAsr::new(directory).with_language(language);
            anyhow::ensure!(engine.is_ready(), "Whisper model is not ready");
            Ok((Box::new(engine), identity))
        }
        "openai_audio" | "custom" => {
            let base_url = if config.asr.base_url.trim().is_empty() {
                "https://api.openai.com/v1".into()
            } else {
                config.asr.base_url.clone()
            };
            let model = if config.asr.model.trim().is_empty() {
                "whisper-1".into()
            } else {
                config.asr.model.clone()
            };
            let identity = format!("cloud:{base_url}|{model}|{}", config.asr.language);
            Ok((
                Box::new(OpenAiAudioAsr::new(OpenAiAudioConfig {
                    base_url,
                    api_key: config.asr.api_key.clone(),
                    model,
                    timeout: Duration::from_secs(config.asr.timeout_secs.max(30)),
                    language: (!config.asr.language.trim().is_empty())
                        .then(|| config.asr.language.clone()),
                })?),
                identity,
            ))
        }
        provider => bail!("unsupported benchmark ASR provider: {provider}"),
    }
}

fn build_corrector(config: &LumenConfig, system_prompt: &str) -> Result<BuiltCorrector> {
    if !config.corrector.enabled || config.corrector.provider == "none" || system_prompt.is_empty()
    {
        return Ok(BuiltCorrector {
            engine: None,
            label: "none".into(),
            identity: "none".into(),
        });
    }
    let base_url =
        if config.corrector.base_url.trim().is_empty() && config.corrector.provider == "ollama" {
            "http://127.0.0.1:11434/v1".to_string()
        } else {
            config.corrector.base_url.clone()
        };
    let model = if config.corrector.model.trim().is_empty() && config.corrector.provider == "ollama"
    {
        "qwen3.5:9b".to_string()
    } else {
        config.corrector.model.clone()
    };
    anyhow::ensure!(
        !base_url.trim().is_empty(),
        "corrector base_url is required"
    );
    anyhow::ensure!(!model.trim().is_empty(), "corrector model is required");
    let engine_id = if config.corrector.provider == "ollama" {
        CorrectorEngineId::Ollama
    } else {
        CorrectorEngineId::OpenAiCompatible
    };
    let corrector = OpenAiCompatCorrector::new(OpenAiCompatConfig {
        base_url: base_url.clone(),
        api_key: config.corrector.api_key.clone(),
        model: model.clone(),
        engine_id,
        timeout: Duration::from_secs(config.corrector.timeout_secs.max(5)),
    })?;
    let label = format!(
        "{}:{}|{}",
        config.corrector.provider, model, config.output.cleanup
    );
    let identity = format!(
        "{}|{}|{}|{}",
        config.corrector.provider,
        base_url,
        model,
        config.corrector.timeout_secs.max(5)
    );
    Ok(BuiltCorrector {
        engine: Some(Box::new(corrector)),
        label,
        identity,
    })
}

fn load_dictionary(path: &Path) -> Result<DictionaryContext> {
    if !path.is_file() {
        return Ok(DictionaryContext::default());
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open Lumen dictionary read-only: {}", path.display()))?;
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='dictionary_entries')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(DictionaryContext::default());
    }
    let mut statement = connection.prepare(
        "SELECT kind, term, from_text, to_text FROM dictionary_entries WHERE confirmed = 1 ORDER BY kind, term, from_text, to_text",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut dictionary = DictionaryContext::default();
    for row in rows {
        let (kind, term, from, to) = row?;
        match kind.as_str() {
            "term" => {
                if let Some(term) = term.filter(|term| !term.is_empty()) {
                    dictionary.terms.push(term);
                }
            }
            "replacement" => {
                if let (Some(from), Some(to)) = (from, to) {
                    if !from.is_empty() {
                        dictionary.replacements.push((from, to));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(dictionary)
}

fn local_model_identity(directory: &Path) -> Result<String> {
    let mut entries = std::fs::read_dir(directory)
        .with_context(|| format!("read model directory: {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut identity = directory.display().to_string();
    for entry in entries {
        if !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        identity.push_str(&format!(
            "|{}:{}:{}",
            entry.file_name().to_string_lossy(),
            metadata.len(),
            modified
        ));
    }
    Ok(identity)
}

fn pipeline_fingerprint(
    config: &LumenConfig,
    lumen_db: &Path,
    asr_identity: &str,
    corrector_identity: &str,
    system_prompt: &str,
    dictionary: &DictionaryContext,
) -> String {
    let mut fingerprint = StableFingerprint::new();
    for value in [
        config.asr.provider.as_str(),
        config.asr.base_url.as_str(),
        config.asr.model.as_str(),
        config.asr.language.as_str(),
        config.corrector.provider.as_str(),
        config.output.cleanup.as_str(),
        config.output.style.as_str(),
        config.output.casing.as_str(),
        config.output.punctuation.as_str(),
        config.output.custom_instruction.as_str(),
        asr_identity,
        corrector_identity,
        system_prompt,
        lumen_db.to_string_lossy().as_ref(),
    ] {
        fingerprint.update(value);
    }
    fingerprint.update(&config.asr.timeout_secs.to_string());
    fingerprint.update(&config.corrector.timeout_secs.to_string());
    fingerprint.update(if config.asr.api_key.is_empty() {
        "asr_key:empty"
    } else {
        "asr_key:configured"
    });
    fingerprint.update(if config.corrector.api_key.is_empty() {
        "corrector_key:empty"
    } else {
        "corrector_key:configured"
    });
    for rule in &config.output.polish {
        fingerprint.update(rule);
    }
    for term in &dictionary.terms {
        fingerprint.update(term);
    }
    for (from, to) in &dictionary.replacements {
        fingerprint.update(from);
        fingerprint.update(to);
    }
    fingerprint.finish()
}

struct StableFingerprint(u64);

impl StableFingerprint {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn update(&mut self, value: &str) {
        for byte in value.len().to_le_bytes().into_iter().chain(value.bytes()) {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> String {
        format!("fnv1a64:{:016x}", self.0)
    }
}
