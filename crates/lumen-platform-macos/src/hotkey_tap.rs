//! Global keyboard monitor via CGEventTap (press / hold / release).
//!
//! Supports multiple bindings (primary + intent chords) on one tap thread.

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
        _ => None,
    }
}

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

#[derive(Debug, Clone)]
pub struct HotkeyBinding {
    pub id: String,
    pub spec: HotkeySpec,
}

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

/// Single binding (backward compatible).
pub fn start_monitor<F>(spec: HotkeySpec, on_edge: F) -> Result<(), String>
where
    F: Fn(HotkeyEdge) + Send + 'static,
{
    start_multi_monitor(
        vec![HotkeyBinding {
            id: "default".into(),
            spec,
        }],
        move |edge, _id| on_edge(edge),
    )
}

/// Multiple chords on one EventTap; `on_edge(edge, binding_id)`.
pub fn start_multi_monitor<F>(bindings: Vec<HotkeyBinding>, on_edge: F) -> Result<(), String>
where
    F: Fn(HotkeyEdge, String) + Send + 'static,
{
    if bindings.is_empty() {
        return Err("no hotkey bindings".into());
    }
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
                run_tap_loop_multi(bindings, on_edge, stop, ready_tx);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (bindings, on_edge, stop);
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
fn run_tap_loop_multi<F>(
    bindings: Vec<HotkeyBinding>,
    on_edge: F,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) where
    F: Fn(HotkeyEdge, String) + Send + 'static,
{
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        EventField,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    #[derive(Clone)]
    struct Latch {
        active: bool,
        release_after: Option<Instant>,
        key_held: bool,
    }

    let mut latches_init = HashMap::new();
    for b in &bindings {
        latches_init.insert(
            b.id.clone(),
            Latch {
                active: false,
                release_after: None,
                key_held: false,
            },
        );
    }

    let bindings_c = Rc::new(bindings.clone());
    let latches = Rc::new(RefCell::new(latches_init));
    let latches_c = Rc::clone(&latches);
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
                return None;
            }

            let flags = event.get_flags().bits();
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);

            let mut map = latches_c.borrow_mut();
            for b in bindings_c.iter() {
                let latch = map.get_mut(&b.id).unwrap();
                match etype {
                    CGEventType::KeyDown => {
                        if b.spec.keycode == Some(keycode) {
                            latch.key_held = true;
                        }
                    }
                    CGEventType::KeyUp => {
                        if b.spec.keycode == Some(keycode) {
                            latch.key_held = false;
                        }
                    }
                    _ => {}
                }

                let key_arg = if b.spec.keycode.is_some() {
                    if latch.key_held {
                        b.spec.keycode
                    } else {
                        None
                    }
                } else {
                    None
                };
                // For key chords, is_active checks keycode match via key_held path:
                let want = if let Some(need) = b.spec.keycode {
                    b.spec.mods_active(flags) && latch.key_held && Some(need) == b.spec.keycode
                } else {
                    b.spec.mods_active(flags)
                };
                let _ = key_arg;

                if want {
                    latch.release_after = None;
                    if !latch.active {
                        latch.active = true;
                        tracing::info!(id = %b.id, "hotkey press");
                        on_edge_c(HotkeyEdge::Press, b.id.clone());
                    }
                } else if latch.active {
                    let now = Instant::now();
                    match latch.release_after {
                        None => {
                            latch.release_after = Some(now + Duration::from_millis(70));
                        }
                        Some(deadline) if now >= deadline => {
                            latch.release_after = None;
                            latch.active = false;
                            latch.key_held = false;
                            tracing::info!(id = %b.id, "hotkey release");
                            on_edge_c(HotkeyEdge::Release, b.id.clone());
                        }
                        Some(_) => {}
                    }
                }
            }
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
    tracing::info!(n = bindings.len(), "keyboard event tap active (multi)");

    while !stop.load(Ordering::SeqCst) {
        let _ = unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(50), false)
        };
        tap.enable();

        let mut map = latches.borrow_mut();
        for b in &bindings {
            let latch = map.get_mut(&b.id).unwrap();
            if let Some(deadline) = latch.release_after {
                if Instant::now() >= deadline && latch.active {
                    latch.release_after = None;
                    latch.active = false;
                    latch.key_held = false;
                    tracing::info!(id = %b.id, "hotkey release (timeout flush)");
                    on_edge(HotkeyEdge::Release, b.id.clone());
                }
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
}
