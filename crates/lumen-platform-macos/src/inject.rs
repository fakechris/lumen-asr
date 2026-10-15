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
//! - Clipboard restore snapshots every pasteboard item and type (images, rich
//!   text, file URLs — not just plain text) and only restores while the
//!   pasteboard still holds our write, so a copy made during the paste window
//!   is never clobbered

use async_trait::async_trait;
use lumen_inject::{InjectError, TextInjectorBackend};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::ProtocolObject;
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardType, NSPasteboardTypeString, NSPasteboardWriting,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSData, NSString};
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
        set_clipboard(text).map(|_| ())
    }
}

fn paste_with_restore_sync(text: &str, preserve: bool) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    wait_hotkey_modifiers_clear(Duration::from_millis(400));

    let previous = preserve.then(ClipboardSnapshot::capture);

    let our_change = set_clipboard(text)?;
    // Pasteboard readiness.
    thread::sleep(Duration::from_millis(40));
    simulate_cmd_v()?;
    // Give terminal/Electron time to consume pasteboard.
    thread::sleep(Duration::from_millis(350));

    if let Some(previous) = previous {
        match restore_decision(our_change, clipboard_change_count()) {
            RestoreDecision::Restore => {
                if let Err(e) = previous.restore() {
                    tracing::warn!(error = %e, "clipboard restore failed");
                }
            }
            RestoreDecision::KeepNewerContents => {
                tracing::info!(
                    "pasteboard changed by another writer during paste; keeping the newer contents"
                );
            }
        }
    }
    Ok(())
}

/// Whether to put the user's previous clipboard back after our paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreDecision {
    /// The pasteboard still holds our write — restore the snapshot.
    Restore,
    /// Another writer (the user copying, a clipboard manager, another app)
    /// replaced our contents during the paste window; restoring would destroy
    /// that newer copy.
    KeepNewerContents,
}

/// `our_change` is the pasteboard change count right after our own write;
/// `current` is the count when the paste window closes. Any difference means
/// someone else wrote in between.
fn restore_decision(our_change: isize, current: isize) -> RestoreDecision {
    if current == our_change {
        RestoreDecision::Restore
    } else {
        RestoreDecision::KeepNewerContents
    }
}

/// Every item on a pasteboard with the data of each of its types, so non-text
/// contents (images, RTF, file URLs) survive a paste.
#[cfg(target_os = "macos")]
struct ClipboardSnapshot {
    items: Vec<Vec<(Retained<NSPasteboardType>, Retained<NSData>)>>,
}

#[cfg(target_os = "macos")]
impl ClipboardSnapshot {
    fn capture() -> Self {
        Self::capture_from(&NSPasteboard::generalPasteboard())
    }

    fn capture_from(pasteboard: &NSPasteboard) -> Self {
        let mut items = Vec::new();
        if let Some(pb_items) = pasteboard.pasteboardItems() {
            for item in pb_items.iter() {
                let entries: Vec<_> = item
                    .types()
                    .iter()
                    .filter_map(|ty| item.dataForType(&ty).map(|data| (ty, data)))
                    .collect();
                if !entries.is_empty() {
                    items.push(entries);
                }
            }
        }
        Self { items }
    }

    fn restore(&self) -> Result<(), InjectError> {
        self.restore_to(&NSPasteboard::generalPasteboard())
    }

    /// Replace the pasteboard contents with the snapshot; an empty snapshot
    /// leaves it cleared, which is what the user had before the paste.
    fn restore_to(&self, pasteboard: &NSPasteboard) -> Result<(), InjectError> {
        pasteboard.clearContents();
        if self.items.is_empty() {
            return Ok(());
        }
        let writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = self
            .items
            .iter()
            .map(|entries| {
                let item = NSPasteboardItem::new();
                for (ty, data) in entries {
                    item.setData_forType(data, ty);
                }
                ProtocolObject::from_retained(item)
            })
            .collect();
        if pasteboard.writeObjects(&NSArray::from_retained_slice(&writers)) {
            Ok(())
        } else {
            Err(InjectError::Other(
                "failed to write previous contents back to pasteboard".into(),
            ))
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct ClipboardSnapshot;

#[cfg(not(target_os = "macos"))]
impl ClipboardSnapshot {
    fn capture() -> Self {
        Self
    }

    fn restore(&self) -> Result<(), InjectError> {
        Err(InjectError::NotSupported("not macOS".into()))
    }
}

#[cfg(target_os = "macos")]
fn clipboard_change_count() -> isize {
    NSPasteboard::generalPasteboard().changeCount()
}

#[cfg(not(target_os = "macos"))]
fn clipboard_change_count() -> isize {
    0
}

/// Write `text` to the general pasteboard; returns the change count of our
/// write so the caller can later tell whether anyone else wrote after us.
#[cfg(target_os = "macos")]
fn set_clipboard(text: &str) -> Result<isize, InjectError> {
    let pasteboard = NSPasteboard::generalPasteboard();
    set_pasteboard_text(&pasteboard, text)?;
    Ok(pasteboard.changeCount())
}

#[cfg(target_os = "macos")]
fn set_pasteboard_text(pasteboard: &NSPasteboard, text: &str) -> Result<(), InjectError> {
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

#[cfg(not(target_os = "macos"))]
fn set_clipboard(text: &str) -> Result<isize, InjectError> {
    let _ = text;
    Err(InjectError::NotSupported("not macOS".into()))
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

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::{set_pasteboard_text, ClipboardSnapshot};
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString};
    use objc2_foundation::{NSData, NSString};

    fn pasteboard_text(pasteboard: &NSPasteboard) -> Option<String> {
        let string_type = unsafe { NSPasteboardTypeString };
        pasteboard.stringForType(string_type).map(|v| v.to_string())
    }

    #[test]
    fn clipboard_copy_preserves_unicode_for_native_consumers() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let expected = "中文剪贴板测试：你好，世界。🚀";

        set_pasteboard_text(&pasteboard, expected).expect("copy Unicode fixture");

        assert_eq!(pasteboard_text(&pasteboard).as_deref(), Some(expected));
    }

    #[test]
    fn snapshot_restores_non_text_types_alongside_text() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let png_type = unsafe { NSPasteboardTypePNG };
        let string_type = unsafe { NSPasteboardTypeString };
        let image_bytes = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 3];
        pasteboard.clearContents();
        pasteboard.setData_forType(Some(&NSData::with_bytes(&image_bytes)), png_type);
        pasteboard.setString_forType(&NSString::from_str("caption"), string_type);

        let snapshot = ClipboardSnapshot::capture_from(&pasteboard);
        set_pasteboard_text(&pasteboard, "dictated text").expect("overwrite");
        snapshot.restore_to(&pasteboard).expect("restore");

        let restored = pasteboard
            .dataForType(png_type)
            .expect("image data restored")
            .to_vec();
        assert_eq!(restored, image_bytes);
        assert_eq!(pasteboard_text(&pasteboard).as_deref(), Some("caption"));
    }

    #[test]
    fn empty_snapshot_restores_to_empty_pasteboard() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        pasteboard.clearContents();

        let snapshot = ClipboardSnapshot::capture_from(&pasteboard);
        set_pasteboard_text(&pasteboard, "dictated text").expect("overwrite");
        snapshot.restore_to(&pasteboard).expect("restore");

        assert_eq!(pasteboard_text(&pasteboard), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{restore_decision, RestoreDecision};

    #[test]
    fn restores_when_pasteboard_still_holds_our_write() {
        assert_eq!(restore_decision(42, 42), RestoreDecision::Restore);
    }

    #[test]
    fn keeps_contents_written_by_someone_else_during_paste() {
        assert_eq!(restore_decision(42, 43), RestoreDecision::KeepNewerContents);
        assert_eq!(restore_decision(42, 50), RestoreDecision::KeepNewerContents);
    }
}
