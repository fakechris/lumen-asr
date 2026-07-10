//! Build corrector instances and apply correction with dictionary context.

use crate::config::{AppConfig, CorrectorConfig};
use lumen_corrector::{
    correct_or_fallback_with, preprocess_only, CorrectResult, Corrector, DictionaryContext,
    NullCorrector, OpenAiCompatConfig, OpenAiCompatCorrector,
};
use lumen_core::CorrectorEngineId;
use lumen_dictionary::{split_for_injection, DictionaryEntry};
use lumen_prompts::build_system_prompt;
use std::time::Duration;

pub fn dictionary_context(entries: &[DictionaryEntry]) -> DictionaryContext {
    let (terms, replacements) = split_for_injection(entries);
    DictionaryContext {
        terms,
        replacements,
    }
}

/// Build a boxed corrector from settings. `None` provider uses NullCorrector.
pub fn build_corrector(cfg: &CorrectorConfig) -> Result<Box<dyn Corrector + Send + Sync>, String> {
    if !cfg.enabled || cfg.provider == "none" {
        return Ok(Box::new(NullCorrector));
    }

    let engine_id = match cfg.provider.as_str() {
        "ollama" => CorrectorEngineId::Ollama,
        "openai_compatible" | "openai" => CorrectorEngineId::OpenAiCompatible,
        other => {
            return Err(format!("unknown corrector provider: {other}"));
        }
    };

    let base_url = if cfg.base_url.trim().is_empty() {
        match engine_id {
            CorrectorEngineId::Ollama => "http://127.0.0.1:11434/v1".into(),
            _ => return Err("base_url required for openai_compatible".into()),
        }
    } else {
        cfg.base_url.clone()
    };

    let model = if cfg.model.trim().is_empty() {
        "qwen2.5:7b".into()
    } else {
        cfg.model.clone()
    };

    let oc = OpenAiCompatConfig {
        base_url,
        api_key: cfg.api_key.clone(),
        model,
        engine_id,
        timeout: Duration::from_secs(cfg.timeout_secs.max(5)),
    };

    OpenAiCompatCorrector::new(oc)
        .map(|c| Box::new(c) as Box<dyn Corrector + Send + Sync>)
        .map_err(|e| e.to_string())
}

pub async fn run_correct(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
) -> CorrectResult {
    let dict = dictionary_context(entries);
    let level = app.output.cleanup_level();

    // cleanup=none → rules only, keep ASR mistakes.
    if !level.uses_model() || !app.corrector.enabled || app.corrector.provider == "none" {
        let mut r = preprocess_only(text, &dict);
        if !level.uses_model() {
            r.engine = CorrectorEngineId::None;
        }
        return r;
    }

    let system = build_system_prompt(level);
    let temperature = level.temperature();
    match build_corrector(&app.corrector) {
        Ok(c) => {
            correct_or_fallback_with(c.as_ref(), text, dict, system, temperature).await
        }
        Err(e) => {
            tracing::warn!(error = %e, "corrector build failed, preprocess only");
            correct_or_fallback_with(&NullCorrector, text, dict, system, temperature).await
        }
    }
}

pub fn engine_label(app: &AppConfig) -> String {
    let level = app.output.cleanup_level();
    if !level.uses_model() {
        return format!("cleanup:{}", level.as_str());
    }
    if !app.corrector.enabled || app.corrector.provider == "none" {
        return "none".into();
    }
    format!(
        "{}:{}|{}",
        app.corrector.provider,
        app.corrector.model,
        level.as_str()
    )
}
