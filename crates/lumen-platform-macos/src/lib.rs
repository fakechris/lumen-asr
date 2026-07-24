//! macOS platform adapters: permissions, text injection, frontmost app, hotkeys.

mod focused_field;
mod hotkey_tap;
mod inject;
mod permissions;

pub use focused_field::{focused_text_field_snapshot, FocusedTextFieldSnapshot};
pub use hotkey_tap::{
    start_monitor, start_multi_monitor, stop_monitor, HotkeyBinding, HotkeyEdge, HotkeyMode,
    HotkeySpec,
};
pub use inject::MacTextInjectorBackend;
pub use permissions::{
    ensure_accessibility_onboarding, is_accessibility_trusted, prompt_accessibility, MacPermissions,
};

use async_trait::async_trait;
use lumen_core::FocusInfo;
use lumen_platform::{FrontmostApp, PlatformError};

pub struct MacFrontmost;

#[async_trait]
impl FrontmostApp for MacFrontmost {
    async fn focus_info(&self) -> Result<FocusInfo, PlatformError> {
        Ok(frontmost_focus_info().unwrap_or_default())
    }
}

/// Open System Settings privacy panes.
pub fn open_url(url: &str) -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| PlatformError::Message(e.to_string()))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err(PlatformError::Message("not macOS".into()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrontmostTarget {
    pub name: Option<String>,
    pub bundle_id: Option<String>,
    /// Native process identifier captured with the frontmost application.
    ///
    /// Terminal pane adapters use this only to prove that a multiplexer client
    /// belongs to the selected outer terminal. It is never persisted.
    pub process_id: Option<u32>,
}

/// Best-effort frontmost process name + bundle id.
pub fn frontmost_focus_info() -> Option<FocusInfo> {
    let t = frontmost_target()?;
    Some(FocusInfo {
        app_name: t.name,
        bundle_id: t.bundle_id,
        window_title: None,
    })
}

pub fn frontmost_app_name() -> Option<String> {
    frontmost_target().and_then(|t| t.name)
}

/// Prefer NSWorkspace (fast, process-local); fall back to System Events.
pub fn frontmost_target() -> Option<FrontmostTarget> {
    frontmost_target_native().or_else(frontmost_target_osascript)
}

#[cfg(target_os = "macos")]
fn frontmost_target_native() -> Option<FrontmostTarget> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    // NSWorkspace is main-thread preferred but frontmostApplication is used
    // widely off-main for focus snapshots; treat as best-effort.
    let ws = NSWorkspace::sharedWorkspace();
    let app = ws.frontmostApplication()?;
    let name = app
        .localizedName()
        .map(|s: objc2::rc::Retained<NSString>| s.to_string())
        .filter(|s| !s.is_empty());
    let bundle_id = app
        .bundleIdentifier()
        .map(|s: objc2::rc::Retained<NSString>| s.to_string())
        .filter(|s| !s.is_empty());
    let process_id = u32::try_from(app.processIdentifier())
        .ok()
        .filter(|process_id| *process_id > 0);
    if name.is_none() && bundle_id.is_none() {
        return None;
    }
    Some(FrontmostTarget {
        name,
        bundle_id,
        process_id,
    })
}

#[cfg(not(target_os = "macos"))]
fn frontmost_target_native() -> Option<FrontmostTarget> {
    None
}

fn frontmost_target_osascript() -> Option<FrontmostTarget> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"
tell application "System Events"
  set p to first application process whose frontmost is true
  set n to name of p
  set b to ""
  try
    set b to bundle identifier of p
  end try
  set processId to ""
  try
    set processId to unix id of p as text
  end try
  return n & linefeed & b & linefeed & processId
end tell
"#;
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let mut lines = s.lines();
        let name = lines.next().map(str::trim).filter(|x| !x.is_empty())?;
        let bundle = lines
            .next()
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string());
        let process_id = lines
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse().ok())
            .filter(|process_id| *process_id > 0);
        Some(FrontmostTarget {
            name: Some(name.to_string()),
            bundle_id: bundle,
            process_id,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Activate target app so subsequent key events go to its focused field.
/// Prefer activating an already-running app by bundle id (no new launch).
pub fn activate_target(target: &FrontmostTarget) -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(bid) = target.bundle_id.as_deref() {
            if !bid.is_empty() {
                if activate_by_bundle_id(bid) {
                    return true;
                }
                if std::process::Command::new("open")
                    .args(["-b", bid])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    return true;
                }
            }
        }
        if let Some(name) = target.name.as_deref() {
            if is_self_app_name(name) {
                return false;
            }
            if std::process::Command::new("open")
                .args(["-a", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return true;
            }
            let script = format!(
                r#"tell application "{}" to activate"#,
                name.replace('"', "\\\"")
            );
            return std::process::Command::new("osascript")
                .args(["-e", &script])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
        false
    }
}

#[cfg(target_os = "macos")]
fn activate_by_bundle_id(bundle_id: &str) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    use objc2_foundation::NSString;

    let bid = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bid);
    let Some(app) = apps.firstObject() else {
        return false;
    };
    // Bring existing process forward without relaunching (preserves caret when possible).
    // Empty options: ActivateIgnoringOtherApps is a no-op on modern macOS.
    app.activateWithOptions(NSApplicationActivationOptions::empty())
}

pub fn activate_app_by_name(name: &str) -> bool {
    activate_target(&FrontmostTarget {
        name: Some(name.to_string()),
        bundle_id: None,
        process_id: None,
    })
}

pub fn is_self_app_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("lumen") || n.contains("lumen-asr") || n.contains("lumen asr")
}

pub fn is_self_target(t: &FrontmostTarget) -> bool {
    t.name.as_deref().map(is_self_app_name).unwrap_or(false)
        || t.bundle_id
            .as_deref()
            .map(|b| b.to_ascii_lowercase().contains("lumenasr"))
            .unwrap_or(false)
}
