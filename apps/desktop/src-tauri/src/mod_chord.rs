//! Modifier-only global chords (e.g. Fn, Alt+Shift) via HID flag polling.
//!
//! Supports multiple chords at once (primary + translate) with **most-specific wins**.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModChord {
    pub fn_key: bool,
    pub alt: bool,
    pub shift: bool,
    pub control: bool,
    pub meta: bool,
}

impl ModChord {
    pub fn count(self) -> u8 {
        (self.fn_key as u8)
            + (self.alt as u8)
            + (self.shift as u8)
            + (self.control as u8)
            + (self.meta as u8)
    }

    pub fn parse_modifier_only(s: &str) -> Option<Self> {
        let mut chord = ModChord {
            fn_key: false,
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
                "FN" | "FUNCTION" | "GLOBE" => chord.fn_key = true,
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
        if saw_key || (chord.count() < 2 && !chord.fn_key) {
            None
        } else {
            Some(chord)
        }
    }

    /// Required mods down; extras OK (single-chord path).
    ///
    /// `phys_fn` is the *physical* Fn/Globe key state (keyCode 63), never the
    /// shared secondary-Fn flag bit — arrow keys, F1–F12, Home/End, Page
    /// Up/Down and forward-delete all raise that bit and would otherwise
    /// misfire a bare-Fn chord.
    pub fn is_active(self, flags: u64, phys_fn: bool) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        (!self.fn_key || phys_fn)
            && (!self.alt || alt)
            && (!self.shift || shift)
            && (!self.control || control)
            && (!self.meta || meta)
    }

    /// Exact modifier set — use when several pure-mod chords are registered.
    ///
    /// Fn is judged from `phys_fn` (physical keyCode 63), not the flag bit; see
    /// [`Self::is_active`] for why the flag bit is unreliable.
    pub fn is_exact(self, flags: u64, phys_fn: bool) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        self.fn_key == phys_fn
            && self.alt == alt
            && self.shift == shift
            && self.control == control
            && self.meta == meta
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
// NOTE: The secondary-Fn flag bit (kCGEventFlagMaskSecondaryFn, 0x0080_0000) is
// deliberately NOT polled here. It is shared by the whole function-key class
// (arrows, F1–F12, nav keys, forward-delete), so polling it from
// CGEventSourceFlagsState misfires the Fn chord on any of those keys. Physical
// Fn is instead read via `read_physical_fn()` (keyCode 63, see hotkey_tap).
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(v_key: i32) -> i16;
}

#[cfg(target_os = "windows")]
fn read_mod_flags() -> u64 {
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;

    fn down(v_key: i32) -> bool {
        // The high bit reports the current key state. The low bit is a
        // transition flag and must not be used for hold-to-talk semantics.
        unsafe { GetAsyncKeyState(v_key) < 0 }
    }

    let mut flags = 0;
    if down(VK_SHIFT) {
        flags |= FLAG_SHIFT;
    }
    if down(VK_CONTROL) {
        flags |= FLAG_CONTROL;
    }
    if down(VK_MENU) {
        flags |= FLAG_ALTERNATE;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        flags |= FLAG_COMMAND;
    }
    flags
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_mod_flags() -> u64 {
    0
}

/// Physical Fn/Globe key state (keyCode 63), maintained by the FlagsChanged
/// event tap. This is authoritative for Fn — never the shared secondary-Fn flag
/// bit. When no event tap is running (fallback path without Accessibility), it
/// stays `false`, which is the safe outcome: not firing beats misfiring.
#[cfg(target_os = "macos")]
fn read_physical_fn() -> bool {
    lumen_platform_macos::physical_fn_down()
}

/// Non-macOS has no Fn key concept at this layer, so Fn is always `false`.
#[cfg(not(target_os = "macos"))]
fn read_physical_fn() -> bool {
    false
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
                let phys_fn = read_physical_fn();
                // Prefer exact match with most modifiers.
                let mut best: Option<(String, u8)> = None;
                for (id, chord) in &chords {
                    if chord.is_exact(flags, phys_fn) {
                        let score = chord.count();
                        if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                            best = Some((id.clone(), score));
                        }
                    }
                }
                // Soft match only if nothing exact (single-chord UX: extras OK).
                if best.is_none() && chords.len() == 1 {
                    let (id, chord) = &chords[0];
                    if chord.is_active(flags, phys_fn) {
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
        assert!(c.alt && c.shift && !c.fn_key && !c.control && !c.meta);
    }

    // The secondary-Fn flag bit shared by the function-key class. A bare-Fn
    // chord must NEVER match on this bit alone (physical Fn not held).
    const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

    #[test]
    fn parses_single_fn() {
        let c = ModChord::parse_modifier_only("Fn").unwrap();
        assert!(c.fn_key);
        // Physical Fn held → exact match; flags carry no Fn signal now.
        assert!(c.is_exact(0, true));
        assert!(!c.is_exact(0, false));
        // A real extra modifier while Fn is held is no longer a bare-Fn chord.
        assert!(!c.is_exact(FLAG_SHIFT, true));
    }

    #[test]
    fn fn_ignores_shared_secondary_fn_flag() {
        // Regression for the misfire bug: arrow keys, F1–F12 and nav keys raise
        // the secondary-Fn flag bit but never press the physical Fn key. With
        // the flag set yet physical Fn NOT held, a bare-Fn chord must not fire.
        let c = ModChord::parse_modifier_only("Fn").unwrap();
        assert!(!c.is_exact(FLAG_SECONDARY_FN, false));
        assert!(!c.is_active(FLAG_SECONDARY_FN, false));
        // Genuine physical Fn press still matches (flag bit present or not).
        assert!(c.is_exact(FLAG_SECONDARY_FN, true));
        assert!(c.is_active(0, true));
    }

    #[test]
    fn active_allows_extra_mods() {
        let c = ModChord::parse_modifier_only("Alt+Shift").unwrap();
        let flags = FLAG_ALTERNATE | FLAG_SHIFT;
        assert!(c.is_active(flags, false));
        assert!(c.is_active(flags | FLAG_COMMAND, false));
        assert!(!c.is_active(FLAG_ALTERNATE, false));
    }

    #[test]
    fn exact_rejects_extra_mods() {
        let c = ModChord::parse_modifier_only("Control+Alt").unwrap();
        let flags = FLAG_CONTROL | FLAG_ALTERNATE;
        assert!(c.is_exact(flags, false));
        assert!(!c.is_exact(flags | FLAG_SHIFT, false));
        // Physical Fn held must break an exact non-Fn chord.
        assert!(!c.is_exact(flags, true));
    }

    #[test]
    fn rejects_with_main_key() {
        assert!(ModChord::parse_modifier_only("Alt+Space").is_none());
    }
}
