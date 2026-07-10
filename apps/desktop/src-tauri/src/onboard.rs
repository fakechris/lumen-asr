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
        max_step_stage_b: 6, // full wizard: 0…6
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

/// Finish onboarding (full wizard).
#[tauri::command]
pub fn complete_onboarding(
    state: State<'_, AppState>,
    complete_all: Option<bool>,
) -> Result<OnboardingStateDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    let _ = complete_all;
    guard.onboarding.completed = true;
    guard.onboarding.skipped = false;
    guard.onboarding.step = 6;
    guard.onboarding.completed_at = Some(chrono::Utc::now().to_rfc3339());
    guard.save()?;
    tracing::info!("onboarding completed");
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
