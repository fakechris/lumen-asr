//! Model-based corrector with rule preprocess.
//!
//! Product rule: **models are required for correction quality**.
//! Rules only normalize; on model failure we fail-soft to preprocessed text.

mod openai_compat;
mod preprocess;

pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatCorrector};
pub use preprocess::preprocess;

use async_trait::async_trait;
use lumen_core::CorrectorEngineId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CorrectorError {
    #[error("request timed out")]
    Timeout,
    #[error("http error: {0}")]
    Http(String),
    #[error("provider rejected request with status {0}")]
    ProviderRejected(u16),
    #[error("malformed provider response")]
    MalformedResponse,
    #[error("empty model output")]
    EmptyOutput,
    #[error("filtered by provider")]
    Filtered,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CorrectorFallbackReason {
    Timeout,
    Http,
    Authentication,
    RateLimited,
    ProviderClientError,
    ProviderServerError,
    ProviderRejected,
    MalformedResponse,
    EmptyOutput,
    EmptyAfterSanitization,
    BuildFailed,
    Other,
}

impl CorrectorFallbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Http => "http",
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::ProviderClientError => "provider_client_error",
            Self::ProviderServerError => "provider_server_error",
            Self::ProviderRejected => "provider_rejected",
            Self::MalformedResponse => "malformed_response",
            Self::EmptyOutput => "empty_output",
            Self::EmptyAfterSanitization => "empty_after_sanitization",
            Self::BuildFailed => "build_failed",
            Self::Other => "other",
        }
    }
}

impl CorrectorError {
    fn fallback_reason(&self) -> CorrectorFallbackReason {
        match self {
            Self::Timeout => CorrectorFallbackReason::Timeout,
            Self::Http(_) => CorrectorFallbackReason::Http,
            Self::ProviderRejected(401 | 403) => CorrectorFallbackReason::Authentication,
            Self::ProviderRejected(429) => CorrectorFallbackReason::RateLimited,
            Self::ProviderRejected(400..=499) => CorrectorFallbackReason::ProviderClientError,
            Self::ProviderRejected(500..=599) => CorrectorFallbackReason::ProviderServerError,
            Self::ProviderRejected(_) => CorrectorFallbackReason::ProviderRejected,
            Self::MalformedResponse => CorrectorFallbackReason::MalformedResponse,
            Self::EmptyOutput => CorrectorFallbackReason::EmptyOutput,
            Self::Filtered => CorrectorFallbackReason::ProviderRejected,
            Self::Other(_) => CorrectorFallbackReason::Other,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DictionaryContext {
    pub terms: Vec<String>,
    pub replacements: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectRequest {
    pub text: String,
    pub dictionary: DictionaryContext,
    /// Full system prompt (empty → backend default light-ish base).
    #[serde(default)]
    pub system_prompt: String,
    /// Sampling temperature hint for the provider.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_temperature() -> f32 {
    0.3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectResult {
    pub text: String,
    pub engine: CorrectorEngineId,
    /// True if model ran successfully (not just preprocess fallback).
    pub model_applied: bool,
    /// Sanitized category only; never contains provider bodies or credentials.
    #[serde(default)]
    pub fallback_reason: Option<CorrectorFallbackReason>,
}

#[async_trait]
pub trait Corrector: Send + Sync {
    fn id(&self) -> CorrectorEngineId;
    async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError>;
}

/// Apply preprocess + replacements only (no model).
pub fn preprocess_only(text: &str, dictionary: &DictionaryContext) -> CorrectResult {
    let pre = preprocess(text);
    let pre = apply_replacements(&pre, &dictionary.replacements);
    CorrectResult {
        text: pre,
        engine: CorrectorEngineId::None,
        model_applied: false,
        fallback_reason: None,
    }
}

/// Apply preprocess, then corrector; on error return preprocessed text.
///
/// `system_prompt` empty → use built-in base prompt (legacy).
pub async fn correct_or_fallback(
    corrector: &dyn Corrector,
    text: &str,
    dictionary: DictionaryContext,
) -> CorrectResult {
    correct_or_fallback_with(
        corrector,
        text,
        dictionary,
        lumen_prompts::build_system_prompt(lumen_prompts::CleanupLevel::Medium),
        lumen_prompts::CleanupLevel::Medium.temperature(),
    )
    .await
}

/// Preprocess then model with explicit system prompt + temperature.
pub async fn correct_or_fallback_with(
    corrector: &dyn Corrector,
    text: &str,
    dictionary: DictionaryContext,
    system_prompt: String,
    temperature: f32,
) -> CorrectResult {
    let pre = preprocess(text);
    let pre = apply_replacements(&pre, &dictionary.replacements);

    let system_prompt = if system_prompt.trim().is_empty() {
        lumen_prompts::build_system_prompt(lumen_prompts::CleanupLevel::Medium)
    } else {
        system_prompt
    };

    match corrector
        .correct(CorrectRequest {
            text: pre.clone(),
            dictionary,
            system_prompt,
            temperature,
        })
        .await
    {
        Ok(mut r) => {
            // Always strip thinking blocks (Ollama/Qwen/Kimi/etc.) — dictation must
            // never paste chain-of-thought into the user's cursor.
            r.text = crate::openai_compat::strip_thinking_tags(r.text.trim());
            if r.text.is_empty() {
                CorrectResult {
                    text: pre,
                    engine: corrector.id(),
                    model_applied: false,
                    fallback_reason: Some(CorrectorFallbackReason::EmptyAfterSanitization),
                }
            } else {
                r
            }
        }
        Err(e) => {
            let fallback_reason = e.fallback_reason();
            tracing::warn!(
                reason = fallback_reason.as_str(),
                "corrector failed, using preprocess fallback"
            );
            CorrectResult {
                text: pre,
                engine: corrector.id(),
                model_applied: false,
                fallback_reason: Some(fallback_reason),
            }
        }
    }
}

fn apply_replacements(text: &str, replacements: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in replacements {
        if from.is_empty() {
            continue;
        }
        out = out.replace(from, to);
    }
    out
}

/// No-op corrector (rules/preprocess only path for tests).
pub struct NullCorrector;

#[async_trait]
impl Corrector for NullCorrector {
    fn id(&self) -> CorrectorEngineId {
        CorrectorEngineId::None
    }

    async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
        Ok(CorrectResult {
            text: req.text,
            engine: CorrectorEngineId::None,
            model_applied: false,
            fallback_reason: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fallback_on_null_is_preprocessed() {
        let r =
            correct_or_fallback(&NullCorrector, "你好  世界", DictionaryContext::default()).await;
        assert_eq!(r.text, "你好 世界");
        assert!(!r.model_applied);
    }

    #[test]
    fn replacements_apply() {
        let s = apply_replacements("用脱肯鉴权", &[("脱肯".into(), "Token".into())]);
        assert_eq!(s, "用Token鉴权");
    }

    struct TimeoutCorrector;

    #[async_trait]
    impl Corrector for TimeoutCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }

        async fn correct(&self, _req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            Err(CorrectorError::Timeout)
        }
    }

    #[tokio::test]
    async fn fallback_persists_a_sanitized_timeout_category() {
        let result =
            correct_or_fallback(&TimeoutCorrector, "hello", DictionaryContext::default()).await;

        assert!(!result.model_applied);
        assert_eq!(
            result.fallback_reason,
            Some(CorrectorFallbackReason::Timeout)
        );
    }

    #[test]
    fn provider_statuses_map_to_retry_relevant_sanitized_categories() {
        assert_eq!(
            CorrectorError::ProviderRejected(401).fallback_reason(),
            CorrectorFallbackReason::Authentication
        );
        assert_eq!(
            CorrectorError::ProviderRejected(429).fallback_reason(),
            CorrectorFallbackReason::RateLimited
        );
        assert_eq!(
            CorrectorError::ProviderRejected(422).fallback_reason(),
            CorrectorFallbackReason::ProviderClientError
        );
        assert_eq!(
            CorrectorError::ProviderRejected(503).fallback_reason(),
            CorrectorFallbackReason::ProviderServerError
        );
    }
}
