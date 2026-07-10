//! macOS platform adapters.
//!
//! M0: scaffolding with safe stubs so the workspace compiles on all hosts.
//! M4: real Microphone / Accessibility / paste-with-restore / AX insert.

use async_trait::async_trait;
use lumen_core::FocusInfo;
use lumen_inject::{InjectError, TextInjectorBackend};
use lumen_platform::{
    FrontmostApp, PermissionState, PermissionStatus, Permissions, PlatformError,
};

/// Permission checks — stub until M4 wires `AVCaptureDevice` + `AXIsProcessTrusted`.
pub struct MacPermissions;

#[async_trait]
impl Permissions for MacPermissions {
    async fn status(&self) -> Result<PermissionStatus, PlatformError> {
        // Conservative stub: not determined until implemented.
        Ok(PermissionStatus {
            microphone: PermissionState::NotDetermined,
            accessibility: PermissionState::NotDetermined,
        })
    }

    async fn request_microphone(&self) -> Result<PermissionState, PlatformError> {
        Err(PlatformError::Message(
            "request_microphone pending M4 native bridge".into(),
        ))
    }

    async fn open_accessibility_settings(&self) -> Result<(), PlatformError> {
        open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    }

    async fn open_microphone_settings(&self) -> Result<(), PlatformError> {
        open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
    }
}

fn open_url(url: &str) -> Result<(), PlatformError> {
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

pub struct MacFrontmost;

#[async_trait]
impl FrontmostApp for MacFrontmost {
    async fn focus_info(&self) -> Result<FocusInfo, PlatformError> {
        // M4: NSWorkspace.frontmostApplication
        Ok(FocusInfo::default())
    }
}

/// Inject backend stub — real paste/AX/type in M4.
pub struct MacTextInjectorBackend;

#[async_trait]
impl TextInjectorBackend for MacTextInjectorBackend {
    async fn paste_with_restore(&self, _text: &str, _preserve: bool) -> Result<(), InjectError> {
        Err(InjectError::NotSupported(
            "macOS paste_with_restore pending M4".into(),
        ))
    }

    async fn ax_insert(&self, _text: &str) -> Result<(), InjectError> {
        Err(InjectError::NotSupported(
            "macOS ax_insert pending M4".into(),
        ))
    }

    async fn type_unicode(&self, _text: &str) -> Result<(), InjectError> {
        Err(InjectError::NotSupported(
            "macOS type_unicode pending M4".into(),
        ))
    }

    async fn copy_only(&self, _text: &str) -> Result<(), InjectError> {
        // Can be implemented with pbcopy even before full inject.
        #[cfg(target_os = "macos")]
        {
            use std::io::Write;
            let mut child = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| InjectError::Other(e.to_string()))?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin
                    .write_all(_text.as_bytes())
                    .map_err(|e| InjectError::Other(e.to_string()))?;
            }
            let status = child
                .wait()
                .map_err(|e| InjectError::Other(e.to_string()))?;
            if status.success() {
                Ok(())
            } else {
                Err(InjectError::Other("pbcopy failed".into()))
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = _text;
            Err(InjectError::NotSupported("not macOS".into()))
        }
    }
}
