//! Build corrector instances and apply correction with dictionary context.

use crate::config::{AppConfig, CorrectorConfig};
use lumen_core::CorrectorEngineId;
use lumen_corrector::{
    correct_or_fallback_with, preprocess_only, CorrectResult, Corrector, CorrectorFallbackReason,
    DictionaryContext, NullCorrector, OpenAiCompatConfig, OpenAiCompatCorrector,
};
use lumen_dictionary::{split_for_injection, DictionaryEntry};
use lumen_prompts::{build_system_prompt_from, effective_cleanup, IntentSpec, PromptBuildInput};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct CorrectorRunIdentity {
    pub provider: String,
    pub model: Option<String>,
    pub engine_label: String,
    pub prompt_hash: Option<String>,
    pub prompt_hash_algorithm: Option<String>,
    pub temperature: Option<f64>,
}

pub fn run_identity(app: &AppConfig, intent: IntentSpec) -> CorrectorRunIdentity {
    let input = app.corrector_prompt_input(intent);
    let level = effective_cleanup(&input);
    let prompt = build_system_prompt_from(&input);
    let model_enabled =
        level.uses_model() && app.corrector.enabled && app.corrector.provider != "none";
    let model = model_enabled.then(|| effective_model(&app.corrector));
    let engine_label = if model_enabled {
        format!(
            "{}:{}|{}",
            app.corrector.provider,
            model.as_deref().unwrap_or_default(),
            level.as_str()
        )
    } else if level.uses_model() {
        "none".into()
    } else {
        format!("cleanup:{}", level.as_str())
    };
    CorrectorRunIdentity {
        provider: if model_enabled {
            app.corrector.provider.clone()
        } else {
            "none".into()
        },
        model,
        engine_label,
        prompt_hash: (model_enabled && !prompt.is_empty())
            .then(|| blake3::hash(prompt.as_bytes()).to_hex().to_string()),
        prompt_hash_algorithm: (model_enabled && !prompt.is_empty()).then(|| "blake3".into()),
        temperature: model_enabled.then(|| level.temperature() as f64),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectorOutcomeIdentity {
    pub engine: String,
    pub fallback: bool,
}

pub fn corrector_outcome_identity(
    identity: &CorrectorRunIdentity,
    model_applied: bool,
) -> CorrectorOutcomeIdentity {
    let fallback = identity.provider != "none" && !model_applied;
    CorrectorOutcomeIdentity {
        engine: if fallback {
            format!("{}:fallback", identity.engine_label)
        } else {
            identity.engine_label.clone()
        },
        fallback,
    }
}

pub fn dictionary_context(entries: &[DictionaryEntry]) -> DictionaryContext {
    let (terms, replacements) = split_for_injection(entries);
    DictionaryContext {
        terms,
        replacements,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryRunIdentity {
    pub hash: String,
    pub hash_algorithm: &'static str,
    pub term_count: u32,
    pub replacement_count: u32,
}

pub fn dictionary_run_identity(entries: &[DictionaryEntry]) -> DictionaryRunIdentity {
    let context = dictionary_context(entries);
    let mut canonical = Vec::new();
    for term in &context.terms {
        canonical.extend_from_slice(&(term.len() as u64).to_le_bytes());
        canonical.extend_from_slice(term.as_bytes());
    }
    canonical.extend_from_slice(b"\0replacements\0");
    for (from, to) in &context.replacements {
        canonical.extend_from_slice(&(from.len() as u64).to_le_bytes());
        canonical.extend_from_slice(from.as_bytes());
        canonical.extend_from_slice(&(to.len() as u64).to_le_bytes());
        canonical.extend_from_slice(to.as_bytes());
    }
    DictionaryRunIdentity {
        hash: blake3::hash(&canonical).to_hex().to_string(),
        hash_algorithm: "blake3",
        term_count: context.terms.len() as u32,
        replacement_count: context.replacements.len() as u32,
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
    let input: PromptBuildInput = app.corrector_prompt_input(intent);
    let level = effective_cleanup(&input);

    // No model: cleanup none (and not translate).
    let system = build_system_prompt_from(&input);
    if system.is_empty() || !app.corrector.enabled || app.corrector.provider == "none" {
        let mut r = preprocess_only(text, &dict);
        r.engine = CorrectorEngineId::None;
        return r;
    }

    let temperature = level.temperature();
    match build_corrector(&app.corrector) {
        Ok(c) => correct_or_fallback_with(c.as_ref(), text, dict, system, temperature).await,
        Err(_) => {
            tracing::warn!(
                reason = "build_failed",
                "corrector build failed, preprocess only"
            );
            let mut result = preprocess_only(text, &dict);
            result.fallback_reason = Some(CorrectorFallbackReason::BuildFailed);
            result
        }
    }
}

pub fn engine_label(app: &AppConfig) -> String {
    run_identity(app, IntentSpec::Default).engine_label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_identity_uses_effective_model_and_stable_prompt_hash() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "test-corrector-model".into();

        let first = run_identity(&app, IntentSpec::Default);
        let second = run_identity(&app, IntentSpec::Default);

        assert_eq!(first.provider, "minimax");
        assert_eq!(first.model.as_deref(), Some("test-corrector-model"));
        assert_eq!(first.prompt_hash, second.prompt_hash);
        assert_eq!(first.prompt_hash_algorithm.as_deref(), Some("blake3"));
        assert!((first.temperature.unwrap() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn run_identity_tracks_the_active_asr_cleanup_profile() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "test-corrector-model".into();

        app.asr.provider = "local_sensevoice".into();
        let sensevoice = run_identity(&app, IntentSpec::Default);
        assert!(sensevoice.engine_label.ends_with("|medium"));
        assert!((sensevoice.temperature.unwrap() - 0.3).abs() < 1e-6);

        app.asr.provider = "local_qwen".into();
        let qwen = run_identity(&app, IntentSpec::Default);
        assert!(qwen.engine_label.ends_with("|light"));
        assert!((qwen.temperature.unwrap() - 0.2).abs() < 1e-6);
        assert_ne!(sensevoice.prompt_hash, qwen.prompt_hash);
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
    fn raw_intent_identity_is_cleanup_only_not_fallback() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "test-corrector-model".into();

        let identity = run_identity(&app, IntentSpec::Raw);
        let outcome = corrector_outcome_identity(&identity, false);

        assert_eq!(identity.provider, "none");
        assert_eq!(identity.engine_label, "cleanup:none");
        assert_eq!(outcome.engine, "cleanup:none");
        assert!(!outcome.fallback);
    }

    #[test]
    fn configured_model_failure_is_the_only_fallback() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "test-corrector-model".into();

        let identity = run_identity(&app, IntentSpec::Default);
        let success = corrector_outcome_identity(&identity, true);
        let fallback = corrector_outcome_identity(&identity, false);

        assert_eq!(success.engine, identity.engine_label);
        assert!(!success.fallback);
        assert_eq!(
            fallback.engine,
            format!("{}:fallback", identity.engine_label)
        );
        assert!(fallback.fallback);
    }

    #[test]
    fn engine_label_uses_the_model_that_will_run() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model.clear();

        assert!(engine_label(&app).contains("MiniMax-M3"));
    }

    #[test]
    fn dictionary_identity_changes_with_effective_context_not_entry_metadata() {
        let first = DictionaryEntry::replacement("cotex", "Codex");
        let mut same_context = DictionaryEntry::replacement("cotex", "Codex");
        same_context.hit_count = 99;
        let changed = DictionaryEntry::replacement("cortex", "Codex");

        let first_identity = dictionary_run_identity(&[first]);
        let same_identity = dictionary_run_identity(&[same_context]);
        let changed_identity = dictionary_run_identity(&[changed]);

        assert_eq!(first_identity, same_identity);
        assert_ne!(first_identity.hash, changed_identity.hash);
        assert_eq!(first_identity.term_count, 0);
        assert_eq!(first_identity.replacement_count, 1);
    }

    #[tokio::test]
    async fn corrector_build_failure_returns_a_sanitized_fallback_category() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "missing-preset".into();
        app.corrector.base_url.clear();

        let result = run_correct_with_intent(&app, "hello", &[], IntentSpec::Default).await;

        assert!(!result.model_applied);
        assert_eq!(
            result.fallback_reason,
            Some(CorrectorFallbackReason::BuildFailed)
        );
    }
}
