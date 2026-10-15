//! OpenAI-compatible chat completions corrector (Ollama, LM Studio, cloud, …).

use crate::{CorrectRequest, CorrectResult, Corrector, CorrectorError, DictionaryContext};
use async_trait::async_trait;
use lumen_core::CorrectorEngineId;
use lumen_prompts::{
    build_system_prompt, corrector_user_message_with_context, format_dictionary_block, CleanupLevel,
};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// Floor of the output-token budget: enough for any short dictation.
const MIN_OUTPUT_TOKENS: u32 = 1024;
/// Ceiling of the derived budget; an explicit `CorrectRequest::max_tokens` is
/// not capped by it.
const MAX_OUTPUT_TOKENS: u32 = 8192;
/// Tokens allowed per input character. A CJK character costs about one to two
/// tokens on common tokenizers, so two per character covers cleanup (output
/// close to input length) and translation with headroom.
const OUTPUT_TOKENS_PER_INPUT_CHAR: u32 = 2;
/// Fixed headroom on top of the per-character estimate (punctuation, line
/// breaks, residual thinking tags the provider could not suppress).
const OUTPUT_TOKEN_HEADROOM: u32 = 256;
/// Context window requested from Ollama for short requests.
const OLLAMA_MIN_CTX: u32 = 4096;
/// Upper bound on the Ollama context window we request.
const OLLAMA_MAX_CTX: u32 = 32768;

/// Output-token budget for an input transcript when the caller sets none:
/// `chars * 2 + 256`, clamped to `[1024, 8192]`.
pub fn output_token_budget(input: &str) -> u32 {
    let chars = u32::try_from(input.chars().count()).unwrap_or(u32::MAX);
    chars
        .saturating_mul(OUTPUT_TOKENS_PER_INPUT_CHAR)
        .saturating_add(OUTPUT_TOKEN_HEADROOM)
        .clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS)
}

/// Tokens charged per message for role markers and chat-template framing.
const PROMPT_TOKENS_PER_MESSAGE: u32 = 8;
/// Slack added to the Ollama window on top of prompt + output estimates.
const OLLAMA_CTX_MARGIN: u32 = 256;
/// The Ollama window is rounded up to a multiple of this.
const OLLAMA_CTX_GRANULARITY: u32 = 1024;

/// Upper-bound token estimate for prompt text. CJK and other wide characters
/// cost at most about two tokens each on common tokenizers; ASCII and other
/// narrow text averages three or more characters per token, so it is charged
/// one token per three characters, rounded up.
fn estimate_prompt_tokens(text: &str) -> u32 {
    let (wide, narrow) = text.chars().fold((0u64, 0u64), |(wide, narrow), c| {
        if is_wide_char(c) {
            (wide + 1, narrow)
        } else {
            (wide, narrow + 1)
        }
    });
    u32::try_from(wide * 2 + narrow.div_ceil(3)).unwrap_or(u32::MAX)
}

/// CJK ideographs, kana, hangul, CJK punctuation and fullwidth forms.
fn is_wide_char(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF
    )
}

/// Ollama `num_ctx` that holds every prompt message plus `max_tokens` of
/// output. Ollama keeps only the tail of a prompt that overflows `num_ctx`, so
/// an undersized window silently drops the head of the system prompt.
fn ollama_num_ctx(messages: &[&str], max_tokens: u32) -> u32 {
    let prompt = messages.iter().fold(0u32, |total, message| {
        total
            .saturating_add(estimate_prompt_tokens(message))
            .saturating_add(PROMPT_TOKENS_PER_MESSAGE)
    });
    let needed = prompt
        .saturating_add(max_tokens)
        .saturating_add(OLLAMA_CTX_MARGIN);
    if needed > OLLAMA_MAX_CTX {
        tracing::warn!(
            prompt_tokens = prompt,
            max_tokens,
            needed,
            cap = OLLAMA_MAX_CTX,
            "ollama context estimate exceeds the cap; the prompt head may be dropped"
        );
    }
    needed
        .div_ceil(OLLAMA_CTX_GRANULARITY)
        .saturating_mul(OLLAMA_CTX_GRANULARITY)
        .clamp(OLLAMA_MIN_CTX, OLLAMA_MAX_CTX)
}

/// Strip common model thinking wrappers (+ unclosed Qwen-style think blocks).
static THINKING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?s)<think>.*?</think>|<thought>.*?</thought>|<thinking>.*?</thinking>|</?think_[a-zA-Z0-9_]+>|💭.*?\n|<think>.*",
    )
    .expect("thinking regex")
});

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub engine_id: CorrectorEngineId,
    pub timeout: Duration,
}

impl OpenAiCompatConfig {
    pub fn ollama(model: impl Into<String>) -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1".into(),
            api_key: String::new(),
            model: model.into(),
            engine_id: CorrectorEngineId::Ollama,
            timeout: Duration::from_secs(60),
        }
    }
}

pub struct OpenAiCompatCorrector {
    client: Client,
    config: OpenAiCompatConfig,
}

impl OpenAiCompatCorrector {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, CorrectorError> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| CorrectorError::Http(e.to_string()))?;
        Ok(Self { client, config })
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
    /// `"stop"` on a complete answer, `"length"` when the provider hit the
    /// output-token limit. Missing on some gateways; treated as complete.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Option<String>,
}

#[async_trait]
impl Corrector for OpenAiCompatCorrector {
    fn id(&self) -> CorrectorEngineId {
        self.config.engine_id.clone()
    }

    async fn correct(&self, req: CorrectRequest) -> Result<CorrectResult, CorrectorError> {
        let dict_block = dict_block_opt(&req.dictionary);
        let user = corrector_user_message_with_context(
            &req.text,
            dict_block.as_deref(),
            req.context_json.as_deref(),
        );
        let system = if req.system_prompt.trim().is_empty() {
            build_system_prompt(CleanupLevel::Medium)
        } else {
            req.system_prompt.clone()
        };
        let temperature = if req.temperature > 0.0 {
            req.temperature
        } else {
            0.3
        };

        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/chat/completions");

        // Size the output budget from the transcript so long dictation is not
        // cut off; long-form callers (minutes) set `req.max_tokens` explicitly.
        let max_tokens = req
            .max_tokens
            .unwrap_or_else(|| output_token_budget(&req.text));
        let mut body = json!({
            "model": self.config.model,
            "temperature": temperature.clamp(0.01, 1.0),
            "max_tokens": max_tokens,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });

        inject_no_thinking_params(&self.config, &mut body, max_tokens);

        let mut builder = self.client.post(&url).json(&body);
        if !self.config.api_key.is_empty() {
            builder = builder.bearer_auth(&self.config.api_key);
        }

        let resp = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                CorrectorError::Timeout
            } else {
                CorrectorError::Http(error.to_string())
            }
        })?;

        if !resp.status().is_success() {
            return Err(CorrectorError::ProviderRejected(resp.status().as_u16()));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|_| CorrectorError::MalformedResponse)?;

        let choice = parsed.choices.first();
        // A length stop means the answer is only a prefix; inserting it would
        // drop the end of what the user said.
        if choice.and_then(|c| c.finish_reason.as_deref()) == Some("length") {
            return Err(CorrectorError::OutputTruncated);
        }

        let text = choice
            .and_then(|c| c.message.content.clone())
            .map(|s| strip_fences(&s))
            .map(|s| strip_thinking_tags(&s))
            .filter(|s| !s.is_empty())
            .ok_or(CorrectorError::EmptyOutput)?;

        Ok(CorrectResult {
            text,
            engine: self.id(),
            model_applied: true,
            fallback_reason: None,
        })
    }
}

/// Local Ollama (any model): disable thinking chain for speed.
fn is_local_ollama(cfg: &OpenAiCompatConfig) -> bool {
    if matches!(cfg.engine_id, CorrectorEngineId::Ollama) {
        return true;
    }
    let u = cfg.base_url.to_ascii_lowercase();
    u.contains("127.0.0.1:11434") || u.contains("localhost:11434") || u.contains("0.0.0.0:11434")
}

/// Request-side: turn off thinking for providers that support it.
///
/// Dictation / short translate should not pay CoT cost:
/// - Ollama: `think: false`
/// - Qwen/DashScope: `enable_thinking` + `think`
/// - DeepSeek reasoner: `thinking: {type:disabled}`
/// - Gemini: thinkingBudget 0 / thinkingConfig
/// - MiniMax-M3 (OpenAI path): `thinking: {type:disabled}` — works (dictation default)
/// - MiniMax-M2.x / `*-highspeed`: cannot fully disable CoT; prefer M3 for cleanup
fn inject_no_thinking_params(
    cfg: &OpenAiCompatConfig,
    body: &mut serde_json::Value,
    max_tokens: u32,
) {
    let url = cfg.base_url.to_ascii_lowercase();
    let model = cfg.model.to_ascii_lowercase();

    if is_local_ollama(cfg) {
        body["think"] = json!(false);
        // Ollama applies `options.num_predict` over the OpenAI `max_tokens`, so
        // both carry the same budget. `num_ctx` must also hold the prompt.
        let messages: Vec<&str> = body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| message["content"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        let num_ctx = ollama_num_ctx(&messages, max_tokens);
        body["options"] = json!({
            "num_ctx": num_ctx,
            "num_predict": max_tokens,
        });
        return;
    }

    // DashScope / Qwen3 compatible mode.
    if url.contains("dashscope") || model.contains("qwen3") || model.starts_with("qwen") {
        body["enable_thinking"] = json!(false);
        body["think"] = json!(false);
    }

    // DeepSeek reasoner / thinking models.
    if model.contains("reasoner") || model.contains("r1") || model.contains("thinking") {
        body["thinking"] = json!({ "type": "disabled" });
    }

    // MiniMax (OpenAI-compatible Chat Completions).
    // M3: on this path omit ⇒ thinking ON — must send disabled explicitly.
    // M2.x: official docs say thinking cannot be fully disabled (param ignored);
    // we still send disabled for forward-compat + strip residual tags.
    let is_minimax = url.contains("minimax") || model.contains("minimax");
    if is_minimax {
        body["thinking"] = json!({ "type": "disabled" });
        // Prefer split fields so answer stays in `content` when gateway supports it.
        body["reasoning_split"] = json!(true);
    }

    // Gemini OpenAI-compat: some builds honor thinkingBudget 0.
    if url.contains("generativelanguage") || model.contains("gemini") {
        body["extra_body"] = json!({
            "google": {
                "thinking_config": { "thinking_budget": 0 }
            }
        });
    }

    // OpenRouter-style reasoning disable.
    if url.contains("openrouter") {
        body["reasoning"] = json!({ "effort": "none", "exclude": true });
    }
}

/// Response-side strip of thinking tags. Always run for dictation safety.
pub fn strip_thinking_tags(text: &str) -> String {
    let cleaned = THINKING_RE.replace_all(text, "");
    // Fallback: if still starts with residual open tag without close.
    let cleaned = cleaned.trim();
    if let Some(idx) = cleaned.rfind("</think>") {
        return cleaned[idx + "</think>".len()..].trim().to_string();
    }
    cleaned.to_string()
}

fn dict_block_opt(d: &DictionaryContext) -> Option<String> {
    if d.terms.is_empty() && d.replacements.is_empty() {
        None
    } else {
        Some(format_dictionary_block(&d.terms, &d.replacements))
    }
}

fn strip_fences(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest
            .strip_prefix("text")
            .or_else(|| rest.strip_prefix("markdown"))
            .unwrap_or(rest);
        let rest = rest.trim_start_matches('\n');
        if let Some(idx) = rest.rfind("```") {
            return rest[..idx].trim().to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_qwen_think_block() {
        let raw = r#"<think>
The user said something.
I should only output clean text.
</think>

Hello world."#;
        let out = strip_thinking_tags(raw);
        assert_eq!(out, "Hello world.");
        assert!(!out.contains("think"));
    }

    #[test]
    fn strips_thinking_xml() {
        let raw = "<thinking>plan</thinking>final";
        assert_eq!(strip_thinking_tags(raw), "final");
    }

    #[test]
    fn output_budget_scales_with_input_and_is_clamped() {
        assert_eq!(output_token_budget(""), MIN_OUTPUT_TOKENS);
        assert_eq!(output_token_budget(&"字".repeat(100)), MIN_OUTPUT_TOKENS);
        assert_eq!(output_token_budget(&"字".repeat(1000)), 2256);
        assert_eq!(output_token_budget(&"字".repeat(10_000)), MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn prompt_estimate_charges_wide_chars_more_than_ascii() {
        assert_eq!(estimate_prompt_tokens(""), 0);
        assert_eq!(estimate_prompt_tokens("字字字"), 6);
        assert_eq!(estimate_prompt_tokens("かなカナ한글"), 12);
        assert_eq!(estimate_prompt_tokens("，。"), 4);
        assert_eq!(estimate_prompt_tokens("abcdefg"), 3);
        assert_eq!(estimate_prompt_tokens("ab字"), 3);
    }

    #[test]
    fn ollama_window_covers_prompt_and_output() {
        // Short prompt: floor.
        assert_eq!(ollama_num_ctx(&["system", "hello"], 1024), OLLAMA_MIN_CTX);
        // 6000 CJK chars of prompt (12000) + 8 + 4096 output + 256 margin
        // = 16360, rounded up to 16384.
        let prompt = "字".repeat(6000);
        assert_eq!(ollama_num_ctx(&[&prompt], 4096), 16384);
        // Output alone no longer decides the window.
        assert!(ollama_num_ctx(&[&prompt], 1024) > 2 * 1024);
        // Over the cap: clamped.
        let huge = "字".repeat(40_000);
        assert_eq!(ollama_num_ctx(&[&huge], 8192), OLLAMA_MAX_CTX);
        assert_eq!(ollama_num_ctx(&[], u32::MAX), OLLAMA_MAX_CTX);
    }

    /// Serves one canned chat-completions response on loopback and returns
    /// the base URL plus a handle yielding the raw request it received.
    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, handle)
    }

    fn corrector_for(base_url: String, engine_id: CorrectorEngineId) -> OpenAiCompatCorrector {
        OpenAiCompatCorrector::new(OpenAiCompatConfig {
            base_url,
            api_key: String::new(),
            model: "test-model".into(),
            engine_id,
            timeout: Duration::from_secs(5),
        })
        .unwrap()
    }

    fn request(text: &str, max_tokens: Option<u32>) -> CorrectRequest {
        CorrectRequest {
            text: text.into(),
            dictionary: DictionaryContext::default(),
            context_json: None,
            system_prompt: String::new(),
            temperature: 0.3,
            max_tokens,
        }
    }

    fn request_json(raw: &str) -> serde_json::Value {
        let body = raw.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn length_finish_reason_is_reported_as_truncation() {
        let (base, server) = serve_once(
            r#"{"choices":[{"message":{"content":"half of the"},"finish_reason":"length"}]}"#,
        );
        let corrector = corrector_for(base, CorrectorEngineId::OpenAiCompatible);

        let err = corrector
            .correct(request("half of the sentence", None))
            .await
            .unwrap_err();
        server.join().unwrap();

        assert!(matches!(err, CorrectorError::OutputTruncated));
        assert_eq!(
            err.fallback_reason(),
            crate::CorrectorFallbackReason::OutputTruncated
        );
    }

    #[tokio::test]
    async fn stop_finish_reason_returns_text() {
        let (base, server) = serve_once(
            r#"{"choices":[{"message":{"content":"Hello world."},"finish_reason":"stop"}]}"#,
        );
        let corrector = corrector_for(base, CorrectorEngineId::OpenAiCompatible);

        let result = corrector
            .correct(request("hello world", None))
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(result.text, "Hello world.");
        assert!(result.model_applied);
    }

    #[tokio::test]
    async fn missing_finish_reason_is_treated_as_complete() {
        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let corrector = corrector_for(base, CorrectorEngineId::OpenAiCompatible);

        let result = corrector.correct(request("ok", None)).await.unwrap();
        server.join().unwrap();

        assert_eq!(result.text, "ok");
    }

    #[tokio::test]
    async fn derived_budget_is_sent_to_cloud_and_ollama() {
        let long_input = "字".repeat(2000);

        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"x"}}]}"#);
        corrector_for(base, CorrectorEngineId::OpenAiCompatible)
            .correct(request(&long_input, None))
            .await
            .unwrap();
        let cloud = request_json(&server.join().unwrap());
        assert_eq!(cloud["max_tokens"], 4256);
        assert!(cloud.get("options").is_none());

        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"x"}}]}"#);
        corrector_for(base, CorrectorEngineId::Ollama)
            .correct(request(&long_input, None))
            .await
            .unwrap();
        let ollama = request_json(&server.join().unwrap());
        assert_eq!(ollama["max_tokens"], 4256);
        assert_eq!(ollama["options"]["num_predict"], 4256);
        assert_eq!(
            ollama["options"]["num_ctx"],
            sent_window(&ollama, 4256),
            "num_ctx is derived from the messages actually sent"
        );
    }

    /// Recomputes the expected window from the messages in a captured body.
    fn sent_window(body: &serde_json::Value, max_tokens: u32) -> u32 {
        let messages: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["content"].as_str().unwrap())
            .collect();
        ollama_num_ctx(&messages, max_tokens)
    }

    #[tokio::test]
    async fn ollama_window_holds_a_long_minutes_prompt() {
        // A minutes-style call: ~12k-char CJK window in the system prompt and
        // an explicit output budget.
        let mut req = request("会议记录", Some(4096));
        req.system_prompt = "纪".repeat(12_000);

        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"x"}}]}"#);
        corrector_for(base, CorrectorEngineId::Ollama)
            .correct(req)
            .await
            .unwrap();
        let body = request_json(&server.join().unwrap());

        let num_ctx = body["options"]["num_ctx"].as_u64().unwrap() as u32;
        // 12k CJK chars alone estimate to 24k tokens; plus 4096 of output.
        assert!(
            num_ctx >= 24_000 + 4096,
            "num_ctx {num_ctx} drops the prompt head"
        );
        assert!(num_ctx <= OLLAMA_MAX_CTX);
        assert_eq!(num_ctx % OLLAMA_CTX_GRANULARITY, 0);
        assert_eq!(num_ctx, sent_window(&body, 4096));
        assert_eq!(body["options"]["num_predict"], 4096);
    }

    #[tokio::test]
    async fn explicit_budget_overrides_the_derived_one() {
        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"x"}}]}"#);
        corrector_for(base, CorrectorEngineId::Ollama)
            .correct(request("short", Some(12_000)))
            .await
            .unwrap();
        let body = request_json(&server.join().unwrap());
        assert_eq!(body["max_tokens"], 12_000);
        assert_eq!(body["options"]["num_predict"], 12_000);
    }
}
