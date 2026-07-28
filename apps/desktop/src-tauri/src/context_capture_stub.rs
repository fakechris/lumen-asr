//! Fail-closed context adapter for platforms without a native collector.
//!
//! Windows dictation does not depend on the macOS Accessibility/Vision/
//! Keychain implementation. Context projections that would leave the process
//! are rejected until Windows secure storage and capture sources are wired.

use std::path::Path;
use std::sync::Mutex;

use lumen_store::{ContextInputRef, ContextStageUsage, PipelineStage, Store};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ContextCaptureConfig;

#[derive(Debug, Clone, Default)]
pub struct TargetHint {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub document_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureId(pub Uuid);

#[derive(Debug, Clone)]
pub struct FrozenContextInput {
    pub input_ref: ContextInputRef,
    pub corrector_projection: Option<CorrectorContextProjection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectorContextProjection {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<CorrectorTargetProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<CorrectorEditorProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<CorrectorBrowserProjection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_text: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectorTargetProjection {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub document_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectorEditorProjection {
    pub role: Option<String>,
    pub title: Option<String>,
    pub label: Option<String>,
    pub placeholder: Option<String>,
    pub selected_text: Option<String>,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    pub nearby_before: Option<String>,
    pub nearby_after: Option<String>,
    pub field_text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectorBrowserProjection {
    pub title: Option<String>,
    pub domain: Option<String>,
    pub page_language: Option<String>,
    pub selection_text: Option<String>,
    pub nearby_before: Option<String>,
    pub nearby_after: Option<String>,
}

impl CorrectorContextProjection {
    pub fn source_names(&self) -> Vec<String> {
        let mut sources = Vec::new();
        if self.target.is_some() {
            sources.push("target".into());
        }
        if self.editor.is_some() {
            sources.push("editor_ax".into());
        }
        if self.browser.is_some() {
            sources.push("browser".into());
        }
        if !self.visible_text.is_empty() {
            sources.push("visible_text".into());
        }
        sources
    }

    pub fn to_model_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Clone)]
pub struct ActiveContextCapture {
    pub session_id: Uuid,
    pub capture_id: CaptureId,
}

pub struct ContextRecorder {
    active: Mutex<Option<ActiveContextCapture>>,
}

pub struct StageUsageInput<'a> {
    pub capture_id: Option<Uuid>,
    pub attempt_id: Uuid,
    pub stage: PipelineStage,
    pub sources: Vec<String>,
    pub projection: Option<&'a [u8]>,
    pub captured: bool,
    pub selected: bool,
    pub consumed: bool,
    pub sent: bool,
    pub not_used_reason: Option<String>,
}

impl ContextRecorder {
    pub fn new(_config: &ContextCaptureConfig, _data_dir: &Path) -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    pub fn begin(&self, target_hint: Option<TargetHint>) -> Uuid {
        if let Some(hint) = target_hint {
            let _ = (
                hint.app_name,
                hint.bundle_id,
                hint.window_title,
                hint.document_url,
            );
        }
        let session_id = Uuid::new_v4();
        if let Ok(mut active) = self.active.lock() {
            *active = Some(ActiveContextCapture {
                session_id,
                capture_id: CaptureId(Uuid::new_v4()),
            });
        }
        session_id
    }

    pub fn take_active(&self) -> Option<ActiveContextCapture> {
        self.active.lock().ok().and_then(|mut active| active.take())
    }

    pub fn clear_active(&self) {
        if let Ok(mut active) = self.active.lock() {
            *active = None;
        }
    }

    pub fn record_stage_usage(
        &self,
        input: StageUsageInput<'_>,
    ) -> Result<ContextStageUsage, String> {
        let _ = (input.capture_id, input.attempt_id);
        if input.projection.is_some() {
            return Err(
                "Windows context projection storage is unavailable; input was not disclosed".into(),
            );
        }
        Ok(ContextStageUsage {
            stage: input.stage,
            sources: input.sources,
            captured: input.captured,
            selected: input.selected,
            consumed: input.consumed,
            sent: input.sent,
            not_used_reason: input
                .not_used_reason
                .or_else(|| Some("context_capture_unsupported_on_windows".into())),
            ..ContextStageUsage::default()
        })
    }

    pub fn load_stage_projection(
        &self,
        _capture_id: Option<Uuid>,
        _attempt_id: Uuid,
        _usage: &ContextStageUsage,
    ) -> Result<Vec<u8>, String> {
        Err("Windows context projection storage is unavailable".into())
    }
}

impl ActiveContextCapture {
    pub async fn freeze(
        &self,
        _store: &Mutex<Option<Store>>,
    ) -> Result<FrozenContextInput, String> {
        Ok(FrozenContextInput {
            input_ref: ContextInputRef {
                capture_id: self.capture_id.0,
                revision: 0,
                snapshot_hash: String::new(),
                context_schema_version: 1,
                capture_profile: "disabled".into(),
                source_presence_bitmap: 0,
                source_status_summary: "unavailable".into(),
            },
            corrector_projection: None,
        })
    }

    pub async fn archive(&self, store: &Mutex<Option<Store>>) -> Result<ContextInputRef, String> {
        self.freeze(store).await.map(|frozen| frozen.input_ref)
    }
}
