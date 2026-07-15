//! OpenAI-compatible chat completions corrector (Ollama, LM Studio, cloud, …).

use crate::{CorrectRequest, CorrectResult, Corrector, CorrectorError, DictionaryContext};
use async_trait::async_trait;
use lumen_core::CorrectorEngineId;
use lumen_prompts::{
    build_system_prompt, corrector_user_message, format_dictionary_block, CleanupLevel,
};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

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
        let user = corrector_user_message(&req.text, dict_block.as_deref());
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

        // Competitor defaults: temperature ~0.3, max_tokens for short dictation outputs.
        let mut body = json!({
            "model": self.config.model,
            "temperature": temperature.clamp(0.01, 1.0),
            "max_tokens": 1024,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });

        inject_no_thinking_params(&self.config, &mut body);

        let mut builder = self.client.post(&url).json(&body);
        if !self.config.api_key.is_empty() {
            builder = builder.bearer_auth(&self.config.api_key);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| CorrectorError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CorrectorError::Http(format!("{status}: {body}")));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| CorrectorError::Http(e.to_string()))?;

        let text = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .map(|s| strip_fences(&s))
            .map(|s| strip_thinking_tags(&s))
            .filter(|s| !s.is_empty())
            .ok_or(CorrectorError::EmptyOutput)?;

        Ok(CorrectResult {
            text,
            engine: self.id(),
            model_applied: true,
        })
    }
}

/// Local Ollama (any model): disable thinking chain for speed.
fn is_local_ollama(cfg: &OpenAiCompatConfig) -> bool {
    if matches!(cfg.engine_id, CorrectorEngineId::Ollama) {
        return true;
    }
    let u = cfg.base_url.to_ascii_lowercase();
    u.contains("127.0.0.1:11434")
        || u.contains("localhost:11434")
        || u.contains("0.0.0.0:11434")
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
fn inject_no_thinking_params(cfg: &OpenAiCompatConfig, body: &mut serde_json::Value) {
    let url = cfg.base_url.to_ascii_lowercase();
    let model = cfg.model.to_ascii_lowercase();

    if is_local_ollama(cfg) {
        body["think"] = json!(false);
        body["options"] = json!({
            "num_ctx": 4096,
            "num_predict": 1024,
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
