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
