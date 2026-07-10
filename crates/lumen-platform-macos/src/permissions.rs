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
            accessibility: ax_status(),
        })
    }

    async fn request_microphone(&self) -> Result<PermissionState, PlatformError> {
        // Opening the default input stream triggers the system prompt when Info.plist
        // has NSMicrophoneUsageDescription and status is NotDetermined.
        #[cfg(target_os = "macos")]
        {
            // Probe: list devices / try short cpal open is done by caller.
            // Here we only open System Settings if already denied.
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
        // Prefer modern Settings URL, fall back to legacy pane.
        if open_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        )
        .is_err()
        {
            open_url(
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility",
            )?;
        }
        Ok(())
    }

    async fn open_microphone_settings(&self) -> Result<(), PlatformError> {
        open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
    }
}

fn ax_status() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        if ax_is_process_trusted() {
            PermissionState::Granted
        } else {
            // Prompt once with options so the app appears in the list.
            let _ = ax_is_process_trusted_with_prompt();
            if ax_is_process_trusted() {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::Denied
    }
}

fn mic_status() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        // Heuristic: if we can open the default input device config, treat as usable.
        // Precise TCC status needs AVFoundation; cpal open will trigger the real prompt.
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
fn ax_is_process_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
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
