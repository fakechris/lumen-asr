//! Build corrector instances and apply correction with dictionary context.

use crate::config::{AppConfig, CorrectorConfig};
use lumen_corrector::{
    correct_or_fallback_with, preprocess_only, CorrectResult, Corrector, DictionaryContext,
    NullCorrector, OpenAiCompatConfig, OpenAiCompatCorrector,
};
use lumen_core::CorrectorEngineId;
use lumen_dictionary::{split_for_injection, DictionaryEntry};
use lumen_prompts::{
    build_system_prompt_from, effective_cleanup, IntentSpec, PromptBuildInput,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct CorrectorRunIdentity {
    pub provider: String,
    pub model: Option<String>,
    pub prompt_hash: Option<String>,
    pub prompt_hash_algorithm: Option<String>,
    pub temperature: Option<f64>,
}

pub fn run_identity(app: &AppConfig, intent: IntentSpec) -> CorrectorRunIdentity {
    let input = app.output.prompt_input(intent);
    let level = effective_cleanup(&input);
    let prompt = build_system_prompt_from(&input);
    let model_enabled =
        level.uses_model() && app.corrector.enabled && app.corrector.provider != "none";
    CorrectorRunIdentity {
        provider: if model_enabled {
            app.corrector.provider.clone()
        } else {
            "none".into()
        },
        model: model_enabled.then(|| effective_model(&app.corrector)),
        prompt_hash: (model_enabled && !prompt.is_empty())
            .then(|| blake3::hash(prompt.as_bytes()).to_hex().to_string()),
        prompt_hash_algorithm: (model_enabled && !prompt.is_empty()).then(|| "blake3".into()),
        temperature: model_enabled.then(|| level.temperature() as f64),
    }
}

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
        "none" => return Ok(Box::new(NullCorrector)),
        // All cloud / compatible presets share OpenAI chat completions shape.
        _ => CorrectorEngineId::OpenAiCompatible,
    };

    let base_url = if cfg.base_url.trim().is_empty() {
        match engine_id {
            CorrectorEngineId::Ollama => "http://127.0.0.1:11434/v1".into(),
            _ => {
                // Fill from preset defaults when user only picked provider id.
                crate::provider_presets::llm_preset_by_id(&cfg.provider)
                    .map(|p| p.base_url)
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| "base_url required for online corrector".to_string())?
            }
        }
    } else {
        cfg.base_url.clone()
    };

    let model = effective_model(cfg);

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

fn effective_model(cfg: &CorrectorConfig) -> String {
    if cfg.model.trim().is_empty() {
        crate::provider_presets::llm_preset_by_id(&cfg.provider)
            .map(|p| p.default_model)
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| "qwen3.5:9b".into())
    } else {
        cfg.model.clone()
    }
}

pub async fn run_correct(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
) -> CorrectResult {
    run_correct_with_intent(app, text, entries, IntentSpec::Default).await
}

pub async fn run_correct_with_intent(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
    intent: IntentSpec,
) -> CorrectResult {
    let dict = dictionary_context(entries);
    let input: PromptBuildInput = app.output.prompt_input(intent);
    let level = effective_cleanup(&input);

    // No model: cleanup none (and not translate).
    let system = build_system_prompt_from(&input);
    if system.is_empty()
        || !app.corrector.enabled
        || app.corrector.provider == "none"
    {
        let mut r = preprocess_only(text, &dict);
        r.engine = CorrectorEngineId::None;
        return r;
    }

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
        effective_model(&app.corrector),
        level.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_identity_uses_effective_model_and_stable_prompt_hash() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "MiniMax-M3".into();

        let first = run_identity(&app, IntentSpec::Default);
        let second = run_identity(&app, IntentSpec::Default);

        assert_eq!(first.provider, "minimax");
        assert_eq!(first.model.as_deref(), Some("MiniMax-M3"));
        assert_eq!(first.prompt_hash, second.prompt_hash);
        assert_eq!(first.prompt_hash_algorithm.as_deref(), Some("blake3"));
        assert!((first.temperature.unwrap() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn disabled_corrector_identity_does_not_claim_a_model() {
        let mut app = AppConfig::default();
        app.corrector.enabled = false;
        let identity = run_identity(&app, IntentSpec::Default);

        assert_eq!(identity.provider, "none");
        assert_eq!(identity.model, None);
        assert_eq!(identity.prompt_hash, None);
        assert_eq!(identity.temperature, None);
    }

    #[test]
    fn engine_label_uses_the_model_that_will_run() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model.clear();

        assert!(engine_label(&app).contains("MiniMax-M3"));
    }
}
