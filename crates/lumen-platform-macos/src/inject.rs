//! macOS text injection: paste-first + clipboard restore, AX optional, unicode type.

use async_trait::async_trait;
use lumen_inject::{InjectError, TextInjectorBackend};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

pub struct MacTextInjectorBackend;

#[async_trait]
impl TextInjectorBackend for MacTextInjectorBackend {
    async fn paste_with_restore(&self, text: &str, preserve: bool) -> Result<(), InjectError> {
        // Short blocking work (clipboard + key event + ~450ms restore delay).
        paste_with_restore_sync(text, preserve)
    }

    async fn ax_insert(&self, text: &str) -> Result<(), InjectError> {
        // Prefer paste path for compatibility; AX set-value is app-specific and flaky.
        let _ = text;
        Err(InjectError::NotSupported(
            "AX insert not implemented; use paste".into(),
        ))
    }

    async fn type_unicode(&self, text: &str) -> Result<(), InjectError> {
        type_unicode_sync(text)
    }

    async fn copy_only(&self, text: &str) -> Result<(), InjectError> {
        set_clipboard(text)
    }
}

fn paste_with_restore_sync(text: &str, preserve: bool) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    let previous = if preserve {
        get_clipboard().unwrap_or_default()
    } else {
        String::new()
    };

    set_clipboard(text)?;
    // Let pasteboard settle (Wispr-like delayed readiness).
    thread::sleep(Duration::from_millis(30));
    simulate_cmd_v()?;
    // Wait for target app to consume pasteboard before restore.
    thread::sleep(Duration::from_millis(450));

    if preserve {
        // Always attempt restore — even if previous was empty.
        let _ = set_clipboard(&previous);
    }
    Ok(())
}

fn set_clipboard(text: &str) -> Result<(), InjectError> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| InjectError::Other(format!("pbcopy: {e}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| InjectError::Other("pbcopy stdin missing".into()))?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| InjectError::Other(format!("pbcopy write: {e}")))?;
    }
    let status = child
        .wait()
        .map_err(|e| InjectError::Other(format!("pbcopy wait: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(InjectError::Other("pbcopy failed".into()))
    }
}

fn get_clipboard() -> Result<String, InjectError> {
    let output = Command::new("pbpaste")
        .output()
        .map_err(|e| InjectError::Other(format!("pbpaste: {e}")))?;
    if !output.status.success() {
        return Err(InjectError::Other("pbpaste failed".into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Simulate ⌘V via CGEvent (requires Accessibility).
fn simulate_cmd_v() -> Result<(), InjectError> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::{CGEvent, CGEventFlags, CGKeyCode};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

        // kVK_ANSI_V = 0x09
        const KEY_V: CGKeyCode = 0x09;

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| InjectError::Other("CGEventSource failed".into()))?;

        let down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
            .map_err(|_| InjectError::Other("key down failed".into()))?;
        down.set_flags(CGEventFlags::CGEventFlagCommand);
        down.post(core_graphics::event::CGEventTapLocation::HID);

        let up = CGEvent::new_keyboard_event(source, KEY_V, false)
            .map_err(|_| InjectError::Other("key up failed".into()))?;
        up.set_flags(CGEventFlags::CGEventFlagCommand);
        up.post(core_graphics::event::CGEventTapLocation::HID);

        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(InjectError::NotSupported("not macOS".into()))
    }
}

fn type_unicode_sync(text: &str) -> Result<(), InjectError> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::{CGEvent, CGEventTapLocation};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| InjectError::Other("CGEventSource failed".into()))?;

        // Chunk to avoid huge single events.
        for ch in text.chars() {
            let s = ch.to_string();
            let event = CGEvent::new_keyboard_event(source.clone(), 0, true)
                .map_err(|_| InjectError::Other("unicode event failed".into()))?;
            event.set_string_from_utf16_unchecked(&s.encode_utf16().collect::<Vec<_>>());
            event.post(CGEventTapLocation::HID);
            // tiny gap for IME-heavy apps
            if ch == '\n' {
                thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err(InjectError::NotSupported("not macOS".into()))
    }
}
