//! Modifier-only global chords (e.g. Alt+Shift) via HID flag polling.
//!
//! Design for reliable push-to-talk:
//! - **Active** when all *required* modifiers are down (extra mods OK)
//! - Short debounce on press, **longer sticky debounce on release** so brief
//!   flag flicker while holding does not stop recording
//! - Does not enforce min-hold itself (dictation layer owns that lightly)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

    /// True when every required modifier is down. Extra modifiers are allowed
    /// (critical — exact match breaks when Caps/Fn/IME sets other bits).
    pub fn is_active(self, flags: u64) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        (!self.alt || alt)
            && (!self.shift || shift)
            && (!self.control || control)
            && (!self.meta || meta)
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const HID_SYSTEM_STATE: u32 = 1;

/// ~16ms poll. Press after 2 stable samples (~32ms). Release after 12 (~190ms)
/// of required-mods-not-all-down — sticky so hold does not false-release.
const POLL_MS: u64 = 16;
const DEBOUNCE_ON: u8 = 2;
const DEBOUNCE_OFF: u8 = 12;

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
            let mut latched = false;
            let mut on_count: u8 = 0;
            let mut off_count: u8 = 0;
            let boot = Instant::now();

            tracing::info!(?chord, "mod-chord watcher running");

            while !stop.load(Ordering::SeqCst) {
                // Brief ignore only after (re)register so setup keys don't fire.
                if boot.elapsed() < Duration::from_millis(150) {
                    thread::sleep(Duration::from_millis(POLL_MS));
                    continue;
                }

                let active = chord.is_active(read_mod_flags());
                if active {
                    on_count = on_count.saturating_add(1);
                    off_count = 0;
                } else {
                    off_count = off_count.saturating_add(1);
                    on_count = 0;
                }

                if !latched && on_count >= DEBOUNCE_ON {
                    latched = true;
                    on_count = 0;
                    tracing::info!(?chord, "mod-chord PRESS");
                    on_press();
                } else if latched && off_count >= DEBOUNCE_OFF {
                    latched = false;
                    off_count = 0;
                    tracing::info!(?chord, "mod-chord RELEASE");
                    on_release();
                }

                thread::sleep(Duration::from_millis(POLL_MS));
            }
            tracing::info!("mod-chord watcher stopped");
        })
        .ok();
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
    fn active_allows_extra_mods() {
        let c = ModChord::parse_modifier_only("Alt+Shift").unwrap();
        // Alt+Shift only
        let flags = FLAG_ALTERNATE | FLAG_SHIFT;
        assert!(c.is_active(flags));
        // Alt+Shift+Cmd still active (extras OK)
        assert!(c.is_active(flags | FLAG_COMMAND));
        // Only Alt — not active
        assert!(!c.is_active(FLAG_ALTERNATE));
    }

    #[test]
    fn rejects_with_main_key() {
        assert!(ModChord::parse_modifier_only("Alt+Space").is_none());
    }
}
