//! First-run onboarding state (Stage B: welcome + permissions + mic level).

use crate::config::OnboardingConfig;
use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

// Version 2 is the first Windows onboarding that verifies the real microphone
// capture result and explains the copy-only output mode. Keep macOS on its
// existing version so this Windows migration never re-prompts Mac users.
const CURRENT_ONBOARDING_VERSION: u32 = if cfg!(target_os = "windows") { 2 } else { 1 };

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
    let current = cfg.version >= CURRENT_ONBOARDING_VERSION;
    let completed = current && cfg.completed;
    let skipped = current && cfg.skipped;
    let show_wizard = !completed && !skipped;
    OnboardingStateDto {
        completed,
        skipped,
        version: CURRENT_ONBOARDING_VERSION,
        // An older completed wizard must restart at Welcome, not reopen on its
        // old final step.
        step: if current { cfg.step } else { 0 },
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
    guard.onboarding.version = CURRENT_ONBOARDING_VERSION;
    guard.onboarding.completed = false;
    guard.onboarding.skipped = false;
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
    guard.onboarding.version = CURRENT_ONBOARDING_VERSION;
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
    guard.onboarding.version = CURRENT_ONBOARDING_VERSION;
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
    guard.onboarding.version = CURRENT_ONBOARDING_VERSION;
    guard.onboarding.step = 0;
    guard.onboarding.completed_at = None;
    guard.save()?;
    Ok(dto_from(&guard.onboarding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_current_onboarding_stays_closed() {
        let cfg = OnboardingConfig {
            completed: true,
            version: CURRENT_ONBOARDING_VERSION,
            step: 6,
            ..OnboardingConfig::default()
        };

        let dto = dto_from(&cfg);
        assert!(dto.completed);
        assert!(!dto.show_wizard);
        assert_eq!(dto.step, 6);
    }

    #[test]
    fn older_onboarding_restarts_at_welcome() {
        let cfg = OnboardingConfig {
            completed: true,
            version: CURRENT_ONBOARDING_VERSION - 1,
            step: 6,
            ..OnboardingConfig::default()
        };

        let dto = dto_from(&cfg);
        assert!(!dto.completed);
        assert!(dto.show_wizard);
        assert_eq!(dto.step, 0);
    }
}
