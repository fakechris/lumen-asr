//! Floating capsule overlay — the visible popup while dictating.
//!
//! Must **not** steal keyboard focus from the app the user is typing into.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const CAPSULE_LABEL: &str = "capsule";
pub const MAIN_LABEL: &str = "main";

/// Create the always-on-top capsule window (initially hidden).
pub fn ensure_capsule(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window(CAPSULE_LABEL).is_some() {
        return Ok(());
    }

    let win = WebviewWindowBuilder::new(
        app,
        CAPSULE_LABEL,
        WebviewUrl::App("index.html?window=capsule".into()),
    )
    .title("Lumen")
    .inner_size(280.0, 64.0)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .focused(false)
    .focusable(false)
    .build()?;

    position_top_center(&win);
    let _ = win.set_focusable(false);
    tracing::info!("capsule window created");
    Ok(())
}

fn position_top_center(win: &tauri::WebviewWindow) {
    if let Some(monitor) = win.current_monitor().ok().flatten() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let w = 280.0 * scale;
        let x = (size.width as f64 - w) / 2.0 / scale;
        let y = 56.0;
        let _ = win.set_position(tauri::LogicalPosition::new(x, y));
    }
}

/// Show/hide capsule. `phase` is logged for debugging (listening/processing/idle).
pub fn set_capsule_visible(app: &AppHandle, visible: bool, phase: &str) {
    // Ensure window exists (hotkey may fire before setup finishes on slow disks).
    if app.get_webview_window(CAPSULE_LABEL).is_none() {
        if let Err(e) = ensure_capsule(app) {
            tracing::warn!(error = %e, "capsule ensure failed");
            return;
        }
    }
    let Some(win) = app.get_webview_window(CAPSULE_LABEL) else {
        tracing::warn!("capsule window missing after ensure");
        return;
    };
    let _ = win.set_focusable(false);
    if visible {
        position_top_center(&win);
        match win.show() {
            Ok(()) => tracing::info!(%phase, "capsule shown"),
            Err(e) => tracing::warn!(error = %e, %phase, "capsule show failed"),
        }
        let _ = win.set_always_on_top(true);
        let _ = win.set_focusable(false);
    } else {
        match win.hide() {
            Ok(()) => tracing::info!(%phase, "capsule hidden"),
            Err(e) => tracing::warn!(error = %e, %phase, "capsule hide failed"),
        }
    }
}

pub fn ensure_main_stays_background(_app: &AppHandle) {}
