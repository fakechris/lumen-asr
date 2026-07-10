//! Corrector discovery: Ollama + OpenAI-compatible env (onboarding Stage D).

use crate::config::CorrectorConfig;
use crate::AppState;
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, State};

static PULL_CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectorProbeResult {
    pub ollama_running: bool,
    pub ollama_url: String,
    pub ollama_models: Vec<String>,
    pub has_qwen25_7b: bool,
    pub env_openai_base: Option<String>,
    pub env_openai_key_set: bool,
    pub env_lumen_model: Option<String>,
    pub suggested_provider: String,
    pub suggested_base_url: String,
    pub suggested_model: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaPullProgress {
    pub phase: String,
    pub message: String,
}

fn ollama_base() -> String {
    std::env::var("OLLAMA_HOST")
        .or_else(|_| std::env::var("LUMEN_OLLAMA_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:11434".into())
        .trim_end_matches('/')
        .to_string()
}

fn list_ollama_models_http(base: &str) -> Result<Vec<String>, String> {
    let url = format!("{base}/api/tags");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ollama HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let mut models = Vec::new();
    if let Some(arr) = v.get("models").and_then(|m| m.as_array()) {
        for m in arr {
            if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    }
    models.sort();
    Ok(models)
}

fn has_qwen(models: &[String]) -> bool {
    models.iter().any(|m| {
        let l = m.to_ascii_lowercase();
        l.contains("qwen2.5:7b") || l.starts_with("qwen2.5:7b") || l == "qwen2.5:7b"
    })
}

#[tauri::command]
pub fn probe_corrector() -> Result<CorrectorProbeResult, String> {
    let ollama_url = ollama_base();
    let (ollama_running, ollama_models) = match list_ollama_models_http(&ollama_url) {
        Ok(m) => (true, m),
        Err(_) => (false, vec![]),
    };
    let has_qwen25_7b = has_qwen(&ollama_models);

    let env_openai_base = std::env::var("OPENAI_API_BASE")
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .or_else(|_| std::env::var("LUMEN_CORRECTOR_BASE_URL"))
        .ok()
        .filter(|s| !s.is_empty());
    let env_openai_key_set = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("LUMEN_CORRECTOR_API_KEY"))
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let env_lumen_model = std::env::var("LUMEN_CORRECTOR_MODEL")
        .ok()
        .filter(|s| !s.is_empty());

    let (suggested_provider, suggested_base_url, suggested_model, message) =
        if ollama_running {
            let model = if has_qwen25_7b {
                "qwen2.5:7b".into()
            } else if let Some(m) = ollama_models.first() {
                m.clone()
            } else {
                "qwen2.5:7b".into()
            };
            (
                "ollama".into(),
                format!("{ollama_url}/v1"),
                model,
                if has_qwen25_7b {
                    "Ollama 可用，已检测到 qwen2.5:7b".into()
                } else if ollama_models.is_empty() {
                    "Ollama 在运行但还没有模型，建议拉取 qwen2.5:7b".into()
                } else {
                    format!("Ollama 可用，共 {} 个模型", ollama_models.len())
                },
            )
        } else if let Some(ref base) = env_openai_base {
            (
                "openai_compatible".into(),
                base.clone(),
                env_lumen_model
                    .clone()
                    .unwrap_or_else(|| "gpt-4o-mini".into()),
                "检测到环境变量中的 OpenAI 兼容配置".into(),
            )
        } else {
            (
                "none".into(),
                format!("{ollama_url}/v1"),
                "qwen2.5:7b".into(),
                "未检测到 Ollama 或 OpenAI 兼容环境变量，可跳过修正或稍后安装 Ollama".into(),
            )
        };

    Ok(CorrectorProbeResult {
        ollama_running,
        ollama_url,
        ollama_models,
        has_qwen25_7b,
        env_openai_base,
        env_openai_key_set,
        env_lumen_model,
        suggested_provider,
        suggested_base_url,
        suggested_model,
        message,
    })
}

#[tauri::command]
pub fn ollama_list_models() -> Result<Vec<String>, String> {
    list_ollama_models_http(&ollama_base())
}

#[tauri::command]
pub fn cancel_ollama_pull() -> Result<(), String> {
    PULL_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaPullInput {
    pub model: Option<String>,
}

#[tauri::command]
pub async fn ollama_pull_model(
    app: AppHandle,
    input: OllamaPullInput,
) -> Result<CorrectorProbeResult, String> {
    let model = input
        .model
        .unwrap_or_else(|| "qwen2.5:7b".into())
        .trim()
        .to_string();
    if model.is_empty() {
        return Err("model name empty".into());
    }
    PULL_CANCEL.store(false, Ordering::SeqCst);

    let app2 = app.clone();
    let model2 = model.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = app2.emit(
            "ollama-pull-progress",
            OllamaPullProgress {
                phase: "pulling".into(),
                message: format!("正在拉取 {model2} …"),
            },
        );
        let status = Command::new("ollama")
            .args(["pull", &model2])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        match status {
            Ok(s) if s.success() => {
                let _ = app2.emit(
                    "ollama-pull-progress",
                    OllamaPullProgress {
                        phase: "done".into(),
                        message: format!("{model2} 已就绪"),
                    },
                );
                Ok(())
            }
            Ok(s) => Err(format!("ollama pull failed (exit {:?})", s.code())),
            Err(e) => Err(format!(
                "无法启动 ollama（是否已安装？）: {e}. 可执行: brew install ollama"
            )),
        }
    })
    .await
    .map_err(|e| e.to_string())??;

    probe_corrector()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyCorrectorSuggestionInput {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub enabled: Option<bool>,
    pub api_key: Option<String>,
}

#[tauri::command]
pub fn apply_corrector_suggestion(
    state: State<'_, AppState>,
    input: ApplyCorrectorSuggestionInput,
) -> Result<crate::corrector_cmd::CorrectorStatus, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    guard.corrector.enabled = input.enabled.unwrap_or(input.provider != "none");
    guard.corrector.provider = input.provider;
    guard.corrector.base_url = input.base_url;
    guard.corrector.model = input.model;
    if let Some(k) = input.api_key {
        guard.corrector.api_key = k;
    } else if guard.corrector.api_key.is_empty() {
        if let Ok(k) = std::env::var("OPENAI_API_KEY") {
            guard.corrector.api_key = k;
        } else if let Ok(k) = std::env::var("LUMEN_CORRECTOR_API_KEY") {
            guard.corrector.api_key = k;
        }
    }
    guard.save()?;
    // Re-export status via corrector_cmd helper fields
    Ok(crate::corrector_cmd::status_from(&guard))
}

#[allow(dead_code)]
fn _cfg_touch(_: &CorrectorConfig) {}
