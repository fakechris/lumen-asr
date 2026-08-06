//! macOS power monitoring for meeting recordings: battery level snapshots and
//! an imminent-sleep observer.
//!
//! A long meeting recording can be silently cut off when the Mac runs out of
//! battery or is put to sleep (lid close, forced sleep). The idle-sleep hold
//! held during recording only covers *idle* system sleep; it cannot stop a
//! drained battery or a lid close. This module surfaces the two signals the app
//! layer needs to warn the user **before** audio is lost:
//!
//! - [`battery_status`] reads the current power-source state via IOKit so the
//!   app can warn when recording on a low battery.
//! - [`install_will_sleep_observer`] fires a callback when the system is about
//!   to sleep (`NSWorkspaceWillSleepNotification`).
//!
//! Both are best-effort and degrade to nothing off-macOS or on any failure.

/// A snapshot of the primary battery's charge and power source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryStatus {
    /// Charge as a whole percent in `0..=100`.
    pub percent: u8,
    /// `true` when running on AC (wall) power, `false` when on battery.
    pub on_ac: bool,
}

/// Read the primary battery's charge and power source.
///
/// Returns `None` when there is no battery to read (a desktop Mac), off-macOS,
/// or on any IOKit/CoreFoundation failure — it never panics.
pub fn battery_status() -> Option<BatteryStatus> {
    #[cfg(target_os = "macos")]
    {
        imp::battery_status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Register a callback for `NSWorkspaceWillSleepNotification` on the shared
/// workspace notification center, so the app can warn that recording is about
/// to be interrupted by system sleep.
///
/// The observer lives for the whole process: it is registered once at app
/// startup and the retained observer token is intentionally leaked (there is no
/// unregister path — the callback is wanted for the app's entire lifetime).
///
/// **Threading:** must be called on the main thread. AppKit's workspace
/// notification center is main-thread affine, and the will-sleep notification
/// is delivered on the main thread; the caller registers this during Tauri
/// `setup(...)`, which runs on the main thread. `on_will_sleep` is `Send + Sync`
/// so the retained block satisfies the API's sendability requirement.
///
/// Off-macOS this is a no-op.
pub fn install_will_sleep_observer<F>(on_will_sleep: F)
where
    F: Fn() + Send + Sync + 'static,
{
    #[cfg(target_os = "macos")]
    {
        imp::install_will_sleep_observer(on_will_sleep);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = on_will_sleep;
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::BatteryStatus;
    use std::os::raw::c_void;

    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    // IOKit power-source description keys and values (IOKit/ps/IOPSKeys.h). These
    // are plain string constants in the SDK headers, so we build the CFStrings
    // from the same literals rather than link the extern key symbols.
    const KEY_TYPE: &str = "Type";
    const KEY_CURRENT_CAPACITY: &str = "Current Capacity";
    const KEY_MAX_CAPACITY: &str = "Max Capacity";
    const KEY_POWER_SOURCE_STATE: &str = "Power Source State";
    const VALUE_INTERNAL_BATTERY: &str = "InternalBattery";
    const VALUE_AC_POWER: &str = "AC Power";

    // SAFETY: standard IOKit power-sources entry points.
    //
    // - `IOPSCopyPowerSourcesInfo` returns a +1 (create-rule) blob we own.
    // - `IOPSCopyPowerSourcesList` returns a +1 (create-rule) array we own.
    // - `IOPSGetPowerSourceDescription` returns a +0 (get-rule) dictionary we
    //   must not release.
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
        fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFDictionaryRef;
    }

    /// Look up `key` in a power-source description dictionary and take an owned
    /// [`CFType`] copy of the value. `None` when the key is absent.
    fn dict_value(dict: &CFDictionary, key: &'static str) -> Option<CFType> {
        let cf_key = CFString::from_static_string(key);
        let value: *const c_void = *dict.find(cf_key.as_CFTypeRef())?;
        if value.is_null() {
            return None;
        }
        // Get-rule: retain a +1 we own, released when the returned CFType drops.
        Some(unsafe { CFType::wrap_under_get_rule(value) })
    }

    /// Turn one power-source description into a [`BatteryStatus`]. `None` when the
    /// source is not the internal battery — e.g. an external UPS, which also
    /// carries capacity keys but must not be reported as the machine's battery.
    fn status_from_description(dict: &CFDictionary) -> Option<BatteryStatus> {
        let source_type = dict_value(dict, KEY_TYPE)?
            .downcast::<CFString>()?
            .to_string();
        if source_type != VALUE_INTERNAL_BATTERY {
            return None;
        }
        let current = dict_value(dict, KEY_CURRENT_CAPACITY)?
            .downcast::<CFNumber>()?
            .to_i32()?;
        let max = dict_value(dict, KEY_MAX_CAPACITY)?
            .downcast::<CFNumber>()?
            .to_i32()?;
        if max <= 0 {
            return None;
        }
        let state = dict_value(dict, KEY_POWER_SOURCE_STATE)?
            .downcast::<CFString>()?
            .to_string();
        let percent = ((current as i64 * 100) / max as i64).clamp(0, 100) as u8;
        Some(BatteryStatus {
            percent,
            on_ac: state == VALUE_AC_POWER,
        })
    }

    pub(super) fn battery_status() -> Option<BatteryStatus> {
        // SAFETY: create-rule blob; wrapped so it releases on drop.
        let blob_ref = unsafe { IOPSCopyPowerSourcesInfo() };
        if blob_ref.is_null() {
            return None;
        }
        let blob = unsafe { CFType::wrap_under_create_rule(blob_ref) };

        // SAFETY: create-rule array of power-source handles.
        let list_ref = unsafe { IOPSCopyPowerSourcesList(blob.as_CFTypeRef()) };
        if list_ref.is_null() {
            return None;
        }
        let list = unsafe { CFArray::<CFType>::wrap_under_create_rule(list_ref) };

        for source in list.iter() {
            // SAFETY: get-rule dictionary owned by `blob`; do not release it.
            let desc_ref = unsafe {
                IOPSGetPowerSourceDescription(blob.as_CFTypeRef(), source.as_CFTypeRef())
            };
            if desc_ref.is_null() {
                continue;
            }
            let desc = unsafe { CFDictionary::wrap_under_get_rule(desc_ref) };
            if let Some(status) = status_from_description(&desc) {
                return Some(status);
            }
        }
        None
    }

    pub(super) fn install_will_sleep_observer<F>(on_will_sleep: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        use block2::RcBlock;
        use objc2_app_kit::{NSWorkspace, NSWorkspaceWillSleepNotification};
        use objc2_foundation::NSNotification;
        use std::ptr::NonNull;

        // The block is invoked by AppKit on the main thread when the system is
        // about to sleep; it just forwards to the caller's closure.
        let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            on_will_sleep();
        });

        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: `addObserverForName:object:queue:usingBlock:` with a well-known
        // notification name, no object filter, and no queue (deliver on the
        // posting thread). The returned observer token is retained by us and
        // deliberately leaked below so the observer lives for the app's lifetime.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceWillSleepNotification),
                None,
                None,
                &block,
            )
        };
        // Intentionally leak the observer token: there is no unregister path and
        // the callback is wanted for the whole process lifetime.
        std::mem::forget(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_status_does_not_panic() {
        // Host-dependent: `Some` on a laptop, `None` on a desktop / off-macOS.
        // The only invariant we can assert everywhere is that it returns.
        let status = battery_status();
        if let Some(s) = status {
            assert!(s.percent <= 100);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn battery_status_is_none_off_macos() {
        assert!(battery_status().is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn install_will_sleep_observer_is_noop_off_macos() {
        // Purely a compile+call check; the stub does nothing.
        install_will_sleep_observer(|| {});
    }
}
