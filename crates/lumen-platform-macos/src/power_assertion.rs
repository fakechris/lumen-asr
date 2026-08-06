//! Hold a macOS activity that prevents user-idle system sleep and disables App
//! Nap for the duration of a meeting recording.
//!
//! While a long recording is running, macOS may put the machine into idle
//! system sleep or suspend the process under App Nap when the app is in the
//! background. Either one stops the audio capture callbacks from firing, so
//! minutes of the meeting are silently dropped. Holding an
//! `NSProcessInfo` activity for the recording keeps the process scheduled and
//! keeps the system awake while the user is idle, so capture is not suspended.
//!
//! This cannot prevent lid-close or battery-dead sleep — only the idle / App
//! Nap case. The guard is infallible: if the activity cannot be acquired it
//! degrades to a no-op so recording is never blocked.

/// RAII guard that holds an idle-system-sleep / App-Nap activity while it is
/// alive and releases it on drop.
///
/// Acquire it when a meeting recording starts and drop it when the recording
/// stops. Acquisition never fails; on any platform or runtime issue the guard
/// is a no-op.
pub struct MeetingPowerGuard {
    #[cfg(target_os = "macos")]
    token: Option<objc2::rc::Retained<objc2::runtime::AnyObject>>,
}

// SAFETY: the only field is an opaque NSProcessInfo activity token. It is never
// dereferenced by us; we only pass it back to `endActivity:` and let its
// `Retained` release it. Objective-C reference counting is atomic, and
// `endActivity:` may be called from any thread, so moving the guard across
// threads and sharing an owning `Mutex` between threads is sound. This is
// required so `AppState` (a Tauri `State`, which must be `Send + Sync`) can
// hold the guard in a `Mutex`.
#[cfg(target_os = "macos")]
unsafe impl Send for MeetingPowerGuard {}
#[cfg(target_os = "macos")]
unsafe impl Sync for MeetingPowerGuard {}

#[cfg(target_os = "macos")]
mod imp {
    use super::MeetingPowerGuard;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    // NSActivityOptions bitmask (Foundation/NSProcessInfo.h).
    //
    // `NSActivityUserInitiated` marks the work as user-visible so the process is
    // not napped; `NSActivityIdleSystemSleepDisabled` keeps the system awake
    // while the user is idle. OR-ed into the `NSActivityOptions` (NSUInteger).
    const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF;
    const NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED: u64 = 1 << 20;

    const REASON: &str = "Lumen meeting recording";

    fn process_info() -> Option<*mut AnyObject> {
        let cls = AnyClass::get(c"NSProcessInfo")?;
        // `processInfo` returns the shared, process-lived singleton (+0, not
        // owned by us) — safe to use transiently for the message sends below.
        let info: *mut AnyObject = unsafe { msg_send![cls, processInfo] };
        (!info.is_null()).then_some(info)
    }

    pub(super) fn acquire() -> MeetingPowerGuard {
        let token = process_info().and_then(|info| {
            let reason = NSString::from_str(REASON);
            let options = NS_ACTIVITY_USER_INITIATED | NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED;
            // SAFETY: `beginActivityWithOptions:reason:` is a documented
            // NSProcessInfo method. It returns an autoreleased (+0) activity
            // token; retain it so we own a +1 that outlives the autorelease
            // pool, and hand that to `Retained::from_raw`.
            unsafe {
                let token: *mut AnyObject =
                    msg_send![info, beginActivityWithOptions: options, reason: &*reason];
                if token.is_null() {
                    return None;
                }
                let retained: *mut AnyObject = msg_send![token, retain];
                Retained::from_raw(retained)
            }
        });
        MeetingPowerGuard { token }
    }

    pub(super) fn release(guard: &mut MeetingPowerGuard) {
        // Take the token first so it is released exactly once even if we bail.
        let Some(token) = guard.token.take() else {
            return;
        };
        let Some(info) = process_info() else {
            return;
        };
        // SAFETY: `endActivity:` is the documented counterpart to
        // `beginActivityWithOptions:reason:`; `token` is the activity we began.
        // Dropping `token` afterwards releases the +1 we retained in `acquire`.
        unsafe {
            let _: () = msg_send![info, endActivity: &*token];
        }
    }
}

impl MeetingPowerGuard {
    /// Acquire the activity. Infallible: returns a live guard on success and a
    /// no-op guard if the activity is unavailable. Never panics.
    #[cfg(target_os = "macos")]
    pub fn acquire() -> Self {
        imp::acquire()
    }

    /// Off-macOS: no activity to hold, always a no-op guard.
    #[cfg(not(target_os = "macos"))]
    pub fn acquire() -> Self {
        MeetingPowerGuard {}
    }
}

impl Drop for MeetingPowerGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        imp::release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::MeetingPowerGuard;

    #[test]
    fn acquire_then_drop_does_not_panic() {
        let guard = MeetingPowerGuard::acquire();
        drop(guard);
    }

    #[test]
    fn repeated_acquire_release_is_stable() {
        for _ in 0..3 {
            let _guard = MeetingPowerGuard::acquire();
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_is_noop() {
        // The guard carries no state off macOS; acquiring and dropping is a
        // pure no-op that must never fail.
        let guard = MeetingPowerGuard::acquire();
        drop(guard);
    }
}
