//! OpenAI-compatible batch audio transcription (Whisper / gpt-4o-transcribe style).

use crate::{AsrEngine, AsrError, AsrRequest, AsrResult};
use async_trait::async_trait;
use lumen_core::AsrEngineId;
use reqwest::multipart::{Form, Part};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAiAudioConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub language: Option<String>,
}

pub struct OpenAiAudioAsr {
    client: reqwest::Client,
    config: OpenAiAudioConfig,
}

impl OpenAiAudioAsr {
    pub fn new(config: OpenAiAudioConfig) -> Result<Self, AsrError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AsrError::Inference(e.to_string()))?;
        Ok(Self { client, config })
    }
}

#[async_trait]
impl AsrEngine for OpenAiAudioAsr {
    fn id(&self) -> AsrEngineId {
        AsrEngineId::Other
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        let wav = samples_to_wav_mono_i16(&req.samples, req.sample_rate);
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/audio/transcriptions");

        let part = Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| AsrError::Inference(e.to_string()))?;
        let mut form = Form::new()
            .part("file", part)
            .text("model", self.config.model.clone());
        if let Some(lang) = &self.config.language {
            if !lang.is_empty() {
                form = form.text("language", lang.clone());
            }
        }

        let mut builder = self.client.post(&url).multipart(form);
        if !self.config.api_key.is_empty() {
            builder = builder.bearer_auth(&self.config.api_key);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| AsrError::Inference(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AsrError::Inference(format!("{status}: {body}")));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AsrError::Inference(e.to_string()))?;
        let text = v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(AsrResult {
            text,
            engine: AsrEngineId::Other,
            language: self.config.language.clone(),
        })
    }
}

fn samples_to_wav_mono_i16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let sr = if sample_rate == 0 { 16000 } else { sample_rate };
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sr.to_le_bytes());
    out.extend_from_slice(&(sr * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}
