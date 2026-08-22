//! Platform capability types (permission state/status, the `Permissions`
//! trait, `PlatformError`) and LumenAsr data paths.

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
            std::env::temp_dir().join("LumenAsr")
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
