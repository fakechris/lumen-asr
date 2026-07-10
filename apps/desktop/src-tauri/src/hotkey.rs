//! Global toggle hotkey registration (M5).
//!
//! - Normal chords (modifier + key) → `tauri-plugin-global-shortcut`
//! - Modifier-only (e.g. Alt+Shift) → macOS HID flag watcher (`mod_chord`)

use crate::config::HotkeyConfig;
use crate::dictation;
use crate::mod_chord::{self, ModChord};
use crate::AppState;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyDto {
    pub enabled: bool,
    pub toggle: String,
    pub show_capsule: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyInput {
    pub enabled: Option<bool>,
    pub toggle: Option<String>,
    pub show_capsule: Option<bool>,
}

#[tauri::command]
pub fn get_hotkey_config(state: State<'_, AppState>) -> Result<HotkeyDto, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(HotkeyDto {
        enabled: cfg.hotkey.enabled,
        toggle: cfg.hotkey.toggle.clone(),
        show_capsule: cfg.hotkey.show_capsule,
    })
}

#[tauri::command]
pub fn save_hotkey_config(
    app: AppHandle,
    state: State<'_, AppState>,
    input: HotkeyInput,
) -> Result<HotkeyDto, String> {
    {
        let mut guard = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        if let Some(v) = input.enabled {
            guard.hotkey.enabled = v;
        }
        if let Some(v) = input.toggle {
            guard.hotkey.toggle = v;
        }
        if let Some(v) = input.show_capsule {
            guard.hotkey.show_capsule = v;
        }
        guard.save()?;
    }
    reregister(&app)?;
    get_hotkey_config(state)
}

/// Register (or clear) the global toggle shortcut from current config.
pub fn reregister(app: &AppHandle) -> Result<(), String> {
    let cfg = {
        let state = app.state::<AppState>();
        let guard = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        guard.hotkey.clone()
    };
    reregister_with(app, &cfg)
}

pub fn reregister_with(app: &AppHandle, cfg: &HotkeyConfig) -> Result<(), String> {
    // Clear plugin shortcuts + any modifier-only watcher.
    let _ = app.global_shortcut().unregister_all();
    mod_chord::stop_watcher();

    if !cfg.enabled || cfg.toggle.trim().is_empty() {
        tracing::info!("global hotkey disabled");
        return Ok(());
    }

    let toggle = cfg.toggle.trim();

    // Pure modifier chords (⌥⇧, ⌃⇧, …) — global-hotkey cannot parse these.
    if let Some(chord) = ModChord::parse_modifier_only(toggle) {
        let handle = app.clone();
        mod_chord::start_watcher(chord, move || {
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = dictation::toggle_dictation(handle.clone()).await {
                    tracing::warn!(error = %e, "mod-chord toggle failed");
                    dictation::emit_dictation(
                        &handle,
                        dictation::DictationUiEvent::Error { message: e },
                    );
                }
            });
        });
        tracing::info!(hotkey = %toggle, "modifier-only hotkey watcher registered");
        return Ok(());
    }

    let shortcut: Shortcut = toggle
        .parse()
        .map_err(|e| format!("invalid hotkey '{toggle}': {e}"))?;

    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            if event.state != ShortcutState::Pressed {
                return;
            }
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = dictation::toggle_dictation(handle.clone()).await {
                    tracing::warn!(error = %e, "hotkey toggle failed");
                    dictation::emit_dictation(
                        &handle,
                        dictation::DictationUiEvent::Error { message: e },
                    );
                }
            });
        })
        .map_err(|e| format!("register hotkey failed: {e}"))?;

    tracing::info!(hotkey = %toggle, "global hotkey registered");
    Ok(())
}

/// Pause global shortcuts while the UI is capturing a new chord
/// (competitor pattern: click-to-record must not fire dictation).
#[tauri::command]
pub fn pause_hotkeys(app: AppHandle) -> Result<(), String> {
    let _ = app.global_shortcut().unregister_all();
    mod_chord::stop_watcher();
    tracing::info!("global hotkeys paused for capture");
    Ok(())
}

/// Re-register from saved config after capture cancel / complete.
#[tauri::command]
pub fn resume_hotkeys(app: AppHandle) -> Result<(), String> {
    reregister(&app)
}

/// Initial plugin setup helper — call after manage(AppState).
pub fn setup_hotkeys(app: &AppHandle) -> Result<(), String> {
    reregister(app)
}
