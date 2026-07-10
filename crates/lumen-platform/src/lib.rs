//! Platform capability traits — implemented per OS.

use async_trait::async_trait;
use lumen_core::FocusInfo;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
    Restricted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub microphone: PermissionState,
    pub accessibility: PermissionState,
}

impl PermissionStatus {
    pub fn can_record(&self) -> bool {
        self.microphone == PermissionState::Granted
    }

    /// Full inject path requires accessibility on macOS.
    pub fn can_inject(&self) -> bool {
        self.accessibility == PermissionState::Granted
    }

    /// Product copy-only mode: mic yes, accessibility no.
    pub fn copy_only_ok(&self) -> bool {
        self.can_record() && !self.can_inject()
    }
}

#[async_trait]
pub trait Permissions: Send + Sync {
    async fn status(&self) -> Result<PermissionStatus, PlatformError>;
    async fn request_microphone(&self) -> Result<PermissionState, PlatformError>;
    /// Accessibility usually cannot be granted in-app; open System Settings.
    async fn open_accessibility_settings(&self) -> Result<(), PlatformError>;
    async fn open_microphone_settings(&self) -> Result<(), PlatformError>;
}

#[async_trait]
pub trait FrontmostApp: Send + Sync {
    async fn focus_info(&self) -> Result<FocusInfo, PlatformError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeySpec {
    /// Human-readable, e.g. "RightMeta" or "Cmd+Shift+Space".
    pub combo: String,
    pub toggle: bool,
}

#[async_trait]
pub trait HotkeyListener: Send + Sync {
    async fn start(&self, spec: HotkeySpec) -> Result<(), PlatformError>;
    async fn stop(&self) -> Result<(), PlatformError>;
}

/// Paths under Application Support.
pub fn default_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home)
            .join("Library/Application Support/LumenAsr")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home).join(".lumen-asr")
    }
}

pub fn default_db_path() -> std::path::PathBuf {
    default_data_dir().join("lumen.sqlite")
}

pub fn default_config_path() -> std::path::PathBuf {
    default_data_dir().join("config.toml")
}

pub fn default_models_dir() -> std::path::PathBuf {
    default_data_dir().join("models")
}
