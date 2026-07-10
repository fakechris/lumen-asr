//! OpenAI-compatible chat completions corrector (Ollama, LM Studio, cloud, …).

use crate::{CorrectRequest, CorrectResult, Corrector, CorrectorError, DictionaryContext};
use async_trait::async_trait;
use lumen_core::CorrectorEngineId;
use lumen_prompts::{
    build_system_prompt, corrector_user_message, format_dictionary_block, CleanupLevel,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

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

        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let body = json!({
            "model": self.config.model,
            "temperature": temperature,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });

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
            .filter(|s| !s.is_empty())
            .ok_or(CorrectorError::EmptyOutput)?;

        Ok(CorrectResult {
            text,
            engine: self.id(),
            model_applied: true,
        })
    }
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
