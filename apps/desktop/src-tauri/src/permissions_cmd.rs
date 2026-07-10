//! Permission status + open System Settings.

use lumen_platform::{PermissionStatus, Permissions};
use lumen_platform_macos::{is_accessibility_trusted, prompt_accessibility, MacPermissions};
use serde::Serialize;
use tauri::State;

use crate::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDto {
    pub microphone: String,
    pub accessibility: String,
    /// True when AXIsProcessTrusted — required for inject / event-tap.
    pub accessibility_trusted: bool,
    pub can_record: bool,
    pub can_inject: bool,
    pub copy_only_ok: bool,
    /// Executable basename shown in System Settings (may differ for debug vs .app).
    pub process_hint: String,
    /// Full path of the running binary — enable *this* entry in Accessibility.
    pub process_path: String,
}

fn map_status(s: PermissionStatus) -> PermissionDto {
    use lumen_platform::PermissionState;
    let mic = match s.microphone {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Restricted => "restricted",
        PermissionState::NotDetermined => "not_determined",
    };
    let trusted = is_accessibility_trusted();
    let ax = if trusted {
        "granted"
    } else {
        match s.accessibility {
            PermissionState::Granted => "granted",
            PermissionState::Denied => "needs_enable",
            PermissionState::Restricted => "restricted",
            PermissionState::NotDetermined => "needs_enable",
        }
    };
    let can_record = matches!(
        s.microphone,
        PermissionState::Granted | PermissionState::NotDetermined
    );
    PermissionDto {
        microphone: mic.into(),
        accessibility: ax.into(),
        accessibility_trusted: trusted,
        can_record,
        can_inject: trusted,
        copy_only_ok: can_record,
        process_hint: process_hint(),
        process_path: process_path(),
    }
}

fn process_hint() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Lumen ASR".into())
}

fn process_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into())
}

#[tauri::command]
pub async fn get_permission_status() -> Result<PermissionDto, String> {
    let p = MacPermissions;
    let s = p.status().await.map_err(|e| e.to_string())?;
    Ok(map_status(s))
}

/// Lightweight poll for wizard / settings (same as get; named for intent).
#[tauri::command]
pub async fn poll_permissions() -> Result<PermissionDto, String> {
    get_permission_status().await
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

/// User-initiated: try once to appear in the Accessibility list, then open Settings.
/// Does **not** grant permission — user must flip the switch for *this* process path.
#[tauri::command]
pub async fn request_accessibility_access() -> Result<PermissionDto, String> {
    let before = is_accessibility_trusted();
    if !before {
        // May register the app in the list (often no dialog on modern macOS).
        let _ = prompt_accessibility();
    }
    let _ = MacPermissions.open_accessibility_settings().await;
    let after = is_accessibility_trusted();
    tracing::info!(
        before,
        after,
        process = %process_hint(),
        path = %process_path(),
        "accessibility request (open Settings; user must enable toggle)"
    );
    get_permission_status().await
}

#[tauri::command]
pub async fn request_microphone_access(state: State<'_, AppState>) -> Result<PermissionDto, String> {
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

/// Startup: log only — do not open Settings or force system prompts.
pub fn bootstrap_permissions() {
    let trusted = is_accessibility_trusted();
    tracing::info!(
        accessibility_trusted = trusted,
        process = %process_hint(),
        path = %process_path(),
        "permission bootstrap (no auto Settings open)"
    );
    if !trusted {
        tracing::warn!(
            "Accessibility not granted for this process — inject/event-tap need it. Enable in System Settings → Privacy & Security → Accessibility"
        );
    }
}
