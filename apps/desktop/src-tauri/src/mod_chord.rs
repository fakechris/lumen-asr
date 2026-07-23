//! Modifier-only global chords (e.g. Alt+Shift, Control+Alt) via HID flag polling.
//!
//! Supports multiple chords at once (primary + translate) with **most-specific wins**.

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
        (self.alt as u8) + (self.shift as u8) + (self.control as u8) + (self.meta as u8)
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

    /// Required mods down; extras OK (single-chord path).
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

    /// Exact modifier set — use when several pure-mod chords are registered.
    pub fn is_exact(self, flags: u64) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        self.alt == alt && self.shift == shift && self.control == control && self.meta == meta
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const HID_SYSTEM_STATE: u32 = 1;

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

/// Single chord (legacy API).
pub fn start_watcher<F, G>(chord: ModChord, on_press: F, on_release: G)
where
    F: Fn() + Send + 'static,
    G: Fn() + Send + 'static,
{
    start_multi_watcher(vec![("default".into(), chord)], move |id, press| {
        if id == "default" {
            if press {
                on_press();
            } else {
                on_release();
            }
        }
    });
}

/// Multiple pure-mod chords. `on_edge(id, is_press)`.
/// Most-specific exact match wins (more modifiers = higher priority).
pub fn start_multi_watcher<F>(chords: Vec<(String, ModChord)>, on_edge: F)
where
    F: Fn(String, bool) + Send + 'static,
{
    stop_watcher();
    if chords.is_empty() {
        return;
    }
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
            let mut active_id: Option<String> = None;
            let mut on_count: u8 = 0;
            let mut off_count: u8 = 0;
            let mut pending_id: Option<String> = None;
            let boot = Instant::now();

            tracing::info!(n = chords.len(), "mod-chord multi watcher running");

            while !stop.load(Ordering::SeqCst) {
                if boot.elapsed() < Duration::from_millis(150) {
                    thread::sleep(Duration::from_millis(POLL_MS));
                    continue;
                }

                let flags = read_mod_flags();
                // Prefer exact match with most modifiers.
                let mut best: Option<(String, u8)> = None;
                for (id, chord) in &chords {
                    if chord.is_exact(flags) {
                        let score = chord.count();
                        if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                            best = Some((id.clone(), score));
                        }
                    }
                }
                // Soft match only if nothing exact (single-chord UX: extras OK).
                if best.is_none() && chords.len() == 1 {
                    let (id, chord) = &chords[0];
                    if chord.is_active(flags) {
                        best = Some((id.clone(), chord.count()));
                    }
                }

                let matched = best.map(|(id, _)| id);

                match (&active_id, &matched) {
                    (None, Some(id)) => {
                        if pending_id.as_ref() == Some(id) {
                            on_count = on_count.saturating_add(1);
                        } else {
                            pending_id = Some(id.clone());
                            on_count = 1;
                        }
                        off_count = 0;
                        if on_count >= DEBOUNCE_ON {
                            active_id = Some(id.clone());
                            pending_id = None;
                            on_count = 0;
                            tracing::info!(%id, "mod-chord PRESS");
                            on_edge(id.clone(), true);
                        }
                    }
                    (Some(cur), Some(id)) if cur == id => {
                        off_count = 0;
                        on_count = 0;
                        pending_id = None;
                    }
                    (Some(cur), other) => {
                        // Released or switched to another chord.
                        let switch = other.as_ref().map(|id| id != cur).unwrap_or(false);
                        if other.is_none() || switch {
                            off_count = off_count.saturating_add(1);
                            on_count = 0;
                            if off_count >= DEBOUNCE_OFF {
                                let id = cur.clone();
                                active_id = None;
                                off_count = 0;
                                pending_id = None;
                                tracing::info!(%id, "mod-chord RELEASE");
                                on_edge(id, false);
                                // If switched, next loop will press new id.
                            }
                        }
                    }
                    (None, None) => {
                        on_count = 0;
                        off_count = 0;
                        pending_id = None;
                    }
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
        let flags = FLAG_ALTERNATE | FLAG_SHIFT;
        assert!(c.is_active(flags));
        assert!(c.is_active(flags | FLAG_COMMAND));
        assert!(!c.is_active(FLAG_ALTERNATE));
    }

    #[test]
    fn exact_rejects_extra_mods() {
        let c = ModChord::parse_modifier_only("Control+Alt").unwrap();
        let flags = FLAG_CONTROL | FLAG_ALTERNATE;
        assert!(c.is_exact(flags));
        assert!(!c.is_exact(flags | FLAG_SHIFT));
    }

    #[test]
    fn rejects_with_main_key() {
        assert!(ModChord::parse_modifier_only("Alt+Space").is_none());
    }
}
