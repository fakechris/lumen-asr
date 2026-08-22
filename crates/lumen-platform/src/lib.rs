//! Platform facade — the generic capability types (permission state/status,
//! the interactive `Permissions` trait, `PlatformError`) now live in the
//! shared lumen-suite `lumen-platform` crate and are re-exported here so
//! existing `lumen_platform::` call sites are unchanged. Only the product's
//! LumenAsr data paths remain local.

pub use lumen_platform_suite::{PermissionState, PermissionStatus, Permissions, PlatformError};

/// Paths under Application Support.
pub fn default_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home).join("Library/Application Support/LumenAsr")
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) =
            std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())
        {
            std::path::PathBuf::from(local_app_data).join("LumenAsr")
        } else {
            std::path::temp_dir().join("LumenAsr")
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
