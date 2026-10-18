//! Global Esc that cancels an active dictation.
//!
//! The key is grabbed only while a dictation is recording or processing and is
//! released as soon as the dictation returns to idle, so Esc keeps its normal
//! meaning in every other app the rest of the time. On macOS the grab is a
//! Carbon `RegisterEventHotKey` (keycode 53, no modifiers): it consumes the key
//! press and needs no Accessibility permission. Other platforms are a no-op.
//!
//! [`set_esc_cancel_active`] must be called on the main thread: the Carbon
//! Event Manager is not thread-safe, and hot-key events are delivered through
//! the main run loop.

use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, String>;
type Handler = Arc<dyn Fn() + Send + Sync>;

trait EscHotkeyBackend {
    fn register(&mut self) -> Result<()>;
    fn unregister(&mut self) -> Result<()>;
}

/// Tracks whether the Esc grab is held and only talks to the backend on an
/// actual idle <-> active edge, so repeated syncs are cheap and idempotent.
struct EscRegistration<B> {
    backend: B,
    registered: bool,
}

impl<B: EscHotkeyBackend> EscRegistration<B> {
    const fn new(backend: B) -> Self {
        Self {
            backend,
            registered: false,
        }
    }

    /// Converge on `active`. Returns whether Esc is grabbed afterwards.
    ///
    /// A failed register leaves the grab released so the next sync retries.
    /// A failed unregister still counts as released: keeping a stale "held"
    /// flag would make the next activation skip registering a fresh grab.
    fn sync(&mut self, active: bool) -> Result<bool> {
        match (active, self.registered) {
            (true, false) => {
                self.backend.register()?;
                self.registered = true;
            }
            (false, true) => {
                self.registered = false;
                self.backend.unregister()?;
            }
            _ => {}
        }
        Ok(self.registered)
    }
}

static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static REGISTRATION: Mutex<EscRegistration<Native>> =
    Mutex::new(EscRegistration::new(Native::new()));

/// True when this platform can grab Esc globally.
pub const fn esc_cancel_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Set the callback run (on the main thread) when the grabbed Esc is pressed.
/// Keep it short; hand real work off to another thread or task.
pub fn set_esc_cancel_handler(handler: impl Fn() + Send + Sync + 'static) {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(handler));
}

/// Grab (`true`) or release (`false`) the global Esc. Idempotent. Call on the
/// main thread. Returns whether Esc is grabbed afterwards.
pub fn set_esc_cancel_active(active: bool) -> Result<bool> {
    REGISTRATION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sync(active)
}

#[allow(dead_code)] // only reached from the Carbon callback on macOS
fn dispatch_esc_pressed() {
    let handler = HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(handler) = handler {
        handler();
    }
}

#[cfg(not(target_os = "macos"))]
struct Native;

#[cfg(not(target_os = "macos"))]
impl Native {
    const fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "macos"))]
impl EscHotkeyBackend for Native {
    fn register(&mut self) -> Result<()> {
        Ok(())
    }
    fn unregister(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
use carbon::Native;

#[cfg(target_os = "macos")]
mod carbon {
    use super::{dispatch_esc_pressed, EscHotkeyBackend, Result};
    use std::ffi::{c_ulong, c_void};
    use std::ptr;

    type OsStatus = i32;
    type EventTargetRef = *mut c_void;
    type EventHandlerCallRef = *mut c_void;
    type EventRef = *mut c_void;
    type EventHandlerRef = *mut c_void;
    type EventHotKeyRef = *mut c_void;
    type EventHandlerUpp = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OsStatus;

    #[repr(C)]
    struct EventTypeSpec {
        event_class: u32,
        event_kind: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }

    const NO_ERR: OsStatus = 0;
    const EVENT_NOT_HANDLED_ERR: OsStatus = -9874;
    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
    const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");
    /// kVK_Escape.
    const ESCAPE_KEY_CODE: u32 = 53;
    /// Distinguishes our hot key from any other registered in this process.
    const SIGNATURE: u32 = u32::from_be_bytes(*b"LmEs");
    const ESC_ID: u32 = 1;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn GetApplicationEventTarget() -> EventTargetRef;
        fn InstallEventHandler(
            target: EventTargetRef,
            handler: EventHandlerUpp,
            num_types: c_ulong,
            list: *const EventTypeSpec,
            user_data: *mut c_void,
            out_ref: *mut EventHandlerRef,
        ) -> OsStatus;
        fn RegisterEventHotKey(
            key_code: u32,
            modifiers: u32,
            id: EventHotKeyId,
            target: EventTargetRef,
            options: u32,
            out_ref: *mut EventHotKeyRef,
        ) -> OsStatus;
        fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OsStatus;
        fn GetEventParameter(
            event: EventRef,
            name: u32,
            desired_type: u32,
            out_actual_type: *mut u32,
            buffer_size: c_ulong,
            out_actual_size: *mut c_ulong,
            out_data: *mut c_void,
        ) -> OsStatus;
    }

    extern "C" fn on_hot_key(
        _call: EventHandlerCallRef,
        event: EventRef,
        _user_data: *mut c_void,
    ) -> OsStatus {
        let mut id = EventHotKeyId {
            signature: 0,
            id: 0,
        };
        let status = unsafe {
            GetEventParameter(
                event,
                K_EVENT_PARAM_DIRECT_OBJECT,
                TYPE_EVENT_HOT_KEY_ID,
                ptr::null_mut(),
                std::mem::size_of::<EventHotKeyId>() as c_ulong,
                ptr::null_mut(),
                &mut id as *mut EventHotKeyId as *mut c_void,
            )
        };
        // Leave every other hot key in the process to its own handler.
        if status != NO_ERR || id.signature != SIGNATURE || id.id != ESC_ID {
            return EVENT_NOT_HANDLED_ERR;
        }
        // Unwinding across the C boundary aborts the process.
        if std::panic::catch_unwind(dispatch_esc_pressed).is_err() {
            tracing::error!("esc cancel handler panicked");
        }
        NO_ERR
    }

    pub(super) struct Native {
        handler_installed: bool,
        hot_key: Option<HotKeyRef>,
    }

    /// Carbon hot-key handle; only touched on the main thread.
    struct HotKeyRef(EventHotKeyRef);
    // SAFETY: the handle is an opaque token only passed back to Carbon, and
    // every call that uses it happens on the main thread (module contract).
    unsafe impl Send for HotKeyRef {}

    impl Native {
        pub(super) const fn new() -> Self {
            Self {
                handler_installed: false,
                hot_key: None,
            }
        }

        fn ensure_handler(&mut self) -> Result<()> {
            if self.handler_installed {
                return Ok(());
            }
            let spec = EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_PRESSED,
            };
            let mut handler_ref: EventHandlerRef = ptr::null_mut();
            // The handler lives for the process lifetime; its ref is never removed.
            let status = unsafe {
                InstallEventHandler(
                    GetApplicationEventTarget(),
                    on_hot_key,
                    1,
                    &spec,
                    ptr::null_mut(),
                    &mut handler_ref,
                )
            };
            if status != NO_ERR {
                return Err(format!("InstallEventHandler failed: OSStatus {status}"));
            }
            self.handler_installed = true;
            Ok(())
        }
    }

    impl EscHotkeyBackend for Native {
        fn register(&mut self) -> Result<()> {
            self.ensure_handler()?;
            let mut hot_key: EventHotKeyRef = ptr::null_mut();
            let status = unsafe {
                RegisterEventHotKey(
                    ESCAPE_KEY_CODE,
                    0,
                    EventHotKeyId {
                        signature: SIGNATURE,
                        id: ESC_ID,
                    },
                    GetApplicationEventTarget(),
                    0,
                    &mut hot_key,
                )
            };
            if status != NO_ERR || hot_key.is_null() {
                return Err(format!(
                    "RegisterEventHotKey(Esc) failed: OSStatus {status}"
                ));
            }
            self.hot_key = Some(HotKeyRef(hot_key));
            Ok(())
        }

        fn unregister(&mut self) -> Result<()> {
            let Some(HotKeyRef(hot_key)) = self.hot_key.take() else {
                return Ok(());
            };
            let status = unsafe { UnregisterEventHotKey(hot_key) };
            if status != NO_ERR {
                return Err(format!(
                    "UnregisterEventHotKey(Esc) failed: OSStatus {status}"
                ));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: Vec<&'static str>,
        fail_register: bool,
        fail_unregister: bool,
    }

    impl EscHotkeyBackend for Fake {
        fn register(&mut self) -> Result<()> {
            self.calls.push("register");
            if self.fail_register {
                return Err("taken".into());
            }
            Ok(())
        }
        fn unregister(&mut self) -> Result<()> {
            self.calls.push("unregister");
            if self.fail_unregister {
                return Err("gone".into());
            }
            Ok(())
        }
    }

    fn registration() -> EscRegistration<Fake> {
        EscRegistration::new(Fake::default())
    }

    #[test]
    fn idle_never_grabs_esc() {
        let mut r = registration();
        assert!(!r.sync(false).unwrap());
        assert!(!r.sync(false).unwrap());
        assert!(r.backend.calls.is_empty());
    }

    #[test]
    fn grabs_once_while_active_and_releases_on_idle() {
        let mut r = registration();
        assert!(r.sync(true).unwrap());
        // recording -> processing keeps the same grab
        assert!(r.sync(true).unwrap());
        assert!(!r.sync(false).unwrap());
        assert!(!r.sync(false).unwrap());
        assert!(r.sync(true).unwrap());
        assert_eq!(r.backend.calls, vec!["register", "unregister", "register"]);
    }

    #[test]
    fn failed_register_stays_released_and_retries_next_activation() {
        let mut r = registration();
        r.backend.fail_register = true;
        assert!(r.sync(true).is_err());
        assert!(!r.registered);
        // Going idle must not try to release a grab we never got.
        assert!(!r.sync(false).unwrap());
        r.backend.fail_register = false;
        assert!(r.sync(true).unwrap());
        assert_eq!(r.backend.calls, vec!["register", "register"]);
    }

    #[test]
    fn failed_unregister_still_counts_as_released() {
        let mut r = registration();
        r.sync(true).unwrap();
        r.backend.fail_unregister = true;
        assert!(r.sync(false).is_err());
        assert!(!r.registered);
        r.backend.fail_unregister = false;
        assert!(r.sync(true).unwrap());
        assert_eq!(r.backend.calls, vec!["register", "unregister", "register"]);
    }
}
