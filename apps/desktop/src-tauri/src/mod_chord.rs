//! Modifier-only global chords (e.g. Alt+Shift).
//!
//! `global-hotkey` requires a main key, so pure modifier combos use HID flag
//! polling with **debounce / hysteresis** to avoid flag flicker thrashing
//! (rapid start→stop→start that freezes the app and feels like a crash).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

    /// Exact match of the four modifiers (no extras).
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

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const HID_SYSTEM_STATE: u32 = 1;

/// Samples at ~12ms: need this many consecutive hits to fire edge.
const DEBOUNCE_ON: u8 = 4; // ~48ms stable before press
const DEBOUNCE_OFF: u8 = 5; // ~60ms stable before release
/// Ignore release if the chord was held less than this (bounce / accidental tap).
const MIN_HOLD: Duration = Duration::from_millis(350);

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

/// Watch modifier chord with debounce. `on_press` / `on_release` fire on stable edges.
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
            let mut latched = false; // debounced "held" state
            let mut on_count: u8 = 0;
            let mut off_count: u8 = 0;
            let mut held_since: Option<Instant> = None;
            let boot = Instant::now();

            while !stop.load(Ordering::SeqCst) {
                // Ignore first 300ms after register (setup keys).
                if boot.elapsed() < Duration::from_millis(300) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }

                let now = chord.matches_exact(read_mod_flags());
                if now {
                    on_count = on_count.saturating_add(1);
                    off_count = 0;
                } else {
                    off_count = off_count.saturating_add(1);
                    on_count = 0;
                }

                if !latched && on_count >= DEBOUNCE_ON {
                    latched = true;
                    held_since = Some(Instant::now());
                    on_count = 0;
                    tracing::debug!(?chord, "mod-chord press (debounced)");
                    on_press();
                } else if latched && off_count >= DEBOUNCE_OFF {
                    latched = false;
                    off_count = 0;
                    let held = held_since
                        .map(|t| t.elapsed())
                        .unwrap_or(Duration::ZERO);
                    held_since = None;
                    if held < MIN_HOLD {
                        tracing::info!(
                            ?held,
                            "mod-chord release ignored (held < {:?})",
                            MIN_HOLD
                        );
                        // Signal cancel via a short-hold release callback path:
                        // dictation layer treats quick stop as cancel if still recording briefly.
                        // Still call on_release so recording is cleaned up rather than stuck.
                    } else {
                        tracing::debug!(?chord, ?held, "mod-chord release (debounced)");
                    }
                    on_release();
                }

                thread::sleep(Duration::from_millis(12));
            }
        })
        .ok();

    tracing::info!(?chord, "modifier-only chord watcher started (debounced PTT)");
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
