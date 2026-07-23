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
    /// True when EventTap multi-binding is active (best path).
    pub event_tap_active: bool,
    /// Human hint when intents cannot fully register.
    pub register_note: String,
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

static EVENT_TAP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REGISTER_NOTE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn set_register_status(tap: bool, note: impl Into<String>) {
    EVENT_TAP_ACTIVE.store(tap, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut g) = REGISTER_NOTE.lock() {
        *g = note.into();
    }
}

fn register_status() -> (bool, String) {
    (
        EVENT_TAP_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
        REGISTER_NOTE.lock().map(|g| g.clone()).unwrap_or_default(),
    )
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
    let (tap, note) = register_status();
    Ok(HotkeyDto {
        enabled: guard.hotkey.enabled,
        toggle: guard.hotkey.toggle.clone(),
        show_capsule: guard.hotkey.show_capsule,
        mode: guard.hotkey.mode.clone(),
        intents: guard.hotkey.intents.iter().map(intent_dto).collect(),
        event_tap_active: tap,
        register_note: note,
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
        // Never rewrite user chords on save — only fill empty defaults.
        ensure_default_intents(&mut guard.hotkey);
        guard.save()?;
    }
    reregister(&app)?;
    get_hotkey_config(state)
}

pub fn reregister(app: &AppHandle) -> Result<(), String> {
    let cfg = {
        let state = app.state::<AppState>();
        let mut guard = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        ensure_default_intents(&mut guard.hotkey);
        guard.hotkey.clone()
    };
    reregister_with(app, &cfg)
}

fn spawn_start(app: AppHandle, intent: IntentSpec) {
    tauri::async_runtime::spawn(async move {
        tracing::info!(?intent, "hotkey → start");
        if let Err(e) = dictation::dictation_start_with_intent(app.clone(), intent).await {
            tracing::warn!(error = %e, "dictation start failed");
        }
    });
}

fn spawn_stop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tracing::info!("hotkey → stop");
        if let Err(e) = dictation::dictation_stop(app.clone()).await {
            tracing::warn!(error = %e, "dictation stop failed");
        }
    });
}

fn spawn_toggle(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tracing::info!("hotkey → toggle");
        if let Err(e) = dictation::toggle_dictation(app.clone()).await {
            tracing::warn!(error = %e, "dictation toggle failed");
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
    set_register_status(false, String::new());

    if !cfg.enabled || cfg.toggle.trim().is_empty() {
        tracing::info!("global hotkey disabled");
        set_register_status(false, "热键已关闭".to_string());
        return Ok(());
    }

    let toggle = cfg.toggle.trim();
    let hold = cfg.is_hold_mode();

    #[cfg(target_os = "macos")]
    {
        use lumen_platform_macos::{HotkeyBinding, HotkeyEdge, HotkeyMode, HotkeySpec};

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
            match HotkeySpec::parse(intent.chord.trim(), mode) {
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
                match edge {
                    HotkeyEdge::Press => {
                        if hold_primary {
                            spawn_start(app_c.clone(), intent);
                        } else {
                            dictation::set_session_intent(intent);
                            spawn_toggle(app_c.clone());
                        }
                    }
                    HotkeyEdge::Release => {
                        if hold_primary {
                            spawn_stop(app_c.clone());
                        }
                    }
                }
            }) {
                Ok(()) => {
                    let n = cfg.intents.iter().filter(|i| i.enabled).count();
                    tracing::info!(
                        primary = %toggle,
                        intents = n,
                        "event-tap multi hotkeys registered"
                    );
                    set_register_status(true, format!("EventTap 已注册主热键 + {n} 个意图键"));
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "event-tap multi unavailable — falling back");
                }
            }
        }
    }

    // ── Fallback: pure-mod multi-watcher + global_shortcut for key chords ──
    register_fallback(app, cfg, hold, toggle)
}

fn register_fallback(
    app: &AppHandle,
    cfg: &HotkeyConfig,
    hold: bool,
    toggle: &str,
) -> Result<(), String> {
    let mut mod_bindings: Vec<(String, ModChord)> = Vec::new();
    let mut notes = Vec::new();

    if let Some(chord) = ModChord::parse_modifier_only(toggle) {
        mod_bindings.push(("default".into(), chord));
    }

    for intent in &cfg.intents {
        if !intent.enabled || intent.chord.trim().is_empty() {
            continue;
        }
        if let Some(chord) = ModChord::parse_modifier_only(intent.chord.trim()) {
            mod_bindings.push((intent.id.clone(), chord));
        }
    }

    if !mod_bindings.is_empty() {
        let app_c = app.clone();
        let cfg_c = cfg.clone();
        let hold_mode = hold;
        mod_chord::start_multi_watcher(mod_bindings.clone(), move |id, press| {
            let intent = resolve_intent(&cfg_c, &id);
            if hold_mode {
                if press {
                    spawn_start(app_c.clone(), intent);
                } else {
                    spawn_stop(app_c.clone());
                }
            } else if press {
                dictation::set_session_intent(intent);
                spawn_toggle(app_c.clone());
            }
        });
        notes.push(format!(
            "修饰键监听: {}",
            mod_bindings
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        tracing::info!(
            n = mod_bindings.len(),
            "modifier-only multi hotkeys registered (fallback)"
        );
    }

    // Key chords (e.g. Alt+Shift+T) via global_shortcut when EventTap is down.
    let mut key_chord_count = 0usize;
    if ModChord::parse_modifier_only(toggle).is_none() {
        if let Ok(shortcut) = toggle.parse::<Shortcut>() {
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
                .map_err(|e| format!("register primary hotkey failed: {e}"))?;
            key_chord_count += 1;
        }
    }

    for intent in &cfg.intents {
        if !intent.enabled || intent.chord.trim().is_empty() {
            continue;
        }
        if ModChord::parse_modifier_only(intent.chord.trim()).is_some() {
            continue; // already in multi mod watcher
        }
        let chord = intent.chord.trim();
        let Ok(shortcut) = chord.parse::<Shortcut>() else {
            notes.push(format!("无法解析意图键 {}", chord));
            continue;
        };
        let handle = app.clone();
        let intent_spec = intent.to_intent_spec();
        let hold_mode = hold;
        let id = intent.id.clone();
        match app
            .global_shortcut()
            .on_shortcut(shortcut, move |_app, _s, event| {
                let handle = handle.clone();
                let intent_spec = intent_spec.clone();
                match event.state {
                    ShortcutState::Pressed => {
                        if hold_mode {
                            spawn_start(handle, intent_spec);
                        } else {
                            dictation::set_session_intent(intent_spec);
                            spawn_toggle(handle);
                        }
                    }
                    ShortcutState::Released => {
                        if hold_mode {
                            spawn_stop(handle);
                        }
                    }
                }
            }) {
            Ok(()) => {
                key_chord_count += 1;
                tracing::info!(%id, chord, "intent global_shortcut registered (fallback)");
            }
            Err(e) => {
                tracing::warn!(%id, error = %e, "intent global_shortcut failed");
                notes.push(format!("意图键 {id} 注册失败: {e}"));
            }
        }
    }

    if mod_bindings.is_empty() && key_chord_count == 0 {
        return Err("no hotkeys could be registered".into());
    }

    let note = if notes.is_empty() {
        format!("回退注册成功（修饰键组 + {key_chord_count} 个含字母键）")
    } else {
        format!("回退注册（EventTap 需辅助功能）。{}", notes.join("；"))
    };
    set_register_status(false, note);
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
