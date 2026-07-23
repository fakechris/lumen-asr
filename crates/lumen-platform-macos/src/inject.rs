//! macOS text injection.
//!
//! Strategy (Auto):
//! 1. **Unicode CGEvent type** into the current key focus (no app activate)
//! 2. **Clipboard + ⌘V** fallback (after modifiers clear)
//!
//! Rules:
//! - Do not `open -a` / activate unless our app accidentally became frontmost
//! - Wait until Alt/Shift/Ctrl are physically up before synthesizing keys
//!   (hotkey chord still held would turn ⌘V into ⌥⇧⌘V)

use async_trait::async_trait;
use lumen_inject::{InjectError, TextInjectorBackend};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use std::thread;
use std::time::{Duration, Instant};

pub struct MacTextInjectorBackend;

#[async_trait]
impl TextInjectorBackend for MacTextInjectorBackend {
    async fn paste_with_restore(&self, text: &str, preserve: bool) -> Result<(), InjectError> {
        paste_with_restore_sync(text, preserve)
    }

    async fn ax_insert(&self, text: &str) -> Result<(), InjectError> {
        // Terminal apps rarely expose AXValue for the shell — use type/paste.
        let _ = text;
        Err(InjectError::NotSupported(
            "AX insert not used; prefer type/paste".into(),
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

    wait_hotkey_modifiers_clear(Duration::from_millis(400));

    let previous = if preserve {
        get_clipboard().unwrap_or_default()
    } else {
        String::new()
    };

    set_clipboard(text)?;
    // Pasteboard readiness.
    thread::sleep(Duration::from_millis(40));
    simulate_cmd_v()?;
    // Give terminal/Electron time to consume pasteboard.
    thread::sleep(Duration::from_millis(350));

    if preserve {
        let _ = set_clipboard(&previous);
    }
    Ok(())
}

fn set_clipboard(text: &str) -> Result<(), InjectError> {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let value = NSString::from_str(text);
    let string_type = unsafe { NSPasteboardTypeString };
    if pasteboard.setString_forType(&value, string_type) {
        Ok(())
    } else {
        Err(InjectError::Other(
            "failed to write Unicode text to pasteboard".into(),
        ))
    }
}

fn get_clipboard() -> Result<String, InjectError> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let string_type = unsafe { NSPasteboardTypeString };
    pasteboard
        .stringForType(string_type)
        .map(|value| value.to_string())
        .ok_or_else(|| InjectError::Other("pasteboard does not contain text".into()))
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const HOTKEY_MODS: u64 = FLAG_SHIFT | FLAG_CONTROL | FLAG_ALTERNATE; // not Command

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: u32) -> u64;
}

/// Wait until physical Alt/Shift/Ctrl are up so synthetic ⌘V is not remapped.
fn wait_hotkey_modifiers_clear(timeout: Duration) {
    #[cfg(target_os = "macos")]
    {
        let start = Instant::now();
        loop {
            let flags = unsafe { CGEventSourceFlagsState(1) }; // HID system
            if flags & HOTKEY_MODS == 0 {
                // Extra beat after clear — OS keyboard state lag.
                thread::sleep(Duration::from_millis(20));
                return;
            }
            if start.elapsed() >= timeout {
                tracing::warn!(
                    flags = format!("{:#x}", flags),
                    "modifiers still down before inject — continuing"
                );
                return;
            }
            thread::sleep(Duration::from_millis(12));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = timeout;
    }
}

/// Simulate ⌘V via CGEvent (requires Accessibility).
fn simulate_cmd_v() -> Result<(), InjectError> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

        const KEY_V: CGKeyCode = 0x09; // kVK_ANSI_V

        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .or_else(|_| CGEventSource::new(CGEventSourceStateID::HIDSystemState))
            .map_err(|_| InjectError::Other("CGEventSource failed".into()))?;

        let down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
            .map_err(|_| InjectError::Other("key down failed".into()))?;
        down.set_flags(CGEventFlags::CGEventFlagCommand);
        down.post(CGEventTapLocation::HID);

        thread::sleep(Duration::from_millis(8));

        let up = CGEvent::new_keyboard_event(source, KEY_V, false)
            .map_err(|_| InjectError::Other("key up failed".into()))?;
        up.set_flags(CGEventFlags::CGEventFlagCommand);
        up.post(CGEventTapLocation::HID);

        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(InjectError::NotSupported("not macOS".into()))
    }
}

/// Insert by synthesizing Unicode key events at the current key focus.
fn type_unicode_sync(text: &str) -> Result<(), InjectError> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

        wait_hotkey_modifiers_clear(Duration::from_millis(400));

        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .or_else(|_| CGEventSource::new(CGEventSourceStateID::HIDSystemState))
            .map_err(|_| InjectError::Other("CGEventSource failed".into()))?;

        // Type in small chunks for better compatibility (terminals, Electron).
        let mut buf = String::new();
        for ch in text.chars() {
            buf.push(ch);
            if ch.is_whitespace() || buf.chars().count() >= 8 {
                post_unicode_chunk(&source, &buf)?;
                buf.clear();
                if ch == '\n' {
                    thread::sleep(Duration::from_millis(4));
                }
            }
        }
        if !buf.is_empty() {
            post_unicode_chunk(&source, &buf)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err(InjectError::NotSupported("not macOS".into()))
    }
}

#[cfg(target_os = "macos")]
fn post_unicode_chunk(
    source: &core_graphics::event_source::CGEventSource,
    chunk: &str,
) -> Result<(), InjectError> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};

    let event = CGEvent::new_keyboard_event(source.clone(), 0, true)
        .map_err(|_| InjectError::Other("unicode event failed".into()))?;
    let utf16: Vec<u16> = chunk.encode_utf16().collect();
    event.set_string_from_utf16_unchecked(&utf16);
    // Explicit empty flags so leftover Alt/Shift state is not applied.
    event.set_flags(CGEventFlags::empty());
    event.post(CGEventTapLocation::HID);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::set_clipboard;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    #[test]
    fn clipboard_copy_preserves_unicode_for_native_consumers() {
        let pasteboard = NSPasteboard::generalPasteboard();
        let string_type = unsafe { NSPasteboardTypeString };
        let previous = pasteboard
            .stringForType(string_type)
            .map(|value| value.to_string());
        let expected = "中文剪贴板测试：你好，世界。🚀";

        set_clipboard(expected).expect("copy Unicode fixture");
        let actual = pasteboard
            .stringForType(string_type)
            .map(|value| value.to_string());

        pasteboard.clearContents();
        if let Some(previous) = previous {
            assert!(pasteboard.setString_forType(&NSString::from_str(&previous), string_type));
        }

        assert_eq!(actual.as_deref(), Some(expected));
    }
}
