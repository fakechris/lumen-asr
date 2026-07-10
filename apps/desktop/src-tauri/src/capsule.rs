//! Floating capsule overlay window (M5).
//!
//! Must **never** steal keyboard focus from the app the user is typing into.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const CAPSULE_LABEL: &str = "capsule";
pub const MAIN_LABEL: &str = "main";

/// Create the always-on-top capsule window (initially hidden, non-focusable).
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
    .inner_size(300.0, 72.0)
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

    if let Some(monitor) = win.current_monitor().ok().flatten() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let w = 300.0 * scale;
        let x = (size.width as f64 - w) / 2.0 / scale;
        let y = 48.0;
        let _ = win.set_position(tauri::LogicalPosition::new(x, y));
    }

    let _ = win.set_focusable(false);
    Ok(())
}

/// Show/hide capsule without activating the app or stealing key focus.
pub fn set_capsule_visible(app: &AppHandle, visible: bool, _phase: &str) {
    let Some(win) = app.get_webview_window(CAPSULE_LABEL) else {
        return;
    };
    let _ = win.set_focusable(false);
    if visible {
        // show() can still order the window front; keep it non-focusable.
        let _ = win.show();
        let _ = win.set_focusable(false);
    } else {
        let _ = win.hide();
    }
}

/// No-op focus guard. (Previously toggled main focusable which could glitch the UI.)
pub fn ensure_main_stays_background(_app: &AppHandle) {
    // Do not call set_focus / set_focusable on main during hotkey dictation.
}
