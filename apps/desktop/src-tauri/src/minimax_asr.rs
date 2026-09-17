//! MiniMax batch ASR for dictation: one synchronous multipart POST to
//! `/v1/speech_to_text`; the transcript comes back in the same response.
//!
//! Verified against the official OpenAPI spec (platform.minimaxi.com,
//! api-reference/speech/speech-to-text):
//!
//! - Endpoint: `POST https://api.minimaxi.com/v1/speech_to_text`.
//! - Auth: `Authorization: Bearer <API key>`（platform.minimaxi.com 接口密钥）.
//! - Multipart form: `model` (= `asr-1.0`) + `file`（建议 16 kHz 单声道 WAV）.
//! - `language` travels as a request *header* (BCP-47), not a form field;
//!   absent/empty enables 混合语言识别.
//! - Errors are OpenAI-style envelopes:
//!   `{"type":"error","error":{"type","message","http_code"}}`.
//!
//! Provider caps: ≤500 s and ≤50 MB per request (longer audio is rejected
//! with 400, never truncated). We send 16 kHz mono PCM WAV, so the local
//! 16 MB cap equals exactly 500 s of audio.
//!
//! Out of scope: `stream=true` SSE（实测为整段识别完成后一次性推送增量，
//! 对听写无收益）、`verbose_json`/srt/vtt 输出与说话人分离（听写只需全文）.

use async_trait::async_trait;
use lumen_asr::{AsrEngine, AsrEngineId, AsrError, AsrRequest, AsrResult};
use std::time::Duration;

pub const DEFAULT_SPEECH_TO_TEXT_URL: &str = "https://api.minimaxi.com/v1/speech_to_text";
pub const DEFAULT_MODEL: &str = "asr-1.0";
/// Transcript label written into session records.
pub const ENGINE_LABEL: &str = "minimax";

/// 500 s × 16 kHz × 2 B: the provider's 500-second cap expressed as a WAV
/// byte cap at our capture rate (the size cap of 50 MB is never the binding
/// constraint at 16 kHz mono).
pub const MAX_AUDIO_BYTES: usize = 16_000_000;

#[derive(Debug, Clone)]
pub struct MinimaxAsrConfig {
    /// Full endpoint URL (`/v1/speech_to_text` is a complete path, not a base).
    pub base_url: String,
    pub api_key: String,
    /// Provider model version; only `asr-1.0` exists today.
    pub model: String,
    /// Optional BCP-47 hint sent as the `language` header; empty = 混合识别.
    pub language: String,
    pub timeout: Duration,
    pub max_audio_bytes: usize,
}

impl Default for MinimaxAsrConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_SPEECH_TO_TEXT_URL.into(),
            api_key: String::new(),
            model: DEFAULT_MODEL.into(),
            language: String::new(),
            timeout: Duration::from_secs(120),
            max_audio_bytes: MAX_AUDIO_BYTES,
        }
    }
}

pub struct MinimaxAsr {
    client: reqwest::Client,
    config: MinimaxAsrConfig,
}

impl MinimaxAsr {
    pub fn new(config: MinimaxAsrConfig) -> Result<Self, AsrError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AsrError::Inference(format!("http client: {e}")))?;
        Ok(Self { client, config })
    }

    fn model(&self) -> &str {
        let model = self.config.model.trim();
        if model.is_empty() {
            DEFAULT_MODEL
        } else {
            model
        }
    }
}

/// `language` is a request header per the spec; empty/absent = 混合语言识别.
fn language_header(language: &str) -> Option<(&'static str, String)> {
    let language = language.trim();
    (!language.is_empty()).then(|| ("language", language.to_string()))
}

/// Multipart form: `model` + `file`（the only required fields; response_format
/// stays at its `json` default, so the body is `{text, duration}`）.
fn build_form(model: &str, wav: Vec<u8>) -> Result<reqwest::multipart::Form, AsrError> {
    let file = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| AsrError::Inference(format!("multipart mime: {e}")))?;
    Ok(reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .part("file", file))
}

/// Interpret one provider response. Errors may arrive on any HTTP status as
/// an OpenAI-style envelope; `http_code` inside is authoritative when present.
fn parse_response(http_status: reqwest::StatusCode, body: &str) -> Result<String, AsrError> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(err) = value.get("error") {
        let code = match err.get("http_code") {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => http_status.as_str().to_string(),
        };
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let code = if code.is_empty() {
            http_status.as_str().to_string()
        } else {
            code
        };
        return Err(AsrError::Inference(api_error_message(&code, &message)));
    }
    if !http_status.is_success() {
        return Err(AsrError::Inference(format!(
            "provider rejected request with status {http_status}: {body}"
        )));
    }
    Ok(value
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string())
}

/// User-facing Chinese message for a documented provider HTTP code.
fn api_error_message(code: &str, message: &str) -> String {
    match code {
        "400" => format!("MiniMax ASR 拒绝了该音频：超过 500 秒或格式不支持（{message}）"),
        "401" => "MiniMax ASR 鉴权失败：API Key 无效或未开通语音识别".to_string(),
        "402" => "MiniMax ASR 余额不足，请充值后重试".to_string(),
        "413" => "MiniMax ASR 请求体超过 50 MB 上限".to_string(),
        "422" => "MiniMax ASR 拒绝处理该音频：内容未通过安全审核".to_string(),
        "429" => "MiniMax ASR 触发限流，请稍后重试".to_string(),
        "500" => "MiniMax ASR 服务端错误，请稍后重试".to_string(),
        _ => format!("MiniMax ASR 识别失败（HTTP {code}）：{message}"),
    }
}

#[async_trait]
impl AsrEngine for MinimaxAsr {
    fn id(&self) -> AsrEngineId {
        // The shared EngineKind set has no MiniMax variant; the product label
        // travels in `engine_label` instead.
        AsrEngineId::Other
    }

    fn is_supported(&self) -> bool {
        !self.config.api_key.trim().is_empty() && !self.config.base_url.trim().is_empty()
    }

    fn max_audio_bytes(&self) -> Option<usize> {
        Some(self.config.max_audio_bytes)
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        if !self.is_supported() {
            return Err(AsrError::Unsupported(
                "minimax api key / endpoint not configured".into(),
            ));
        }
        let wav = lumen_asr::pcm_to_wav_bytes(&req.samples, req.sample_rate);
        if wav.len() > self.config.max_audio_bytes {
            return Err(AsrError::AudioTooLarge {
                actual: wav.len(),
                max: self.config.max_audio_bytes,
            });
        }
        let form = build_form(self.model(), wav)?;
        let mut builder = self
            .client
            .post(self.config.base_url.trim())
            .bearer_auth(self.config.api_key.trim())
            .multipart(form);
        if let Some((name, value)) = language_header(&self.config.language) {
            builder = builder.header(name, value);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| AsrError::Inference(format!("http: {e}")))?;
        let http_status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let text = parse_response(http_status, &body)?;

        let mut result = AsrResult::new(text, AsrEngineId::Other);
        result.engine_label = ENGINE_LABEL.into();
        result.diagnostics.model = Some(self.model().to_string());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(api_key: &str) -> MinimaxAsrConfig {
        MinimaxAsrConfig {
            api_key: api_key.into(),
            ..MinimaxAsrConfig::default()
        }
    }

    #[test]
    fn default_config_targets_the_documented_endpoint() {
        let cfg = MinimaxAsrConfig::default();
        assert_eq!(cfg.base_url, "https://api.minimaxi.com/v1/speech_to_text");
        assert_eq!(cfg.model, "asr-1.0");
        // 500 s at 16 kHz mono PCM16 — exactly the provider duration cap.
        assert_eq!(cfg.max_audio_bytes, 16_000_000);
    }

    #[test]
    fn language_header_is_only_sent_when_set() {
        assert_eq!(language_header("zh"), Some(("language", "zh".to_string())));
        assert_eq!(
            language_header("  en-US  "),
            Some(("language", "en-US".to_string()))
        );
        assert_eq!(language_header(""), None);
        assert_eq!(language_header("   "), None);
    }

    #[test]
    fn empty_model_falls_back_to_asr_1_0() {
        let engine = MinimaxAsr::new(MinimaxAsrConfig {
            model: "  ".into(),
            ..config("key")
        })
        .unwrap();
        assert_eq!(engine.model(), "asr-1.0");
    }

    #[test]
    fn build_form_succeeds_for_the_documented_shape() {
        // reqwest keeps Form/Part fields opaque; this guards the construction
        // path (model text + named WAV part) end to end.
        let form = build_form("asr-1.0", b"RIFF".to_vec()).unwrap();
        let _ = form.boundary();
    }

    #[test]
    fn success_response_yields_transcript_text() {
        let body = r#"{"text":"你好，这是一段中文语音识别测试。","duration":5.247}"#;
        let text = parse_response(reqwest::StatusCode::OK, body).unwrap();
        assert_eq!(text, "你好，这是一段中文语音识别测试。");
    }

    #[test]
    fn text_is_trimmed_and_missing_text_yields_empty_string() {
        assert_eq!(
            parse_response(reqwest::StatusCode::OK, r#"{"text":"  嗨  "}"#).unwrap(),
            "嗨"
        );
        assert_eq!(parse_response(reqwest::StatusCode::OK, "{}").unwrap(), "");
        assert_eq!(
            parse_response(reqwest::StatusCode::OK, "not json at all").unwrap(),
            ""
        );
    }

    #[test]
    fn documented_error_codes_map_to_chinese_messages() {
        let envelope = |code: &str, message: &str| {
            format!(
                r#"{{"type":"error","error":{{"type":"error","message":"{message}","http_code":"{code}"}}}}"#
            )
        };
        let cases = [
            ("400", "音频超过 500 秒", "超过 500 秒"),
            ("401", "invalid api key", "鉴权失败"),
            ("402", "insufficient balance (1008)", "余额不足"),
            ("413", "request body too large", "50 MB"),
            ("422", "sensitive content (1026)", "安全审核"),
            ("429", "rate limit, please retry later (1002)", "限流"),
            ("500", "internal error (1000)", "服务端错误"),
            ("418", "teapot", "418"),
        ];
        for (code, message, expected) in cases {
            let err = parse_response(
                reqwest::StatusCode::from_u16(code.parse().unwrap()).unwrap(),
                &envelope(code, message),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("MiniMax"), "{err}");
            assert!(err.contains(expected), "code {code}: {err}");
        }
    }

    #[test]
    fn numeric_http_code_and_missing_code_are_tolerated() {
        let err = parse_response(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"rate limit","http_code":429}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("限流"), "{err}");

        let err = parse_response(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"bad key"}}"#,
        )
        .unwrap_err()
        .to_string();
        // Missing http_code falls back to the HTTP status: only 401 maps to
        // the 鉴权失败 copy.
        assert!(err.contains("鉴权失败"), "{err}");
    }

    #[test]
    fn non_json_error_body_falls_back_to_http_status() {
        let err = parse_response(reqwest::StatusCode::UNAUTHORIZED, "unauthorized")
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[tokio::test]
    async fn engine_rejects_empty_audio_and_missing_credentials() {
        let engine = MinimaxAsr::new(config("key")).unwrap();
        let err = engine
            .transcribe(AsrRequest::new(vec![], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::EmptyAudio));

        let engine = MinimaxAsr::new(config("")).unwrap();
        assert!(!engine.is_supported());
        let err = engine
            .transcribe(AsrRequest::new(vec![0.0; 1600], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::Unsupported(_)));
    }

    #[tokio::test]
    async fn engine_enforces_the_audio_size_cap() {
        let mut cfg = config("key");
        cfg.max_audio_bytes = 128;
        let engine = MinimaxAsr::new(cfg).unwrap();
        let err = engine
            .transcribe(AsrRequest::new(vec![0.0; 1600], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::AudioTooLarge { .. }));
    }

    #[test]
    fn engine_identity_uses_the_product_label() {
        let engine = MinimaxAsr::new(config("key")).unwrap();
        assert_eq!(engine.id(), AsrEngineId::Other);
        assert_eq!(engine.max_audio_bytes(), Some(16_000_000));
    }

    #[test]
    fn wav_payload_is_riff() {
        let wav = lumen_asr::pcm_to_wav_bytes(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }
}
