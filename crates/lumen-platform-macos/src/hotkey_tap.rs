//! Global keyboard monitor via CGEventTap (press / hold / release).
//!
//! Dedicated CFRunLoop thread. Supports modifier-only chords (FlagsChanged)
//! and modifier+key chords (KeyDown/KeyUp).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEdge {
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyMode {
    Hold,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeySpec {
    pub alt: bool,
    pub shift: bool,
    pub control: bool,
    pub meta: bool,
    pub keycode: Option<i64>,
    pub mode: HotkeyMode,
}

impl HotkeySpec {
    pub fn parse(s: &str, mode: HotkeyMode) -> Result<Self, String> {
        let mut alt = false;
        let mut shift = false;
        let mut control = false;
        let mut meta = false;
        let mut keycode: Option<i64> = None;

        for raw in s.split('+') {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            let u = t.to_ascii_uppercase();
            match u.as_str() {
                "OPTION" | "ALT" => alt = true,
                "SHIFT" => shift = true,
                "CONTROL" | "CTRL" => control = true,
                "COMMAND" | "CMD" | "SUPER" | "META" => meta = true,
                "COMMANDORCONTROL" | "COMMANDORCTRL" | "CMDORCTRL" | "CMDORCONTROL" => {
                    meta = true;
                }
                other => {
                    let code = key_name_to_keycode(other)
                        .ok_or_else(|| format!("unsupported key in hotkey: {t}"))?;
                    if keycode.is_some() {
                        return Err("only one non-modifier key is supported".into());
                    }
                    keycode = Some(code);
                }
            }
        }

        let mod_count = (alt as u8) + (shift as u8) + (control as u8) + (meta as u8);
        if mod_count == 0 {
            return Err("hotkey must include at least one modifier".into());
        }
        if keycode.is_none() && mod_count < 2 {
            return Err("modifier-only hotkey needs at least two modifiers".into());
        }

        Ok(Self {
            alt,
            shift,
            control,
            meta,
            keycode,
            mode,
        })
    }

    /// Required modifiers must be down; extra modifiers are allowed.
    fn mods_active(&self, flags: u64) -> bool {
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

fn key_name_to_keycode(name: &str) -> Option<i64> {
    match name {
        "SPACE" => Some(0x31),
        "ENTER" | "RETURN" => Some(0x24),
        "TAB" => Some(0x30),
        "ESCAPE" | "ESC" => Some(0x35),
        "DELETE" | "BACKSPACE" => Some(0x33),
        "UP" | "ARROWUP" => Some(0x7E),
        "DOWN" | "ARROWDOWN" => Some(0x7D),
        "LEFT" | "ARROWLEFT" => Some(0x7B),
        "RIGHT" | "ARROWRIGHT" => Some(0x7C),
        "A" => Some(0x00),
        "S" => Some(0x01),
        "D" => Some(0x02),
        "F" => Some(0x03),
        "H" => Some(0x04),
        "G" => Some(0x05),
        "Z" => Some(0x06),
        "X" => Some(0x07),
        "C" => Some(0x08),
        "V" => Some(0x09),
        "B" => Some(0x0B),
        "Q" => Some(0x0C),
        "W" => Some(0x0D),
        "E" => Some(0x0E),
        "R" => Some(0x0F),
        "Y" => Some(0x10),
        "T" => Some(0x11),
        "1" | "DIGIT1" => Some(0x12),
        "2" | "DIGIT2" => Some(0x13),
        "3" | "DIGIT3" => Some(0x14),
        "4" | "DIGIT4" => Some(0x15),
        "6" | "DIGIT6" => Some(0x16),
        "5" | "DIGIT5" => Some(0x17),
        "9" | "DIGIT9" => Some(0x19),
        "7" | "DIGIT7" => Some(0x1A),
        "8" | "DIGIT8" => Some(0x1C),
        "0" | "DIGIT0" => Some(0x1D),
        "O" => Some(0x1F),
        "U" => Some(0x20),
        "I" => Some(0x22),
        "P" => Some(0x23),
        "L" => Some(0x25),
        "J" => Some(0x26),
        "K" => Some(0x28),
        "N" => Some(0x2D),
        "M" => Some(0x2E),
        "F1" => Some(0x7A),
        "F2" => Some(0x78),
        "F3" => Some(0x63),
        "F4" => Some(0x76),
        "F5" => Some(0x60),
        "F6" => Some(0x61),
        "F7" => Some(0x62),
        "F8" => Some(0x64),
        "F9" => Some(0x65),
        "F10" => Some(0x6D),
        "F11" => Some(0x67),
        "F12" => Some(0x6F),
        "F13" => Some(0x69),
        "F14" => Some(0x6B),
        "F15" => Some(0x71),
        "F16" => Some(0x6A),
        "F17" => Some(0x40),
        "F18" => Some(0x4F),
        "F19" => Some(0x50),
        "F20" => Some(0x5A),
        _ => None,
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

struct MonitorState {
    stop: Arc<AtomicBool>,
}

static MONITOR: Mutex<Option<MonitorState>> = Mutex::new(None);

pub fn stop_monitor() {
    if let Ok(mut g) = MONITOR.lock() {
        if let Some(m) = g.take() {
            m.stop.store(true, Ordering::SeqCst);
        }
    }
}

pub fn start_monitor<F>(spec: HotkeySpec, on_edge: F) -> Result<(), String>
where
    F: Fn(HotkeyEdge) + Send + 'static,
{
    stop_monitor();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut g = MONITOR.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(MonitorState {
            stop: Arc::clone(&stop),
        });
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    thread::Builder::new()
        .name("lumen-hotkey-tap".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            {
                run_tap_loop(spec, on_edge, stop, ready_tx);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (spec, on_edge, stop);
                let _ = ready_tx.send(Err("hotkey tap only available on macOS".into()));
            }
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(r) => r,
        Err(_) => Err("hotkey monitor failed to start in time".into()),
    }
}

#[cfg(target_os = "macos")]
fn run_tap_loop<F>(
    spec: HotkeySpec,
    on_edge: F,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) where
    F: Fn(HotkeyEdge) + Send + 'static,
{
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        EventField,
    };
    use std::cell::Cell;
    use std::rc::Rc;

    let latched = Rc::new(Cell::new(false));
    let key_held = Rc::new(Cell::new(false));
    let release_after = Rc::new(Cell::new(None::<Instant>));
    let latched_c = Rc::clone(&latched);
    let key_held_c = Rc::clone(&key_held);
    let release_after_c = Rc::clone(&release_after);
    let spec_c = spec.clone();
    let on_edge = Rc::new(on_edge);
    let on_edge_c = Rc::clone(&on_edge);

    let tap = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ],
        move |_proxy, etype, event| {
            if matches!(
                etype,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                tracing::warn!("keyboard event tap disabled by system; will re-enable");
                // Returning None keeps the original event (crate maps None → original).
                return None;
            }

            let flags = event.get_flags().bits();
            let mods_ok = spec_c.mods_active(flags);
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);

            match etype {
                CGEventType::KeyDown => {
                    if let Some(need) = spec_c.keycode {
                        if keycode == need {
                            key_held_c.set(true);
                        }
                    }
                }
                CGEventType::KeyUp => {
                    if let Some(need) = spec_c.keycode {
                        if keycode == need {
                            key_held_c.set(false);
                        }
                    }
                }
                _ => {}
            }

            let want_active = if spec_c.keycode.is_some() {
                mods_ok && key_held_c.get()
            } else {
                mods_ok
            };

            let currently = latched_c.get();
            if want_active {
                release_after_c.set(None);
                if !currently {
                    latched_c.set(true);
                    tracing::info!(?spec_c, "hotkey press");
                    on_edge_c(HotkeyEdge::Press);
                }
            } else if currently {
                let now = Instant::now();
                match release_after_c.get() {
                    None => {
                        // Sticky window against FlagsChanged flicker while holding.
                        release_after_c.set(Some(now + Duration::from_millis(70)));
                    }
                    Some(deadline) if now >= deadline => {
                        release_after_c.set(None);
                        latched_c.set(false);
                        key_held_c.set(false);
                        tracing::info!(?spec_c, "hotkey release");
                        on_edge_c(HotkeyEdge::Release);
                    }
                    Some(_) => {}
                }
            }

            // Pass original event through (do not swallow).
            None
        },
    ) {
        Ok(t) => t,
        Err(()) => {
            let _ = ready_tx.send(Err(
                "failed to create keyboard event tap (Accessibility permission required)".into(),
            ));
            return;
        }
    };

    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = ready_tx.send(Err("failed to create run loop source".into()));
            return;
        }
    };

    let rl = CFRunLoop::get_current();
    unsafe {
        rl.add_source(&source, kCFRunLoopCommonModes);
    }
    tap.enable();
    let _ = ready_tx.send(Ok(()));
    tracing::info!(?spec, "keyboard event tap active");

    while !stop.load(Ordering::SeqCst) {
        let _ = unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(50), false)
        };
        // Re-enable if the system disabled the tap (timeout / user input).
        tap.enable();

        // Flush sticky release when no further events arrive.
        if let Some(deadline) = release_after.get() {
            if Instant::now() >= deadline && latched.get() {
                release_after.set(None);
                latched.set(false);
                key_held.set(false);
                tracing::info!(?spec, "hotkey release (timeout flush)");
                on_edge(HotkeyEdge::Release);
            }
        }
    }

    unsafe {
        rl.remove_source(&source, kCFRunLoopCommonModes);
    }
    tracing::info!("keyboard event tap stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alt_shift() {
        let s = HotkeySpec::parse("Alt+Shift", HotkeyMode::Hold).unwrap();
        assert!(s.alt && s.shift && s.keycode.is_none());
    }

    #[test]
    fn parse_alt_space() {
        let s = HotkeySpec::parse("Alt+Space", HotkeyMode::Hold).unwrap();
        assert!(s.alt && s.keycode == Some(0x31));
    }

    #[test]
    fn reject_bare_key() {
        assert!(HotkeySpec::parse("A", HotkeyMode::Hold).is_err());
    }

    #[test]
    fn mods_allow_extras() {
        let s = HotkeySpec::parse("Alt+Shift", HotkeyMode::Hold).unwrap();
        let flags = FLAG_ALTERNATE | FLAG_SHIFT | FLAG_COMMAND;
        assert!(s.mods_active(flags));
        assert!(!s.mods_active(FLAG_ALTERNATE));
    }
}
