//! Floating capsule overlay window (M5).

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const CAPSULE_LABEL: &str = "capsule";

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
    .inner_size(300.0, 72.0)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .focused(false)
    .build()?;

    // Center near top of primary monitor.
    if let Some(monitor) = win.current_monitor().ok().flatten() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let w = 300.0 * scale;
        let x = (size.width as f64 - w) / 2.0 / scale;
        let y = 48.0;
        let _ = win.set_position(tauri::LogicalPosition::new(x, y));
    }

    Ok(())
}

/// Show/hide capsule without stealing keyboard focus from the typing target.
pub fn set_capsule_visible(app: &AppHandle, visible: bool, _phase: &str) {
    let Some(win) = app.get_webview_window(CAPSULE_LABEL) else {
        return;
    };
    if visible {
        let _ = win.show();
    } else {
        let _ = win.hide();
    }
}
