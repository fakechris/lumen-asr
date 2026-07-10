//! Global hotkey registration.
//!
//! Default product mode: **hold** (push-to-talk) — press starts, release stops.
//! Optional **toggle** mode for click-style chords.
//!
//! On macOS, all chords (including modifier-only like Alt+Shift) use CGEventTap
//! for reliable press/release edges. Falls back to HID flag polling / global
//! shortcut plugin if the tap cannot be created.

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
    pub mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyInput {
    pub enabled: Option<bool>,
    pub toggle: Option<String>,
    pub show_capsule: Option<bool>,
    pub mode: Option<String>,
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
        mode: cfg.hotkey.mode.clone(),
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
        if let Some(v) = input.mode {
            let m = v.to_ascii_lowercase();
            guard.hotkey.mode = if m == "toggle" || m == "click" {
                "toggle".into()
            } else {
                "hold".into()
            };
        }
        guard.save()?;
    }
    reregister(&app)?;
    get_hotkey_config(state)
}

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

fn spawn_start(app: AppHandle) {
    // Fire-and-forget; dictation layer serializes with PHASE atomics.
    tauri::async_runtime::spawn(async move {
        tracing::info!("hotkey → start");
        if let Err(e) = dictation::dictation_start(app.clone()).await {
            tracing::warn!(error = %e, "dictation start failed");
            dictation::emit_dictation(
                &app,
                dictation::DictationUiEvent::Error { message: e },
            );
        }
    });
}

fn spawn_stop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tracing::info!("hotkey → stop");
        if let Err(e) = dictation::dictation_stop(app.clone()).await {
            tracing::warn!(error = %e, "dictation stop failed");
            dictation::emit_dictation(
                &app,
                dictation::DictationUiEvent::Error { message: e },
            );
        }
    });
}

fn spawn_toggle(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tracing::info!("hotkey → toggle");
        if let Err(e) = dictation::toggle_dictation(app.clone()).await {
            tracing::warn!(error = %e, "dictation toggle failed");
            dictation::emit_dictation(
                &app,
                dictation::DictationUiEvent::Error { message: e },
            );
        }
    });
}

pub fn reregister_with(app: &AppHandle, cfg: &HotkeyConfig) -> Result<(), String> {
    let _ = app.global_shortcut().unregister_all();
    mod_chord::stop_watcher();
    lumen_platform_macos::stop_monitor();

    if !cfg.enabled || cfg.toggle.trim().is_empty() {
        tracing::info!("global hotkey disabled");
        return Ok(());
    }

    let toggle = cfg.toggle.trim();
    let hold = cfg.is_hold_mode();

    // Primary path on macOS: CGEventTap (press/hold/release for all chord types).
    #[cfg(target_os = "macos")]
    {
        use lumen_platform_macos::{HotkeyEdge, HotkeyMode, HotkeySpec};

        let mode = if hold {
            HotkeyMode::Hold
        } else {
            HotkeyMode::Toggle
        };
        match HotkeySpec::parse(toggle, mode) {
            Ok(spec) => {
                let app_c = app.clone();
                let hold_mode = hold;
                match lumen_platform_macos::start_monitor(spec, move |edge| {
                    match edge {
                        HotkeyEdge::Press => {
                            if hold_mode {
                                spawn_start(app_c.clone());
                            } else {
                                spawn_toggle(app_c.clone());
                            }
                        }
                        HotkeyEdge::Release => {
                            if hold_mode {
                                spawn_stop(app_c.clone());
                            }
                        }
                    }
                }) {
                    Ok(()) => {
                        tracing::info!(hotkey = %toggle, hold, "event-tap hotkey registered");
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            accessibility = lumen_platform_macos::is_accessibility_trusted(),
                            "event-tap unavailable — falling back (enable Accessibility if denied)"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "event-tap parse failed — falling back");
            }
        }
    }

    // Fallback: modifier-only via HID flag polling.
    if let Some(chord) = ModChord::parse_modifier_only(toggle) {
        let app_press = app.clone();
        let app_release = app.clone();
        if hold {
            mod_chord::start_watcher(
                chord,
                move || spawn_start(app_press.clone()),
                move || spawn_stop(app_release.clone()),
            );
        } else {
            mod_chord::start_watcher(
                chord,
                move || spawn_toggle(app_press.clone()),
                move || { /* release ignored in toggle mode */ },
            );
        }
        tracing::info!(hotkey = %toggle, hold, "modifier-only hotkey registered (fallback)");
        return Ok(());
    }

    // Fallback: normal chords via global-shortcut plugin.
    let shortcut: Shortcut = toggle
        .parse()
        .map_err(|e| format!("invalid hotkey '{toggle}': {e}"))?;

    let handle = app.clone();
    let hold_mode = hold;
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            let handle = handle.clone();
            match event.state {
                ShortcutState::Pressed => {
                    if hold_mode {
                        spawn_start(handle);
                    } else {
                        spawn_toggle(handle);
                    }
                }
                ShortcutState::Released => {
                    if hold_mode {
                        spawn_stop(handle);
                    }
                }
            }
        })
        .map_err(|e| format!("register hotkey failed: {e}"))?;

    tracing::info!(hotkey = %toggle, hold, "global hotkey registered (fallback)");
    Ok(())
}

#[tauri::command]
pub fn pause_hotkeys(app: AppHandle) -> Result<(), String> {
    let _ = app.global_shortcut().unregister_all();
    mod_chord::stop_watcher();
    lumen_platform_macos::stop_monitor();
    tracing::info!("global hotkeys paused for capture");
    Ok(())
}

#[tauri::command]
pub fn resume_hotkeys(app: AppHandle) -> Result<(), String> {
    reregister(&app)
}

pub fn setup_hotkeys(app: &AppHandle) -> Result<(), String> {
    reregister(app)
}
