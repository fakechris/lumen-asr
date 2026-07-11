//! ASR engine abstraction + microphone capture.
//!
//! Default product path: SenseVoice via sherpa-onnx.
//! Alternative: Whisper (same `AsrEngine` port).

mod audio;
mod cloud_openai;
mod paths;
mod sensevoice;
mod whisper;

pub use audio::{resample_linear, AudioCapture, AudioDeviceInfo, AudioError, CaptureResult};
pub use cloud_openai::{OpenAiAudioAsr, OpenAiAudioConfig};
pub use paths::{
    default_sensevoice_dir, default_whisper_dir, sensevoice_ready, whisper_ready,
};
pub use sensevoice::SenseVoiceSherpaAsr;
pub use whisper::WhisperAsr;

use async_trait::async_trait;
use lumen_core::AsrEngineId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    pub language: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AsrRequest {
    /// PCM f32 mono samples in [-1, 1].
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub hotwords: Vec<String>,
}

#[async_trait]
pub trait AsrEngine: Send + Sync {
    fn id(&self) -> AsrEngineId;
    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError>;
}

/// Deterministic stub for tests.
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

/// Product Application Support models root.
pub fn app_models_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(home).join("Library/Application Support/LumenAsr/models")
    }
    #[cfg(not(target_os = "macos"))]
    {
        PathBuf::from(home).join(".lumen-asr/models")
    }
}

/// Normalize capture to 16 kHz mono for ASR engines.
pub fn prepare_for_asr(capture: &CaptureResult) -> Vec<f32> {
    const TARGET: u32 = 16_000;
    resample_linear(&capture.samples, capture.sample_rate, TARGET)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    SenseVoice,
    Whisper,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SenseVoice => "sensevoice",
            Self::Whisper => "whisper",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sensevoice" | "sensevoice_sherpa" | "sherpa" => Some(Self::SenseVoice),
            "whisper" => Some(Self::Whisper),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub kind: EngineKind,
    pub ready: bool,
    pub model_dir: String,
}

pub fn sensevoice_status() -> EngineStatus {
    let dir = default_sensevoice_dir();
    EngineStatus {
        kind: EngineKind::SenseVoice,
        ready: sensevoice_ready(&dir),
        model_dir: dir.display().to_string(),
    }
}

pub fn whisper_status() -> EngineStatus {
    let dir = default_whisper_dir();
    EngineStatus {
        kind: EngineKind::Whisper,
        ready: whisper_ready(&dir),
        model_dir: dir.display().to_string(),
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

    #[test]
    fn prepare_resamples() {
        let cap = CaptureResult {
            samples: vec![0.0, 1.0, 0.0, -1.0],
            sample_rate: 32000,
        };
        let out = prepare_for_asr(&cap);
        assert!(!out.is_empty());
    }
}
