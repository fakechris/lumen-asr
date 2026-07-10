//! Corrector settings + manual correct IPC (M3).

use crate::config::{AppConfig, CorrectorConfig};
use crate::corrector_svc::{engine_label, run_correct};
use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CorrectorStatus {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// api_key is never returned in full; only whether set.
    pub has_api_key: bool,
    pub timeout_secs: u64,
    pub label: String,
    /// none | light | medium | strong
    pub cleanup: String,
    pub style: String,
    pub casing: String,
    pub punctuation: String,
    pub polish: Vec<String>,
    pub custom_enabled: bool,
    pub custom_instruction: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectorConfigInput {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub timeout_secs: Option<u64>,
    pub cleanup: Option<String>,
    pub style: Option<String>,
    pub casing: Option<String>,
    pub punctuation: Option<String>,
    pub polish: Option<Vec<String>>,
    pub custom_enabled: Option<bool>,
    pub custom_instruction: Option<String>,
}

#[tauri::command]
pub fn get_corrector_config(state: State<'_, AppState>) -> Result<CorrectorStatus, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();
    Ok(status_from(&cfg))
}

#[tauri::command]
pub fn save_corrector_config(
    state: State<'_, AppState>,
    input: CorrectorConfigInput,
) -> Result<CorrectorStatus, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;

    if let Some(v) = input.enabled {
        guard.corrector.enabled = v;
    }
    if let Some(v) = input.provider {
        guard.corrector.provider = v;
    }
    if let Some(v) = input.base_url {
        guard.corrector.base_url = v;
    }
    if let Some(v) = input.model {
        guard.corrector.model = v;
    }
    if let Some(v) = input.api_key {
        // Empty string means leave unchanged; use single space to clear intentionally? Prefer: only update if Some and not sentinel.
        // Convention: omit field to keep; pass "" to clear.
        guard.corrector.api_key = v;
    }
    if let Some(v) = input.timeout_secs {
        guard.corrector.timeout_secs = v.max(5);
    }
    if let Some(v) = input.cleanup {
        if lumen_prompts::CleanupLevel::parse(&v).is_some() {
            guard.output.cleanup = v.to_ascii_lowercase();
        } else {
            return Err(format!("unknown cleanup level: {v}"));
        }
    }
    if let Some(v) = input.style {
        if lumen_prompts::Style::parse(&v).is_some() {
            guard.output.style = v.to_ascii_lowercase();
        } else {
            return Err(format!("unknown style: {v}"));
        }
    }
    if let Some(v) = input.casing {
        if lumen_prompts::Casing::parse(&v).is_some() {
            guard.output.casing = v.to_ascii_lowercase();
        } else {
            return Err(format!("unknown casing: {v}"));
        }
    }
    if let Some(v) = input.punctuation {
        if lumen_prompts::PunctPolicy::parse(&v).is_some() {
            guard.output.punctuation = v.to_ascii_lowercase();
        } else {
            return Err(format!("unknown punctuation: {v}"));
        }
    }
    if let Some(v) = input.polish {
        for p in &v {
            if lumen_prompts::PolishRule::parse(p).is_none() {
                return Err(format!("unknown polish rule: {p}"));
            }
        }
        guard.output.polish = v;
    }
    if let Some(v) = input.custom_enabled {
        guard.output.custom_enabled = v;
    }
    if let Some(v) = input.custom_instruction {
        guard.output.custom_instruction = v;
    }

    guard.save()?;
    Ok(status_from(&guard))
}

pub(crate) fn status_from(cfg: &AppConfig) -> CorrectorStatus {
    CorrectorStatus {
        enabled: cfg.corrector.enabled,
        provider: cfg.corrector.provider.clone(),
        base_url: cfg.corrector.base_url.clone(),
        model: cfg.corrector.model.clone(),
        has_api_key: !cfg.corrector.api_key.is_empty(),
        timeout_secs: cfg.corrector.timeout_secs,
        label: engine_label(cfg),
        cleanup: cfg.output.cleanup_level().as_str().into(),
        style: cfg.output.style().as_str().into(),
        casing: cfg.output.casing().as_str().into(),
        punctuation: cfg.output.punctuation().as_str().into(),
        polish: cfg
            .output
            .polish_rules()
            .iter()
            .map(|r| r.as_str().to_string())
            .collect(),
        custom_enabled: cfg.output.custom_enabled,
        custom_instruction: cfg.output.custom_instruction.clone(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectTextInput {
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectTextOutcome {
    pub text: String,
    pub model_applied: bool,
    pub corrector_engine: String,
}

#[tauri::command]
pub async fn correct_text(
    state: State<'_, AppState>,
    input: CorrectTextInput,
) -> Result<CorrectTextOutcome, String> {
    let text = input.text;
    if text.trim().is_empty() {
        return Ok(CorrectTextOutcome {
            text,
            model_applied: false,
            corrector_engine: "none".into(),
        });
    }

    let entries = {
        let store_guard = state
            .store
            .lock()
            .map_err(|_| "store lock poisoned".to_string())?;
        match store_guard.as_ref() {
            Some(s) => s.list_dictionary().unwrap_or_default(),
            None => vec![],
        }
    };

    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .clone();

    let corr = run_correct(&cfg, &text, &entries).await;
    Ok(CorrectTextOutcome {
        text: corr.text,
        model_applied: corr.model_applied,
        corrector_engine: if corr.model_applied {
            engine_label(&cfg)
        } else {
            format!("{}:fallback", engine_label(&cfg))
        },
    })
}

/// Default config snapshot for UI factory reset.
#[tauri::command]
pub fn default_corrector_config() -> CorrectorConfig {
    CorrectorConfig::default()
}
