use crate::config::{AppConfig, AsrServiceConfig};
use crate::context_capture::{CorrectorContextProjection, StageUsageInput};
use crate::corrector_svc::{
    corrector_outcome_identity, dictionary_context, dictionary_run_identity,
    run_correct_with_intent_and_context, run_identity,
};
use crate::dictation::{canonical_asr_provider, engine_kind_for_provider};
use crate::session_debug::{self, SessionDebugMeta};
use crate::AppState;
use lumen_asr::{model_identity_from_path, AsrResult, EngineKind, QwenShadowStatus};
use lumen_core::SessionRecord;
use lumen_corrector::CorrectorFallbackReason;
use lumen_platform_macos::FrontmostTarget;
use lumen_prompts::IntentSpec;
use lumen_store::{
    AttemptStatus, ContextStageUsage, DictationAttemptRecord, EnhancementMode, PipelineIdentity,
    PipelineIssueKind, PipelineStage, PipelineStageIssue,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

pub(crate) fn build_pipeline_identity(
    state: &AppState,
    config: &AppConfig,
    engine_kind: EngineKind,
    asr_engine: &str,
    corrector_engine: &str,
    intent: IntentSpec,
) -> PipelineIdentity {
    let corrector = run_identity(config, intent);
    let (asr_model, asr_model_revision) =
        active_asr_model_identity(state, &config.asr, engine_kind);
    PipelineIdentity {
        schema_version: 2,
        asr_provider: canonical_asr_provider(&config.asr.provider),
        asr_engine: asr_engine.to_owned(),
        asr_model,
        asr_model_revision,
        corrector_provider: corrector.provider,
        corrector_engine: corrector_engine.to_owned(),
        corrector_model: corrector.model,
        prompt_hash: corrector.prompt_hash,
        prompt_hash_algorithm: corrector.prompt_hash_algorithm,
        temperature: corrector.temperature,
        dictionary_context_hash: None,
        dictionary_context_hash_algorithm: None,
        dictionary_term_count: 0,
        dictionary_replacement_count: 0,
        enhancement_mode: EnhancementMode::None,
    }
}

fn active_asr_model_identity(
    state: &AppState,
    config: &AsrServiceConfig,
    engine_kind: EngineKind,
) -> (Option<String>, Option<String>) {
    let provider = canonical_asr_provider(&config.provider);
    if !provider.starts_with("local_") {
        let model = match provider.as_str() {
            "openai_audio" | "custom" if config.model.trim().is_empty() => "whisper-1",
            _ => config.model.trim(),
        };
        return ((!model.is_empty()).then(|| model.to_owned()), None);
    }

    match engine_kind_for_provider(&provider).unwrap_or(engine_kind) {
        EngineKind::SenseVoice => state
            .sensevoice
            .lock()
            .map(|engine| model_identity_from_path(engine.model_dir()))
            .unwrap_or_default(),
        EngineKind::Qwen => state
            .qwen
            .lock()
            .map(|engine| model_identity_from_path(engine.model_dir()))
            .unwrap_or_default(),
        EngineKind::Whisper => state
            .whisper
            .lock()
            .map(|engine| model_identity_from_path(engine.model_dir()))
            .unwrap_or_default(),
    }
}

fn identity_enhancement(asr_raw: &str) -> String {
    asr_raw.to_owned()
}

pub(crate) fn apply_asr_result(
    attempt: &mut DictationAttemptRecord,
    result: &AsrResult,
    asr_started: Instant,
) -> (String, String) {
    let asr_wall_ms = elapsed_ms(asr_started);
    if let Some(shadow) = result.diagnostics.qwen_shadow.as_ref() {
        if shadow.status != QwenShadowStatus::Disabled {
            attempt.pipeline_identity.enhancement_mode = EnhancementMode::QwenShadow;
            attempt.pipeline_metrics.enhancement_ms =
                shadow.shadow_total_ms.unwrap_or_default().max(0.0);
        }
        if matches!(
            shadow.status,
            QwenShadowStatus::Failed | QwenShadowStatus::Unavailable
        ) {
            attempt
                .pipeline_metrics
                .stage_issues
                .push(PipelineStageIssue {
                    stage: PipelineStage::Enhancement,
                    kind: PipelineIssueKind::Fallback,
                    message: shadow
                        .fallback_reason
                        .clone()
                        .unwrap_or_else(|| "qwen_shadow_unavailable".into()),
                });
        }
    }
    attempt.pipeline_metrics.asr_ms =
        (asr_wall_ms - attempt.pipeline_metrics.enhancement_ms).max(0.0);
    attempt.pipeline_metrics.asr_worker_reused = result.diagnostics.worker_reused;
    attempt.pipeline_metrics.asr_runtime = Some(result.diagnostics.clone());
    attempt.pipeline_metrics.set_asr_rtf();
    if result.diagnostics.model.is_some() {
        attempt.pipeline_identity.asr_model = result.diagnostics.model.clone();
        attempt.pipeline_identity.asr_model_revision = result.diagnostics.model_revision.clone();
    }

    let asr_raw = result.text.trim().to_string();
    let enhanced_text = identity_enhancement(&asr_raw);
    attempt.asr_raw = Some(asr_raw.clone());
    attempt.asr_enhanced = Some(enhanced_text.clone());
    (asr_raw, enhanced_text)
}

pub(crate) fn mark_attempt_failed(
    attempt: &mut DictationAttemptRecord,
    stage: PipelineStage,
    message: &str,
    pipeline_started: Instant,
) {
    attempt.status = AttemptStatus::Failed;
    attempt.failed_stage = Some(stage);
    attempt.failure_message = Some(message.into());
    attempt.pipeline_metrics.total_ms = elapsed_ms(pipeline_started);
}

pub(crate) struct CorrectionStageOutput {
    pub text: String,
    pub engine: String,
    pub model_applied: bool,
}

fn select_corrector_context(
    use_captured_context: bool,
    intent_allows_context: bool,
    provider: &str,
    captured_context: Option<&CorrectorContextProjection>,
) -> Result<(Option<String>, bool), String> {
    let projection = captured_context
        .map(CorrectorContextProjection::to_model_json)
        .transpose()
        .map_err(|error| error.to_string())?;
    let selected =
        use_captured_context && intent_allows_context && provider != "none" && projection.is_some();
    Ok((projection, selected))
}

fn corrector_context_not_used_reason(
    captured: bool,
    has_projection: bool,
    use_captured_context: bool,
    intent_allows_context: bool,
    provider: &str,
    model_attempted: bool,
) -> Option<String> {
    if !captured {
        Some("captured_context_unavailable".into())
    } else if !has_projection {
        Some("no_projectable_captured_context".into())
    } else if !use_captured_context {
        Some("captured_context_disabled_for_corrector".into())
    } else if !intent_allows_context {
        Some("captured_context_disabled_for_translate".into())
    } else if provider == "none" {
        Some("corrector_model_disabled".into())
    } else if !model_attempted {
        Some("corrector_model_not_invoked".into())
    } else {
        None
    }
}

pub(crate) async fn run_corrector_stage(
    state: &AppState,
    config: &AppConfig,
    enhanced_text: &str,
    intent: IntentSpec,
    captured_context: Option<&CorrectorContextProjection>,
    attempt: &mut DictationAttemptRecord,
) -> Result<CorrectionStageOutput, String> {
    let (entries, dictionary_error) = match state.store.lock() {
        Ok(store_guard) => match store_guard.as_ref() {
            Some(store) => match store.list_dictionary() {
                Ok(entries) => (entries, None),
                Err(error) => {
                    tracing::warn!(error = %error, "dictionary unavailable for corrector");
                    (Vec::new(), Some("dictionary unavailable".to_string()))
                }
            },
            None => (vec![], Some("dictionary store unavailable".to_string())),
        },
        Err(_) => {
            tracing::warn!("dictionary store lock poisoned; continuing without dictionary");
            (Vec::new(), Some("dictionary unavailable".to_string()))
        }
    };
    let projected_dictionary = dictionary_context(&entries);
    let dictionary_requested =
        !projected_dictionary.terms.is_empty() || !projected_dictionary.replacements.is_empty();
    let dictionary_projection =
        serde_json::to_vec(&projected_dictionary).map_err(|error| error.to_string())?;
    let dictionary_captured = dictionary_error.is_none();
    if let Some(message) = dictionary_error {
        attempt
            .pipeline_metrics
            .stage_issues
            .push(PipelineStageIssue {
                stage: PipelineStage::Corrector,
                kind: PipelineIssueKind::InputUnavailable,
                message,
            });
    }

    let run = run_identity(config, intent.clone());
    let intent_allows_context = !matches!(intent, IntentSpec::Translate { .. });
    let (context_projection, context_requested) = select_corrector_context(
        config.corrector.use_captured_context,
        intent_allows_context,
        &run.provider,
        captured_context,
    )?;
    let capture_id = attempt
        .pipeline_inputs
        .context
        .as_ref()
        .map(|input| input.capture_id);
    let captured_context_available = attempt
        .pipeline_inputs
        .context
        .as_ref()
        .is_some_and(|input| input.source_presence_bitmap != 0);
    let context_sources = captured_context
        .map(CorrectorContextProjection::source_names)
        .filter(|sources| !sources.is_empty())
        .unwrap_or_else(|| vec!["captured_context".into()]);

    // Persist every exact payload before a provider can receive it. If this
    // preflight fails, continue correction without that input rather than
    // creating an unauditable disclosure.
    let mut context_selected = context_requested;
    let mut context_provenance_failed = false;
    let mut context_usage = match state.context.record_stage_usage(StageUsageInput {
        capture_id,
        attempt_id: attempt.id,
        stage: PipelineStage::Corrector,
        sources: context_sources.clone(),
        projection: context_selected
            .then_some(context_projection.as_deref())
            .flatten()
            .map(str::as_bytes),
        captured: captured_context_available,
        selected: context_selected,
        consumed: false,
        sent: false,
        not_used_reason: corrector_context_not_used_reason(
            captured_context_available,
            context_projection.is_some(),
            config.corrector.use_captured_context,
            intent_allows_context,
            &run.provider,
            true,
        ),
    }) {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(error = %error, "failed to persist captured-context provenance");
            context_selected = false;
            context_provenance_failed = true;
            attempt
                .pipeline_metrics
                .stage_issues
                .push(PipelineStageIssue {
                    stage: PipelineStage::Corrector,
                    kind: PipelineIssueKind::InputUnavailable,
                    message: "captured-context provenance unavailable".into(),
                });
            ContextStageUsage {
                stage: PipelineStage::Corrector,
                sources: context_sources,
                captured: captured_context_available,
                not_used_reason: Some("captured_context_provenance_persistence_failed".into()),
                ..ContextStageUsage::default()
            }
        }
    };

    let mut dictionary_selected = dictionary_requested;
    let mut dictionary_provenance_failed = false;
    let mut dictionary_usage = match state.context.record_stage_usage(StageUsageInput {
        capture_id,
        attempt_id: attempt.id,
        stage: PipelineStage::Corrector,
        sources: vec!["personal_dictionary".into()],
        projection: dictionary_selected.then_some(dictionary_projection.as_slice()),
        captured: dictionary_captured,
        selected: dictionary_selected,
        consumed: false,
        sent: false,
        not_used_reason: if !dictionary_captured {
            Some("personal_dictionary_unavailable".into())
        } else if !dictionary_selected {
            Some("no_personal_dictionary_context".into())
        } else {
            None
        },
    }) {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(error = %error, "failed to persist corrector input provenance");
            dictionary_selected = false;
            dictionary_provenance_failed = true;
            attempt
                .pipeline_metrics
                .stage_issues
                .push(PipelineStageIssue {
                    stage: PipelineStage::Corrector,
                    kind: PipelineIssueKind::InputUnavailable,
                    message: "corrector input provenance unavailable".into(),
                });
            ContextStageUsage {
                stage: PipelineStage::Corrector,
                sources: vec!["personal_dictionary".into()],
                captured: dictionary_captured,
                not_used_reason: Some("dictionary_provenance_persistence_failed".into()),
                ..ContextStageUsage::default()
            }
        }
    };

    let effective_entries = if dictionary_selected {
        entries.as_slice()
    } else {
        &[]
    };
    let dictionary = dictionary_run_identity(effective_entries);
    attempt.pipeline_identity.dictionary_context_hash = Some(dictionary.hash);
    attempt.pipeline_identity.dictionary_context_hash_algorithm =
        Some(dictionary.hash_algorithm.into());
    attempt.pipeline_identity.dictionary_term_count = dictionary.term_count;
    attempt.pipeline_identity.dictionary_replacement_count = dictionary.replacement_count;

    let corrector_started = Instant::now();
    let result = run_correct_with_intent_and_context(
        config,
        enhanced_text,
        effective_entries,
        intent,
        context_selected
            .then_some(context_projection.as_deref())
            .flatten(),
    )
    .await;
    attempt.pipeline_metrics.corrector_ms = elapsed_ms(corrector_started);
    let text = result.text.trim().to_string();
    // `sent` means the model adapter was invoked with this projection. A
    // timeout or provider rejection still counts as sent; local preprocessing
    // and model-construction failures do not.
    let model_attempted = run.provider != "none"
        && !matches!(
            result.fallback_reason.as_ref(),
            Some(CorrectorFallbackReason::BuildFailed)
        );
    context_usage.selected = context_selected;
    context_usage.consumed = context_selected && model_attempted;
    context_usage.sent = context_selected && model_attempted;
    if !context_provenance_failed {
        context_usage.not_used_reason = corrector_context_not_used_reason(
            captured_context_available,
            context_projection.is_some(),
            config.corrector.use_captured_context,
            intent_allows_context,
            &run.provider,
            model_attempted,
        );
    }
    attempt.pipeline_inputs.stage_usages.push(context_usage);

    dictionary_usage.selected = dictionary_selected;
    // Replacement rules are applied locally during preprocess even when no
    // provider runs or model construction fails.
    dictionary_usage.consumed = dictionary_selected;
    dictionary_usage.sent = dictionary_selected && model_attempted;
    if !dictionary_provenance_failed {
        dictionary_usage.not_used_reason = if !dictionary_captured {
            Some("personal_dictionary_unavailable".into())
        } else if !dictionary_selected {
            Some("no_personal_dictionary_context".into())
        } else if !model_attempted {
            Some("corrector_model_not_invoked".into())
        } else {
            None
        };
    }
    attempt.pipeline_inputs.stage_usages.push(dictionary_usage);
    let outcome_identity = corrector_outcome_identity(&run, result.model_applied);
    let engine = outcome_identity.engine;
    attempt.corrected = Some(text.clone());
    attempt.pipeline_identity.corrector_engine = engine.clone();
    attempt.pipeline_metrics.corrector_fallback = outcome_identity.fallback;
    if attempt.pipeline_metrics.corrector_fallback {
        let reason = result
            .fallback_reason
            .map(|value| value.as_str())
            .unwrap_or("model_not_applied");
        attempt
            .pipeline_metrics
            .stage_issues
            .push(PipelineStageIssue {
                stage: PipelineStage::Corrector,
                kind: PipelineIssueKind::Fallback,
                message: reason.into(),
            });
    }
    tracing::info!(
        attempt_id = %attempt.id,
        corrected_chars = text.chars().count(),
        corrector_engine = %engine,
        model_applied = result.model_applied,
        "corrector result"
    );

    Ok(CorrectionStageOutput {
        text,
        engine,
        model_applied: result.model_applied,
    })
}

pub(crate) fn persist_attempt(
    state: &AppState,
    save: bool,
    session: &SessionRecord,
    attempt: DictationAttemptRecord,
) -> Result<DictationAttemptRecord, String> {
    if !save {
        return Ok(attempt);
    }
    let store_guard = state
        .store
        .lock()
        .map_err(|_| "store lock poisoned".to_string())?;
    let Some(store) = store_guard.as_ref() else {
        return Ok(attempt);
    };
    store
        .save_session_and_append_attempt(session, attempt)
        .map_err(|error| error.to_string())
}

pub(crate) struct AttemptDebug<'a> {
    pub target: Option<&'a FrontmostTarget>,
    pub frontmost_before_insert: Option<String>,
    pub sample_rate_capture: u32,
    pub num_samples_capture: usize,
    pub samples_asr: &'a [f32],
    pub rms: f32,
    pub peak: f32,
    pub notes: Vec<String>,
}

pub(crate) fn write_attempt_debug(
    session: &mut SessionRecord,
    attempt: &DictationAttemptRecord,
    debug: AttemptDebug<'_>,
) {
    let debug_dir = session_debug::new_session_dir(&session.id.to_string());
    let meta = SessionDebugMeta {
        session_id: session.id.to_string(),
        attempt_id: attempt.id.to_string(),
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        target_app: debug.target.and_then(|target| target.name.clone()),
        target_bundle_id: debug.target.and_then(|target| target.bundle_id.clone()),
        frontmost_before_insert: debug.frontmost_before_insert,
        sample_rate_capture: debug.sample_rate_capture,
        num_samples_capture: debug.num_samples_capture,
        sample_rate_asr: 16_000,
        num_samples_asr: debug.samples_asr.len(),
        duration_ms: attempt.pipeline_metrics.audio_duration_ms,
        rms: debug.rms,
        peak: debug.peak,
        asr_engine: attempt.pipeline_identity.asr_engine.clone(),
        corrector_engine: attempt.pipeline_identity.corrector_engine.clone(),
        asr_text: attempt.asr_raw.clone().unwrap_or_default(),
        corrected_text: attempt.corrected.clone().unwrap_or_default(),
        insert_strategy: format!("{:?}", session.insert_strategy),
        insert_ok: attempt.pipeline_metrics.insert_succeeded,
        failed_stage: attempt.failed_stage,
        failure_message: attempt.failure_message.clone(),
        pipeline_metrics: attempt.pipeline_metrics.clone(),
        notes: debug.notes,
    };
    if let Err(error) = session_debug::write_session_debug(&debug_dir, &meta, debug.samples_asr) {
        tracing::warn!(error = %error, "failed to write session debug");
    } else {
        session.audio_path = Some(debug_dir.join("audio_16k.wav").display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_asr_result, corrector_context_not_used_reason, mark_attempt_failed,
        select_corrector_context,
    };
    use crate::context_capture::{CorrectorContextProjection, CorrectorTargetProjection};
    use lumen_asr::{AsrResult, AsrRuntimeDiagnostics, QwenShadowDiagnostics, QwenShadowStatus};
    use lumen_core::AsrEngineId;
    use lumen_store::{
        AttemptStatus, DictationAttemptRecord, EnhancementMode, PipelineIssueKind, PipelineStage,
    };
    use std::time::Instant;
    use uuid::Uuid;

    fn captured_projection() -> CorrectorContextProjection {
        CorrectorContextProjection {
            schema_version: 1,
            target: Some(CorrectorTargetProjection {
                app_name: Some("TextEdit".into()),
                ..CorrectorTargetProjection::default()
            }),
            ..CorrectorContextProjection::default()
        }
    }

    #[test]
    fn captured_context_upload_requires_both_opt_in_and_a_model_provider() {
        let projection = captured_projection();
        for (enabled, intent_allows_context, provider, present, expected_selected) in [
            (false, true, "minimax", true, false),
            (true, true, "none", true, false),
            (true, true, "minimax", false, false),
            (true, true, "minimax", true, true),
            (true, false, "minimax", true, false),
        ] {
            let (json, selected) = select_corrector_context(
                enabled,
                intent_allows_context,
                provider,
                present.then_some(&projection),
            )
            .unwrap();
            assert_eq!(json.is_some(), present);
            assert_eq!(
                selected, expected_selected,
                "enabled={enabled} intent_allows_context={intent_allows_context} \
                 provider={provider} present={present}"
            );
        }
    }

    #[test]
    fn context_provenance_reasons_distinguish_every_non_sent_path() {
        assert_eq!(
            corrector_context_not_used_reason(false, false, true, true, "minimax", true).as_deref(),
            Some("captured_context_unavailable")
        );
        assert_eq!(
            corrector_context_not_used_reason(true, false, true, true, "minimax", true).as_deref(),
            Some("no_projectable_captured_context")
        );
        assert_eq!(
            corrector_context_not_used_reason(true, true, false, true, "minimax", true).as_deref(),
            Some("captured_context_disabled_for_corrector")
        );
        assert_eq!(
            corrector_context_not_used_reason(true, true, true, true, "none", false).as_deref(),
            Some("corrector_model_disabled")
        );
        assert_eq!(
            corrector_context_not_used_reason(true, true, true, true, "minimax", false).as_deref(),
            Some("corrector_model_not_invoked")
        );
        assert_eq!(
            corrector_context_not_used_reason(true, true, true, true, "minimax", true),
            None
        );
        assert_eq!(
            corrector_context_not_used_reason(true, true, true, false, "minimax", true).as_deref(),
            Some("captured_context_disabled_for_translate")
        );
    }

    #[test]
    fn apply_asr_result_records_identity_enhancement_and_runtime_evidence() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        attempt.pipeline_metrics.audio_duration_ms = 2_000;
        let result = AsrResult {
            text: "  原始听写  ".into(),
            engine: AsrEngineId::Qwen3Asr,
            language: Some("zh".into()),
            diagnostics: AsrRuntimeDiagnostics {
                worker_reused: Some(true),
                model: Some("Qwen3-ASR-0.6B-8bit".into()),
                model_revision: Some("revision-1".into()),
                qwen_shadow: Some(QwenShadowDiagnostics {
                    status: QwenShadowStatus::Completed,
                    shadow_total_ms: Some(245.0),
                    user_output_changed: false,
                    ..QwenShadowDiagnostics::default()
                }),
                ..AsrRuntimeDiagnostics::default()
            },
        };

        let (raw, enhanced) = apply_asr_result(&mut attempt, &result, Instant::now());

        assert_eq!(raw, "原始听写");
        assert_eq!(enhanced, raw);
        assert_eq!(attempt.asr_raw.as_deref(), Some(raw.as_str()));
        assert_eq!(attempt.asr_enhanced.as_deref(), Some(enhanced.as_str()));
        assert_eq!(attempt.pipeline_metrics.asr_worker_reused, Some(true));
        assert_eq!(
            attempt
                .pipeline_metrics
                .asr_runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_reused),
            Some(true)
        );
        assert_eq!(
            attempt.pipeline_identity.asr_model.as_deref(),
            Some("Qwen3-ASR-0.6B-8bit")
        );
        assert_eq!(
            attempt.pipeline_identity.asr_model_revision.as_deref(),
            Some("revision-1")
        );
        assert_eq!(
            attempt.pipeline_identity.enhancement_mode,
            EnhancementMode::QwenShadow
        );
        assert_eq!(attempt.pipeline_metrics.enhancement_ms, 245.0);
        assert!(attempt.pipeline_metrics.asr_rtf.is_some());
    }

    #[test]
    fn disabled_qwen_shadow_does_not_claim_an_enhancement_stage() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        let result = AsrResult {
            text: "原始听写".into(),
            engine: AsrEngineId::Qwen3Asr,
            language: Some("zh".into()),
            diagnostics: AsrRuntimeDiagnostics {
                qwen_shadow: Some(QwenShadowDiagnostics {
                    status: QwenShadowStatus::Disabled,
                    ..QwenShadowDiagnostics::default()
                }),
                ..AsrRuntimeDiagnostics::default()
            },
        };

        apply_asr_result(&mut attempt, &result, Instant::now());

        assert_eq!(
            attempt.pipeline_identity.enhancement_mode,
            EnhancementMode::None
        );
        assert_eq!(attempt.pipeline_metrics.enhancement_ms, 0.0);
    }

    #[test]
    fn failed_qwen_shadow_records_an_enhancement_fallback() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        let result = AsrResult {
            text: "原始听写".into(),
            engine: AsrEngineId::Qwen3Asr,
            language: Some("zh".into()),
            diagnostics: AsrRuntimeDiagnostics {
                qwen_shadow: Some(QwenShadowDiagnostics {
                    status: QwenShadowStatus::Failed,
                    fallback_reason: Some("shadow_runtime_error".into()),
                    ..QwenShadowDiagnostics::default()
                }),
                ..AsrRuntimeDiagnostics::default()
            },
        };

        apply_asr_result(&mut attempt, &result, Instant::now());

        assert_eq!(
            attempt.pipeline_identity.enhancement_mode,
            EnhancementMode::QwenShadow
        );
        assert_eq!(attempt.pipeline_metrics.stage_issues.len(), 1);
        let issue = &attempt.pipeline_metrics.stage_issues[0];
        assert_eq!(issue.stage, PipelineStage::Enhancement);
        assert_eq!(issue.kind, PipelineIssueKind::Fallback);
        assert_eq!(issue.message, "shadow_runtime_error");
    }

    #[test]
    fn mark_attempt_failed_preserves_forward_text_and_records_failure_stage() {
        let mut attempt = DictationAttemptRecord::new(Uuid::new_v4());
        attempt.asr_raw = Some("raw".into());
        attempt.asr_enhanced = Some("enhanced".into());

        mark_attempt_failed(
            &mut attempt,
            PipelineStage::Corrector,
            "corrector unavailable",
            Instant::now(),
        );

        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert_eq!(attempt.failed_stage, Some(PipelineStage::Corrector));
        assert_eq!(
            attempt.failure_message.as_deref(),
            Some("corrector unavailable")
        );
        assert_eq!(attempt.asr_raw.as_deref(), Some("raw"));
        assert_eq!(attempt.asr_enhanced.as_deref(), Some("enhanced"));
        assert!(attempt.corrected.is_none());
        assert!(attempt.pipeline_metrics.total_ms >= 0.0);
    }
}
