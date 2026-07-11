//! App settings persisted as TOML under Application Support.

use lumen_platform::default_config_path;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub corrector: CorrectorConfig,
    pub output: OutputConfig,
    pub inject: InjectConfig,
    pub hotkey: HotkeyConfig,
    pub learning: LearningConfig,
    pub onboarding: OnboardingConfig,
    pub audio: AudioConfig,
    /// Speech recognition backend (local or cloud).
    pub asr: AsrServiceConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            corrector: CorrectorConfig::default(),
            output: OutputConfig::default(),
            inject: InjectConfig::default(),
            hotkey: HotkeyConfig::default(),
            learning: LearningConfig::default(),
            onboarding: OnboardingConfig::default(),
            audio: AudioConfig::default(),
            asr: AsrServiceConfig::default(),
        }
    }
}

/// ASR provider selection (local SenseVoice vs cloud transcription).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrServiceConfig {
    /// local_sensevoice | local_whisper | openai_audio | …
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Optional BCP-47 / ISO language hint for cloud ASR.
    pub language: String,
    pub timeout_secs: u64,
}

impl Default for AsrServiceConfig {
    fn default() -> Self {
        Self {
            provider: "local_sensevoice".into(),
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            language: String::new(),
            timeout_secs: 120,
        }
    }
}

/// Post-ASR text shaping profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// none | light | medium | strong — default medium.
    pub cleanup: String,
    /// formal | neutral | casual | very_casual
    pub style: String,
    /// preserve | sentence | lower
    pub casing: String,
    /// preserve | standard | light
    pub punctuation: String,
    /// multi: concise, clarity, reorder, structure, keep_tone
    pub polish: Vec<String>,
    pub custom_enabled: bool,
    pub custom_instruction: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            cleanup: "medium".into(),
            style: "neutral".into(),
            casing: "sentence".into(),
            punctuation: "standard".into(),
            polish: vec![],
            custom_enabled: false,
            custom_instruction: String::new(),
        }
    }
}

impl OutputConfig {
    pub fn cleanup_level(&self) -> lumen_prompts::CleanupLevel {
        lumen_prompts::CleanupLevel::parse(&self.cleanup)
            .unwrap_or(lumen_prompts::CleanupLevel::Medium)
    }

    pub fn style(&self) -> lumen_prompts::Style {
        lumen_prompts::Style::parse(&self.style).unwrap_or_default()
    }

    pub fn casing(&self) -> lumen_prompts::Casing {
        lumen_prompts::Casing::parse(&self.casing).unwrap_or_default()
    }

    pub fn punctuation(&self) -> lumen_prompts::PunctPolicy {
        lumen_prompts::PunctPolicy::parse(&self.punctuation).unwrap_or_default()
    }

    pub fn polish_rules(&self) -> Vec<lumen_prompts::PolishRule> {
        self.polish
            .iter()
            .filter_map(|s| lumen_prompts::PolishRule::parse(s))
            .collect()
    }

    pub fn prompt_input(
        &self,
        intent: lumen_prompts::IntentSpec,
    ) -> lumen_prompts::PromptBuildInput {
        let custom = if self.custom_enabled {
            let t = self.custom_instruction.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        } else {
            None
        };
        lumen_prompts::PromptBuildInput {
            cleanup: self.cleanup_level(),
            style: self.style(),
            casing: self.casing(),
            punctuation: self.punctuation(),
            polish: self.polish_rules(),
            custom,
            intent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OnboardingConfig {
    pub completed: bool,
    pub skipped: bool,
    /// Bump to re-prompt critical setup after product changes.
    pub version: u32,
    /// Current wizard step (0 = welcome …).
    pub step: u32,
    pub completed_at: Option<String>,
}

impl Default for OnboardingConfig {
    fn default() -> Self {
        Self {
            completed: false,
            skipped: false,
            version: 1,
            step: 0,
            completed_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Empty = system default input.
    pub device_name: Option<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { device_name: None }
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
                .unwrap_or_else(|_| "qwen3.5:9b".into()),
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
    /// Independent intent chords (translate, raw, …).
    pub intents: Vec<HotkeyIntentConfig>,
}

/// Secondary hold-to-talk with a different post-ASR intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyIntentConfig {
    pub id: String,
    pub chord: String,
    /// hold | toggle (default hold)
    pub mode: String,
    /// default | translate | raw
    pub intent: String,
    /// For intent=translate
    pub target_language: String,
    pub enabled: bool,
}

impl Default for HotkeyIntentConfig {
    fn default() -> Self {
        Self {
            id: "translate".into(),
            chord: "Alt+Shift+T".into(),
            mode: "hold".into(),
            intent: "translate".into(),
            target_language: "en".into(),
            // Ship enabled: secondary translate chord is a core product path.
            enabled: true,
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            toggle: "Alt+Space".into(),
            show_capsule: true,
            mode: "hold".into(),
            intents: vec![HotkeyIntentConfig::default()],
        }
    }
}

/// Ensure a usable translate intent exists; align mode with primary hotkey.
pub fn ensure_default_intents(cfg: &mut HotkeyConfig) {
    let primary_mode = if cfg.is_hold_mode() {
        "hold"
    } else {
        "toggle"
    };
    if cfg.intents.is_empty() {
        cfg.intents.push(HotkeyIntentConfig {
            mode: primary_mode.into(),
            ..HotkeyIntentConfig::default()
        });
        return;
    }
    for i in &mut cfg.intents {
        // One global hold/toggle setting — per-intent mode confused users.
        i.mode = primary_mode.into();
        if i.chord.trim().is_empty() {
            i.chord = "Alt+Shift+T".into();
        }
        if i.intent.eq_ignore_ascii_case("translate") && i.target_language.trim().is_empty() {
            i.target_language = "en".into();
        }
        // Never rewrite a user-chosen chord (including pure modifiers like Control+Alt).
    }
    if !cfg
        .intents
        .iter()
        .any(|i| i.intent.eq_ignore_ascii_case("translate"))
    {
        cfg.intents.insert(
            0,
            HotkeyIntentConfig {
                mode: primary_mode.into(),
                ..HotkeyIntentConfig::default()
            },
        );
    }
}

// Note: session audio/ASR dumps land in:
//   ~/Library/Application Support/LumenAsr/debug/

impl HotkeyConfig {
    pub fn is_hold_mode(&self) -> bool {
        !matches!(self.mode.to_ascii_lowercase().as_str(), "toggle" | "click")
    }
}

impl HotkeyIntentConfig {
    pub fn to_intent_spec(&self) -> lumen_prompts::IntentSpec {
        match self.intent.to_ascii_lowercase().as_str() {
            "translate" => lumen_prompts::IntentSpec::Translate {
                target_language: if self.target_language.trim().is_empty() {
                    "en".into()
                } else {
                    self.target_language.clone()
                },
            },
            "raw" => lumen_prompts::IntentSpec::Raw,
            _ => lumen_prompts::IntentSpec::Default,
        }
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
            Ok(s) => match toml::from_str::<Self>(&s) {
                Ok(mut c) => {
                    ensure_default_intents(&mut c.hotkey);
                    c
                }
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
