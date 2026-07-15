//! Build corrector instances and apply correction with dictionary context.

use crate::config::{AppConfig, CorrectorConfig};
use lumen_corrector::{
    correct_or_fallback_with_context, preprocess_only, CorrectResult, Corrector,
    DictionaryContext, NullCorrector, OpenAiCompatConfig, OpenAiCompatCorrector,
};
use lumen_core::CorrectorEngineId;
use lumen_dictionary::{split_for_injection, DictionaryEntry};
use lumen_prompts::{
    build_system_prompt_from, effective_cleanup, IntentSpec, PromptBuildInput,
};
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

    let model = if cfg.model.trim().is_empty() {
        crate::provider_presets::llm_preset_by_id(&cfg.provider)
            .map(|p| p.default_model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "qwen3.5:9b".into())
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
    run_correct_with_intent(app, text, entries, IntentSpec::Default).await
}

pub async fn run_correct_with_intent(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
    intent: IntentSpec,
) -> CorrectResult {
    run_correct_with_intent_and_context(app, text, entries, intent, None).await
}

pub async fn run_correct_with_intent_and_context(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
    intent: IntentSpec,
    window_context: Option<String>,
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
            correct_or_fallback_with_context(
                c.as_ref(),
                text,
                dict,
                system,
                temperature,
                window_context,
            )
            .await
        }
        Err(e) => {
            tracing::warn!(error = %e, "corrector build failed, preprocess only");
            correct_or_fallback_with_context(
                &NullCorrector,
                text,
                dict,
                system,
                temperature,
                window_context,
            )
            .await
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
