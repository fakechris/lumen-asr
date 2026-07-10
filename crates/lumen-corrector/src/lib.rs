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

/// Apply preprocess, then corrector; on error return preprocessed text.
pub async fn correct_or_fallback(
    corrector: &dyn Corrector,
    text: &str,
    dictionary: DictionaryContext,
) -> CorrectResult {
    let pre = preprocess(text);
    // Deterministic replacements before model (high-precision user rules).
    let pre = apply_replacements(&pre, &dictionary.replacements);

    match corrector
        .correct(CorrectRequest {
            text: pre.clone(),
            dictionary,
        })
        .await
    {
        Ok(mut r) => {
            r.text = r.text.trim().to_string();
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
}
