//! Text insert IPC (M4).

use crate::config::InjectConfig;
use crate::AppState;
use lumen_core::InsertStrategy;
use lumen_inject::{InsertOutcome, TextInjector};
use lumen_platform_macos::MacTextInjectorBackend;
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectConfigDto {
    pub mode: String,
    pub preserve_clipboard: bool,
    pub auto_insert: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectConfigInput {
    pub mode: Option<String>,
    pub preserve_clipboard: Option<bool>,
    pub auto_insert: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertTextOutcome {
    pub strategy: String,
    pub restored_clipboard: bool,
}

fn strategy_str(s: InsertStrategy) -> &'static str {
    match s {
        InsertStrategy::Paste => "paste",
        InsertStrategy::Ax => "ax",
        InsertStrategy::Type => "type",
        InsertStrategy::CopyOnly => "copy_only",
        InsertStrategy::None => "none",
    }
}

#[tauri::command]
pub fn get_inject_config(state: State<'_, AppState>) -> Result<InjectConfigDto, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(InjectConfigDto {
        mode: cfg.inject.mode.clone(),
        preserve_clipboard: cfg.inject.preserve_clipboard,
        auto_insert: cfg.inject.auto_insert,
    })
}

#[tauri::command]
pub fn save_inject_config(
    state: State<'_, AppState>,
    input: InjectConfigInput,
) -> Result<InjectConfigDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    if let Some(m) = input.mode {
        guard.inject.mode = m;
    }
    if let Some(v) = input.preserve_clipboard {
        guard.inject.preserve_clipboard = v;
    }
    if let Some(v) = input.auto_insert {
        guard.inject.auto_insert = v;
    }
    guard.save()?;
    Ok(InjectConfigDto {
        mode: guard.inject.mode.clone(),
        preserve_clipboard: guard.inject.preserve_clipboard,
        auto_insert: guard.inject.auto_insert,
    })
}

/// Insert text into the frontmost app using configured policy.
#[tauri::command]
pub async fn insert_text(
    state: State<'_, AppState>,
    text: String,
) -> Result<InsertTextOutcome, String> {
    let policy = {
        let cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        cfg.inject.to_policy()
    };

    // Small delay so the user can refocus the target app after clicking our UI.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let injector = TextInjector::new(MacTextInjectorBackend, policy);
    let out: InsertOutcome = injector.insert(&text).await.map_err(|e| e.to_string())?;
    Ok(InsertTextOutcome {
        strategy: strategy_str(out.strategy).into(),
        restored_clipboard: out.restored_clipboard,
    })
}

pub async fn insert_with_config(cfg: &InjectConfig, text: &str) -> Result<InsertOutcome, String> {
    if text.is_empty() {
        return Ok(InsertOutcome {
            strategy: InsertStrategy::None,
            restored_clipboard: false,
        });
    }
    if !lumen_platform_macos::is_accessibility_trusted() {
        return Err(
            "Accessibility permission required to insert into other apps (System Settings → Privacy & Security → Accessibility)"
                .into(),
        );
    }
    let injector = TextInjector::new(MacTextInjectorBackend, cfg.to_policy());
    injector.insert(text).await.map_err(|e| e.to_string())
}

pub async fn copy_only(text: &str) -> Result<(), String> {
    use lumen_inject::TextInjectorBackend;
    MacTextInjectorBackend
        .copy_only(text)
        .await
        .map_err(|e| e.to_string())
}
