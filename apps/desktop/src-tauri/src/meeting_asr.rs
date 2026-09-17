//! Meeting transcription engine selection: local (default) vs cloud.
//!
//! The offline meeting pipeline diarizes locally and hands each speaker turn's
//! audio to an injected [`AsrEngine`]. `local` keeps that engine on-device
//! (SenseVoice, with a Paraformer fallback) — meeting audio never leaves the
//! machine. `cloud` swaps in the online ASR configured under 设置 → 语音识别
//! (e.g. MiniMax ASR), turning every turn into an HTTP request: usually better
//! accuracy on hard audio, at the cost of uploading the recording and paying
//! per audio-second. A ready local engine rides along as a per-turn fallback,
//! so one failed request degrades one turn instead of failing the meeting.

use std::sync::Arc;

use async_trait::async_trait;
use lumen_asr::{AsrEngine, AsrEngineId, AsrError, AsrRequest, AsrResult};

use crate::AppState;

/// Turns in flight while a cloud meeting transcription runs. Bounded well
/// below provider rate limits; the local engines run serially (`1`) because
/// their `transcribe` is CPU-bound.
pub const MEETING_TURN_CONCURRENCY: usize = 4;

/// Engine selection for one offline meeting run, from
/// `meeting.transcribe_engine` plus the cloud credentials configured for
/// dictation.
pub(crate) struct MeetingAsrSelection {
    pub engine: Arc<dyn AsrEngine>,
    /// Value for `MeetingOptions.turn_concurrency`.
    pub turn_concurrency: usize,
}

/// Resolve the meeting transcription engine. `local` (the default, and the
/// fallback for unknown values) returns the on-device engine; `cloud` returns
/// the configured online engine (with a local understudy) and fails with an
/// actionable message when the cloud side is not actually configured.
pub(crate) fn build_meeting_transcribe_engine(
    state: &AppState,
) -> Result<MeetingAsrSelection, String> {
    let (mode, asr_cfg) = {
        let cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        (cfg.meeting.transcribe_engine.clone(), cfg.asr.clone())
    };
    if !mode.eq_ignore_ascii_case("cloud") {
        return Ok(MeetingAsrSelection {
            engine: crate::dictation::build_meeting_asr_engine(state)?,
            turn_concurrency: 1,
        });
    }

    let provider = crate::dictation::canonical_asr_provider(&asr_cfg.provider);
    let provider = provider.as_str();
    // Mirror `asr_status_from`'s readiness rules so an unconfigured cloud
    // selection fails here — loudly — instead of silently degrading turn by turn.
    let cloud_ready = match provider {
        "volcengine" | "minimax" => !asr_cfg.api_key.trim().is_empty(),
        "openai_audio" | "custom" => {
            !asr_cfg.api_key.trim().is_empty() || !asr_cfg.base_url.trim().is_empty()
        }
        _ => false,
    };
    if !cloud_ready {
        return Err(match provider {
            p if p.starts_with("local_") || p.is_empty() => {
                "云端会议转写需要先在「设置 → 语音识别」选择在线 ASR（如 MiniMax ASR）并填写 API Key。".to_string()
            }
            "aliyun_qwen" | "soniox" | "stepfun" | "mimo" => format!(
                "在线 ASR「{provider}」仅预置了 endpoint，尚未接入客户端；请改用 MiniMax ASR、火山引擎或 OpenAI Audio。"
            ),
            _ => "云端会议转写需要在「设置 → 语音识别」填写在线 ASR 的 API Key。".to_string(),
        });
    }

    let primary = crate::dictation::build_cloud_asr_engine(provider, &asr_cfg)?;
    tracing::info!(
        provider,
        turn_concurrency = MEETING_TURN_CONCURRENCY,
        "meeting transcription: cloud ASR enabled (meeting audio will be uploaded to the provider)"
    );
    let engine = match crate::dictation::build_meeting_asr_engine(state) {
        Ok(local) => {
            tracing::info!("meeting cloud turns fall back to the local engine on errors");
            Arc::new(FallbackAsr::new(primary, local))
        }
        // No provisioned local engine: run cloud-only rather than failing —
        // the user asked for cloud, and a fallback they never provisioned
        // cannot be owed to them.
        Err(_) => primary,
    };
    Ok(MeetingAsrSelection {
        engine,
        turn_concurrency: MEETING_TURN_CONCURRENCY,
    })
}

/// Primary-first engine pair: try the primary, and on any error retry the
/// request against the fallback. `AsrEngine: Send + Sync`, so the pair shares
/// the pipeline's per-turn tasks.
pub struct FallbackAsr {
    primary: Arc<dyn AsrEngine>,
    fallback: Arc<dyn AsrEngine>,
}

impl FallbackAsr {
    pub fn new(primary: Arc<dyn AsrEngine>, fallback: Arc<dyn AsrEngine>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl AsrEngine for FallbackAsr {
    fn id(&self) -> AsrEngineId {
        self.primary.id()
    }

    fn is_supported(&self) -> bool {
        self.primary.is_supported() || self.fallback.is_supported()
    }

    // A request must fit whichever engine may actually run it, so the binding
    // (smaller) cap wins; `None` = uncapped.
    fn max_audio_bytes(&self) -> Option<usize> {
        match (
            self.primary.max_audio_bytes(),
            self.fallback.max_audio_bytes(),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        }
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        match self.primary.transcribe(req.clone()).await {
            Ok(result) => Ok(result),
            Err(primary_error) => {
                tracing::warn!(
                    error = %primary_error,
                    "primary ASR failed; falling back to the local engine"
                );
                self.fallback
                    .transcribe(req)
                    .await
                    .map_err(|fallback_error| {
                        tracing::warn!(error = %fallback_error, "local fallback ASR also failed");
                        // The user selected the primary; its error is the
                        // actionable one for them.
                        primary_error
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticAsr {
        id_label: &'static str,
        engine_id: AsrEngineId,
        error: Option<&'static str>,
        cap: Option<usize>,
    }

    impl StaticAsr {
        fn ok(id_label: &'static str, engine_id: AsrEngineId) -> Self {
            Self {
                id_label,
                engine_id,
                error: None,
                cap: None,
            }
        }
    }

    #[async_trait]
    impl AsrEngine for StaticAsr {
        fn id(&self) -> AsrEngineId {
            self.engine_id.clone()
        }

        fn is_supported(&self) -> bool {
            true
        }

        fn max_audio_bytes(&self) -> Option<usize> {
            self.cap
        }

        async fn transcribe(&self, _req: AsrRequest) -> Result<AsrResult, AsrError> {
            match self.error {
                Some(message) => Err(AsrError::Inference(message.to_string())),
                None => {
                    let mut result =
                        AsrResult::new(self.id_label.to_string(), self.engine_id.clone());
                    result.engine_label = self.id_label.to_string();
                    Ok(result)
                }
            }
        }
    }

    fn request() -> AsrRequest {
        AsrRequest::new(vec![0.0; 160], 16_000)
    }

    #[tokio::test]
    async fn primary_success_never_touches_the_fallback() {
        let engine = FallbackAsr::new(
            Arc::new(StaticAsr::ok("cloud", AsrEngineId::Other)),
            Arc::new(StaticAsr::ok("local", AsrEngineId::SenseVoiceSherpa)),
        );
        let result = engine.transcribe(request()).await.unwrap();
        assert_eq!(result.text, "cloud");
    }

    #[tokio::test]
    async fn primary_error_falls_back_to_local_result() {
        let mut primary = StaticAsr::ok("cloud", AsrEngineId::Other);
        primary.error = Some("http: connection reset");
        let engine = FallbackAsr::new(
            Arc::new(primary),
            Arc::new(StaticAsr::ok("local", AsrEngineId::SenseVoiceSherpa)),
        );
        let result = engine.transcribe(request()).await.unwrap();
        assert_eq!(result.text, "local");
    }

    #[tokio::test]
    async fn double_failure_reports_the_primary_error() {
        let mut primary = StaticAsr::ok("cloud", AsrEngineId::Other);
        primary.error = Some("provider rejected request with status 401");
        let mut fallback = StaticAsr::ok("local", AsrEngineId::SenseVoiceSherpa);
        fallback.error = Some("local also broken");
        let engine = FallbackAsr::new(Arc::new(primary), Arc::new(fallback));
        let err = engine.transcribe(request()).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }

    #[test]
    fn identity_follows_primary_and_cap_takes_the_minimum() {
        let mut primary = StaticAsr::ok("cloud", AsrEngineId::Other);
        primary.cap = Some(16_000_000);
        let engine = FallbackAsr::new(
            Arc::new(primary),
            Arc::new(StaticAsr::ok("local", AsrEngineId::SenseVoiceSherpa)),
        );
        assert_eq!(engine.id(), AsrEngineId::Other);
        assert_eq!(engine.max_audio_bytes(), Some(16_000_000));

        let mut uncapped = StaticAsr::ok("cloud", AsrEngineId::Other);
        uncapped.cap = None;
        let mut capped = StaticAsr::ok("local", AsrEngineId::SenseVoiceSherpa);
        capped.cap = Some(8_000_000);
        let engine = FallbackAsr::new(Arc::new(uncapped), Arc::new(capped));
        assert_eq!(engine.max_audio_bytes(), Some(8_000_000));
    }
}
