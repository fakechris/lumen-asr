//! Desktop UI preferences (sound cues, …).

use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfigDto {
    pub sounds: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfigInput {
    pub sounds: Option<bool>,
}

#[tauri::command]
pub fn get_ui_config(state: State<'_, AppState>) -> Result<UiConfigDto, String> {
    let config = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_owned())?;
    Ok(UiConfigDto {
        sounds: config.ui.sounds,
    })
}

#[tauri::command]
pub fn save_ui_config(
    state: State<'_, AppState>,
    input: UiConfigInput,
) -> Result<UiConfigDto, String> {
    let mut config = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_owned())?;
    if let Some(value) = input.sounds {
        config.ui.sounds = value;
    }
    config.save()?;
    Ok(UiConfigDto {
        sounds: config.ui.sounds,
    })
}
