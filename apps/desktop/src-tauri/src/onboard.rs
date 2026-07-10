//! First-run onboarding state (Stage B: welcome + permissions + mic level).

use crate::config::OnboardingConfig;
use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStateDto {
    pub completed: bool,
    pub skipped: bool,
    pub version: u32,
    pub step: u32,
    /// Wizard should show when not completed and not skipped.
    pub show_wizard: bool,
    pub max_step_stage_b: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStepInput {
    pub step: u32,
}

fn dto_from(cfg: &OnboardingConfig) -> OnboardingStateDto {
    let show_wizard = !cfg.completed && !cfg.skipped;
    OnboardingStateDto {
        completed: cfg.completed,
        skipped: cfg.skipped,
        version: cfg.version,
        step: cfg.step,
        show_wizard,
        max_step_stage_b: 2, // 0 welcome, 1 permissions, 2 mic
    }
}

#[tauri::command]
pub fn get_onboarding_state(state: State<'_, AppState>) -> Result<OnboardingStateDto, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(dto_from(&cfg.onboarding))
}

#[tauri::command]
pub fn set_onboarding_step(
    state: State<'_, AppState>,
    input: OnboardingStepInput,
) -> Result<OnboardingStateDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    guard.onboarding.step = input.step.min(6);
    guard.save()?;
    Ok(dto_from(&guard.onboarding))
}

#[tauri::command]
pub fn skip_onboarding(state: State<'_, AppState>) -> Result<OnboardingStateDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    guard.onboarding.skipped = true;
    guard.onboarding.completed = false;
    guard.save()?;
    tracing::info!("onboarding skipped");
    Ok(dto_from(&guard.onboarding))
}

/// Mark Stage B portion done for now; full wizard (C–E) will extend this later.
/// `complete_all=true` finishes onboarding entirely (user finished step 2 and continues).
#[tauri::command]
pub fn complete_onboarding(
    state: State<'_, AppState>,
    complete_all: Option<bool>,
) -> Result<OnboardingStateDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    let all = complete_all.unwrap_or(true);
    if all {
        guard.onboarding.completed = true;
        guard.onboarding.skipped = false;
        guard.onboarding.step = 6;
        guard.onboarding.completed_at = Some(chrono::Utc::now().to_rfc3339());
    } else {
        // Finished Stage B only — advance step; keep wizard for later stages when shipped.
        guard.onboarding.step = 3;
        // Until C–E exist, treat Stage B finish as completed so user is not stuck.
        guard.onboarding.completed = true;
        guard.onboarding.completed_at = Some(chrono::Utc::now().to_rfc3339());
    }
    guard.save()?;
    tracing::info!(all, "onboarding completed");
    Ok(dto_from(&guard.onboarding))
}

#[tauri::command]
pub fn reopen_onboarding(state: State<'_, AppState>) -> Result<OnboardingStateDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    guard.onboarding.completed = false;
    guard.onboarding.skipped = false;
    guard.onboarding.step = 0;
    guard.onboarding.completed_at = None;
    guard.save()?;
    Ok(dto_from(&guard.onboarding))
}
