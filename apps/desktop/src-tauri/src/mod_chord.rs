//! Modifier-only global chords (e.g. Alt+Shift).
//!
//! `global-hotkey` requires a main key, so pure modifier combos use HID flag
//! polling. Supports rising (press) and falling (release) edges for push-to-talk.

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

    /// Target mods are all down (extra mods allowed). Better for hold while speaking
    /// if OS briefly sets sticky flags.
    pub fn matches_subset(self, flags: u64) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        (!self.alt || alt)
            && (!self.shift || shift)
            && (!self.control || control)
            && (!self.meta || meta)
            && (self.alt || self.shift || self.control || self.meta)
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const HID_SYSTEM_STATE: u32 = 1;

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

pub fn stop_watcher() {
    if let Ok(mut guard) = WATCHER.lock() {
        if let Some(w) = guard.take() {
            w.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Watch modifier chord. `on_press` = rising edge, `on_release` = falling edge.
pub fn start_watcher<F, G>(chord: ModChord, on_press: F, on_release: G)
where
    F: Fn() + Send + 'static,
    G: Fn() + Send + 'static,
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
            let start = std::time::Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if start.elapsed() < Duration::from_millis(250) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let flags = read_mod_flags();
                // Subset match: all required mods down (extras OK) so holding is stable.
                let now = chord.matches_subset(flags);
                if now && !prev_match {
                    on_press();
                } else if !now && prev_match {
                    on_release();
                }
                prev_match = now;
                thread::sleep(Duration::from_millis(12));
            }
        })
        .ok();

    tracing::info!(?chord, "modifier-only chord watcher started (press+release)");
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
