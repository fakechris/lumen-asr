//! SenseVoice offline ASR via sherpa-onnx.

use crate::paths::{sensevoice_model_path, sensevoice_tokens_path};
use crate::{AsrEngine, AsrError, AsrRequest, AsrResult};
use async_trait::async_trait;
use lumen_core::AsrEngineId;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "sherpa")]
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};

struct SenseVoiceInner {
    model_dir: PathBuf,
    language: String,
    #[cfg(feature = "sherpa")]
    recognizer: Mutex<Option<OfflineRecognizer>>,
}

pub struct SenseVoiceSherpaAsr {
    inner: Arc<SenseVoiceInner>,
}

impl Clone for SenseVoiceSherpaAsr {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SenseVoiceSherpaAsr {
    pub fn new(model_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(SenseVoiceInner {
                model_dir: model_dir.into(),
                language: "auto".into(),
                #[cfg(feature = "sherpa")]
                recognizer: Mutex::new(None),
            }),
        }
    }

    pub fn with_language(self, language: impl Into<String>) -> Self {
        // Replace language without dropping warm recognizer of the old Arc.
        Self {
            inner: Arc::new(SenseVoiceInner {
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
        sensevoice_model_path(&self.inner.model_dir).is_some()
            && sensevoice_tokens_path(&self.inner.model_dir).is_some()
    }
}

#[cfg(feature = "sherpa")]
impl SenseVoiceInner {
    fn ensure_recognizer(&self) -> Result<(), AsrError> {
        let mut guard = self.recognizer.lock();
        if guard.is_some() {
            return Ok(());
        }
        let model = sensevoice_model_path(&self.model_dir).ok_or_else(|| {
            AsrError::NotConfigured(format!(
                "SenseVoice model not found under {}",
                self.model_dir.display()
            ))
        })?;
        let tokens = sensevoice_tokens_path(&self.model_dir).ok_or_else(|| {
            AsrError::NotConfigured(format!(
                "tokens.txt not found under {}",
                self.model_dir.display()
            ))
        })?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(model.display().to_string()),
            language: Some(self.language.clone()),
            use_itn: true,
        };
        config.model_config.tokens = Some(tokens.display().to_string());
        config.model_config.num_threads = 2;
        config.model_config.provider = Some("cpu".into());

        tracing::info!(model = %model.display(), "creating SenseVoice OfflineRecognizer");
        let rec = OfflineRecognizer::create(&config).ok_or_else(|| {
            AsrError::Inference(format!(
                "failed to create SenseVoice recognizer (check model paths under {})",
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
            .ok_or_else(|| AsrError::NotConfigured("recognizer missing".into()))?;

        let stream = recognizer.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|r| r.text)
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(cleanup_sensevoice_text(&text))
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
                engine: AsrEngineId::SenseVoiceSherpa,
                language: Some(self.inner.language.clone()),
            })
        }
    }
}

fn cleanup_sensevoice_text(text: &str) -> String {
    let mut s = text.to_string();
    for tag in [
        "<|zh|>",
        "<|en|>",
        "<|yue|>",
        "<|ja|>",
        "<|ko|>",
        "<|nospeech|>",
        "<|EMO_UNKNOWN|>",
        "<|Event_UNK|>",
        "<|woitn|>",
        "<|withitn|>",
    ] {
        s = s.replace(tag, "");
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_tags() {
        assert_eq!(cleanup_sensevoice_text("<|zh|>你好"), "你好");
    }
}
