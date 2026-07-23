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
pub async fn request_microphone_access(
    state: State<'_, AppState>,
) -> Result<PermissionDto, String> {
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
