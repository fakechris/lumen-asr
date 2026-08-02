//! Global keyboard monitor via CGEventTap (press / hold / release).
//!
//! Supports multiple bindings (primary + intent chords) on one tap thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    pub fn_key: bool,
    pub alt: bool,
    pub shift: bool,
    pub control: bool,
    pub meta: bool,
    pub keycode: Option<i64>,
    pub mode: HotkeyMode,
}

impl HotkeySpec {
    pub fn parse(s: &str, mode: HotkeyMode) -> Result<Self, String> {
        let mut fn_key = false;
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
                "FN" | "FUNCTION" | "GLOBE" => fn_key = true,
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

        let mod_count =
            (fn_key as u8) + (alt as u8) + (shift as u8) + (control as u8) + (meta as u8);
        if mod_count == 0 {
            return Err("hotkey must include at least one modifier".into());
        }
        // Fn is useful and unambiguous as a standalone hold-to-record key.
        // Other modifier-only shortcuts still require at least two modifiers.
        if keycode.is_none() && mod_count < 2 && !fn_key {
            return Err("modifier-only hotkey needs at least two modifiers".into());
        }

        Ok(Self {
            fn_key,
            alt,
            shift,
            control,
            meta,
            keycode,
            mode,
        })
    }

    fn mods_active(&self, flags: u64, phys_fn: bool) -> bool {
        let alt = flags & FLAG_ALTERNATE != 0;
        let shift = flags & FLAG_SHIFT != 0;
        let control = flags & FLAG_CONTROL != 0;
        let meta = flags & FLAG_COMMAND != 0;
        // Fn is judged from the *physical* Fn/Globe key state (keyCode 63),
        // never the shared secondary-Fn flag bit. Arrow keys, F1–F12, Home/End,
        // Page Up/Down and forward-delete all raise that bit, so matching on it
        // misfires a bare-Fn chord on every one of those keys.
        //
        // Exact modifier set: required on, others off. Prevents Alt+Shift
        // primary from also matching Alt+Control+Shift, etc.
        self.fn_key == phys_fn
            && self.alt == alt
            && self.shift == shift
            && self.control == control
            && self.meta == meta
    }

    /// Subset check used only for the *hold-release* decision: is every modifier
    /// this binding requires still held? Extra modifiers the binding doesn't care
    /// about are intentionally ignored.
    ///
    /// Why this exists separately from [`mods_active`](Self::mods_active): press
    /// detection must stay exact (so Alt+Shift doesn't arm while Alt+Shift+T is
    /// the active chord), but a hold-to-record must NOT stop just because a
    /// transient extra modifier appears — most commonly ⌘ while you cmd-tab away
    /// mid-recording, or a FlagsChanged an app emits when it becomes frontmost.
    /// With the exact check, "Fn held + ⌘ tapped" reads as "no longer a bare-Fn
    /// chord" and tears down the recording after the 70 ms grace. The subset
    /// check keeps it alive until a required modifier (Fn itself) is actually
    /// lifted.
    fn required_mods_active(&self, flags: u64, phys_fn: bool) -> bool {
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

    /// Higher = more specific. Prefer key+mods over pure modifier chords.
    fn specificity(&self) -> u32 {
        let mods = (self.fn_key as u32)
            + (self.alt as u32)
            + (self.shift as u32)
            + (self.control as u32)
            + (self.meta as u32);
        let key = if self.keycode.is_some() { 100 } else { 0 };
        key + mods
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
// kCGEventFlagMaskSecondaryFn / NX_SECONDARYFNMASK. Shared by the whole
// function-key class (arrows, F1–F12, nav keys, forward-delete), so it only
// tells us "physical Fn is down" when it rides a FlagsChanged event whose
// keycode is the physical Fn/Globe key (kVK_Function).
#[cfg(any(target_os = "macos", test))]
const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

/// Virtual keycode of the physical Fn/Globe key (kVK_Function). A FlagsChanged
/// event reports this keycode only when the real Fn/Globe key toggles — the
/// function-key class carries the secondary-Fn *flag* but never this keycode.
#[cfg(any(target_os = "macos", test))]
const KVK_FUNCTION: i64 = 0x3F; // 63

/// Whether the physical Fn/Globe key is currently held. Updated *only* from
/// FlagsChanged events whose keycode is `KVK_FUNCTION`, so the function-key
/// class can never flip it. Reading the secondary-Fn flag bit directly would
/// misfire on arrows, F-keys and nav keys.
static PHYSICAL_FN_DOWN: AtomicBool = AtomicBool::new(false);

/// Monotonic tap generation. Bumped every time a monitor stops (or a new one is
/// about to start), so a lingering callback belonging to a torn-down tap can
/// detect it is stale and refuse to publish physical-Fn updates. Without this,
/// a Fn release that lands while the old tap is being replaced could leave
/// `PHYSICAL_FN_DOWN` stuck `true` and re-introduce the misfire.
static TAP_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Current physical Fn/Globe held state, as observed by the event tap.
///
/// Non-macOS builds never set it, so it stays `false` (there is no Fn key
/// concept at this layer on Windows/Linux).
pub fn physical_fn_down() -> bool {
    PHYSICAL_FN_DOWN.load(Ordering::SeqCst)
}

/// Clear physical Fn and retire the current tap generation. Called at every
/// lifecycle transition where we stop observing key events (monitor stop /
/// restart): after this, any in-flight callback from the old tap is stale and
/// cannot resurrect the state. Clearing is the safe direction — a missed Fn
/// release must fail to `false` (not firing) rather than stick `true`
/// (misfiring on the next ordinary key).
fn reset_physical_fn_tracking() {
    // Bump first so a concurrent stale callback observes the new generation and
    // skips its store, then clear the state it may have left set.
    TAP_GENERATION.fetch_add(1, Ordering::SeqCst);
    PHYSICAL_FN_DOWN.store(false, Ordering::SeqCst);
}

/// Claim a fresh tap generation for a newly started tap. Any generation issued
/// earlier becomes stale.
#[cfg(any(target_os = "macos", test))]
fn begin_tap_generation() -> u64 {
    TAP_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Whether `generation` is still the live tap (i.e. no stop/restart happened
/// since it was claimed).
#[cfg(any(target_os = "macos", test))]
fn tap_generation_is_current(generation: u64) -> bool {
    TAP_GENERATION.load(Ordering::SeqCst) == generation
}

// Read the secondary-Fn bit straight from the HID event source
// (`CGEventSourceFlagsState`). Unlike the CGEventTap *event stream*, this
// reports the physical key state and is NOT disturbed when macOS emits a
// spurious Fn event during a Space switch — so it is the tie-breaker we trust
// when a Fn FlagsChanged event looks suspicious.
#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state_id: u32) -> u64;
}
// kCGEventSourceStateHIDSystemState — live hardware keyboard state.
#[cfg(target_os = "macos")]
const HID_SYSTEM_STATE: u32 = 1;

#[cfg(target_os = "macos")]
fn hid_secondary_fn_flag() -> bool {
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) & FLAG_SECONDARY_FN != 0 }
}

/// Publish physical Fn from a FlagsChanged observation, but only while
/// `generation` is still live and only for the physical Fn/Globe keycode. A
/// torn-down tap (stale generation) is ignored, and every other key — which
/// merely rides the shared secondary-Fn flag — is filtered out by the keycode.
///
/// `hid_fn` is the live secondary-Fn state read from the HID event source. A Fn
/// FlagsChanged that claims "released" is cross-checked against it: switching
/// Spaces (Mission Control) makes macOS emit a bogus keyCode-63 "released" event
/// while the key is physically still down, and trusting it would stop an
/// in-flight recording. If the HID flag still says Fn is held, the event is
/// treated as spurious and the held state is preserved.
#[cfg(any(target_os = "macos", test))]
fn observe_flags_changed(generation: u64, keycode: i64, flags: u64, hid_fn: bool) {
    if keycode != KVK_FUNCTION || !tap_generation_is_current(generation) {
        return;
    }
    if flags & FLAG_SECONDARY_FN != 0 {
        PHYSICAL_FN_DOWN.store(true, Ordering::SeqCst);
        return;
    }
    // Event claims Fn released. Cross-check the live HID flag: if Fn is still
    // physically held, this is the spurious Fn-up macOS emits on a Space switch.
    if hid_fn {
        tracing::info!(
            "ignored spurious Fn-up (keycode 63, no fn flag): HID still reports Fn held (likely Space switch)"
        );
        return;
    }
    PHYSICAL_FN_DOWN.store(false, Ordering::SeqCst);
}

/// Clear physical Fn when the system disables the tap: we stop receiving events,
/// so a Fn release during the gap would otherwise be lost. Guarded on the live
/// generation so a stale tap can't clear the live one's state.
#[cfg(any(target_os = "macos", test))]
fn observe_tap_disabled(generation: u64) {
    if tap_generation_is_current(generation) {
        PHYSICAL_FN_DOWN.store(false, Ordering::SeqCst);
    }
}

#[cfg(target_os = "macos")]
fn monitor_tap_options() -> core_graphics::event::CGEventTapOptions {
    // Hotkeys only observe the global keyboard stream. A passive tap makes
    // that product boundary enforceable by macOS: callback bugs, stalls, or
    // lifecycle races can never suppress typing in this or any other app.
    core_graphics::event::CGEventTapOptions::ListenOnly
}

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
    // We are no longer observing key events; retire the generation and clear Fn
    // so a release missed during teardown can't leave a bare-Fn chord armed.
    reset_physical_fn_tracking();
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
                // Claim a fresh generation so this tap — and only this tap —
                // may publish physical-Fn updates until the next stop/restart.
                let generation = begin_tap_generation();
                run_tap_loop_multi(bindings, on_edge, stop, generation, ready_tx);
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
    generation: u64,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) where
    F: Fn(HotkeyEdge, String) + Send + 'static,
{
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapPlacement, CGEventType, EventField,
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
        monitor_tap_options(),
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
                // Lost our event stream: clear Fn so a release missed during the
                // outage can't leave a bare-Fn chord armed once we re-enable.
                observe_tap_disabled(generation);
                tracing::warn!("keyboard event tap disabled by system; will re-enable");
                return None;
            }

            let flags = event.get_flags().bits();
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);

            // Physical Fn/Globe state is authoritative and comes *only* from a
            // FlagsChanged event carrying the Fn keycode (63). Every other key —
            // arrows, F1–F12, nav, forward-delete — raises the secondary-Fn flag
            // bit but never this keycode, so it can no longer flip Fn on. The
            // update is generation-guarded so a torn-down tap can't publish.
            if matches!(etype, CGEventType::FlagsChanged) {
                // Only the Fn key's own FlagsChanged drives physical-Fn state,
                // so only pay the HID FFI cost for that keycode.
                let hid_fn = if keycode == KVK_FUNCTION {
                    hid_secondary_fn_flag()
                } else {
                    false
                };
                observe_flags_changed(generation, keycode, flags, hid_fn);
            }
            let phys_fn = PHYSICAL_FN_DOWN.load(Ordering::SeqCst);

            let mut map = latches_c.borrow_mut();
            // Track per-binding key held, then pick the single most-specific match.
            // e.g. primary Alt+Shift must lose to translate Alt+Shift+T while T is held.
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
            }

            // If any key-chord binding currently matches, prefer it over pure modifier chords
            // (so Alt+Shift+T wins over Alt+Shift while T is held).
            let mut best_id: Option<String> = None;
            let mut best_score: i32 = -1;
            for b in bindings_c.iter() {
                let latch = map.get(&b.id).unwrap();
                let want = if b.spec.keycode.is_some() {
                    b.spec.mods_active(flags, phys_fn) && latch.key_held
                } else {
                    b.spec.mods_active(flags, phys_fn)
                };
                if want {
                    let score = b.spec.specificity() as i32;
                    if score > best_score {
                        best_score = score;
                        best_id = Some(b.id.clone());
                    }
                }
            }

            for b in bindings_c.iter() {
                let latch = map.get_mut(&b.id).unwrap();
                let want = best_id.as_ref().map(|id| id == &b.id).unwrap_or(false);

                if want {
                    latch.release_after = None;
                    if !latch.active {
                        latch.active = true;
                        tracing::info!(id = %b.id, score = best_score, "hotkey press");
                        on_edge_c(HotkeyEdge::Press, b.id.clone());
                    }
                } else if latch.active {
                    // `want` is false. Three possible reasons:
                    //   1. superseded — a more-specific chord exactly matched;
                    //   2. a required modifier (or the key) was genuinely lifted;
                    //   3. a transient extra modifier appeared (⌘ during a cmd-tab,
                    //      or a FlagsChanged the frontmost app emitted on focus).
                    // Only (1) and (2) should stop a hold. (3) must keep the
                    // recording alive, or switching windows mid-record kills it.
                    let superseded = best_id.is_some(); // another chord matched
                    let required_held = b.spec.required_mods_active(flags, phys_fn)
                        && (b.spec.keycode.is_none() || latch.key_held);
                    if superseded || !required_held {
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
                    } else {
                        // transient extra modifier — keep holding, cancel any
                        // pending release so a flicker doesn't stop the recording.
                        latch.release_after = None;
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

    #[test]
    fn fn_matches_physical_key_not_shared_flag() {
        let s = HotkeySpec::parse("Fn", HotkeyMode::Hold).unwrap();
        assert!(s.fn_key && s.keycode.is_none());
        // Physical Fn/Globe held (keyCode 63) → match, regardless of raw flags.
        assert!(s.mods_active(0, true));
        assert!(s.mods_active(FLAG_SECONDARY_FN, true));
        // Regression: the shared secondary-Fn bit alone (an arrow key, an F-key,
        // a nav key) must NOT match a bare-Fn chord — physical Fn isn't held.
        assert!(!s.mods_active(FLAG_SECONDARY_FN, false));
        assert!(!s.mods_active(0, false));
        // A real extra modifier while Fn is held is still not a bare-Fn chord.
        assert!(!s.mods_active(FLAG_SECONDARY_FN | FLAG_SHIFT, true));
    }

    #[test]
    fn required_mods_ignores_transient_extra_modifier() {
        // Hold-release subset check: a bare-Fn hold must stay "required held"
        // while an extra modifier (⌘ during a cmd-tab) is also down, so an
        // in-flight recording isn't torn down. Only lifting Fn clears it.
        let s = HotkeySpec::parse("Fn", HotkeyMode::Hold).unwrap();
        // Fn held, no extras → exact and subset both true.
        assert!(s.mods_active(0, true));
        assert!(s.required_mods_active(0, true));
        // Fn held + ⌘ (cmd-tab flicker): exact false, subset still true → keep.
        assert!(!s.mods_active(FLAG_COMMAND, true));
        assert!(s.required_mods_active(FLAG_COMMAND, true));
        // Fn released → subset false → release.
        assert!(!s.required_mods_active(FLAG_COMMAND, false));
        assert!(!s.required_mods_active(0, false));

        // Two-modifier chord: Alt+Shift stays held while ⌘ is also down, but
        // lifting Shift clears it.
        let as_ = HotkeySpec::parse("Alt+Shift", HotkeyMode::Hold).unwrap();
        assert!(as_.required_mods_active(FLAG_ALTERNATE | FLAG_SHIFT | FLAG_COMMAND, false));
        assert!(!as_.required_mods_active(FLAG_ALTERNATE, false));
    }

    #[test]
    fn globe_is_an_alias_for_fn() {
        let s = HotkeySpec::parse("Globe", HotkeyMode::Hold).unwrap();
        assert!(s.fn_key);
    }

    #[test]
    fn key_chord_more_specific_than_mods_only() {
        let mods = HotkeySpec::parse("Alt+Shift", HotkeyMode::Hold).unwrap();
        let key = HotkeySpec::parse("Alt+Shift+T", HotkeyMode::Hold).unwrap();
        assert!(key.specificity() > mods.specificity());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keyboard_monitor_cannot_intercept_input() {
        use core_graphics::event::CGEventTapOptions;

        assert_eq!(
            monitor_tap_options() as u32,
            CGEventTapOptions::ListenOnly as u32,
            "global hotkeys only observe input and must use a passive event tap"
        );
    }

    // The physical-Fn state is a process-global atomic; serialize the tests that
    // mutate it so parallel runs don't clobber each other.
    static FN_STATE_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn arrow_keycode_never_publishes_physical_fn() {
        let _g = FN_STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        reset_physical_fn_tracking();
        let generation = begin_tap_generation();
        // Up arrow (0x7E) raises the shared secondary-Fn flag bit but is NOT
        // keyCode 63 — the exact root cause of the misfire. It must not arm Fn.
        observe_flags_changed(generation, 0x7E, FLAG_SECONDARY_FN, false);
        assert!(!physical_fn_down(), "arrow key must not set physical Fn");
        // The genuine Fn/Globe keycode still tracks correctly.
        observe_flags_changed(generation, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(physical_fn_down(), "physical Fn/Globe key sets the state");
        reset_physical_fn_tracking();
    }

    #[test]
    fn stop_clears_and_invalidates_physical_fn() {
        let _g = FN_STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        reset_physical_fn_tracking();

        let generation = begin_tap_generation();
        observe_flags_changed(generation, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(physical_fn_down(), "Fn should be set while observed");

        // Stopping the monitor must clear Fn immediately...
        reset_physical_fn_tracking();
        assert!(!physical_fn_down(), "stop must clear physical Fn");

        // ...and retire this generation, so the old tap's lingering callback
        // (e.g. a stray Fn-down event after the Fn-up was missed) cannot
        // resurrect the state and re-arm a bare-Fn chord.
        observe_flags_changed(generation, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(!physical_fn_down(), "a stale tap must not publish Fn state");
    }

    #[test]
    fn tap_disabled_clears_physical_fn() {
        let _g = FN_STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        reset_physical_fn_tracking();

        let generation = begin_tap_generation();
        observe_flags_changed(generation, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(physical_fn_down());

        // System disables the tap: a Fn release during the gap would be lost, so
        // Fn must be cleared to avoid a later ordinary key matching bare-Fn.
        observe_tap_disabled(generation);
        assert!(!physical_fn_down(), "tap disable must clear physical Fn");
        reset_physical_fn_tracking();
    }

    #[test]
    fn spurious_fn_up_during_space_switch_is_ignored() {
        // Switching Spaces makes macOS emit a keyCode-63 FlagsChanged with NO
        // secondary-Fn flag while the key is physically still down. The HID flag
        // still reports Fn held, so the bogus event must NOT clear physical Fn —
        // otherwise an in-flight recording gets torn down.
        let _g = FN_STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        reset_physical_fn_tracking();

        let generation = begin_tap_generation();
        // Fn genuinely pressed.
        observe_flags_changed(generation, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(physical_fn_down());

        // Spurious Fn-up: event claims released, but HID still reports held.
        observe_flags_changed(generation, KVK_FUNCTION, 0, true);
        assert!(
            physical_fn_down(),
            "spurious Fn-up with HID still held must not clear Fn"
        );

        // Genuine Fn-up: HID agrees Fn is up → clears.
        observe_flags_changed(generation, KVK_FUNCTION, 0, false);
        assert!(!physical_fn_down(), "real Fn-up (HID clear) must clear Fn");
        reset_physical_fn_tracking();
    }

    #[test]
    fn restart_resumes_tracking() {
        let _g = FN_STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        reset_physical_fn_tracking();

        // Old monitor observed Fn down, then stopped (state cleared, gen retired).
        let old = begin_tap_generation();
        observe_flags_changed(old, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        reset_physical_fn_tracking();
        assert!(!physical_fn_down());

        // A fresh monitor claims a new generation and tracks correctly again.
        let new = begin_tap_generation();
        assert_ne!(old, new, "restart must claim a distinct generation");
        observe_flags_changed(new, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(physical_fn_down(), "restarted tap tracks Fn down");
        observe_flags_changed(new, KVK_FUNCTION, 0, false);
        assert!(!physical_fn_down(), "restarted tap tracks Fn up");

        // The retired old generation still cannot publish after the restart.
        observe_flags_changed(old, KVK_FUNCTION, FLAG_SECONDARY_FN, false);
        assert!(!physical_fn_down(), "retired generation stays inert");
        reset_physical_fn_tracking();
    }
}
