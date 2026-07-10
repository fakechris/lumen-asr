//! Microphone + Accessibility permission checks (macOS).

use crate::open_url;
use async_trait::async_trait;
use lumen_platform::{PermissionState, PermissionStatus, Permissions, PlatformError};

pub struct MacPermissions;

#[async_trait]
impl Permissions for MacPermissions {
    async fn status(&self) -> Result<PermissionStatus, PlatformError> {
        Ok(PermissionStatus {
            microphone: mic_status(),
            accessibility: if is_accessibility_trusted() {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
        })
    }

    async fn request_microphone(&self) -> Result<PermissionState, PlatformError> {
        #[cfg(target_os = "macos")]
        {
            let st = mic_status();
            if st == PermissionState::Denied || st == PermissionState::Restricted {
                let _ = self.open_microphone_settings().await;
            }
            Ok(st)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(PermissionState::Denied)
        }
    }

    async fn open_accessibility_settings(&self) -> Result<(), PlatformError> {
        open_accessibility_settings_urls()
    }

    async fn open_microphone_settings(&self) -> Result<(), PlatformError> {
        // Try modern + legacy URLs.
        if open_url(
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone",
        )
        .is_err()
        {
            open_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
            )?;
        }
        Ok(())
    }
}

/// True when macOS trusts this process for Accessibility (inject / event tap).
pub fn is_accessibility_trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { AXIsProcessTrusted() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Show the system Accessibility trust dialog (once per denial cycle) and return
/// whether the process is trusted after the call.
///
/// macOS does **not** grant AX from an in-app toggle — the user must flip the
/// switch in System Settings. This only ensures we appear in the list.
pub fn prompt_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        if is_accessibility_trusted() {
            return true;
        }
        let _ = ax_is_process_trusted_with_prompt();
        is_accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Prompt if needed, then open System Settings → Privacy → Accessibility.
pub fn ensure_accessibility_onboarding() -> bool {
    let trusted = prompt_accessibility();
    if !trusted {
        let _ = open_accessibility_settings_urls();
    }
    trusted
}

fn open_accessibility_settings_urls() -> Result<(), PlatformError> {
    // macOS Ventura / Sequoia and older System Preferences.
    let urls = [
        "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
    ];
    let mut last_err = None;
    for url in urls {
        match open_url(url) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    // Last resort: open System Settings app.
    if std::process::Command::new("open")
        .arg("-b")
        .arg("com.apple.systempreferences")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| PlatformError::Message("open Accessibility settings failed".into())))
}

fn mic_status() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        match cpal_default_input_ok() {
            true => PermissionState::Granted,
            false => PermissionState::NotDetermined,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::Denied
    }
}

#[cfg(target_os = "macos")]
fn cpal_default_input_ok() -> bool {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    host.default_input_device()
        .and_then(|d| d.default_input_config().ok())
        .is_some()
}

#[cfg(target_os = "macos")]
fn ax_is_process_trusted_with_prompt() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    // kAXTrustedCheckOptionPrompt
    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef() as _) }
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}
