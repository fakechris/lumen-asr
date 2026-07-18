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
