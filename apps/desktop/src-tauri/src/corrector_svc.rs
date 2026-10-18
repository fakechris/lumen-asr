//! Build corrector instances and apply correction with dictionary context.

use crate::config::{AppConfig, CorrectorConfig};
use crate::dictionary_budget::{
    select_for_prompt, DictionarySelection, DICTIONARY_PROMPT_BUDGET_CHARS,
};
use lumen_core::CorrectorEngineId;
use lumen_corrector::{
    correct_or_fallback_with_context, preprocess_only, CorrectResult, Corrector,
    CorrectorFallbackReason, DictionaryContext, NullCorrector, OpenAiCompatConfig,
    OpenAiCompatCorrector,
};
use lumen_dictionary::DictionaryEntry;
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

/// Dictionary context injected for `asr_text`: every confirmed entry while the
/// rendered block fits the prompt budget, otherwise the most relevant entries
/// (see [`crate::dictionary_budget`]).
pub fn dictionary_context(entries: &[DictionaryEntry], asr_text: &str) -> DictionaryContext {
    dictionary_selection(entries, asr_text).context
}

fn dictionary_selection(entries: &[DictionaryEntry], asr_text: &str) -> DictionarySelection {
    select_for_prompt(entries, asr_text, DICTIONARY_PROMPT_BUDGET_CHARS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryRunIdentity {
    pub hash: String,
    pub hash_algorithm: &'static str,
    /// Terms actually injected (what `hash` covers).
    pub term_count: u32,
    /// Replacement rules actually injected (what `hash` covers).
    pub replacement_count: u32,
    /// Confirmed terms available before the prompt budget was applied.
    pub total_term_count: u32,
    /// Confirmed replacement rules available before the prompt budget was applied.
    pub total_replacement_count: u32,
}

/// Identity of the dictionary context injected for `asr_text`. The hash covers
/// only the injected entries, so it changes whenever the selection does.
pub fn dictionary_run_identity(
    entries: &[DictionaryEntry],
    asr_text: &str,
) -> DictionaryRunIdentity {
    let selection = dictionary_selection(entries, asr_text);
    let context = &selection.context;
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
        total_term_count: selection.total_terms as u32,
        total_replacement_count: selection.total_replacements as u32,
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
    run_correct_with_intent_and_context(app, text, entries, intent, None).await
}

pub async fn run_correct_with_intent_and_context(
    app: &AppConfig,
    text: &str,
    entries: &[DictionaryEntry],
    intent: IntentSpec,
    context_json: Option<&str>,
) -> CorrectResult {
    let dict = dictionary_context(entries, text);
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
        Ok(c) => {
            correct_or_fallback_with_context(
                c.as_ref(),
                text,
                dict,
                context_json.map(str::to_owned),
                system,
                temperature,
            )
            .await
        }
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
    use crate::context_capture::{
        CorrectorBrowserProjection, CorrectorContextProjection, CorrectorEditorProjection,
        CorrectorTargetProjection,
    };

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

        let first_identity = dictionary_run_identity(&[first], "cotex");
        let same_identity = dictionary_run_identity(&[same_context], "cotex");
        let changed_identity = dictionary_run_identity(&[changed], "cotex");

        assert_eq!(first_identity, same_identity);
        assert_ne!(first_identity.hash, changed_identity.hash);
        assert_eq!(first_identity.term_count, 0);
        assert_eq!(first_identity.replacement_count, 1);
    }

    /// Upper bound, in characters, on everything the dictation corrector sends
    /// besides the ASR text itself (system prompt + user message framing,
    /// captured context, dictionary). Each input is capped at its source: the
    /// custom instruction (500), the translate style (200), the captured-context
    /// projection fields (`context_capture` limits) and the dictionary block
    /// ([`DICTIONARY_PROMPT_BUDGET_CHARS`]). Replacement rules that fire on the
    /// utterance may exceed the dictionary budget; they are bounded by the ASR
    /// text they match rather than by this ceiling.
    const CORRECTOR_PROMPT_OVERHEAD_CEILING_CHARS: usize = 12_000;

    fn repeat_to(seed: &str, chars: usize) -> String {
        seed.chars().cycle().take(chars).collect()
    }

    /// Projection with every field filled to its `context_capture` limit. The
    /// capture only fills cursor *or* nearby *or* field text, and browser text
    /// only without editor text; this fills the largest valid combination
    /// (selection + cursor before/after) and the browser metadata.
    fn maximal_context_projection() -> CorrectorContextProjection {
        const TARGET: usize = 256;
        const SELECTED: usize = 1_000;
        const CURSOR: usize = 1_500;
        let text = |chars| Some(repeat_to("上下文参考文本", chars));
        CorrectorContextProjection {
            schema_version: 1,
            target: Some(CorrectorTargetProjection {
                app_name: text(TARGET),
                bundle_id: text(TARGET),
                window_title: text(TARGET),
                document_url: text(TARGET),
            }),
            editor: Some(CorrectorEditorProjection {
                role: text(TARGET),
                title: text(TARGET),
                label: text(TARGET),
                placeholder: text(TARGET),
                selected_text: text(SELECTED),
                cursor_before: text(CURSOR),
                cursor_after: text(CURSOR),
                nearby_before: None,
                nearby_after: None,
                field_text: None,
            }),
            browser: Some(CorrectorBrowserProjection {
                title: text(TARGET),
                domain: text(TARGET),
                page_language: text(TARGET),
                selection_text: None,
                nearby_before: None,
                nearby_after: None,
            }),
            visible_text: Vec::new(),
            truncated: true,
        }
    }

    #[test]
    fn full_corrector_prompt_stays_under_the_overhead_ceiling() {
        let mut app = AppConfig::default();
        app.corrector.enabled = true;
        app.corrector.provider = "minimax".into();
        app.corrector.model = "test-corrector-model".into();
        app.output.cleanup = "strong".into();
        app.output.qwen_cleanup = "strong".into();
        app.output.style = "formal".into();
        app.output.polish = lumen_prompts::PolishRule::all()
            .iter()
            .map(|rule| rule.as_str().to_owned())
            .collect();
        app.output.custom_enabled = true;
        app.output.custom_instruction = repeat_to("自定义整理要求", 4_000);
        let intent = IntentSpec::Translate {
            target_language: "简体中文（中国大陆）".into(),
            style: Some(repeat_to("翻译风格要求", 4_000)),
        };

        let asr_text = repeat_to("今天我们讨论一下 Lumen 语音输入的词典策略。", 300);
        let mut entries: Vec<DictionaryEntry> = (0..1_000)
            .map(|index| DictionaryEntry::term(format!("专业术语{index:04}")))
            .collect();
        entries.extend(
            (0..200).map(|index| DictionaryEntry::replacement(format!("误识{index}"), "Lumen")),
        );
        let dictionary = dictionary_context(&entries, &asr_text);
        let block =
            lumen_prompts::format_dictionary_block(&dictionary.terms, &dictionary.replacements);
        let block_chars = block.chars().count();
        assert!(block_chars <= DICTIONARY_PROMPT_BUDGET_CHARS);
        assert!(
            block_chars > DICTIONARY_PROMPT_BUDGET_CHARS - 20,
            "dictionary should be filled to the budget, got {block_chars}"
        );

        // Translate never sends captured context at runtime; combining both
        // here is a strict upper bound on either real prompt.
        let mut system = build_system_prompt_from(&app.corrector_prompt_input(intent));
        system.push_str("\n\n");
        system.push_str(lumen_prompts::context_safety_system_instruction());
        let context_json = maximal_context_projection().to_model_json().unwrap();
        let user = lumen_prompts::corrector_user_message_with_context(
            &asr_text,
            Some(&block),
            Some(&context_json),
        );

        let total = system.chars().count() + user.chars().count();
        let overhead = total - asr_text.chars().count();
        assert!(
            overhead <= CORRECTOR_PROMPT_OVERHEAD_CEILING_CHARS,
            "prompt overhead {overhead} chars exceeds ceiling {CORRECTOR_PROMPT_OVERHEAD_CEILING_CHARS}"
        );
        eprintln!(
            "corrector prompt overhead: {overhead} chars (system {}, context {}, dictionary {block_chars})",
            system.chars().count(),
            context_json.chars().count()
        );
    }

    #[test]
    fn dictionary_identity_hashes_only_injected_entries() {
        let mut entries: Vec<DictionaryEntry> = (0..400)
            .map(|index| DictionaryEntry::term(format!("填充术语{index:04}")))
            .collect();
        // Oldest entry: only relevance, not recency, can select it.
        let mut relevant = DictionaryEntry::term("Kubernetes");
        relevant.updated_at -= chrono::Duration::days(1);
        entries.insert(0, relevant);

        let first = dictionary_run_identity(&entries, "部署 kubernetes 集群");
        let other_text = dictionary_run_identity(&entries, "完全无关");
        let injected = dictionary_context(&entries, "部署 kubernetes 集群");

        assert_eq!(first.total_term_count, 401);
        assert_eq!(first.term_count as usize, injected.terms.len());
        assert!(first.term_count < first.total_term_count);
        assert!(injected.terms.contains(&"Kubernetes".to_owned()));
        // A different selection must not share the identity of the first one.
        assert!(!dictionary_context(&entries, "完全无关")
            .terms
            .contains(&"Kubernetes".to_owned()));
        assert_ne!(first.hash, other_text.hash);
    }

    #[test]
    fn small_dictionary_identity_counts_every_entry() {
        let entries = [
            DictionaryEntry::term("Codex"),
            DictionaryEntry::replacement("cotex", "Codex"),
        ];
        let identity = dictionary_run_identity(&entries, "unrelated");

        assert_eq!(identity.term_count, 1);
        assert_eq!(identity.total_term_count, 1);
        assert_eq!(identity.replacement_count, 1);
        assert_eq!(identity.total_replacement_count, 1);
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
