//! ASR engine abstraction.
//!
//! Default product path: SenseVoice via sherpa-onnx (feature `sherpa`).
//! Alternative: Whisper (feature `whisper`).
//!
//! M0 ships the port + a deterministic stub for tests/orchestration wiring.

use async_trait::async_trait;
use lumen_core::AsrEngineId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AsrError {
    #[error("asr engine not configured: {0}")]
    NotConfigured(String),
    #[error("empty audio")]
    EmptyAudio,
    #[error("inference failed: {0}")]
    Inference(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsrResult {
    pub text: String,
    pub engine: AsrEngineId,
    /// Optional language hint from model.
    pub language: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AsrRequest {
    /// PCM f32 mono samples.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    /// Optional hotwords / terms for engines that support them.
    pub hotwords: Vec<String>,
}

#[async_trait]
pub trait AsrEngine: Send + Sync {
    fn id(&self) -> AsrEngineId;
    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError>;
}

/// Placeholder engine for wiring tests until sherpa is integrated.
pub struct StubAsr {
    canned: String,
}

impl StubAsr {
    pub fn new(canned: impl Into<String>) -> Self {
        Self {
            canned: canned.into(),
        }
    }
}

#[async_trait]
impl AsrEngine for StubAsr {
    fn id(&self) -> AsrEngineId {
        AsrEngineId::Other
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        Ok(AsrResult {
            text: self.canned.clone(),
            engine: self.id(),
            language: Some("zh".into()),
        })
    }
}

/// SenseVoice via sherpa-onnx — implementation lands in M2.
pub struct SenseVoiceSherpaAsr {
    pub model_dir: std::path::PathBuf,
}

impl SenseVoiceSherpaAsr {
    pub fn new(model_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            model_dir: model_dir.into(),
        }
    }
}

#[async_trait]
impl AsrEngine for SenseVoiceSherpaAsr {
    fn id(&self) -> AsrEngineId {
        AsrEngineId::SenseVoiceSherpa
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        Err(AsrError::NotConfigured(
            "SenseVoiceSherpaAsr: sherpa-onnx integration pending (M2)".into(),
        ))
    }
}

/// Whisper engine placeholder — M2/M3.
pub struct WhisperAsr {
    pub model_dir: std::path::PathBuf,
}

impl WhisperAsr {
    pub fn new(model_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            model_dir: model_dir.into(),
        }
    }
}

#[async_trait]
impl AsrEngine for WhisperAsr {
    fn id(&self) -> AsrEngineId {
        AsrEngineId::Whisper
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        Err(AsrError::NotConfigured(
            "WhisperAsr: integration pending".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_transcribes() {
        let eng = StubAsr::new("hello");
        let r = eng
            .transcribe(AsrRequest {
                samples: vec![0.1, 0.2],
                sample_rate: 16000,
                hotwords: vec![],
            })
            .await
            .unwrap();
        assert_eq!(r.text, "hello");
    }
}
