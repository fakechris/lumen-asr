//! Permission status + open System Settings (M4).

use lumen_platform::{PermissionStatus, Permissions};
use lumen_platform_macos::MacPermissions;
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
    }
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

#[tauri::command]
pub async fn request_microphone_access(state: State<'_, AppState>) -> Result<PermissionDto, String> {
    // Trigger TCC by briefly opening the input stream if not recording.
    if !state.audio.is_recording() {
        match state.audio.start() {
            Ok(()) => {
                // capture a tiny moment then stop
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
