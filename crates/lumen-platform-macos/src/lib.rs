//! macOS platform adapters: permissions + text injection.

mod inject;
mod permissions;

pub use inject::MacTextInjectorBackend;
pub use permissions::MacPermissions;

use async_trait::async_trait;
use lumen_core::FocusInfo;
use lumen_platform::{FrontmostApp, PlatformError};

pub struct MacFrontmost;

#[async_trait]
impl FrontmostApp for MacFrontmost {
    async fn focus_info(&self) -> Result<FocusInfo, PlatformError> {
        #[cfg(target_os = "macos")]
        {
            // Best-effort via `lsappinfo` / osascript — keep lightweight.
            let output = std::process::Command::new("osascript")
                .args([
                    "-e",
                    r#"tell application "System Events" to get name of first application process whose frontmost is true"#,
                ])
                .output()
                .map_err(|e| PlatformError::Message(e.to_string()))?;
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return Ok(FocusInfo {
                        app_name: Some(name),
                        bundle_id: None,
                        window_title: None,
                    });
                }
            }
            Ok(FocusInfo::default())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(FocusInfo::default())
        }
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

/// Frontmost process name via System Events (best-effort).
pub fn frontmost_app_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("osascript")
            .args([
                "-e",
                r#"tell application "System Events" to get name of first application process whose frontmost is true"#,
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Activate an app by process/app name without failing the session if it errors.
/// Used to put focus back on the typing target before paste (must not leave Lumen frontmost).
pub fn activate_app_by_name(name: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        if name.is_empty() {
            return false;
        }
        // Prefer `open -a` which works for most bundle display names / app names.
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
        std::process::Command::new("osascript")
            .args(["-e", &script])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = name;
        false
    }
}

/// True if `name` looks like our own app (never restore focus to ourselves before paste).
pub fn is_self_app_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("lumen") || n.contains("lumen-asr") || n.contains("lumen asr")
}
