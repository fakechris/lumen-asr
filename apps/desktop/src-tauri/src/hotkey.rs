//! Global hotkey registration — primary + independent intent chords.

use crate::config::{ensure_default_intents, HotkeyConfig, HotkeyIntentConfig};
use crate::dictation;
use crate::mod_chord::{self, ModChord};
use crate::AppState;
use lumen_prompts::IntentSpec;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyIntentDto {
    pub id: String,
    pub chord: String,
    pub mode: String,
    pub intent: String,
    pub target_language: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyDto {
    pub enabled: bool,
    pub toggle: String,
    pub show_capsule: bool,
    pub mode: String,
    pub intents: Vec<HotkeyIntentDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyIntentInput {
    pub id: Option<String>,
    pub chord: Option<String>,
    pub mode: Option<String>,
    pub intent: Option<String>,
    pub target_language: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyInput {
    pub enabled: Option<bool>,
    pub toggle: Option<String>,
    pub show_capsule: Option<bool>,
    pub mode: Option<String>,
    pub intents: Option<Vec<HotkeyIntentInput>>,
}

fn intent_dto(i: &HotkeyIntentConfig) -> HotkeyIntentDto {
    HotkeyIntentDto {
        id: i.id.clone(),
        chord: i.chord.clone(),
        mode: i.mode.clone(),
        intent: i.intent.clone(),
        target_language: i.target_language.clone(),
        enabled: i.enabled,
    }
}

#[tauri::command]
pub fn get_hotkey_config(state: State<'_, AppState>) -> Result<HotkeyDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    let before = guard.hotkey.intents.len();
    ensure_default_intents(&mut guard.hotkey);
    if guard.hotkey.intents.len() != before {
        let _ = guard.save();
    }
    Ok(HotkeyDto {
        enabled: guard.hotkey.enabled,
        toggle: guard.hotkey.toggle.clone(),
        show_capsule: guard.hotkey.show_capsule,
        mode: guard.hotkey.mode.clone(),
        intents: guard.hotkey.intents.iter().map(intent_dto).collect(),
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
        if let Some(list) = input.intents {
            guard.hotkey.intents = list
                .into_iter()
                .map(|i| HotkeyIntentConfig {
                    id: i.id.unwrap_or_else(|| "intent".into()),
                    chord: i.chord.unwrap_or_default(),
                    mode: i.mode.unwrap_or_else(|| "hold".into()),
                    intent: i.intent.unwrap_or_else(|| "translate".into()),
                    target_language: i.target_language.unwrap_or_else(|| "en".into()),
                    enabled: i.enabled.unwrap_or(false),
                })
                .collect();
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

fn spawn_start(app: AppHandle, intent: IntentSpec) {
    tauri::async_runtime::spawn(async move {
        tracing::info!(?intent, "hotkey → start");
        if let Err(e) = dictation::dictation_start_with_intent(app.clone(), intent).await {
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

fn resolve_intent(cfg: &HotkeyConfig, binding_id: &str) -> IntentSpec {
    if binding_id == "default" {
        return IntentSpec::Default;
    }
    cfg.intents
        .iter()
        .find(|i| i.id == binding_id)
        .map(|i| i.to_intent_spec())
        .unwrap_or(IntentSpec::Default)
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

    #[cfg(target_os = "macos")]
    {
        use lumen_platform_macos::{
            HotkeyBinding, HotkeyEdge, HotkeyMode, HotkeySpec,
        };

        let mode = if hold {
            HotkeyMode::Hold
        } else {
            HotkeyMode::Toggle
        };

        let mut bindings = Vec::new();
        match HotkeySpec::parse(toggle, mode) {
            Ok(spec) => bindings.push(HotkeyBinding {
                id: "default".into(),
                spec,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "primary hotkey parse failed");
            }
        }

        for intent in &cfg.intents {
            if !intent.enabled || intent.chord.trim().is_empty() {
                continue;
            }
            let imode = if intent.mode.eq_ignore_ascii_case("toggle") {
                HotkeyMode::Toggle
            } else {
                HotkeyMode::Hold
            };
            match HotkeySpec::parse(intent.chord.trim(), imode) {
                Ok(spec) => bindings.push(HotkeyBinding {
                    id: intent.id.clone(),
                    spec,
                }),
                Err(e) => {
                    tracing::warn!(id = %intent.id, error = %e, "intent hotkey parse failed");
                }
            }
        }

        if !bindings.is_empty() {
            let app_c = app.clone();
            let cfg_c = cfg.clone();
            let hold_primary = hold;
            match lumen_platform_macos::start_multi_monitor(bindings, move |edge, id| {
                let intent = resolve_intent(&cfg_c, &id);
                let is_hold = if id == "default" {
                    hold_primary
                } else {
                    cfg_c
                        .intents
                        .iter()
                        .find(|i| i.id == id)
                        .map(|i| !i.mode.eq_ignore_ascii_case("toggle"))
                        .unwrap_or(true)
                };
                match edge {
                    HotkeyEdge::Press => {
                        if is_hold {
                            spawn_start(app_c.clone(), intent);
                        } else {
                            dictation::set_session_intent(intent);
                            spawn_toggle(app_c.clone());
                        }
                    }
                    HotkeyEdge::Release => {
                        if is_hold {
                            spawn_stop(app_c.clone());
                        }
                    }
                }
            }) {
                Ok(()) => {
                    tracing::info!(
                        primary = %toggle,
                        intents = cfg.intents.iter().filter(|i| i.enabled).count(),
                        "event-tap multi hotkeys registered"
                    );
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "event-tap multi unavailable — falling back");
                }
            }
        }
    }

    // Fallback: primary only (legacy paths)
    if let Some(chord) = ModChord::parse_modifier_only(toggle) {
        let app_press = app.clone();
        let app_release = app.clone();
        if hold {
            mod_chord::start_watcher(
                chord,
                move || spawn_start(app_press.clone(), IntentSpec::Default),
                move || spawn_stop(app_release.clone()),
            );
        } else {
            mod_chord::start_watcher(
                chord,
                move || spawn_toggle(app_press.clone()),
                move || {},
            );
        }
        tracing::info!(hotkey = %toggle, hold, "modifier-only hotkey registered (fallback)");
        return Ok(());
    }

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
                        spawn_start(handle, IntentSpec::Default);
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
