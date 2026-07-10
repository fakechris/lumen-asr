//! App settings persisted as TOML under Application Support.

use lumen_platform::default_config_path;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub corrector: CorrectorConfig,
    pub inject: InjectConfig,
    pub hotkey: HotkeyConfig,
    pub learning: LearningConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            corrector: CorrectorConfig::default(),
            inject: InjectConfig::default(),
            hotkey: HotkeyConfig::default(),
            learning: LearningConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrectorConfig {
    /// When false, only rule preprocess + dictionary replacements run.
    pub enabled: bool,
    /// ollama | openai_compatible | none
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

impl Default for CorrectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: std::env::var("LUMEN_CORRECTOR_MODEL")
                .unwrap_or_else(|_| "qwen2.5:7b".into()),
            api_key: std::env::var("LUMEN_CORRECTOR_API_KEY").unwrap_or_default(),
            timeout_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InjectConfig {
    /// auto | paste | type | copy_only
    pub mode: String,
    pub preserve_clipboard: bool,
    /// After stop_and_transcribe, insert into frontmost app when accessibility allows.
    pub auto_insert: bool,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            preserve_clipboard: true,
            auto_insert: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub enabled: bool,
    /// Tauri/global-shortcut format, e.g. "Alt+Space" (set via click-to-record UI)
    pub toggle: String,
    /// Show floating capsule while recording / processing.
    pub show_capsule: bool,
    /// `hold` = push-to-talk (press start, release stop). `toggle` = press to start/stop.
    pub mode: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Option+Space — avoids Spotlight (⌘Space).
            toggle: "Alt+Space".into(),
            // Floating status capsule — required UX feedback while holding hotkey.
            show_capsule: true,
            // Product default: hold to talk (Wispr / Typeless-like).
            mode: "hold".into(),
        }
    }
}

// Note: session audio/ASR dumps land in:
//   ~/Library/Application Support/LumenAsr/debug/

impl HotkeyConfig {
    pub fn is_hold_mode(&self) -> bool {
        !matches!(self.mode.to_ascii_lowercase().as_str(), "toggle" | "click")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningConfig {
    /// When true, promote a replacement after it appears `auto_promote_threshold` times.
    pub auto_promote: bool,
    pub auto_promote_threshold: u32,
    /// After successful paste, poll frontmost field for user edits (best-effort AX/osascript).
    pub post_paste_capture: bool,
    /// Seconds to watch after paste before giving up.
    pub post_paste_seconds: u64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            auto_promote: false,
            auto_promote_threshold: 3,
            post_paste_capture: true,
            post_paste_seconds: 20,
        }
    }
}

impl InjectConfig {
    pub fn to_policy(&self) -> lumen_inject::InsertPolicy {
        use lumen_inject::{InjectMode, InsertPolicy};
        let mode = match self.mode.as_str() {
            "paste" => InjectMode::Paste,
            "type" => InjectMode::Type,
            "copy_only" | "copy" => InjectMode::CopyOnly,
            "ax" => InjectMode::Ax,
            _ => InjectMode::Auto,
        };
        InsertPolicy {
            mode,
            preserve_clipboard: self.preserve_clipboard,
            paste_first: true,
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let path = default_config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &PathBuf) -> Self {
        if !path.exists() {
            let cfg = Self::default();
            if let Err(e) = cfg.save_to(path) {
                tracing::warn!(error = %e, "failed to write default config");
            }
            return cfg;
        }
        match fs::read_to_string(path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "config parse failed, using defaults");
                    Self::default()
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "config read failed, using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&default_config_path())
    }

    pub fn save_to(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, s).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn roundtrip_toml() {
        let mut cfg = AppConfig::default();
        cfg.corrector.model = "test-model".into();
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lumen-cfg-{n}.toml"));
        cfg.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.corrector.model, "test-model");
        let _ = fs::remove_file(path);
    }
}
