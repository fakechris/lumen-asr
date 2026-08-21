//! First-run onboarding state (Stage B: welcome + permissions + mic level).

use crate::config::OnboardingConfig;
use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

// Version 4 (Windows) / 2 (macOS): four-step wizard (welcome → permissions →
// hotkey → ready). Completed users of an older version stay completed; an
// unfinished older wizard restarts at Welcome because the step map changed.
const CURRENT_ONBOARDING_VERSION: u32 = if cfg!(target_os = "windows") { 4 } else { 2 };
const LAST_STEP: u32 = 3;

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
    // Finished users keep their completion across copy/step-map bumps.
    let completed = cfg.completed;
    let skipped = !completed && cfg.skipped;
    let show_wizard = !completed && !skipped;
    OnboardingStateDto {
        completed,
        skipped,
        version: CURRENT_ONBOARDING_VERSION,
        // Unfinished older wizards restart at Welcome (step indices changed).
        step: if current { cfg.step.min(LAST_STEP) } else { 0 },
        show_wizard,
        max_step_stage_b: LAST_STEP,
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
    guard.onboarding.step = input.step.min(LAST_STEP);
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
    guard.onboarding.step = LAST_STEP;
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
            step: LAST_STEP,
            ..OnboardingConfig::default()
        };

        let dto = dto_from(&cfg);
        assert!(dto.completed);
        assert!(!dto.show_wizard);
        assert_eq!(dto.step, LAST_STEP);
    }

    #[test]
    fn completed_older_onboarding_stays_closed() {
        let cfg = OnboardingConfig {
            completed: true,
            version: CURRENT_ONBOARDING_VERSION.saturating_sub(1),
            step: 6,
            ..OnboardingConfig::default()
        };

        let dto = dto_from(&cfg);
        assert!(dto.completed);
        assert!(!dto.show_wizard);
        assert_eq!(dto.step, 0);
    }

    #[test]
    fn unfinished_older_onboarding_restarts_at_welcome() {
        let cfg = OnboardingConfig {
            completed: false,
            skipped: false,
            version: CURRENT_ONBOARDING_VERSION.saturating_sub(1),
            step: 5,
            ..OnboardingConfig::default()
        };

        let dto = dto_from(&cfg);
        assert!(!dto.completed);
        assert!(dto.show_wizard);
        assert_eq!(dto.step, 0);
    }
}
