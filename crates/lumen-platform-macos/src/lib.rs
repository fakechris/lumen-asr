//! macOS platform adapters: permissions + text injection + frontmost app.

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
}

/// Best-effort frontmost process name + bundle id (System Events).
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

pub fn frontmost_target() -> Option<FrontmostTarget> {
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
  return n & linefeed & b
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
        Some(FrontmostTarget {
            name: Some(name.to_string()),
            bundle_id: bundle,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Activate target app so subsequent ⌘V goes to its focused field.
pub fn activate_target(target: &FrontmostTarget) -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(bid) = target.bundle_id.as_deref() {
            if !bid.is_empty()
                && std::process::Command::new("open")
                    .args(["-b", bid])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            {
                return true;
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

pub fn activate_app_by_name(name: &str) -> bool {
    activate_target(&FrontmostTarget {
        name: Some(name.to_string()),
        bundle_id: None,
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
