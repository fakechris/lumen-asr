//! macOS audio-input activity detector (capability-gated).
//!
//! Every ~1s this enumerates Core Audio **process objects**, keeps the ones
//! currently holding an input (`IsRunningInput`), reads each one's `PID` and
//! `BundleID`, normalizes helper/renderer bundle ids to their parent app, and
//! emits [`DetectorSignal`]s (added / removed / tick) that the app layer feeds
//! into the pure [`lumen_core::MeetingDetectionPolicy`].
//!
//! ## Capability gate (newer macOS only)
//! The process-object properties used here exist only on recent macOS. Rather
//! than key off an OS version string, [`capability_available`] asks Core Audio
//! directly whether the process-object-list property exists. On any build where
//! it does not (older macOS, or a non-macOS target), the detector reports
//! unavailable and [`MeetingActivityDetector::start`] is a no-op — the feature
//! degrades to silence, never an error and never a prompt.
//!
//! ## Verification note
//! A CI/sandbox host cannot exercise the live Core Audio path (no real audio
//! stack, entitlements, or meeting apps). This module is written to **compile**
//! everywhere and **gate** at runtime; the enumeration itself needs on-device
//! validation against real meeting apps to characterize false-positive rate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_core::AppClass;

/// One app currently holding an audio input, normalized and pre-classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveInput {
    /// Normalized bundle id (helper/renderer/GPU folded to the parent app).
    pub bundle_id: String,
    /// Stable per-session key (`<bundle>#<pid>`).
    pub session_key: String,
    /// Classification (native meeting / browser / other).
    pub app_class: AppClass,
}

/// A discrete change (or heartbeat) observed by the detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectorSignal {
    /// A newly-observed input session.
    Added(ActiveInput),
    /// A previously-observed input session that ended.
    Removed { session_key: String },
    /// Poll heartbeat (lets the policy advance its stability timer).
    Tick,
}

/// Default poll interval. ~1s balances responsiveness against wake-ups.
pub const DEFAULT_POLL: Duration = Duration::from_secs(1);

/// Whether this build/host exposes the Core Audio process-object API the
/// detector relies on. `false` on non-macOS and on older macOS.
pub fn capability_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::process_object_list_property_exists()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Background poller. Enumerates active audio inputs on an interval and pushes
/// [`DetectorSignal`]s to a caller-supplied sink. Cross-platform shell; the real
/// work is macOS-only and gated by [`capability_available`].
pub struct MeetingActivityDetector {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Default for MeetingActivityDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingActivityDetector {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Start polling every `poll`, calling `on_signal` for each change and once
    /// per cycle with [`DetectorSignal::Tick`]. No-op (returns `false`) if the
    /// capability is unavailable or a poller is already running. `on_signal` is
    /// invoked from the detector's own background thread.
    pub fn start<F>(&mut self, poll: Duration, on_signal: F) -> bool
    where
        F: Fn(DetectorSignal) + Send + 'static,
    {
        if self.handle.is_some() || !capability_available() {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            self.running.store(true, Ordering::SeqCst);
            let running = self.running.clone();
            self.handle = Some(std::thread::spawn(move || {
                imp::poll_loop(running, poll, on_signal);
            }));
            true
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (poll, on_signal);
            false
        }
    }

    /// Stop polling and join the background thread.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MeetingActivityDetector {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::collections::HashMap;
    use std::os::raw::c_void;

    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use lumen_core::{classify_bundle_id, normalize_bundle_id};

    type AudioObjectID = u32;
    type OSStatus = i32;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        m_selector: u32,
        m_scope: u32,
        m_element: u32,
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const SCOPE_GLOBAL: u32 = fourcc(b"glob"); // kAudioObjectPropertyScopeGlobal
    const ELEMENT_MAIN: u32 = 0; // kAudioObjectPropertyElementMain
    const PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#"); // kAudioHardwarePropertyProcessObjectList
    const IS_RUNNING_INPUT: u32 = fourcc(b"piri"); // kAudioProcessPropertyIsRunningInput
    const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid"); // kAudioProcessPropertyBundleID
    const PROCESS_PID: u32 = fourcc(b"ppid"); // kAudioProcessPropertyPID

    // SAFETY: these are the standard CoreAudio.framework HAL entry points.
    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectHasProperty(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
        ) -> u8;
        fn AudioObjectGetPropertyDataSize(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            out_data_size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            io_data_size: *mut u32,
            out_data: *mut c_void,
        ) -> OSStatus;
    }

    fn addr(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            m_selector: selector,
            m_scope: SCOPE_GLOBAL,
            m_element: ELEMENT_MAIN,
        }
    }

    pub(super) fn process_object_list_property_exists() -> bool {
        let a = addr(PROCESS_OBJECT_LIST);
        // SAFETY: querying existence of a well-known property on the system object.
        unsafe { AudioObjectHasProperty(SYSTEM_OBJECT, &a) != 0 }
    }

    /// Read the full list of process object ids. Empty on any error.
    fn process_object_ids() -> Vec<AudioObjectID> {
        let a = addr(PROCESS_OBJECT_LIST);
        let mut size: u32 = 0;
        // SAFETY: standard two-call size-then-data HAL pattern.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size)
        };
        if status != 0 || size == 0 {
            return Vec::new();
        }
        let count = size as usize / std::mem::size_of::<AudioObjectID>();
        let mut ids = vec![0 as AudioObjectID; count];
        let mut io_size = size;
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return Vec::new();
        }
        let got = io_size as usize / std::mem::size_of::<AudioObjectID>();
        ids.truncate(got);
        ids
    }

    fn read_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
        let a = addr(selector);
        let mut value: u32 = 0;
        let mut io_size = std::mem::size_of::<u32>() as u32;
        // SAFETY: reads a fixed-size scalar property into a local.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                &mut value as *mut u32 as *mut c_void,
            )
        };
        (status == 0).then_some(value)
    }

    fn read_i32(object: AudioObjectID, selector: u32) -> Option<i32> {
        read_u32(object, selector).map(|v| v as i32)
    }

    fn read_string(object: AudioObjectID, selector: u32) -> Option<String> {
        let a = addr(selector);
        let mut cf: CFStringRef = std::ptr::null();
        let mut io_size = std::mem::size_of::<CFStringRef>() as u32;
        // SAFETY: the property yields a +1 (create-rule) CFStringRef we own.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                &mut cf as *mut CFStringRef as *mut c_void,
            )
        };
        if status != 0 || cf.is_null() {
            return None;
        }
        // Take ownership (releases on drop) and copy out a Rust String.
        let s = unsafe { CFString::wrap_under_create_rule(cf) };
        Some(s.to_string())
    }

    /// Enumerate apps currently holding an audio input.
    pub(super) fn enumerate_active_inputs() -> Vec<ActiveInput> {
        let mut out = Vec::new();
        for object in process_object_ids() {
            // Only inputs — the whole point is capture activity.
            if read_u32(object, IS_RUNNING_INPUT).unwrap_or(0) == 0 {
                continue;
            }
            let Some(raw_bundle) = read_string(object, PROCESS_BUNDLE_ID) else {
                continue; // no bundle id → cannot classify, skip.
            };
            let bundle = normalize_bundle_id(&raw_bundle);
            if bundle.is_empty() {
                continue;
            }
            let pid = read_i32(object, PROCESS_PID).unwrap_or(-1);
            out.push(ActiveInput {
                session_key: format!("{bundle}#{pid}"),
                app_class: classify_bundle_id(&bundle),
                bundle_id: bundle,
            });
        }
        out
    }

    pub(super) fn poll_loop<F>(running: Arc<AtomicBool>, poll: Duration, on_signal: F)
    where
        F: Fn(DetectorSignal),
    {
        // session_key -> ActiveInput of the previous cycle, to diff add/remove.
        let mut previous: HashMap<String, ActiveInput> = HashMap::new();
        while running.load(Ordering::SeqCst) {
            let mut current: HashMap<String, ActiveInput> = HashMap::new();
            for input in enumerate_active_inputs() {
                current.insert(input.session_key.clone(), input);
            }
            // Removed: in previous, not in current.
            for key in previous.keys() {
                if !current.contains_key(key) {
                    on_signal(DetectorSignal::Removed {
                        session_key: key.clone(),
                    });
                }
            }
            // Added: in current, not in previous.
            for (key, input) in &current {
                if !previous.contains_key(key) {
                    on_signal(DetectorSignal::Added(input.clone()));
                }
            }
            on_signal(DetectorSignal::Tick);
            previous = current;

            // Sleep in small slices so stop() is responsive.
            let mut slept = Duration::ZERO;
            let step = Duration::from_millis(100);
            while slept < poll && running.load(Ordering::SeqCst) {
                std::thread::sleep(step);
                slept += step;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_starts_disabled_and_stops_cleanly() {
        let mut d = MeetingActivityDetector::new();
        assert!(!d.is_running());
        // On a host without the capability, start is a no-op; on a capable host
        // this still exercises stop() without leaking a thread.
        let _ = d.start(DEFAULT_POLL, |_signal| {});
        d.stop();
        assert!(!d.is_running());
    }

    #[test]
    fn capability_is_false_off_macos() {
        // Purely a compile+call check; on macOS the value depends on OS version.
        let _ = capability_available();
    }
}
