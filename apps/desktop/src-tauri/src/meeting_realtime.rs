//! Cloud realtime transcription for meetings: per-track pumps running
//! alongside the recording, collecting timestamped utterances into a
//! sidecar that the offline pipeline uses for gap-filling.

use std::path::Path;
use std::time::Duration;

use crate::realtime_asr::{
    self, MeetingRealtimePump, RealtimeConfig, RealtimeTranscriptSidecar, RealtimeUtterance,
};
use crate::AppState;
use tauri::State;

/// Whether meeting cloud realtime should start for this recording.
pub(crate) fn meeting_realtime_active(state: &AppState) -> bool {
    let (meeting_rt, asr) = {
        let c = state
            .config
            .lock()
            .map(|c| (c.meeting.realtime_transcription, c.asr.clone()))
            .unwrap_or((false, Default::default()));
        c
    };
    meeting_rt
        && realtime_asr::provider_supports_realtime(&asr.provider)
        && asr.keyed_cloud_key_ready()
}

/// Build the RealtimeConfig for a meeting session from the ASR settings.
pub(crate) fn build_meeting_realtime_config(state: &AppState) -> Option<RealtimeConfig> {
    let asr_cfg = state
        .config
        .lock()
        .map(|c| c.asr.clone())
        .unwrap_or_default();
    let provider = crate::dictation::canonical_asr_provider(&asr_cfg.provider);
    if !realtime_asr::provider_supports_realtime(&provider) || !asr_cfg.keyed_cloud_key_ready() {
        return None;
    }
    // Hotwords from the personal dictionary (same as dictation realtime).
    let hotwords = state
        .store
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|s| s.list_dictionary().ok()))
        .flatten()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.term.as_ref())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .take(50)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(RealtimeConfig {
        provider,
        api_key: asr_cfg.api_key.clone(),
        app_id: asr_cfg.volcengine_app_id.clone(),
        base_url: asr_cfg.base_url.clone(),
        language: asr_cfg.language.clone(),
        hotwords,
        model: String::new(),
    })
}

/// Owner of the meeting's realtime pumps (mic + system), stored in AppState.
pub struct MeetingRealtimeOwner {
    pub meeting_id: uuid::Uuid,
    pub mic_pump: MeetingRealtimePump,
    pub system_pump: Option<MeetingRealtimePump>,
}

impl MeetingRealtimeOwner {
    /// Stop all pumps and collect their utterances (bounded) plus each
    /// track's session-health diagnostics for the audio sidecar.
    pub async fn stop_and_collect(
        self,
        deadline: Duration,
    ) -> (
        Vec<RealtimeUtterance>,
        Vec<crate::realtime_asr::RealtimePumpDiagnostics>,
    ) {
        let mic = self.mic_pump.stop_and_collect(deadline).await;
        let mut diagnostics = vec![mic.diagnostics];
        let mut all = mic.utterances;
        if let Some(sys) = self.system_pump {
            let sys = sys.stop_and_collect(deadline).await;
            diagnostics.push(sys.diagnostics);
            all.extend(sys.utterances);
        }
        all.sort_by(|a, b| {
            a.start_seconds
                .partial_cmp(&b.start_seconds)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (all, diagnostics)
    }
}

/// Write the realtime transcript sidecar next to the meeting WAV.
/// Best-effort — a failure logs and the offline pipeline simply runs its
/// full per-turn path (no sidecar = no gap-filling).
pub fn write_sidecar(
    dir: &Path,
    meeting_id: uuid::Uuid,
    provider: &str,
    utterances: &[RealtimeUtterance],
) {
    if utterances.is_empty() {
        return;
    }
    let path = dir.join(format!("{meeting_id}.realtime_transcript.json"));
    let sidecar = RealtimeTranscriptSidecar {
        schema: 1,
        provider: provider.to_string(),
        utterances: utterances.to_vec(),
    };
    match serde_json::to_string_pretty(&sidecar) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&path, json) {
                tracing::warn!(%error, path = %path.display(), "realtime sidecar write failed");
            }
        }
        Err(error) => tracing::warn!(%error, "realtime sidecar serialize failed"),
    }
}

/// Read the realtime transcript sidecar for a meeting (if present).
pub fn read_sidecar(wav_path: &Path) -> Option<RealtimeTranscriptSidecar> {
    let stem = wav_path.file_stem()?.to_string_lossy();
    let uuid_part: String = stem.chars().take(36).collect();
    let path = wav_path.with_file_name(format!("{uuid_part}.realtime_transcript.json"));
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}
