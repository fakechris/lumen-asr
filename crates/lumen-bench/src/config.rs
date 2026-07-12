use anyhow::{Context, Result};
use lumen_prompts::{Casing, CleanupLevel, PolishRule, PunctPolicy, Style};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct LumenConfig {
    pub asr: AsrSettings,
    pub corrector: CorrectorSettings,
    pub output: OutputSettings,
}

impl LumenConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read Lumen config: {}", path.display()))?;
        toml::from_str(&contents).context("parse Lumen config")
    }

    pub fn asr_label(&self) -> String {
        if self.asr.model.trim().is_empty() {
            self.asr.provider.clone()
        } else {
            format!("{}:{}", self.asr.provider, self.asr.model)
        }
    }

    pub fn corrector_label(&self) -> String {
        if !self.corrector.enabled || self.corrector.provider == "none" {
            return "none".into();
        }
        format!(
            "{}:{}|{}",
            self.corrector.provider, self.corrector.model, self.output.cleanup
        )
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct AsrSettings {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub language: String,
    pub timeout_secs: u64,
}

impl Default for AsrSettings {
    fn default() -> Self {
        Self {
            provider: "local_sensevoice".into(),
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            language: String::new(),
            timeout_secs: 120,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct CorrectorSettings {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

impl Default for CorrectorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: std::env::var("LUMEN_CORRECTOR_MODEL").unwrap_or_else(|_| "qwen3.5:9b".into()),
            api_key: std::env::var("LUMEN_CORRECTOR_API_KEY").unwrap_or_default(),
            timeout_secs: 60,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    pub cleanup: String,
    pub style: String,
    pub casing: String,
    pub punctuation: String,
    pub polish: Vec<String>,
    pub custom_enabled: bool,
    pub custom_instruction: String,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            cleanup: "medium".into(),
            style: "neutral".into(),
            casing: "sentence".into(),
            punctuation: "standard".into(),
            polish: Vec::new(),
            custom_enabled: false,
            custom_instruction: String::new(),
        }
    }
}

impl OutputSettings {
    pub fn prompt_input(&self) -> Result<lumen_prompts::PromptBuildInput> {
        let cleanup = CleanupLevel::parse(&self.cleanup)
            .with_context(|| format!("invalid output.cleanup `{}`", self.cleanup))?;
        let style = Style::parse(&self.style)
            .with_context(|| format!("invalid output.style `{}`", self.style))?;
        let casing = Casing::parse(&self.casing)
            .with_context(|| format!("invalid output.casing `{}`", self.casing))?;
        let punctuation = PunctPolicy::parse(&self.punctuation)
            .with_context(|| format!("invalid output.punctuation `{}`", self.punctuation))?;
        let polish = self
            .polish
            .iter()
            .map(|rule| {
                PolishRule::parse(rule)
                    .with_context(|| format!("invalid output.polish rule `{rule}`"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(lumen_prompts::PromptBuildInput {
            cleanup,
            style,
            casing,
            punctuation,
            polish,
            custom: self
                .custom_enabled
                .then(|| self.custom_instruction.trim().to_string())
                .filter(|instruction| !instruction.is_empty()),
            intent: lumen_prompts::IntentSpec::Default,
        })
    }
}

pub fn default_lumen_config_path() -> PathBuf {
    default_lumen_dir().join("config.toml")
}

pub fn default_lumen_db_path() -> PathBuf {
    default_lumen_dir().join("lumen.sqlite")
}

pub fn default_benchmark_dir() -> PathBuf {
    default_lumen_dir()
        .join("benchmarks")
        .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string())
}

pub fn default_reference_db_path() -> PathBuf {
    std::env::var_os("LUMEN_BENCH_REFERENCE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_lumen_dir().join("benchmarks/reference.sqlite"))
}

fn default_lumen_dir() -> PathBuf {
    home_dir().join("Library/Application Support/LumenAsr")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_profile_rejects_unknown_values() {
        let output = OutputSettings {
            cleanup: "meduim".into(),
            ..OutputSettings::default()
        };

        let error = output.prompt_input().unwrap_err().to_string();

        assert!(error.contains("output.cleanup"));
        assert!(error.contains("meduim"));
    }

    #[test]
    fn corrector_defaults_match_desktop_pipeline() {
        let corrector = CorrectorSettings::default();

        assert!(corrector.enabled);
        assert_eq!(corrector.provider, "ollama");
        assert_eq!(corrector.base_url, "http://127.0.0.1:11434/v1");
        assert!(!corrector.model.is_empty());
    }
}
