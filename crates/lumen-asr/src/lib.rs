//! ASR engine abstraction + microphone capture.
//!
//! Product paths: SenseVoice via sherpa-onnx, Qwen3-ASR via a persistent
//! local MLX worker, and Whisper. All implement the same `AsrEngine` port.

mod audio;
mod cloud_openai;
mod install_lock;
mod paths;
mod qwen;
mod sensevoice;
mod whisper;

pub use audio::{resample_linear, AudioCapture, AudioDeviceInfo, AudioError, CaptureResult};
pub use cloud_openai::{OpenAiAudioAsr, OpenAiAudioConfig};
pub use install_lock::ModelInstallLock;
pub use paths::{
    app_models_dir, default_qwen_dir, default_sensevoice_dir, default_sensevoice_dir_with_root,
    default_whisper_dir, default_whisper_dir_with_root, legacy_model_roots, lumen_models_dir,
    lumen_models_dir_with_override, qwen_ready,
    scan_model_candidates, scan_model_candidates_with_root, sensevoice_ready, shared_sensevoice_dir,
    shared_whisper_dir, user_home_dir, whisper_ready, ModelCandidate, ENV_LUMEN_MODELS_DIR,
};
pub use qwen::{QwenAsr, QwenAsrConfig};
pub use sensevoice::SenseVoiceSherpaAsr;
pub use whisper::WhisperAsr;

use async_trait::async_trait;
use lumen_core::AsrEngineId;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
    #[serde(default)]
    pub diagnostics: AsrRuntimeDiagnostics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsrRuntimeDiagnostics {
    /// `Some(false)` means the request paid worker/model cold-start cost.
    /// Engines without a persistent worker leave this unknown.
    pub worker_reused: Option<bool>,
    /// Stable model name without exposing the absolute local filesystem path.
    pub model: Option<String>,
    /// Immutable model revision when the runtime path exposes one.
    pub model_revision: Option<String>,
}

/// Derive a publish-safe identity from the model directory that actually ran.
pub fn model_identity_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let leaf = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let is_hugging_face_snapshot = path
        .parent()
        .and_then(|value| value.file_name())
        .and_then(|value| value.to_str())
        == Some("snapshots");
    if !is_hugging_face_snapshot {
        return (leaf, None);
    }

    let model = path
        .parent()
        .and_then(|value| value.parent())
        .and_then(|value| value.file_name())
        .and_then(|value| value.to_str())
        .map(|value| value.trim_start_matches("models--").replace("--", "/"));
    (model, leaf)
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
            diagnostics: AsrRuntimeDiagnostics::default(),
        })
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
    Qwen,
    Whisper,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SenseVoice => "sensevoice",
            Self::Qwen => "qwen",
            Self::Whisper => "whisper",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sensevoice" | "sensevoice_sherpa" | "sherpa" => Some(Self::SenseVoice),
            "qwen" | "qwen3_asr" | "local_qwen" => Some(Self::Qwen),
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

pub fn qwen_status() -> EngineStatus {
    let dir = default_qwen_dir();
    EngineStatus {
        kind: EngineKind::Qwen,
        ready: qwen_ready(&dir),
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

    #[test]
    fn qwen_engine_kind_accepts_product_provider_names() {
        assert_eq!(EngineKind::parse("qwen"), Some(EngineKind::Qwen));
        assert_eq!(EngineKind::parse("qwen3_asr"), Some(EngineKind::Qwen));
        assert_eq!(EngineKind::parse("local_qwen"), Some(EngineKind::Qwen));
        assert_eq!(EngineKind::Qwen.as_str(), "qwen");
    }

    #[test]
    fn hugging_face_snapshot_identity_omits_local_path() {
        let path =
            Path::new("/tmp/cache/models--mlx-community--Qwen3-ASR-0.6B-8bit/snapshots/abcdef123");
        let (model, revision) = model_identity_from_path(path);

        assert_eq!(model.as_deref(), Some("mlx-community/Qwen3-ASR-0.6B-8bit"));
        assert_eq!(revision.as_deref(), Some("abcdef123"));
        assert!(!model.unwrap().contains("/tmp/cache"));
    }

    #[test]
    fn direct_model_identity_uses_only_directory_name() {
        let (model, revision) =
            model_identity_from_path(Path::new("/private/models/sensevoice-int8"));

        assert_eq!(model.as_deref(), Some("sensevoice-int8"));
        assert_eq!(revision, None);
    }

    #[test]
    fn shared_model_contract_matches_cluster_v1() {
        let bytes = include_bytes!("../../../docs/SHARED_MODELS_CONTRACT.md");
        assert_eq!(fnv1a64(bytes), 0xc877_89f4_de20_5e71);
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }
}
