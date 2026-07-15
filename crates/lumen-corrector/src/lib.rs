//! Model-based corrector with rule preprocess.
//!
//! Product rule: **models are required for correction quality**.
//! Rules only normalize; on model failure we fail-soft to preprocessed text.

mod preprocess;
mod openai_compat;

pub use openai_compat::{OpenAiCompatCorrector, OpenAiCompatConfig};
pub use preprocess::preprocess;

use async_trait::async_trait;
use lumen_core::CorrectorEngineId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CorrectorError {
    #[error("http error: {0}")]
    Http(String),
    #[error("empty model output")]
    EmptyOutput,
    #[error("filtered by provider")]
    Filtered,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
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
    /// Bounded, text-only current-window context. Capture internals never cross
    /// this provider-facing seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_context: Option<String>,
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
    correct_or_fallback_with_context(
        corrector,
        text,
        dictionary,
        system_prompt,
        temperature,
        None,
    )
    .await
}

/// Preprocess then invoke the model with optional current-window context.
pub async fn correct_or_fallback_with_context(
    corrector: &dyn Corrector,
    text: &str,
    dictionary: DictionaryContext,
    system_prompt: String,
    temperature: f32,
    window_context: Option<String>,
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
            window_context,
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
                }
            } else {
                r
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "corrector failed, using preprocess fallback");
            CorrectResult {
                text: pre,
                engine: corrector.id(),
                model_applied: false,
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct CapturingCorrector {
        request: Arc<Mutex<Option<CorrectRequest>>>,
    }

    #[async_trait]
    impl Corrector for CapturingCorrector {
        fn id(&self) -> CorrectorEngineId {
            CorrectorEngineId::OpenAiCompatible
        }

        async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
            *self.request.lock().unwrap() = Some(req.clone());
            Ok(CorrectResult {
                text: req.text,
                engine: self.id(),
                model_applied: true,
            })
        }
    }

    #[tokio::test]
    async fn fallback_on_null_is_preprocessed() {
        let r = correct_or_fallback(
            &NullCorrector,
            "你好  世界",
            DictionaryContext::default(),
        )
        .await;
        assert_eq!(r.text, "你好 世界");
        assert!(!r.model_applied);
    }

    #[test]
    fn replacements_apply() {
        let s = apply_replacements("用脱肯鉴权", &[("脱肯".into(), "Token".into())]);
        assert_eq!(s, "用Token鉴权");
    }

    #[tokio::test]
    async fn optional_window_context_crosses_the_corrector_request_seam() {
        let captured = Arc::new(Mutex::new(None));
        let corrector = CapturingCorrector {
            request: Arc::clone(&captured),
        };

        correct_or_fallback_with_context(
            &corrector,
            "麦克 vision OCR",
            DictionaryContext::default(),
            "system".into(),
            0.3,
            Some("窗口：Docs\n可见文字：macOS Vision".into()),
        )
        .await;

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.window_context.as_deref(),
            Some("窗口：Docs\n可见文字：macOS Vision")
        );
    }

    #[tokio::test]
    async fn legacy_corrector_call_keeps_window_context_absent() {
        let captured = Arc::new(Mutex::new(None));
        let corrector = CapturingCorrector {
            request: Arc::clone(&captured),
        };

        correct_or_fallback_with(
            &corrector,
            "原始文本",
            DictionaryContext::default(),
            "system".into(),
            0.3,
        )
        .await;

        assert!(captured
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .window_context
            .is_none());
    }
}
