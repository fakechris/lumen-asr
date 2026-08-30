//! Volcengine（火山引擎 / 豆包语音）batch ASR for dictation: the synchronous
//! 录音文件识别极速版 HTTP API. One POST carries the whole utterance as base64
//! WAV inside a JSON envelope; the transcript comes back in the same response
//! (no submit/query polling).
//!
//! Verified against the official docs:
//!   https://www.volcengine.com/docs/6561/1631584
//!
//! - Endpoint: `POST https://openspeech.bytedance.com/api/v3/auc/bigmodel/recognize/flash`
//! - Auth headers: `X-Api-App-Key` + `X-Api-Access-Key` (旧版控制台),
//!   or `X-Api-Key` alone (新版控制台 APP Key).
//! - `X-Api-Resource-Id`: fixed `volc.bigasr.auc_turbo`.
//! - Body: `{ "user": {"uid": <app key>}, "audio": {"data": <base64 wav>},
//!   "request": {"model_name": "bigmodel"} }`.
//! - Result code travels in the `X-Api-Status-Code` response header
//!   (`20000000` = success); the transcript is `result.text` in the body.
//!
//! Out of scope: the binary WebSocket streaming protocol and the async
//! submit/query file API (meetings stay on the local streaming engine).

use async_trait::async_trait;
use lumen_asr::{AsrEngine, AsrEngineId, AsrError, AsrRequest, AsrResult};
use std::time::Duration;

pub const DEFAULT_FLASH_URL: &str =
    "https://openspeech.bytedance.com/api/v3/auc/bigmodel/recognize/flash";
pub const RESOURCE_ID: &str = "volc.bigasr.auc_turbo";
/// Transcript label written into session records.
pub const ENGINE_LABEL: &str = "volcengine";

/// Result header carrying the provider status code (`20000000` = success).
const STATUS_HEADER: &str = "x-api-status-code";
const MESSAGE_HEADER: &str = "x-api-message";
const SUCCESS_CODE: &str = "20000000";

#[derive(Debug, Clone)]
pub struct VolcengineAsrConfig {
    /// Full endpoint URL (the flash API is a complete path, not a base).
    pub base_url: String,
    /// 旧版控制台 App ID (`X-Api-App-Key`). Empty switches auth to the
    /// 新版控制台 single `X-Api-Key` header (access_token holds the APP Key).
    pub app_id: String,
    /// Access Token (旧版控制台) or APP Key (新版控制台).
    pub access_token: String,
    pub timeout: Duration,
    /// Request cap; the flash API itself allows 100 MB / 2 h, but dictation
    /// utterances are seconds long.
    pub max_audio_bytes: usize,
}

impl Default for VolcengineAsrConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_FLASH_URL.into(),
            app_id: String::new(),
            access_token: String::new(),
            timeout: Duration::from_secs(120),
            max_audio_bytes: 8 * 1024 * 1024,
        }
    }
}

pub struct VolcengineAsr {
    client: reqwest::Client,
    config: VolcengineAsrConfig,
}

impl VolcengineAsr {
    pub fn new(config: VolcengineAsrConfig) -> Result<Self, AsrError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AsrError::Inference(format!("http client: {e}")))?;
        Ok(Self { client, config })
    }

    /// uid shown to the provider: the App ID when set, otherwise the APP Key.
    fn uid(&self) -> &str {
        if self.config.app_id.trim().is_empty() {
            self.config.access_token.trim()
        } else {
            self.config.app_id.trim()
        }
    }

    /// Auth headers for the documented console generations. Never logs values.
    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("x-api-resource-id", RESOURCE_ID.to_string()),
            ("x-api-request-id", uuid::Uuid::new_v4().to_string()),
            ("x-api-sequence", "-1".to_string()),
        ];
        if self.config.app_id.trim().is_empty() {
            // 新版控制台：单个 APP Key。
            headers.push(("x-api-key", self.config.access_token.trim().to_string()));
        } else {
            headers.push(("x-api-app-key", self.config.app_id.trim().to_string()));
            headers.push((
                "x-api-access-key",
                self.config.access_token.trim().to_string(),
            ));
        }
        headers
    }
}

/// JSON envelope: `audio.data` carries the base64 WAV (`audio.url` is the
/// alternative we never use for dictation — the audio is already local).
fn build_request_body(uid: &str, audio_base64: &str) -> serde_json::Value {
    serde_json::json!({
        "user": { "uid": uid },
        "audio": { "data": audio_base64 },
        "request": { "model_name": "bigmodel" },
    })
}

/// Interpret one provider response. `status_code`/`status_message` come from
/// the `X-Api-Status-Code` / `X-Api-Message` headers; either may be absent
/// when the request failed before reaching the recognition service.
fn parse_response(
    http_status: reqwest::StatusCode,
    status_code: Option<&str>,
    status_message: Option<&str>,
    body: &str,
) -> Result<String, AsrError> {
    match status_code {
        Some(SUCCESS_CODE) => {
            let value: serde_json::Value = serde_json::from_str(body)
                .map_err(|_| AsrError::Inference("malformed provider response".into()))?;
            Ok(value
                .get("result")
                .and_then(|r| r.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string())
        }
        Some(code) => Err(AsrError::Inference(api_error_message(
            code,
            status_message.unwrap_or(""),
        ))),
        None => Err(AsrError::Inference(format!(
            "provider rejected request with status {http_status}: {body}"
        ))),
    }
}

/// User-facing Chinese message for a documented provider status code.
fn api_error_message(code: &str, message: &str) -> String {
    match code {
        "20000003" => "火山引擎 ASR 判定为静音音频：未检测到有效语音".to_string(),
        "45000001" => format!("火山引擎 ASR 请求参数无效：{message}"),
        "45000002" => "火山引擎 ASR 收到空音频".to_string(),
        "45000151" => "火山引擎 ASR 不支持该音频格式".to_string(),
        "55000031" => "火山引擎 ASR 服务繁忙，请稍后重试".to_string(),
        _ => format!("火山引擎 ASR 识别失败（错误码 {code}）：{message}"),
    }
}

/// Standard-alphabet base64 with padding (the workspace carries no base64
/// crate; the encoder is small and fully covered by tests below).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[async_trait]
impl AsrEngine for VolcengineAsr {
    fn id(&self) -> AsrEngineId {
        // The shared EngineKind set has no Volcengine variant; the product
        // label travels in `engine_label` instead.
        AsrEngineId::Other
    }

    fn is_supported(&self) -> bool {
        !self.config.access_token.trim().is_empty() && !self.config.base_url.trim().is_empty()
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
                "volcengine access token / endpoint not configured".into(),
            ));
        }
        let wav = lumen_asr::pcm_to_wav_bytes(&req.samples, req.sample_rate);
        if wav.len() > self.config.max_audio_bytes {
            return Err(AsrError::AudioTooLarge {
                actual: wav.len(),
                max: self.config.max_audio_bytes,
            });
        }
        let body = build_request_body(self.uid(), &base64_encode(&wav));

        let mut builder = self.client.post(self.config.base_url.trim()).json(&body);
        for (name, value) in self.auth_headers() {
            builder = builder.header(name, value);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| AsrError::Inference(format!("http: {e}")))?;
        let http_status = resp.status();
        let status_code = resp
            .headers()
            .get(STATUS_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status_message = resp
            .headers()
            .get(MESSAGE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.text().await.unwrap_or_default();
        let text = parse_response(
            http_status,
            status_code.as_deref(),
            status_message.as_deref(),
            &body,
        )?;

        let mut result = AsrResult::new(text, AsrEngineId::Other);
        result.engine_label = ENGINE_LABEL.into();
        result.diagnostics.model = Some("bigmodel".into());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(app_id: &str, access_token: &str) -> VolcengineAsrConfig {
        VolcengineAsrConfig {
            app_id: app_id.into(),
            access_token: access_token.into(),
            ..VolcengineAsrConfig::default()
        }
    }

    fn header<'a>(headers: &'a [(&'a str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn base64_matches_standard_alphabet() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn request_body_matches_documented_envelope() {
        let body = build_request_body("123456789", "QUJD");
        assert_eq!(body["user"]["uid"], "123456789");
        assert_eq!(body["audio"]["data"], "QUJD");
        // audio.url and audio.data are mutually exclusive; we only send data.
        assert!(body["audio"].get("url").is_none());
        assert_eq!(body["request"]["model_name"], "bigmodel");
    }

    #[test]
    fn legacy_console_sends_app_key_and_access_key() {
        let engine = VolcengineAsr::new(config("123456789", "token-abc")).unwrap();
        let headers = engine.auth_headers();
        assert_eq!(header(&headers, "x-api-app-key"), Some("123456789"));
        assert_eq!(header(&headers, "x-api-access-key"), Some("token-abc"));
        assert_eq!(
            header(&headers, "x-api-resource-id"),
            Some("volc.bigasr.auc_turbo")
        );
        assert_eq!(header(&headers, "x-api-sequence"), Some("-1"));
        assert!(header(&headers, "x-api-key").is_none());
        let request_id = header(&headers, "x-api-request-id").unwrap();
        assert!(uuid::Uuid::parse_str(request_id).is_ok());
        assert_eq!(engine.uid(), "123456789");
    }

    #[test]
    fn new_console_without_app_id_sends_single_api_key() {
        let engine = VolcengineAsr::new(config("", "appkey-xyz")).unwrap();
        let headers = engine.auth_headers();
        assert_eq!(header(&headers, "x-api-key"), Some("appkey-xyz"));
        assert!(header(&headers, "x-api-app-key").is_none());
        assert!(header(&headers, "x-api-access-key").is_none());
        assert_eq!(engine.uid(), "appkey-xyz");
    }

    #[test]
    fn success_response_yields_transcript_text() {
        let body =
            r#"{"audio_info":{"duration":2499},"result":{"text":"关闭透传。","utterances":[]}}"#;
        let text =
            parse_response(reqwest::StatusCode::OK, Some("20000000"), Some("OK"), body).unwrap();
        assert_eq!(text, "关闭透传。");
    }

    #[test]
    fn success_response_with_missing_text_yields_empty_string() {
        let text = parse_response(reqwest::StatusCode::OK, Some("20000000"), None, "{}").unwrap();
        assert_eq!(text, "");
    }

    #[test]
    fn documented_error_codes_map_to_chinese_messages() {
        let err = parse_response(
            reqwest::StatusCode::OK,
            Some("20000003"),
            Some("静音音频"),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("静音"), "{err}");

        let err = parse_response(
            reqwest::StatusCode::OK,
            Some("45000001"),
            Some("bad param"),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("参数无效"), "{err}");

        let err = parse_response(reqwest::StatusCode::OK, Some("45000002"), None, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("空音频"), "{err}");

        let err = parse_response(reqwest::StatusCode::OK, Some("45000151"), None, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("格式"), "{err}");

        let err = parse_response(reqwest::StatusCode::OK, Some("55000031"), None, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("繁忙"), "{err}");

        let err = parse_response(reqwest::StatusCode::OK, Some("55000001"), Some("oops"), "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("55000001"), "{err}");
        assert!(err.contains("oops"), "{err}");
    }

    #[test]
    fn missing_status_header_falls_back_to_http_status() {
        let err = parse_response(
            reqwest::StatusCode::UNAUTHORIZED,
            None,
            None,
            "unauthorized",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[tokio::test]
    async fn engine_rejects_empty_audio_and_missing_credentials() {
        let engine = VolcengineAsr::new(config("123", "token")).unwrap();
        let err = engine
            .transcribe(AsrRequest::new(vec![], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::EmptyAudio));

        let engine = VolcengineAsr::new(config("123", "")).unwrap();
        assert!(!engine.is_supported());
        let err = engine
            .transcribe(AsrRequest::new(vec![0.0; 1600], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::Unsupported(_)));
    }

    #[tokio::test]
    async fn engine_enforces_the_audio_size_cap() {
        let mut cfg = config("123", "token");
        cfg.max_audio_bytes = 128;
        let engine = VolcengineAsr::new(cfg).unwrap();
        let err = engine
            .transcribe(AsrRequest::new(vec![0.0; 1600], 16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, AsrError::AudioTooLarge { .. }));
    }

    #[test]
    fn engine_identity_uses_the_product_label() {
        let engine = VolcengineAsr::new(config("123", "token")).unwrap();
        assert_eq!(engine.id(), AsrEngineId::Other);
        assert_eq!(engine.max_audio_bytes(), Some(8 * 1024 * 1024));
    }

    #[test]
    fn wav_payload_is_riff_before_base64() {
        let wav = lumen_asr::pcm_to_wav_bytes(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // base64 of the RIFF header is stable — guards the encode path used
        // for audio.data end to end.
        let encoded = base64_encode(&wav);
        assert!(encoded.starts_with("UklGR"));
    }
}
