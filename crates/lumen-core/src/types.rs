use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    InProgress,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertStrategy {
    Paste,
    Ax,
    Type,
    CopyOnly,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditSource {
    PreInsertUi,
    PostPasteAx,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictEntryKind {
    Term,
    Replacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictEntrySource {
    Manual,
    Learned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrEngineId {
    SenseVoiceSherpa,
    Whisper,
    Qwen3Asr,
    #[serde(other)]
    Other,
}

impl AsrEngineId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SenseVoiceSherpa => "sensevoice_sherpa",
            Self::Whisper => "whisper",
            Self::Qwen3Asr => "qwen3_asr",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QwenDecodeMode {
    GreedyOnly,
    OfficialFallback,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrTokenEvidence {
    pub chunk_index: u32,
    pub token_index: u32,
    pub token_id: u32,
    pub text: String,
    pub selected_logprob: f64,
    pub entropy: f64,
    pub top1_top2_margin: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QwenRuntimeMetrics {
    pub schema_version: u32,
    pub runtime_version: Option<String>,
    pub decode_mode: QwenDecodeMode,
    pub diagnostics_complete: bool,
    pub fallback_reason: Option<String>,
    pub chunk_count: Option<u32>,
    pub audio_encode_count: Option<u32>,
    pub prompt_prefill_count: Option<u32>,
    pub generated_token_count: Option<u32>,
    pub max_new_tokens: Option<u32>,
    pub finish_reason: Option<String>,
    pub token_evidence_truncated: bool,
    pub audio_feature_ms: Option<f64>,
    pub prompt_prefill_ms: Option<f64>,
    pub greedy_decode_ms: Option<f64>,
    pub worker_total_ms: Option<f64>,
    pub mlx_peak_memory_bytes: Option<u64>,
    pub mlx_active_memory_bytes_before_cleanup: Option<u64>,
    pub mlx_active_memory_bytes_after_cleanup: Option<u64>,
    pub mlx_cache_memory_bytes_after_cleanup: Option<u64>,
    pub process_max_rss_bytes: Option<u64>,
    pub process_user_cpu_ms: Option<f64>,
    pub process_system_cpu_ms: Option<f64>,
}

impl Default for QwenRuntimeMetrics {
    fn default() -> Self {
        Self {
            schema_version: 1,
            runtime_version: None,
            decode_mode: QwenDecodeMode::Unknown,
            diagnostics_complete: false,
            fallback_reason: None,
            chunk_count: None,
            audio_encode_count: None,
            prompt_prefill_count: None,
            generated_token_count: None,
            max_new_tokens: None,
            finish_reason: None,
            token_evidence_truncated: false,
            audio_feature_ms: None,
            prompt_prefill_ms: None,
            greedy_decode_ms: None,
            worker_total_ms: None,
            mlx_peak_memory_bytes: None,
            mlx_active_memory_bytes_before_cleanup: None,
            mlx_active_memory_bytes_after_cleanup: None,
            mlx_cache_memory_bytes_after_cleanup: None,
            process_max_rss_bytes: None,
            process_user_cpu_ms: None,
            process_system_cpu_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrRuntimeDiagnostics {
    /// `Some(false)` means the request paid worker/model cold-start cost.
    /// Engines without a persistent worker leave this unknown.
    pub worker_reused: Option<bool>,
    /// Stable model name without exposing the absolute local filesystem path.
    pub model: Option<String>,
    /// Immutable model revision when the runtime path exposes one.
    pub model_revision: Option<String>,
    pub token_evidence: Vec<AsrTokenEvidence>,
    pub qwen: Option<QwenRuntimeMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectorEngineId {
    Ollama,
    OpenAiCompatible,
    None,
    #[serde(other)]
    Other,
}

impl CorrectorEngineId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "openai_compatible",
            Self::None => "none",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusInfo {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub window_title: Option<String>,
}

/// Persisted session snapshot (maps to `sessions` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub focus: FocusInfo,
    pub asr_raw: Option<String>,
    pub corrected: Option<String>,
    pub pasted: Option<String>,
    pub asr_engine: Option<String>,
    pub corrector_engine: Option<String>,
    pub insert_strategy: InsertStrategy,
    pub audio_path: Option<String>,
    pub status: SessionStatus,
}

impl SessionRecord {
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            focus: FocusInfo::default(),
            asr_raw: None,
            corrected: None,
            pasted: None,
            asr_engine: None,
            corrector_engine: None,
            insert_strategy: InsertStrategy::None,
            audio_path: None,
            status: SessionStatus::InProgress,
        }
    }
}

impl Default for SessionRecord {
    fn default() -> Self {
        Self::new()
    }
}
