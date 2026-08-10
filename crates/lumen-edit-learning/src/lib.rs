//! Durable records shared by edit-learning observation and persistence layers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditSessionState {
    Inserted,
    Observing,
    Editing,
    Quiescent,
    Suspended,
    Finalized,
    Failed,
}

impl EditSessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Observing => "observing",
            Self::Editing => "editing",
            Self::Quiescent => "quiescent",
            Self::Suspended => "suspended",
            Self::Finalized => "finalized",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditSessionRecord {
    pub id: Uuid,
    pub dictation_session_id: Uuid,
    pub attempt_id: Uuid,
    pub surface_key_hash: String,
    pub adapter_kind: String,
    pub state: EditSessionState,
    pub target_app_name: Option<String>,
    pub target_bundle_id: Option<String>,
    pub target_fingerprint_hash: String,
    pub original_text: String,
    pub original_text_hash: String,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub last_edit_at: Option<DateTime<Utc>>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub end_reason: Option<String>,
    pub relocation_attempts: u32,
    pub revision_count: u32,
    pub final_edit_distance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditRevisionRecord {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub ordinal: u32,
    pub observed_at: DateTime<Utc>,
    pub trigger: String,
    pub after_text: String,
    pub after_text_hash: String,
    pub normalized_edit_distance: f64,
    pub locator_confidence: f64,
    pub bounded: bool,
    pub quiescent: bool,
    pub final_revision: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningProposalRecord {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub revision_id: Uuid,
    pub kind: String,
    pub payload_json: String,
    pub confidence: f64,
    pub risk: String,
    pub status: String,
    pub policy_version: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackNotice {
    pub id: Uuid,
    pub edit_session_id: Uuid,
    pub kind: String,
    pub message: String,
    pub proposal_ids: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}
