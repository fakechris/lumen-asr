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
    /// Desktop UI preferences (sound cues, …).
    pub ui: UiConfig,
    /// Voice-activity detection (silence auto-stop / trailing trim).
    pub vad: VadConfig,
    /// Speech recognition backend (local or cloud).
    pub asr: AsrServiceConfig,
    /// Local, encrypted context capture used for replay and pipeline provenance.
    pub context: ContextCaptureConfig,
    /// Meeting recording / processing options.
    pub meeting: MeetingConfig,
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
            ui: UiConfig::default(),
            vad: VadConfig::default(),
            asr: AsrServiceConfig::default(),
            context: ContextCaptureConfig::default(),
            meeting: MeetingConfig::default(),
        }
    }
}

/// Meeting-specific processing options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingConfig {
    /// Run the batched LLM cleanup of the verbatim transcript (fillers /
    /// punctuation / Chinese-English code-switch), boundary-preserving. Requires
    /// an LLM corrector; when none is configured the pass is skipped regardless.
    /// Defaults to `true` so a user with an LLM configured gets a cleaned
    /// transcript automatically.
    pub transcript_cleanup: bool,
    /// Opt-in automatic meeting detection: watch for audio-input activity from
    /// apps enabled in the external runtime catalog and *prompt* (never
    /// auto-record) to start a meeting.
    /// Defaults to `false` — the feature ships off so users enable it
    /// deliberately, keeping first-run false positives out of the default
    /// experience. Only runs when this is on AND the OS capability is present.
    pub detection_enabled: bool,
    /// Link a just-started recording to the calendar: look up the current /
    /// imminent (EventKit) event once at recording start, auto-title an
    /// untitled meeting with the event title, and note the attendee names.
    /// Read-only and best-effort — without calendar permission (or with no
    /// matching event) the recording proceeds exactly as before. Defaults to
    /// `true`; macOS-only (a no-op elsewhere).
    pub calendar_link: bool,
    /// Minutes of continuous microphone silence after which an unattended
    /// recording auto-stops (a meeting nobody stopped). `0` disables the
    /// auto-stop entirely. Defaults to `15`. The recorder measures silence from
    /// the mic track only; when it cannot tell (e.g. the system-AEC mic path is
    /// active) nothing is auto-stopped.
    #[serde(default = "default_silence_auto_stop_minutes")]
    pub silence_auto_stop_minutes: u32,
    /// Hard cap on a meeting's wall-clock length in minutes (" forgot to stop
    /// the recording" protection): past this limit the UI warns with a 60-second
    /// countdown, then asks the front-end to stop. `0` disables the cap.
    /// Defaults to `480` (8 hours). Unlike the silence watchdog — which measures
    /// captured samples so a pause pauses it — this cap is wall-clock on
    /// purpose: pausing must not extend it.
    #[serde(default = "default_max_duration_minutes")]
    pub max_duration_minutes: u32,
    /// When a calendar-linked meeting's end time passes, prompt the user to stop
    /// recording (a reminder with a Stop button — never an auto-stop, since a
    /// calendar end is not necessarily the real end). Defaults to `true`; only
    /// meaningful when the meeting was linked to a calendar event.
    #[serde(default = "default_calendar_end_reminder")]
    pub calendar_end_reminder: bool,
    /// Record configured meeting-app process audio (remote participants) as a
    /// second, synchronized meeting track via a Core Audio process tap. Defaults to
    /// `true` but only takes effect when the OS capability (macOS 14.2+) and
    /// the system-audio permission are present — otherwise recording degrades
    /// to mic-only, never fails.
    pub system_audio: bool,
    /// Also feed the system-audio track into the recording-time live
    /// transcript (a second decoding stream on the already-loaded streaming
    /// model). Defaults to `true`; turn off to cut live-preview CPU on weaker
    /// machines — the system track is still recorded to WAV and fully
    /// transcribed offline either way. No effect when `system_audio` is off or
    /// the streaming model is absent.
    pub system_live_preview: bool,
    /// Hide mic-track echo duplicates of remote speech from the final
    /// transcript: without headphones the remote voice plays through the
    /// loudspeaker and is picked up by the mic again, so the same utterance
    /// would appear once per track. Suppression is multi-evidence (timing,
    /// coverage, text similarity, audio cross-correlation) and fail-open, so
    /// headphone meetings and uncertain pairs are untouched. Defaults to
    /// `true`; only meaningful when a system track was recorded.
    pub echo_suppression: bool,
    /// Spread the user's manual speaker marks to unlabelled segments by
    /// voiceprint: after offline reconciliation, a diar cluster the user never
    /// marked whose voice matches (in this meeting) a cluster they *did* mark
    /// inherits that name, so one person's unmarked speech joins their name
    /// instead of becoming a stray "说话人N". Conservative (confident,
    /// clearly-winning match only) and gated on diarization embeddings; a no-op
    /// on builds without them or when the user made no marks. Defaults to `true`.
    pub annotation_voiceprint_spread: bool,
    /// After a meeting is attributed, enroll each **manually named** speaker's
    /// voiceprint into the global identity library so future meetings
    /// auto-identify the same person (cross-meeting propagation). Trusts the
    /// user's name; withholds only on a confident different-name voiceprint
    /// conflict. The library is local-only (never uploaded). Defaults to `true`.
    pub auto_enroll_speakers: bool,
    /// The enrolled voiceprint identity that is *the user themself* ("这是我").
    /// Purely a rendering hint: when live/offline attribution matches this
    /// identity, the UI shows "我" instead of the enrolled name. `None` until
    /// the user marks an identity as self in the voiceprint library.
    pub self_identity_id: Option<String>,
}

impl Default for MeetingConfig {
    fn default() -> Self {
        Self {
            transcript_cleanup: true,
            detection_enabled: false,
            calendar_link: true,
            silence_auto_stop_minutes: default_silence_auto_stop_minutes(),
            max_duration_minutes: default_max_duration_minutes(),
            calendar_end_reminder: default_calendar_end_reminder(),
            system_audio: true,
            system_live_preview: true,
            echo_suppression: true,
            annotation_voiceprint_spread: true,
            auto_enroll_speakers: true,
            self_identity_id: None,
        }
    }
}

/// Default minutes of continuous mic silence before an unattended recording
/// auto-stops. `0` would disable the feature.
fn default_silence_auto_stop_minutes() -> u32 {
    15
}

/// Default wall-clock cap on one meeting recording: 8 hours. `0` would
/// disable the cap.
fn default_max_duration_minutes() -> u32 {
    480
}

/// Default for the calendar-end stop reminder (a prompt, never an auto-stop).
fn default_calendar_end_reminder() -> bool {
    true
}

impl AppConfig {
    fn apply_platform_fallbacks(&mut self) -> bool {
        #[cfg(target_os = "windows")]
        {
            let mut changed = false;
            if self.asr.migrate_windows_shared_model_dirs() {
                changed = true;
            }
            // Alt+Space is commonly reserved by Windows shell/utilities. Fn is
            // handled by keyboard firmware and is not exposed as a registrable
            // Windows global key, so migrate configurations that could never fire.
            if (self.hotkey.toggle.eq_ignore_ascii_case("Alt+Space") && !self.onboarding.completed)
                || crate::hotkey_validate::contains_fn_key(&self.hotkey.toggle)
            {
                self.hotkey.toggle = "Ctrl+Shift+Space".into();
                changed = true;
            }
            for intent in &mut self.hotkey.intents {
                if crate::hotkey_validate::contains_fn_key(&intent.chord) {
                    intent.chord = "Alt+Shift+T".into();
                    intent.enabled = false;
                    changed = true;
                }
            }
            return changed;
        }
        #[cfg(not(target_os = "windows"))]
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextCaptureConfig {
    pub enabled: bool,
    /// metadata | editor | visible. Vision sources are intentionally excluded.
    pub profile: String,
    pub max_chars: usize,
    pub freeze_deadline_ms: u64,
    pub late_deadline_ms: u64,
}

impl Default for ContextCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: "visible".into(),
            max_chars: 200_000,
            freeze_deadline_ms: 500,
            late_deadline_ms: 5_000,
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
    /// Legacy: Python executable for the former MLX Qwen engine. Still read by
    /// the headless mlx-whisper path; the sherpa-onnx Qwen engine ignores it.
    pub runtime_path: String,
    /// Legacy: opt-in flag for the removed MLX Qwen shadow analysis. Tolerated
    /// on load (older configs carry it) but no longer read anywhere.
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

    /// Older Windows builds persisted the Unix-style `~/.lumen/models` path.
    /// Move that *selection* to the canonical LocalAppData root only when the
    /// same engine is already ready there; model files are never moved or
    /// deleted, and custom user paths are never rewritten.
    #[cfg(target_os = "windows")]
    fn migrate_windows_shared_model_dirs(&mut self) -> bool {
        let Some(home) = std::env::var_os("USERPROFILE")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
        else {
            return false;
        };
        let legacy_root = home.join(".lumen").join("models");
        let shared_root = lumen_asr::lumen_models_dir();
        if legacy_root == shared_root {
            return false;
        }

        let mut changed = false;
        let legacy_sensevoice = legacy_root.join("sensevoice");
        let shared_sensevoice = shared_root.join("sensevoice");
        if PathBuf::from(self.sensevoice_model_dir.trim()) == legacy_sensevoice
            && lumen_asr::sensevoice_ready(&shared_sensevoice)
        {
            self.sensevoice_model_dir = shared_sensevoice.display().to_string();
            changed = true;
        }

        let legacy_whisper = legacy_root.join("whisper");
        let shared_whisper = shared_root.join("whisper");
        if PathBuf::from(self.whisper_model_dir.trim()) == legacy_whisper
            && lumen_asr::whisper_ready(&shared_whisper)
        {
            self.whisper_model_dir = shared_whisper.display().to_string();
            changed = true;
        }

        if PathBuf::from(self.model_dir.trim()) == legacy_sensevoice
            && lumen_asr::sensevoice_ready(&shared_sensevoice)
        {
            self.model_dir = shared_sensevoice.display().to_string();
            changed = true;
        } else if PathBuf::from(self.model_dir.trim()) == legacy_whisper
            && lumen_asr::whisper_ready(&shared_whisper)
        {
            self.model_dir = shared_whisper.display().to_string();
            changed = true;
        }
        changed
    }

    pub fn model_dir_for(&self, engine: lumen_asr::EngineKind) -> PathBuf {
        let engine_specific = match engine {
            lumen_asr::EngineKind::SenseVoice => self.sensevoice_model_dir.trim(),
            lumen_asr::EngineKind::Qwen => self.qwen_model_dir.trim(),
            lumen_asr::EngineKind::Whisper => self.whisper_model_dir.trim(),
            // Speech / OpenAiAudio (shared EngineKind superset) have no local
            // model directory in this app.
            _ => "",
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
            // Non-local engines (Speech / OpenAiAudio) carry no model dir.
            _ => return,
        }
        // Older builds still read this field, so keep it pointed at the active engine.
        self.model_dir = value;
    }

    /// Python executable for the Python-backed mlx-whisper worker. Backed by
    /// the legacy `runtime_path` field (which used to configure the MLX Qwen
    /// engine) so existing configs keep working.
    pub fn python_executable(&self) -> PathBuf {
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
        match (wire.cleanup, wire.qwen_cleanup) {
            (Some(cleanup), qwen_cleanup) => {
                output.cleanup = cleanup.clone();
                // Before Qwen had its own profile, an explicit `cleanup` value
                // controlled every ASR and must keep doing so after upgrade.
                output.qwen_cleanup = qwen_cleanup.unwrap_or(cleanup);
            }
            (None, Some(qwen_cleanup)) => output.qwen_cleanup = qwen_cleanup,
            // No prior choice exists, so retain the new-profile default (`light`).
            (None, None) => {}
        }
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
pub struct UiConfig {
    /// Short start/done/error cues on dictation phase transitions.
    pub sounds: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { sounds: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    /// Silence auto-stop for dictation. Off by default: a wrong threshold
    /// cutting a session mid-thought is worse than holding the key longer.
    pub enabled: bool,
    /// rms (built-in) | silero (sherpa-onnx silero VAD, 16 kHz mono); unknown
    /// modes fall back to rms with a warning.
    pub mode: String,
    /// RMS marking speech onset (≈ clear speech at the mic).
    pub start_threshold: f32,
    /// RMS below which input counts as silence once speaking. Matches the
    /// 0.005 (≈ −46 dBFS) gate used by meeting preflight / diar-rs.
    pub end_threshold: f32,
    /// Sustained silence that ends the current dictation.
    pub silence_timeout_ms: u64,
    /// Drop the silent tail before ASR (keeps a 300ms padding).
    pub trim_trailing: bool,
    /// silero mode only: path to `silero_vad.onnx`. Empty = the shared
    /// lumen-models install (`<models>/silero-vad/silero_vad.onnx`), downloaded
    /// on demand the first time silero mode is used. Missing/unloadable model
    /// falls back to rms for that session.
    pub silero_model_path: String,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "rms".into(),
            start_threshold: 0.02,
            end_threshold: 0.005,
            silence_timeout_ms: 1500,
            trim_trailing: true,
            silero_model_path: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrectorConfig {
    /// When false, only rule preprocess + dictionary replacements run.
    pub enabled: bool,
    /// Send a bounded, source-labelled projection of the current app/editor
    /// context to the configured model corrector.
    pub use_captured_context: bool,
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
            use_captured_context: false,
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
    /// For intent=translate: optional translation style / register.
    /// A preset key (`faithful` | `formal` | `casual` | `social`) or free-form
    /// custom text. `#[serde(default)]` → old configs without it load as `None`
    /// (= faithful translation), so this is backward-compatible.
    #[serde(default)]
    pub translate_style: Option<String>,
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
            translate_style: None,
            // Ship enabled: secondary translate chord is a core product path.
            enabled: true,
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            toggle: default_hotkey_toggle().into(),
            show_capsule: true,
            mode: "hold".into(),
            intents: vec![HotkeyIntentConfig::default()],
        }
    }
}

fn default_hotkey_toggle() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Ctrl+Shift+Space"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "Alt+Space"
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
                style: self
                    .translate_style
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
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
    /// After successful insert, observe a pinned pane API or Accessibility field for user edits.
    pub post_paste_capture: bool,
    /// Seconds to watch after paste before giving up.
    pub post_paste_seconds: u64,
    /// Persist full dictated/edited evidence locally. Disabled by default;
    /// hashes and compact review proposals are still stored.
    pub persist_edit_evidence_text: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            auto_promote: false,
            auto_promote_threshold: 3,
            post_paste_capture: true,
            post_paste_seconds: 20,
            persist_edit_evidence_text: false,
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
            let mut cfg = Self::default();
            cfg.apply_platform_fallbacks();
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
                    if c.apply_platform_fallbacks() {
                        if let Err(error) = c.save_to(path) {
                            tracing::warn!(%error, "failed to persist platform fallbacks");
                        }
                    }
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
    fn runtime_path_expands_home_prefix() {
        let mut asr = AsrServiceConfig::default();
        asr.runtime_path = "~/mlx-env/bin/python".into();

        assert_eq!(
            asr.python_executable(),
            lumen_asr::user_home_dir().join("mlx-env/bin/python")
        );
    }

    #[test]
    fn legacy_qwen_shadow_flag_is_tolerated_and_ignored() {
        let asr: AsrServiceConfig = toml::from_str(
            r#"
provider = "local_qwen"
runtime_path = "/qwen/bin/python"
qwen_shadow_enabled = true
"#,
        )
        .unwrap();

        // Legacy field still deserializes; nothing reads it anymore.
        assert!(asr.qwen_shadow_enabled);
        assert_eq!(asr.runtime_path, "/qwen/bin/python");
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
    fn config_without_an_explicit_cleanup_uses_the_new_qwen_default() {
        let config: AppConfig = toml::from_str("[output]").unwrap();

        assert_eq!(
            config.output.cleanup_level_for_asr_provider("local_qwen"),
            lumen_prompts::CleanupLevel::Light
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

    #[test]
    fn meeting_transcript_cleanup_defaults_on() {
        assert!(MeetingConfig::default().transcript_cleanup);
        assert!(AppConfig::default().meeting.transcript_cleanup);
    }

    #[test]
    fn meeting_system_audio_defaults_on_and_can_opt_out() {
        // Defaults on (capability-gated at runtime)…
        assert!(MeetingConfig::default().system_audio);
        assert!(AppConfig::default().meeting.system_audio);
        // …absent from an existing config → still on…
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert!(existing.meeting.system_audio);
        // …and an explicit opt-out is honored.
        let off: AppConfig = toml::from_str(
            r#"
[meeting]
system_audio = false
"#,
        )
        .unwrap();
        assert!(!off.meeting.system_audio);
    }

    #[test]
    fn meeting_echo_suppression_defaults_on_and_can_opt_out() {
        // Defaults on (only meaningful for dual-track meetings)…
        assert!(MeetingConfig::default().echo_suppression);
        assert!(AppConfig::default().meeting.echo_suppression);
        // …absent from an existing config → still on…
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert!(existing.meeting.echo_suppression);
        // …and an explicit opt-out is honored.
        let off: AppConfig = toml::from_str(
            r#"
[meeting]
echo_suppression = false
"#,
        )
        .unwrap();
        assert!(!off.meeting.echo_suppression);
    }

    #[test]
    fn legacy_meeting_mic_aec_setting_is_ignored() {
        let legacy: AppConfig = toml::from_str(
            r#"
[meeting]
mic_aec = true
"#,
        )
        .unwrap();
        let serialized = toml::to_string(&legacy).unwrap();
        assert!(!serialized.contains("mic_aec"));
    }

    #[test]
    fn meeting_calendar_link_defaults_on_and_can_opt_out() {
        // Defaults on (permission-gated at runtime)…
        assert!(MeetingConfig::default().calendar_link);
        assert!(AppConfig::default().meeting.calendar_link);
        // …absent from an existing config → still on…
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert!(existing.meeting.calendar_link);
        // …and an explicit opt-out is honored.
        let off: AppConfig = toml::from_str(
            r#"
[meeting]
calendar_link = false
"#,
        )
        .unwrap();
        assert!(!off.meeting.calendar_link);
    }

    #[test]
    fn meeting_silence_auto_stop_defaults_to_fifteen_minutes_and_can_opt_out() {
        // Ships at 15 minutes…
        assert_eq!(MeetingConfig::default().silence_auto_stop_minutes, 15);
        assert_eq!(AppConfig::default().meeting.silence_auto_stop_minutes, 15);
        // …absent from an existing config → still 15…
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert_eq!(existing.meeting.silence_auto_stop_minutes, 15);
        // …and an explicit value (including 0 = disabled) is honored.
        let off: AppConfig = toml::from_str(
            r#"
[meeting]
silence_auto_stop_minutes = 0
"#,
        )
        .unwrap();
        assert_eq!(off.meeting.silence_auto_stop_minutes, 0);
    }

    #[test]
    fn meeting_calendar_end_reminder_defaults_on_and_can_opt_out() {
        // Defaults on…
        assert!(MeetingConfig::default().calendar_end_reminder);
        assert!(AppConfig::default().meeting.calendar_end_reminder);
        // …absent from an existing config → still on…
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert!(existing.meeting.calendar_end_reminder);
        // …and an explicit opt-out is honored.
        let off: AppConfig = toml::from_str(
            r#"
[meeting]
calendar_end_reminder = false
"#,
        )
        .unwrap();
        assert!(!off.meeting.calendar_end_reminder);
    }

    #[test]
    fn existing_config_without_meeting_section_defaults_cleanup_on() {
        let config: AppConfig = toml::from_str(
            r#"
[asr]
provider = "local_sensevoice"
"#,
        )
        .unwrap();
        assert!(config.meeting.transcript_cleanup);
    }

    #[test]
    fn meeting_self_identity_defaults_none_and_round_trips() {
        // Unset by default and for existing configs without the field…
        assert_eq!(MeetingConfig::default().self_identity_id, None);
        let existing: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = true
"#,
        )
        .unwrap();
        assert_eq!(existing.meeting.self_identity_id, None);
        // …and an explicit value is honored.
        let set: AppConfig = toml::from_str(
            r#"
[meeting]
self_identity_id = "11111111-2222-3333-4444-555555555555"
"#,
        )
        .unwrap();
        assert_eq!(
            set.meeting.self_identity_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    #[test]
    fn meeting_detection_defaults_off_and_opts_in() {
        // Ships off by default…
        assert!(!MeetingConfig::default().detection_enabled);
        assert!(!AppConfig::default().meeting.detection_enabled);
        // …absent from an existing config → still off…
        let existing: AppConfig = toml::from_str(
            r#"
[asr]
provider = "local_sensevoice"
"#,
        )
        .unwrap();
        assert!(!existing.meeting.detection_enabled);
        // …and can be explicitly enabled.
        let enabled: AppConfig = toml::from_str(
            r#"
[meeting]
detection_enabled = true
"#,
        )
        .unwrap();
        assert!(enabled.meeting.detection_enabled);
    }

    #[test]
    fn meeting_transcript_cleanup_can_be_disabled() {
        let config: AppConfig = toml::from_str(
            r#"
[meeting]
transcript_cleanup = false
"#,
        )
        .unwrap();
        assert!(!config.meeting.transcript_cleanup);
    }

    #[test]
    fn context_capture_defaults_to_bounded_text_without_vision_sources() {
        let context = ContextCaptureConfig::default();

        assert!(context.enabled);
        assert_eq!(context.profile, "visible");
        assert_eq!(context.max_chars, 200_000);
    }

    #[test]
    fn existing_config_without_context_section_enables_auditable_capture() {
        let config: AppConfig = toml::from_str(
            r#"
[asr]
provider = "local_qwen"
"#,
        )
        .unwrap();

        assert!(config.context.enabled);
        assert_eq!(config.context.profile, "visible");
    }

    #[test]
    fn existing_corrector_config_keeps_context_upload_opt_in() {
        let config: AppConfig = toml::from_str(
            r#"
[corrector]
enabled = true
provider = "minimax"
"#,
        )
        .unwrap();

        assert!(!config.corrector.use_captured_context);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_migrates_default_hotkey() {
        let mut config = AppConfig::default();
        config.hotkey.toggle = "Alt+Space".into();

        assert!(config.apply_platform_fallbacks());
        assert_eq!(config.hotkey.toggle, "Ctrl+Shift+Space");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_migrates_unobservable_fn_hotkeys() {
        let mut config = AppConfig::default();
        config.hotkey.toggle = "Fn".into();
        config.hotkey.intents[0].chord = "Fn+T".into();

        assert!(config.apply_platform_fallbacks());
        assert_eq!(config.hotkey.toggle, "Ctrl+Shift+Space");
        assert_eq!(config.hotkey.intents[0].chord, "Alt+Shift+T");
        assert!(!config.hotkey.intents[0].enabled);
    }
}
