//! Permission status + open System Settings.

use lumen_platform::PermissionStatus;
#[cfg(target_os = "macos")]
use lumen_platform::Permissions;
#[cfg(target_os = "macos")]
use lumen_platform_macos::{
    dismiss_accessibility_drag_overlay, is_accessibility_trusted, present_accessibility_drag_overlay,
    prompt_accessibility, MacPermissions,
};
use serde::Serialize;
use tauri::State;

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicU8, Ordering};

use crate::AppState;

#[cfg(target_os = "windows")]
static WINDOWS_MICROPHONE_STATE: AtomicU8 = AtomicU8::new(0);

#[cfg(target_os = "windows")]
fn windows_microphone_state_from_code(code: u8) -> lumen_platform::PermissionState {
    use lumen_platform::PermissionState;
    match code {
        1 => PermissionState::Granted,
        _ => PermissionState::NotDetermined,
    }
}

#[cfg(target_os = "windows")]
fn windows_microphone_state_from_capability_code(
    code: i32,
) -> Option<lumen_platform::PermissionState> {
    use lumen_platform::PermissionState;
    use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;

    match AppCapabilityAccessStatus(code) {
        AppCapabilityAccessStatus::Allowed => Some(PermissionState::Granted),
        AppCapabilityAccessStatus::DeniedByUser => Some(PermissionState::Denied),
        AppCapabilityAccessStatus::DeniedBySystem => Some(PermissionState::Restricted),
        AppCapabilityAccessStatus::UserPromptRequired => Some(PermissionState::NotDetermined),
        // An unpackaged build cannot declare an MSIX capability. Keep that
        // state neutral and let the real CPAL/WASAPI probe decide on use.
        AppCapabilityAccessStatus::NotDeclaredByApp => None,
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn windows_microphone_app_capability_state() -> Option<lumen_platform::PermissionState> {
    use windows::{core::HSTRING, Security::Authorization::AppCapabilityAccess::AppCapability};

    let capability = AppCapability::Create(&HSTRING::from("Microphone")).ok()?;
    let status = capability.CheckAccess().ok()?;
    windows_microphone_state_from_capability_code(status.0)
}

#[cfg(target_os = "windows")]
fn windows_microphone_state() -> lumen_platform::PermissionState {
    // AppCapability is a non-prompting query for packaged MSIX/Store apps and
    // survives process restarts because Windows owns the permission state. It
    // stays authoritative so a Settings change is reflected by the UI poll.
    // Unpackaged NSIS builds fall back to NotDetermined and the real capture
    // probe above remains authoritative for the current process.
    windows_microphone_app_capability_state().unwrap_or_else(|| {
        windows_microphone_state_from_code(WINDOWS_MICROPHONE_STATE.load(Ordering::SeqCst))
    })
}

/// A successful real capture is the most reliable Windows permission probe.
/// Keep Settings in sync when recording starts outside the permission page.
pub(crate) fn mark_microphone_capture_started() {
    #[cfg(target_os = "windows")]
    WINDOWS_MICROPHONE_STATE.store(1, Ordering::SeqCst);
}

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
    /// Executable basename (e.g. lumen-asr-desktop).
    pub process_hint: String,
    /// Full path of the running binary — enable *this* entry in Accessibility.
    pub process_path: String,
    /// Name most likely shown in System Settings Accessibility list.
    pub settings_list_name: String,
    /// Bundle id from Info.plist when running as .app, else empty.
    pub bundle_id: String,
    /// Short codesign summary, e.g. "adhoc" or team id.
    pub codesign_kind: String,
    /// codesign Identifier=… (changes per adhoc build).
    pub codesign_identifier: String,
    /// True when signature is ad-hoc (rebuild often invalidates TCC toggle).
    pub codesign_adhoc: bool,
}

fn map_status(s: PermissionStatus) -> PermissionDto {
    use lumen_platform::PermissionState;
    let mic = match s.microphone {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Restricted => "restricted",
        PermissionState::NotDetermined => "not_determined",
    };
    #[cfg(target_os = "macos")]
    let trusted = is_accessibility_trusted();
    #[cfg(not(target_os = "macos"))]
    let trusted = false;
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
    #[cfg(target_os = "windows")]
    let can_record = matches!(s.microphone, PermissionState::Granted);
    #[cfg(not(target_os = "windows"))]
    let can_record = matches!(
        s.microphone,
        PermissionState::Granted | PermissionState::NotDetermined
    );
    let path = process_path();
    let hint = process_hint();
    let (codesign_kind, codesign_identifier, codesign_adhoc) = codesign_info(&path);
    PermissionDto {
        microphone: mic.into(),
        accessibility: ax.into(),
        accessibility_trusted: trusted,
        can_record,
        can_inject: trusted,
        copy_only_ok: can_record,
        process_hint: hint,
        process_path: path.clone(),
        settings_list_name: settings_list_name(&path),
        bundle_id: bundle_id_from_path(&path),
        codesign_kind,
        codesign_identifier,
        codesign_adhoc,
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

/// System Settings usually shows CFBundleDisplayName for .app, basename for bare binaries.
fn settings_list_name(path: &str) -> String {
    if let Some(app_root) = app_bundle_root(path) {
        if let Some(name) =
            read_plist_string(&app_root.join("Contents/Info.plist"), "CFBundleDisplayName").or_else(
                || read_plist_string(&app_root.join("Contents/Info.plist"), "CFBundleName"),
            )
        {
            return name;
        }
        return "Lumen ASR".into();
    }
    process_hint()
}

fn bundle_id_from_path(path: &str) -> String {
    app_bundle_root(path)
        .and_then(|root| read_plist_string(&root.join("Contents/Info.plist"), "CFBundleIdentifier"))
        .unwrap_or_default()
}

fn app_bundle_root(path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    // …/Foo.app/Contents/MacOS/binary
    let macos = p.parent()?;
    if macos.file_name()?.to_string_lossy() != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_string_lossy() != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension().and_then(|e| e.to_str()) == Some("app") {
        Some(app.to_path_buf())
    } else {
        None
    }
}

fn read_plist_string(path: &std::path::Path, key: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    // Minimal parse: <key>K</key>\n\t<string>V</string>
    let marker = format!("<key>{key}</key>");
    let idx = raw.find(&marker)?;
    let after = &raw[idx + marker.len()..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    let val = after[start..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Parse `codesign -dv` for the running binary. Best-effort; empty on failure.
fn codesign_info(path: &str) -> (String, String, bool) {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        return ("not_applicable".into(), String::new(), false);
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("codesign")
            .args(["-dv", "--verbose=4", path])
            .output();
        let Ok(out) = out else {
            return ("unknown".into(), String::new(), false);
        };
        // codesign writes to stderr
        let text = String::from_utf8_lossy(&out.stderr);
        let mut identifier = String::new();
        let mut signature = String::new();
        let mut team = String::new();
        let mut authority = String::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Identifier=") {
                identifier = v.trim().into();
            } else if let Some(v) = line.strip_prefix("Signature=") {
                signature = v.trim().into();
            } else if let Some(v) = line.strip_prefix("TeamIdentifier=") {
                team = v.trim().into();
            } else if authority.is_empty() {
                // First Authority= is the leaf signer (e.g. "Lumen Local Codesign"
                // or "Apple Development: …"). codesign prints the chain top-down.
                if let Some(v) = line.strip_prefix("Authority=") {
                    authority = v.trim().into();
                }
            }
        }
        // codesign prints the flag label literally, e.g. `flags=0x2(adhoc)` vs
        // `flags=0x0(none)` — match that rather than a fragile `0x2` substring
        // (which would also hit 0x20000 etc.). No leaf Authority + no team is the
        // other adhoc tell.
        let adhoc = signature.eq_ignore_ascii_case("adhoc")
            || text.contains("(adhoc)")
            || (authority.is_empty() && team == "not set");
        let kind = if adhoc {
            "adhoc".into()
        } else if !authority.is_empty() {
            // Show the signer name — the thing that actually keeps TCC stable.
            authority
        } else if !team.is_empty() && team != "not set" {
            format!("signed:{team}")
        } else if !signature.is_empty() {
            signature
        } else {
            "unknown".into()
        };
        (kind, identifier, adhoc)
    }
}

#[tauri::command]
pub async fn get_permission_status() -> Result<PermissionDto, String> {
    #[cfg(target_os = "macos")]
    {
        let p = MacPermissions;
        let s = p.status().await.map_err(|e| e.to_string())?;
        Ok(map_status(s))
    }
    #[cfg(target_os = "windows")]
    {
        use lumen_platform::PermissionState;
        Ok(map_status(PermissionStatus {
            // MSIX/Store builds can query the declared microphone capability
            // without opening the device. A successful real capture remains
            // the fallback for unpackaged builds.
            microphone: windows_microphone_state(),
            accessibility: PermissionState::Restricted,
        }))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        use lumen_platform::PermissionState;
        Ok(map_status(PermissionStatus {
            microphone: PermissionState::NotDetermined,
            accessibility: PermissionState::Restricted,
        }))
    }
}

/// Lightweight poll for wizard / settings (same as get; named for intent).
#[tauri::command]
pub async fn poll_permissions() -> Result<PermissionDto, String> {
    get_permission_status().await
}

#[tauri::command]
pub async fn open_microphone_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        MacPermissions
            .open_microphone_settings()
            .await
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
            .arg("ms-settings:privacy-microphone")
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("open Windows microphone settings: {error}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("opening microphone settings is not implemented on this platform".into())
    }
}

#[tauri::command]
pub async fn open_accessibility_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        MacPermissions
            .open_accessibility_settings()
            .await
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("辅助功能设置仅适用于 macOS。Windows 使用键盘/粘贴插入，无需该权限。".into())
    }
}

/// User-initiated: try once to appear in the Accessibility list, then open Settings.
/// Does **not** grant permission — user must flip the switch for *this* process path.
#[tauri::command]
pub async fn request_accessibility_access() -> Result<PermissionDto, String> {
    #[cfg(target_os = "macos")]
    {
        let before = is_accessibility_trusted();
        if !before {
            // May register the app in the list (often no dialog on modern macOS).
            let _ = prompt_accessibility();
        }
        let _ = MacPermissions.open_accessibility_settings().await;
        let after = is_accessibility_trusted();
        if !after {
            // Settings is open; float a draggable app icon instead of sending
            // the user hunting through Finder with the “+” button.
            present_accessibility_drag_overlay();
        } else {
            dismiss_accessibility_drag_overlay();
        }
        tracing::info!(
            before,
            after,
            process = %process_hint(),
            path = %process_path(),
            "accessibility request (open Settings; drag overlay if still untrusted)"
        );
        get_permission_status().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        get_permission_status().await
    }
}

#[tauri::command]
pub async fn dismiss_accessibility_drag_overlay_cmd() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    dismiss_accessibility_drag_overlay();
    Ok(())
}

#[tauri::command]
pub async fn request_microphone_access(
    state: State<'_, AppState>,
) -> Result<PermissionDto, String> {
    if state.audio.is_recording() {
        mark_microphone_capture_started();
    } else {
        match state.audio.start() {
            Ok(()) => {
                mark_microphone_capture_started();
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                let _ = state.audio.stop();
            }
            Err(e) => {
                #[cfg(target_os = "windows")]
                // AudioCapture reports device, stream, and worker failures in
                // addition to permission failures. Until those are classified
                // separately, a failed probe must remain unknown rather than
                // falsely claiming the user denied microphone access.
                WINDOWS_MICROPHONE_STATE.store(0, Ordering::SeqCst);
                tracing::warn!(error = %e, "mic probe start failed");
            }
        }
    }
    #[cfg(target_os = "macos")]
    let _ = MacPermissions.request_microphone().await;
    get_permission_status().await
}

/// Startup: log only — do not open Settings or force system prompts.
pub fn bootstrap_permissions() {
    #[cfg(target_os = "macos")]
    {
        let trusted = is_accessibility_trusted();
        let path = process_path();
        let (kind, id, adhoc) = codesign_info(&path);
        tracing::info!(
            accessibility_trusted = trusted,
            process = %process_hint(),
            path = %path,
            codesign_kind = %kind,
            codesign_identifier = %id,
            codesign_adhoc = adhoc,
            "permission bootstrap (no auto Settings open)"
        );
        if !trusted {
            tracing::warn!(
            "Accessibility not granted for this process — inject/event-tap need it. Enable in System Settings → Privacy & Security → Accessibility. Adhoc builds need re-enable after each rebuild; then fully quit & reopen."
        );
        }
    }
    #[cfg(target_os = "windows")]
    {
        let microphone = windows_microphone_state();
        tracing::info!(
            path = %process_path(),
            ?microphone,
            "Windows permission bootstrap: package capability checked; capture probe remains available; text insertion uses SendInput"
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    tracing::info!("permission bootstrap unavailable on this platform");
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use lumen_platform::PermissionState;

    fn status(microphone: PermissionState) -> PermissionStatus {
        PermissionStatus {
            microphone,
            accessibility: PermissionState::Restricted,
        }
    }

    #[test]
    fn windows_unknown_microphone_is_not_recordable() {
        let dto = map_status(status(PermissionState::NotDetermined));
        assert_eq!(dto.microphone, "not_determined");
        assert!(!dto.can_record);
    }

    #[test]
    fn windows_granted_microphone_is_recordable() {
        let dto = map_status(status(PermissionState::Granted));
        assert_eq!(dto.microphone, "granted");
        assert!(dto.can_record);
    }

    #[test]
    fn windows_probe_failure_is_not_reported_as_permission_denied() {
        assert!(matches!(
            windows_microphone_state_from_code(0),
            PermissionState::NotDetermined
        ));
        assert!(matches!(
            windows_microphone_state_from_code(2),
            PermissionState::NotDetermined
        ));
    }

    #[test]
    fn windows_app_capability_status_maps_to_permission_state() {
        use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;

        assert!(matches!(
            windows_microphone_state_from_capability_code(AppCapabilityAccessStatus::Allowed.0),
            Some(PermissionState::Granted)
        ));
        assert!(matches!(
            windows_microphone_state_from_capability_code(
                AppCapabilityAccessStatus::DeniedByUser.0
            ),
            Some(PermissionState::Denied)
        ));
        assert!(matches!(
            windows_microphone_state_from_capability_code(
                AppCapabilityAccessStatus::DeniedBySystem.0
            ),
            Some(PermissionState::Restricted)
        ));
        assert!(matches!(
            windows_microphone_state_from_capability_code(
                AppCapabilityAccessStatus::UserPromptRequired.0
            ),
            Some(PermissionState::NotDetermined)
        ));
        assert_eq!(
            windows_microphone_state_from_capability_code(
                AppCapabilityAccessStatus::NotDeclaredByApp.0
            ),
            None
        );
    }
}
