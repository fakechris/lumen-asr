//! Permission status + open System Settings (M4).

use lumen_platform::{PermissionStatus, Permissions};
use lumen_platform_macos::{
    ensure_accessibility_onboarding, is_accessibility_trusted, prompt_accessibility, MacPermissions,
};
use serde::Serialize;
use tauri::State;

use crate::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDto {
    pub microphone: String,
    pub accessibility: String,
    pub can_record: bool,
    pub can_inject: bool,
    pub copy_only_ok: bool,
    /// Process path / name to enable in System Settings (debug builds differ from .app).
    pub process_hint: String,
}

fn map_status(s: PermissionStatus) -> PermissionDto {
    use lumen_platform::PermissionState;
    let mic = match s.microphone {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Restricted => "restricted",
        PermissionState::NotDetermined => "not_determined",
    };
    let ax = match s.accessibility {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Restricted => "restricted",
        PermissionState::NotDetermined => "not_determined",
    };
    // NotDetermined mic still allows start (system will prompt).
    let can_record = matches!(
        s.microphone,
        PermissionState::Granted | PermissionState::NotDetermined
    );
    PermissionDto {
        microphone: mic.into(),
        accessibility: ax.into(),
        can_record,
        can_inject: s.can_inject(),
        copy_only_ok: can_record,
        process_hint: process_hint(),
    }
}

fn process_hint() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Lumen ASR".into())
}

#[tauri::command]
pub async fn get_permission_status() -> Result<PermissionDto, String> {
    let p = MacPermissions;
    let s = p.status().await.map_err(|e| e.to_string())?;
    Ok(map_status(s))
}

#[tauri::command]
pub async fn open_microphone_settings() -> Result<(), String> {
    MacPermissions
        .open_microphone_settings()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn open_accessibility_settings() -> Result<(), String> {
    MacPermissions
        .open_accessibility_settings()
        .await
        .map_err(|e| e.to_string())
}

/// Show system AX prompt (if possible) and open System Settings → Accessibility.
#[tauri::command]
pub async fn request_accessibility_access() -> Result<PermissionDto, String> {
    let trusted = ensure_accessibility_onboarding();
    tracing::info!(
        trusted,
        process = %process_hint(),
        "accessibility onboarding"
    );
    get_permission_status().await
}

#[tauri::command]
pub async fn request_microphone_access(state: State<'_, AppState>) -> Result<PermissionDto, String> {
    // Trigger TCC by briefly opening the input stream if not recording.
    if !state.audio.is_recording() {
        match state.audio.start() {
            Ok(()) => {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                let _ = state.audio.stop();
            }
            Err(e) => {
                tracing::warn!(error = %e, "mic probe start failed");
            }
        }
    }
    let _ = MacPermissions.request_microphone().await;
    get_permission_status().await
}

/// Call once at app startup (after event loop is up).
pub fn bootstrap_permissions() {
    let trusted = is_accessibility_trusted();
    tracing::info!(
        accessibility_trusted = trusted,
        process = %process_hint(),
        "permission bootstrap"
    );
    if !trusted {
        // System dialog + open Settings so the user can enable the toggle.
        let after = prompt_accessibility();
        tracing::warn!(
            after_prompt = after,
            process = %process_hint(),
            "Accessibility not granted — inject/hotkey event-tap need it. Enable in System Settings → Privacy & Security → Accessibility"
        );
        if !after {
            tauri::async_runtime::spawn(async move {
                let _ = MacPermissions.open_accessibility_settings().await;
            });
        }
    }
}
