//! Product-side ASR layer: microphone capture plus re-exports of the shared
//! Lumen cluster crates.
//!
//! The engines (SenseVoice / Whisper via sherpa-onnx, Qwen3-ASR MLX worker,
//! OpenAI-compatible HTTP) live in `lumen-asr-engine`. Product-specific engines
//! that are not yet in the shared suite (e.g. mlx-whisper Metal) live here.
//! Model path resolution / readiness / install locking / downloads live in
//! `lumen-models`.

pub use lumen_asr_engine::{
    MlxWhisperAsr, MlxWhisperConfig, DEFAULT_MLX_WHISPER_MODEL, MLX_WHISPER_WORKER,
};

// Audio capture / VAD / WAV editing / dual-track recording now live in the
// shared lumen-suite `lumen-audio` crate; re-exported so every existing
// `lumen_asr::` call site is unchanged.
pub use lumen_audio::{
    copy_pcm16_wav_range, live_tap_channel, repair_wav_header, trim_trailing_silence, AudioCapture,
    AudioDeviceInfo, AudioError, CaptureResult, LiveAudioPacket, LiveTapSender, MeetingRecorder,
    MeetingRecorderError, RecordingSummary, RepairedWav, SampleSink, SilenceAutoStop, SileroVad,
    SileroVadError, SystemTrackRecorder, SystemTrackSender, TimestampAutoStop, VadAction,
    WavRangeError, WavRangeSummary, WavSink, LIVE_TAP_CAPACITY,
};

// Engine layer (trait, engines, diagnostics, pure audio helpers).
pub use lumen_asr_engine::{
    model_identity_from_path, paraformer_offline_ready, prepare_for_asr, probe_status,
    resample_linear, AsrEngine, AsrEngineId, AsrError, AsrRequest, AsrResult,
    AsrRuntimeDiagnostics, AsrTokenEvidence, EngineKind, EngineStatus, OpenAiAudioAsr,
    OpenAiAudioConfig, ParaformerAsr, ParaformerOfflineModelPaths, QwenAsr, QwenAsrConfig,
    QwenDecodeMode, QwenRuntimeMetrics, QwenShadowCandidate, QwenShadowDiagnostics,
    QwenShadowRequest, QwenShadowScore, QwenShadowSpan, QwenShadowStatus, QwenShadowTerm,
    SenseVoiceSherpaAsr, StreamingAsrEngine, StreamingParaformerAsr, StreamingRecognizer,
    StreamingResult, StreamingStream, StubAsr, WhisperAsr, WordTiming,
};

// Model layer (path resolution, readiness probes, install lock, download).
#[allow(deprecated)]
pub use lumen_models::app_models_dir;
pub use lumen_models::{
    default_paraformer_offline_dir, default_paraformer_offline_dir_with_root,
    default_paraformer_streaming_dir, default_paraformer_streaming_dir_with_root, default_qwen_dir,
    default_sensevoice_dir, default_sensevoice_dir_with_root, default_silero_vad_dir,
    default_whisper_dir, default_whisper_dir_with_root, download_paraformer_offline_package,
    download_paraformer_streaming_package, download_sensevoice_package,
    download_silero_vad_package, legacy_model_roots, lumen_models_dir,
    lumen_models_dir_with_override, paraformer_streaming_ready, qwen_ready, resolve_qwen_asr_dir,
    resolve_sensevoice_dir, scan_model_candidates, scan_model_candidates_with_root,
    sensevoice_ready, shared_sensevoice_dir, shared_silero_vad_dir, shared_whisper_dir,
    silero_vad_model_path, silero_vad_ready, user_home_dir, whisper_ready, DownloadError,
    DownloadProgress, ModelCandidate, ModelInstallLock, ENV_LUMEN_MODELS_DIR,
    PARAFORMER_OFFLINE_ARCHIVE_URL, PARAFORMER_STREAMING_ARCHIVE_URL, SENSEVOICE_ARCHIVE_NAME,
    SENSEVOICE_ARCHIVE_URL,
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

    /// Re-assert the product-side contract pin. Normalize checkout line endings
    /// so the same source has one fingerprint on Windows and macOS.
    #[test]
    fn shared_model_contract_matches_cluster_v1_1() {
        let bytes = include_bytes!("../../../docs/SHARED_MODELS_CONTRACT.md");
        let normalized = bytes
            .split(|byte| *byte == b'\r')
            .flat_map(|segment| segment.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(fnv1a64(&normalized), 0x9481_7905_7ee6_d582);
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }
}
