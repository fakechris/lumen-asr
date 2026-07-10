//! Whisper offline ASR via sherpa-onnx.

use crate::paths::{whisper_decoder_path, whisper_encoder_path, whisper_tokens_path};
use crate::{AsrEngine, AsrError, AsrRequest, AsrResult};
use async_trait::async_trait;
use lumen_core::AsrEngineId;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "sherpa")]
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineWhisperModelConfig};

struct WhisperInner {
    model_dir: PathBuf,
    language: String,
    #[cfg(feature = "sherpa")]
    recognizer: Mutex<Option<OfflineRecognizer>>,
}

pub struct WhisperAsr {
    inner: Arc<WhisperInner>,
}

impl Clone for WhisperAsr {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl WhisperAsr {
    pub fn new(model_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(WhisperInner {
                model_dir: model_dir.into(),
                language: "en".into(),
                #[cfg(feature = "sherpa")]
                recognizer: Mutex::new(None),
            }),
        }
    }

    pub fn with_language(self, language: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(WhisperInner {
                model_dir: self.inner.model_dir.clone(),
                language: language.into(),
                #[cfg(feature = "sherpa")]
                recognizer: Mutex::new(None),
            }),
        }
    }

    pub fn model_dir(&self) -> &Path {
        &self.inner.model_dir
    }

    pub fn is_ready(&self) -> bool {
        whisper_encoder_path(&self.inner.model_dir).is_some()
            && whisper_decoder_path(&self.inner.model_dir).is_some()
            && whisper_tokens_path(&self.inner.model_dir).is_some()
    }
}

#[cfg(feature = "sherpa")]
impl WhisperInner {
    fn ensure_recognizer(&self) -> Result<(), AsrError> {
        let mut guard = self.recognizer.lock();
        if guard.is_some() {
            return Ok(());
        }
        let encoder = whisper_encoder_path(&self.model_dir).ok_or_else(|| {
            AsrError::NotConfigured(format!(
                "Whisper encoder not found under {}",
                self.model_dir.display()
            ))
        })?;
        let decoder = whisper_decoder_path(&self.model_dir).ok_or_else(|| {
            AsrError::NotConfigured(format!(
                "Whisper decoder not found under {}",
                self.model_dir.display()
            ))
        })?;
        let tokens = whisper_tokens_path(&self.model_dir).ok_or_else(|| {
            AsrError::NotConfigured(format!(
                "Whisper tokens not found under {}",
                self.model_dir.display()
            ))
        })?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.whisper = OfflineWhisperModelConfig {
            encoder: Some(encoder.display().to_string()),
            decoder: Some(decoder.display().to_string()),
            language: Some(self.language.clone()),
            task: Some("transcribe".into()),
            tail_paddings: 0,
            enable_token_timestamps: false,
            enable_segment_timestamps: false,
        };
        config.model_config.tokens = Some(tokens.display().to_string());
        config.model_config.num_threads = 2;
        config.model_config.provider = Some("cpu".into());

        tracing::info!(encoder = %encoder.display(), "creating Whisper OfflineRecognizer");
        let rec = OfflineRecognizer::create(&config).ok_or_else(|| {
            AsrError::Inference(format!(
                "failed to create Whisper recognizer (check model paths under {})",
                self.model_dir.display()
            ))
        })?;
        *guard = Some(rec);
        Ok(())
    }

    fn decode_sync(&self, samples: &[f32], sample_rate: u32) -> Result<String, AsrError> {
        self.ensure_recognizer()?;
        let guard = self.recognizer.lock();
        let recognizer = guard
            .as_ref()
            .ok_or_else(|| AsrError::NotConfigured("whisper recognizer missing".into()))?;

        let stream = recognizer.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|r| r.text)
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(text)
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

        #[cfg(not(feature = "sherpa"))]
        {
            let _ = req;
            return Err(AsrError::NotConfigured(
                "build with feature `sherpa`".into(),
            ));
        }

        #[cfg(feature = "sherpa")]
        {
            let inner = Arc::clone(&self.inner);
            let samples = req.samples;
            let sr = req.sample_rate;
            let text = tokio::task::spawn_blocking(move || inner.decode_sync(&samples, sr))
                .await
                .map_err(|e| AsrError::Inference(e.to_string()))??;

            Ok(AsrResult {
                text,
                engine: AsrEngineId::Whisper,
                language: Some(self.inner.language.clone()),
            })
        }
    }
}
