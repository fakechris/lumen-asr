//! App settings persisted as TOML under Application Support.

use lumen_platform::default_config_path;
use serde::{Deserialize, Deserializer, Serialize};
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

/// ASR provider selection across local and cloud transcription engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrServiceConfig {
    /// local_sensevoice | local_qwen | local_whisper | openai_audio | …
    pub provider: String,
    /// Legacy/current-engine model directory retained for backward compatibility.
    pub model_dir: String,
    /// Engine-specific paths preserve independent local pipelines across switches.
    pub sensevoice_model_dir: String,
    pub qwen_model_dir: String,
    pub whisper_model_dir: String,
    /// Python executable containing `mlx_qwen3_asr` for the local Qwen engine.
    pub runtime_path: String,
    /// Opt into bounded same-worker Qwen candidate analysis without changing output.
    pub qwen_shadow_enabled: bool,
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
            model_dir: String::new(),
            sensevoice_model_dir: String::new(),
            qwen_model_dir: String::new(),
            whisper_model_dir: String::new(),
            runtime_path: String::new(),
            qwen_shadow_enabled: false,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            language: String::new(),
            timeout_secs: 120,
        }
    }
}

impl AsrServiceConfig {
    fn migrate_legacy_model_dir(&mut self) {
        let legacy = self.model_dir.trim().to_owned();
        if legacy.is_empty() {
            return;
        }
        let legacy_path = std::path::Path::new(&legacy);
        if lumen_asr::sensevoice_ready(legacy_path) {
            if self.sensevoice_model_dir.trim().is_empty() {
                self.sensevoice_model_dir = legacy.clone();
            }
            return;
        }
        if lumen_asr::qwen_ready(legacy_path) {
            if self.qwen_model_dir.trim().is_empty() {
                self.qwen_model_dir = legacy.clone();
            }
            return;
        }
        if lumen_asr::whisper_ready(legacy_path) {
            if self.whisper_model_dir.trim().is_empty() {
                self.whisper_model_dir = legacy.clone();
            }
            return;
        }
        match self.provider.trim().to_ascii_lowercase().as_str() {
            "sensevoice" | "local_sensevoice" if self.sensevoice_model_dir.trim().is_empty() => {
                self.sensevoice_model_dir = legacy.clone();
            }
            "qwen" | "qwen3_asr" | "local_qwen" if self.qwen_model_dir.trim().is_empty() => {
                self.qwen_model_dir = legacy.clone();
            }
            "whisper" | "local_whisper" if self.whisper_model_dir.trim().is_empty() => {
                self.whisper_model_dir = legacy;
            }
            _ => {}
        }
    }

    pub fn model_dir_for(&self, engine: lumen_asr::EngineKind) -> PathBuf {
        let engine_specific = match engine {
            lumen_asr::EngineKind::SenseVoice => self.sensevoice_model_dir.trim(),
            lumen_asr::EngineKind::Qwen => self.qwen_model_dir.trim(),
            lumen_asr::EngineKind::Whisper => self.whisper_model_dir.trim(),
        };
        PathBuf::from(if engine_specific.is_empty() {
            self.model_dir.trim()
        } else {
            engine_specific
        })
    }

    pub fn set_model_dir_for(&mut self, engine: lumen_asr::EngineKind, path: &std::path::Path) {
        self.migrate_legacy_model_dir();
        let value = path.display().to_string();
        match engine {
            lumen_asr::EngineKind::SenseVoice => self.sensevoice_model_dir = value.clone(),
            lumen_asr::EngineKind::Qwen => self.qwen_model_dir = value.clone(),
            lumen_asr::EngineKind::Whisper => self.whisper_model_dir = value.clone(),
        }
        // Older builds still read this field, so keep it pointed at the active engine.
        self.model_dir = value;
    }

    pub fn qwen_python_executable(&self) -> PathBuf {
        let configured = self.runtime_path.trim();
        if !configured.is_empty() {
            return expand_user_path(configured);
        }
        std::env::var_os("LUMEN_QWEN_PYTHON")
            .filter(|value| !value.is_empty())
            .map(|value| expand_user_path(&value.to_string_lossy()))
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }))
    }
}

fn expand_user_path(value: &str) -> PathBuf {
    if value == "~" {
        return lumen_asr::user_home_dir();
    }
    if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return lumen_asr::user_home_dir().join(relative);
    }
    PathBuf::from(value)
}

/// Post-ASR text shaping profile.
#[derive(Debug, Clone, Serialize)]
pub struct OutputConfig {
    /// Default cleanup for SenseVoice and providers without a dedicated profile.
    pub cleanup: String,
    /// Qwen-specific cleanup. Kept separate so switching ASR does not overwrite
    /// the user's lower-resource SenseVoice pipeline.
    pub qwen_cleanup: String,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OutputConfigWire {
    cleanup: Option<String>,
    qwen_cleanup: Option<String>,
    style: Option<String>,
    casing: Option<String>,
    punctuation: Option<String>,
    polish: Option<Vec<String>>,
    custom_enabled: Option<bool>,
    custom_instruction: Option<String>,
}

impl<'de> Deserialize<'de> for OutputConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OutputConfigWire::deserialize(deserializer)?;
        let mut output = Self::default();
        if let Some(value) = wire.cleanup {
            output.cleanup = value;
        }
        // Before Qwen had its own profile, `cleanup` controlled every ASR. Preserve
        // that explicit behavior when an existing config has no Qwen field.
        output.qwen_cleanup = wire.qwen_cleanup.unwrap_or_else(|| output.cleanup.clone());
        if let Some(value) = wire.style {
            output.style = value;
        }
        if let Some(value) = wire.casing {
            output.casing = value;
        }
        if let Some(value) = wire.punctuation {
            output.punctuation = value;
        }
        if let Some(value) = wire.polish {
            output.polish = value;
        }
        if let Some(value) = wire.custom_enabled {
            output.custom_enabled = value;
        }
        if let Some(value) = wire.custom_instruction {
            output.custom_instruction = value;
        }
        Ok(output)
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            cleanup: "medium".into(),
            qwen_cleanup: "light".into(),
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

    pub fn cleanup_level_for_asr_provider(&self, provider: &str) -> lumen_prompts::CleanupLevel {
        if is_qwen_provider(provider) {
            lumen_prompts::CleanupLevel::parse(&self.qwen_cleanup)
                .unwrap_or(lumen_prompts::CleanupLevel::Light)
        } else {
            self.cleanup_level()
        }
    }

    pub fn cleanup_profile_for_asr_provider(&self, provider: &str) -> &'static str {
        if is_qwen_provider(provider) {
            "qwen"
        } else {
            "default"
        }
    }

    pub fn set_cleanup_for_asr_provider(
        &mut self,
        provider: &str,
        value: &str,
    ) -> Result<(), String> {
        let Some(level) = lumen_prompts::CleanupLevel::parse(value) else {
            return Err(format!("unknown cleanup level: {value}"));
        };
        if is_qwen_provider(provider) {
            self.qwen_cleanup = level.as_str().into();
        } else {
            self.cleanup = level.as_str().into();
        }
        Ok(())
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

    pub fn prompt_input_for_asr_provider(
        &self,
        provider: &str,
        intent: lumen_prompts::IntentSpec,
    ) -> lumen_prompts::PromptBuildInput {
        let mut input = self.prompt_input(intent);
        input.cleanup = self.cleanup_level_for_asr_provider(provider);
        input
    }
}

fn is_qwen_provider(provider: &str) -> bool {
    lumen_asr::EngineKind::parse(&provider.trim().to_ascii_lowercase())
        == Some(lumen_asr::EngineKind::Qwen)
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
            model: std::env::var("LUMEN_CORRECTOR_MODEL").unwrap_or_else(|_| "qwen3.5:9b".into()),
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
    let primary_mode = if cfg.is_hold_mode() { "hold" } else { "toggle" };
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
    pub fn corrector_prompt_input(
        &self,
        intent: lumen_prompts::IntentSpec,
    ) -> lumen_prompts::PromptBuildInput {
        self.output
            .prompt_input_for_asr_provider(&self.asr.provider, intent)
    }

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
                    c.asr.migrate_legacy_model_dir();
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
        cfg.asr.model_dir = "/models/custom-sensevoice".into();
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lumen-cfg-{n}.toml"));
        cfg.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.corrector.model, "test-model");
        assert_eq!(loaded.asr.model_dir, "/models/custom-sensevoice");
        assert_eq!(loaded.asr.sensevoice_model_dir, "/models/custom-sensevoice");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn local_engine_model_paths_survive_provider_round_trips() {
        let mut asr = AsrServiceConfig::default();
        asr.model_dir = "/models/original-sensevoice".into();
        asr.set_model_dir_for(
            lumen_asr::EngineKind::Qwen,
            std::path::Path::new("/models/qwen"),
        );
        asr.provider = "local_qwen".into();
        asr.set_model_dir_for(
            lumen_asr::EngineKind::Whisper,
            std::path::Path::new("/models/whisper"),
        );

        assert_eq!(
            asr.model_dir_for(lumen_asr::EngineKind::SenseVoice),
            PathBuf::from("/models/original-sensevoice")
        );
        assert_eq!(
            asr.model_dir_for(lumen_asr::EngineKind::Qwen),
            PathBuf::from("/models/qwen")
        );
        assert_eq!(
            asr.model_dir_for(lumen_asr::EngineKind::Whisper),
            PathBuf::from("/models/whisper")
        );
    }

    #[test]
    fn legacy_model_migration_does_not_reassign_a_known_model_to_another_engine() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let model_dir = std::env::temp_dir().join(format!("lumen-sensevoice-model-{n}"));
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("model.int8.onnx"), b"model").unwrap();
        std::fs::write(model_dir.join("tokens.txt"), b"tokens").unwrap();

        let mut asr = AsrServiceConfig::default();
        asr.model_dir = model_dir.display().to_string();
        asr.migrate_legacy_model_dir();
        asr.provider = "local_qwen".into();
        asr.migrate_legacy_model_dir();

        assert_eq!(asr.sensevoice_model_dir, model_dir.display().to_string());
        assert!(asr.qwen_model_dir.is_empty());
        let _ = std::fs::remove_dir_all(model_dir);
    }

    #[test]
    fn qwen_runtime_path_expands_home_prefix() {
        let mut asr = AsrServiceConfig::default();
        asr.runtime_path = "~/qwen-env/bin/python".into();

        assert_eq!(
            asr.qwen_python_executable(),
            lumen_asr::user_home_dir().join("qwen-env/bin/python")
        );
    }

    #[test]
    fn qwen_shadow_remains_opt_in_for_existing_configs_without_the_new_field() {
        let asr: AsrServiceConfig = toml::from_str(
            r#"
provider = "local_qwen"
runtime_path = "/qwen/bin/python"
"#,
        )
        .unwrap();

        assert!(!asr.qwen_shadow_enabled);
    }

    #[test]
    fn output_cleanup_defaults_are_isolated_by_asr_provider() {
        let output = OutputConfig::default();

        assert_eq!(
            output.cleanup_level_for_asr_provider("local_sensevoice"),
            lumen_prompts::CleanupLevel::Medium
        );
        assert_eq!(
            output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::Light
        );
    }

    #[test]
    fn existing_config_without_qwen_cleanup_preserves_the_previous_cleanup() {
        let config: AppConfig = toml::from_str(
            r#"
[output]
cleanup = "strong"

[asr]
provider = "local_sensevoice"
"#,
        )
        .unwrap();

        assert_eq!(
            config
                .output
                .cleanup_level_for_asr_provider("local_sensevoice"),
            lumen_prompts::CleanupLevel::Strong
        );
        assert_eq!(
            config.output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::Strong
        );
    }

    #[test]
    fn existing_disabled_cleanup_does_not_enable_qwen_correction_on_upgrade() {
        let config: AppConfig = toml::from_str(
            r#"
[output]
cleanup = "none"

[asr]
provider = "local_qwen"
"#,
        )
        .unwrap();

        assert_eq!(
            config.output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::None
        );
    }

    #[test]
    fn cleanup_profiles_can_be_changed_without_cross_contamination() {
        let mut output = OutputConfig::default();

        output
            .set_cleanup_for_asr_provider("local_qwen", "strong")
            .unwrap();
        assert_eq!(
            output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::Strong
        );
        assert_eq!(
            output.cleanup_level_for_asr_provider("local_sensevoice"),
            lumen_prompts::CleanupLevel::Medium
        );

        output
            .set_cleanup_for_asr_provider("local_sensevoice", "none")
            .unwrap();
        assert_eq!(
            output.cleanup_level_for_asr_provider("local_sensevoice"),
            lumen_prompts::CleanupLevel::None
        );
        assert_eq!(
            output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::Strong
        );
    }
}
