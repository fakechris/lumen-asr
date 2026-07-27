//! Product-side ASR layer: microphone capture plus re-exports of the shared
//! Lumen cluster crates.
//!
//! The engines (SenseVoice / Whisper via sherpa-onnx, Qwen3-ASR MLX worker,
//! OpenAI-compatible HTTP) live in `lumen-asr-engine`, and model path
//! resolution / readiness / install locking / downloads live in
//! `lumen-models` (both from the `lumen-suite` repository). This crate only
//! keeps what is product-specific: cpal microphone capture and thin status
//! helpers that combine the two shared layers.

mod audio;

pub use audio::{AudioCapture, AudioDeviceInfo, AudioError, CaptureResult};

// Engine layer (trait, engines, diagnostics, pure audio helpers).
pub use lumen_asr_engine::{
    model_identity_from_path, prepare_for_asr, probe_status, resample_linear, AsrEngine,
    AsrEngineId, AsrError, AsrRequest, AsrResult, AsrRuntimeDiagnostics, AsrTokenEvidence,
    EngineKind, EngineStatus, OpenAiAudioAsr, OpenAiAudioConfig, QwenAsr, QwenAsrConfig,
    QwenDecodeMode, QwenRuntimeMetrics, QwenShadowCandidate, QwenShadowDiagnostics,
    QwenShadowRequest, QwenShadowScore, QwenShadowSpan, QwenShadowStatus, QwenShadowTerm,
    SenseVoiceSherpaAsr, StubAsr, WhisperAsr,
};

// Model layer (path resolution, readiness probes, install lock, download).
#[allow(deprecated)]
pub use lumen_models::app_models_dir;
pub use lumen_models::{
    default_qwen_dir, default_sensevoice_dir, default_sensevoice_dir_with_root,
    default_whisper_dir, default_whisper_dir_with_root, download_sensevoice_package,
    legacy_model_roots, lumen_models_dir, lumen_models_dir_with_override, qwen_ready,
    scan_model_candidates, scan_model_candidates_with_root, sensevoice_ready,
    shared_sensevoice_dir, shared_whisper_dir, user_home_dir, whisper_ready, DownloadError,
    DownloadProgress, ModelCandidate, ModelInstallLock, ENV_LUMEN_MODELS_DIR,
    SENSEVOICE_ARCHIVE_NAME, SENSEVOICE_ARCHIVE_URL,
};

/// Status of the default SenseVoice install (default dir + readiness probe).
pub fn sensevoice_status() -> EngineStatus {
    let dir = default_sensevoice_dir();
    probe_status(EngineKind::SenseVoice, Some(&dir))
}

/// Status of the default Whisper install.
pub fn whisper_status() -> EngineStatus {
    let dir = default_whisper_dir();
    probe_status(EngineKind::Whisper, Some(&dir))
}

/// Status of the default Qwen3-ASR MLX snapshot.
pub fn qwen_status() -> EngineStatus {
    let dir = default_qwen_dir();
    probe_status(EngineKind::Qwen, Some(&dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_resamples() {
        let cap = CaptureResult {
            samples: vec![0.0, 1.0, 0.0, -1.0],
            sample_rate: 32000,
        };
        let out = prepare_for_asr(&cap.samples, cap.sample_rate);
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
    fn default_status_helpers_report_typed_kinds() {
        assert_eq!(sensevoice_status().kind, EngineKind::SenseVoice);
        assert_eq!(whisper_status().kind, EngineKind::Whisper);
        assert_eq!(qwen_status().kind, EngineKind::Qwen);
    }

    /// Re-assert the cluster model contract pin (the canonical hash test lives
    /// in lumen-suite; this keeps the local doc copy byte-identical).
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
