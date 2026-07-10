//! Modifier-only global chords (e.g. Alt+Shift).
//!
//! `global-hotkey` / Tauri global-shortcut **require a main key**, so pure
//! modifier combos cannot be registered there. On macOS we poll HID modifier
//! flags (same approach class as competitor "hold Option" detectors) and fire
//! on the rising edge of an exact chord match.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Which modifiers must be held (exact match — no extra mods).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModChord {
    pub alt: bool,
    pub shift: bool,
    pub control: bool,
    pub meta: bool,
}

impl ModChord {
    pub fn count(self) -> u8 {
        (self.alt as u8)
            + (self.shift as u8)
            + (self.control as u8)
            + (self.meta as u8)
    }

    /// Parse strings that contain **only** modifiers, e.g. `"Alt+Shift"`.
    /// Returns None if empty, has a main key, or fewer than 2 modifiers.
    pub fn parse_modifier_only(s: &str) -> Option<Self> {
        let mut chord = ModChord {
            alt: false,
            shift: false,
            control: false,
            meta: false,
        };
        let mut saw_key = false;
        for raw in s.split('+') {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            match t.to_ascii_uppercase().as_str() {
                "OPTION" | "ALT" => chord.alt = true,
                "SHIFT" => chord.shift = true,
                "CONTROL" | "CTRL" => chord.control = true,
                "COMMAND" | "CMD" | "SUPER" | "META" => chord.meta = true,
                "COMMANDORCONTROL" | "COMMANDORCTRL" | "CMDORCTRL" | "CMDORCONTROL" => {
                    // Platform-agnostic token → Command on macOS path.
                    chord.meta = true;
                }
                _ => saw_key = true,
            }
        }
        if saw_key || chord.count() < 2 {
            None
        } else {
            Some(chord)
        }
    }

    pub fn matches_exact(self, flags: u64) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        alt == self.alt
            && shift == self.shift
            && control == self.control
            && meta == self.meta
    }
}

// CGEventFlagMask* (Carbon / CoreGraphics)
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const HID_SYSTEM_STATE: u32 = 1; // kCGEventSourceStateHIDSystemState

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: u32) -> u64;
}

#[cfg(target_os = "macos")]
fn read_mod_flags() -> u64 {
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) }
}

#[cfg(not(target_os = "macos"))]
fn read_mod_flags() -> u64 {
    0
}

struct WatcherState {
    stop: Arc<AtomicBool>,
}

static WATCHER: Mutex<Option<WatcherState>> = Mutex::new(None);

/// Stop any running modifier-only watcher.
pub fn stop_watcher() {
    if let Ok(mut guard) = WATCHER.lock() {
        if let Some(w) = guard.take() {
            w.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Start watching for an exact modifier chord. Fires `on_press` on rising edge.
pub fn start_watcher<F>(chord: ModChord, on_press: F)
where
    F: Fn() + Send + 'static,
{
    stop_watcher();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut guard = WATCHER.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(WatcherState {
            stop: Arc::clone(&stop),
        });
    }

    thread::Builder::new()
        .name("lumen-mod-chord".into())
        .spawn(move || {
            let mut prev_match = false;
            // Ignore first 200ms so the keys used to *set* the hotkey don't fire it.
            let start = std::time::Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if start.elapsed() < Duration::from_millis(250) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let flags = read_mod_flags();
                let now = chord.matches_exact(flags);
                if now && !prev_match {
                    on_press();
                    // Debounce re-entry while still held.
                    thread::sleep(Duration::from_millis(280));
                }
                prev_match = now;
                thread::sleep(Duration::from_millis(16));
            }
        })
        .ok();

    tracing::info!(?chord, "modifier-only chord watcher started");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alt_shift() {
        let c = ModChord::parse_modifier_only("Alt+Shift").unwrap();
        assert!(c.alt && c.shift && !c.control && !c.meta);
    }

    #[test]
    fn rejects_with_main_key() {
        assert!(ModChord::parse_modifier_only("Alt+Space").is_none());
        assert!(ModChord::parse_modifier_only("Alt").is_none());
    }
}
