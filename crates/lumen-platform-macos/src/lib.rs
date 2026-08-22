//! macOS platform adapters: permissions, text injection, frontmost app, hotkeys.
//!
//! The generic capability modules (system audio process tap, hotkey CGEvent
//! tap, AUVoiceIO, power monitor/assertion) now live in the shared
//! lumen-suite `lumen-platform-macos` crate and are re-exported here so
//! existing `lumen_platform_macos::` call sites keep working.

mod ax_drag;
mod calendar;
mod edit_surface;
mod focused_field;
mod inject;
mod meeting_activity;
mod permissions;

pub use ax_drag::{
    dismiss_accessibility_drag_overlay, drag_payload_path, present_accessibility_drag_overlay,
};
pub use calendar::{
    current_or_upcoming_event as calendar_current_or_upcoming_event,
    request_access as calendar_request_access, select_event_in_window, CalendarCandidate,
    CalendarEventInfo, CALENDAR_LOOKBACK_MINUTES,
};
pub use edit_surface::MacAccessibilitySurfaceAdapter;
pub use focused_field::{focused_text_field_snapshot, FocusedTextFieldSnapshot};
pub use inject::MacTextInjectorBackend;
pub use meeting_activity::{
    capability_available as meeting_detection_capability_available, ActiveInput, DetectorSignal,
    MeetingActivityDetector, DEFAULT_POLL as MEETING_DETECTION_DEFAULT_POLL,
};
pub use permissions::{
    ensure_accessibility_onboarding, is_accessibility_trusted, prompt_accessibility, MacPermissions,
};

pub use lumen_platform_suite_macos::{
    battery_status, install_will_sleep_observer, physical_fn_down, start_monitor,
    start_multi_monitor, stop_monitor, system_audio_capability_available,
    voice_processing_supported, BatteryStatus, HotkeyBinding, HotkeyEdge, HotkeyMode, HotkeySpec,
    MeetingPowerGuard, SystemAudioCapture, SystemAudioError, SystemAudioSink, SystemAudioTarget,
    VoiceInputSink, VoiceProcessingError, VoiceProcessingInput,
};

use async_trait::async_trait;
use lumen_core::FocusInfo;
use lumen_platform::{FrontmostApp, PlatformError};

pub struct MacFrontmost;

#[cfg(test)]
mod edit_surface_contract_tests {
    use super::MacAccessibilitySurfaceAdapter;
    use lumen_edit_learning::{SurfaceAdapter, SurfaceErrorKind, TargetHint};

    #[test]
    fn accessibility_surface_reservation_requires_a_captured_process_id() {
        let error = match MacAccessibilitySurfaceAdapter.reserve(&TargetHint {
            app_name: Some("Editor".into()),
            bundle_id: Some("test.editor".into()),
            process_id: None,
        }) {
            Ok(_) => panic!("a target without pid cannot own a native AX element"),
            Err(error) => error,
        };

        assert_eq!(error.kind, SurfaceErrorKind::TemporarilyUnavailable);
        assert_eq!(error.code, "target_process_id_unavailable");
    }
}

#[async_trait]
impl FrontmostApp for MacFrontmost {
    async fn focus_info(&self) -> Result<FocusInfo, PlatformError> {
        Ok(frontmost_focus_info().unwrap_or_default())
    }
}

/// Open System Settings privacy panes.
pub fn open_url(url: &str) -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| PlatformError::Message(e.to_string()))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err(PlatformError::Message("not macOS".into()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrontmostTarget {
    pub name: Option<String>,
    pub bundle_id: Option<String>,
    /// Native process identifier captured with the frontmost application.
    ///
    /// Terminal pane adapters use this only to prove that a multiplexer client
    /// belongs to the selected outer terminal. It is never persisted.
    pub process_id: Option<u32>,
}

/// Best-effort frontmost process name + bundle id.
pub fn frontmost_focus_info() -> Option<FocusInfo> {
    let t = frontmost_target()?;
    Some(FocusInfo {
        app_name: t.name,
        bundle_id: t.bundle_id,
        window_title: None,
    })
}

pub fn frontmost_app_name() -> Option<String> {
    frontmost_target().and_then(|t| t.name)
}

/// Prefer NSWorkspace (fast, process-local); fall back to System Events.
pub fn frontmost_target() -> Option<FrontmostTarget> {
    frontmost_target_native().or_else(frontmost_target_osascript)
}

#[cfg(target_os = "macos")]
fn frontmost_target_native() -> Option<FrontmostTarget> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    // NSWorkspace is main-thread preferred but frontmostApplication is used
    // widely off-main for focus snapshots; treat as best-effort.
    let ws = NSWorkspace::sharedWorkspace();
    let app = ws.frontmostApplication()?;
    let name = app
        .localizedName()
        .map(|s: objc2::rc::Retained<NSString>| s.to_string())
        .filter(|s| !s.is_empty());
    let bundle_id = app
        .bundleIdentifier()
        .map(|s: objc2::rc::Retained<NSString>| s.to_string())
        .filter(|s| !s.is_empty());
    let process_id = u32::try_from(app.processIdentifier())
        .ok()
        .filter(|process_id| *process_id > 0);
    if name.is_none() && bundle_id.is_none() {
        return None;
    }
    Some(FrontmostTarget {
        name,
        bundle_id,
        process_id,
    })
}

#[cfg(not(target_os = "macos"))]
fn frontmost_target_native() -> Option<FrontmostTarget> {
    None
}

fn frontmost_target_osascript() -> Option<FrontmostTarget> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"
tell application "System Events"
  set p to first application process whose frontmost is true
  set n to name of p
  set b to ""
  try
    set b to bundle identifier of p
  end try
  set processId to ""
  try
    set processId to unix id of p as text
  end try
  return n & linefeed & b & linefeed & processId
end tell
"#;
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let mut lines = s.lines();
        let name = lines.next().map(str::trim).filter(|x| !x.is_empty())?;
        let bundle = lines
            .next()
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string());
        let process_id = lines
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse().ok())
            .filter(|process_id| *process_id > 0);
        Some(FrontmostTarget {
            name: Some(name.to_string()),
            bundle_id: bundle,
            process_id,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Activate target app so subsequent key events go to its focused field.
/// Prefer activating an already-running app by bundle id (no new launch).
pub fn activate_target(target: &FrontmostTarget) -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(bid) = target.bundle_id.as_deref() {
            if !bid.is_empty() {
                if activate_by_bundle_id(bid) {
                    return true;
                }
                if std::process::Command::new("open")
                    .args(["-b", bid])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    return true;
                }
            }
        }
        if let Some(name) = target.name.as_deref() {
            if is_self_app_name(name) {
                return false;
            }
            if std::process::Command::new("open")
                .args(["-a", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return true;
            }
            let script = format!(
                r#"tell application "{}" to activate"#,
                name.replace('"', "\\\"")
            );
            return std::process::Command::new("osascript")
                .args(["-e", &script])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
        false
    }
}

#[cfg(target_os = "macos")]
fn activate_by_bundle_id(bundle_id: &str) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    use objc2_foundation::NSString;

    let bid = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bid);
    let Some(app) = apps.firstObject() else {
        return false;
    };
    // Bring existing process forward without relaunching (preserves caret when possible).
    // Empty options: ActivateIgnoringOtherApps is a no-op on modern macOS.
    app.activateWithOptions(NSApplicationActivationOptions::empty())
}

pub fn activate_app_by_name(name: &str) -> bool {
    activate_target(&FrontmostTarget {
        name: Some(name.to_string()),
        bundle_id: None,
        process_id: None,
    })
}

pub fn is_self_app_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("lumen") || n.contains("lumen-asr") || n.contains("lumen asr")
}

pub fn is_self_target(t: &FrontmostTarget) -> bool {
    t.name.as_deref().map(is_self_app_name).unwrap_or(false)
        || t.bundle_id
            .as_deref()
            .map(|b| b.to_ascii_lowercase().contains("lumenasr"))
            .unwrap_or(false)
}

/// Run a closure on the main run-loop thread. AppKit window mutations must
/// happen on the main thread; this lets callers on background tasks (e.g. the
/// dictation pipeline showing the capsule) safely touch an NSWindow. Uses
/// libdispatch directly — the closure is boxed and consumed by a C trampoline,
/// so no extra crates are needed.
#[cfg(target_os = "macos")]
pub(crate) fn run_on_main(f: impl FnOnce() + Send + 'static) {
    use std::ffi::c_void;
    extern "C" fn trampoline(ctx: *mut c_void) {
        // SAFETY: `ctx` was created just below via `Box::into_raw` and is
        // consumed exactly once by this single `dispatch_async_f` submission.
        let outer: Box<Box<dyn FnOnce()>> = unsafe { Box::from_raw(ctx as *mut Box<dyn FnOnce()>) };
        let inner: Box<dyn FnOnce()> = *outer;
        inner();
    }
    extern "C" {
        // `dispatch_get_main_queue()` is an inline header function with no
        // exported symbol, so reference its backing global directly. The main
        // queue handle is the ADDRESS of `_dispatch_main_q` (that's what the
        // DISPATCH_GLOBAL_OBJECT macro returns); the type is opaque — we only
        // ever take its address, never dereference.
        static _dispatch_main_q: u8;
        fn dispatch_async_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    }
    let boxed: Box<dyn FnOnce()> = Box::new(f);
    let b: Box<Box<dyn FnOnce()>> = Box::new(boxed);
    let ctx = Box::into_raw(b) as *mut c_void;
    // SAFETY: reading the extern static's address; the symbol is provided by
    // libdispatch. The type is opaque — only its address is used.
    let main_queue = unsafe { &_dispatch_main_q as *const u8 as *mut c_void };
    // SAFETY: hands `ctx` to libdispatch; the trampoline frees it on the main
    // queue exactly once.
    unsafe {
        dispatch_async_f(main_queue, ctx, trampoline);
    }
}

/// Make the window that owns `ns_view` (a webview's content-view pointer) appear
/// on **every** Space, like a system overlay. Sets `NSWindowCollectionBehavior`
/// `canJoinAllSpaces | fullScreenAuxiliary` so the dictation capsule follows the
/// user across Spaces instead of vanishing the moment they switch away from the
/// Space it was created on. Always dispatched to the main thread.
///
/// The NSView is retained (+1) before the async hop and released inside the
/// closure: the raw-window-handle contract only guarantees the pointer while the
/// handle provider is alive, which we can't assume across the thread boundary.
/// `retain`/`release` are thread-safe refcount ops.
#[cfg(target_os = "macos")]
pub fn set_window_visible_on_all_spaces(ns_view: *mut std::ffi::c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    if ns_view.is_null() {
        return;
    }
    // Retain (+1) so the pointer stays valid across the dispatch regardless of
    // the handle provider's lifetime. Released at the end of the closure. Raw
    // pointers are `!Send`, so ferry the retained address through a `usize`.
    let retained: *mut AnyObject = unsafe { msg_send![ns_view as *mut AnyObject, retain] };
    if retained.is_null() {
        return;
    }
    let addr = retained as usize;
    run_on_main(move || {
        // NSWindowCollectionBehavior bits (AppKit/NSWindow.h):
        const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
        const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
        unsafe {
            let view = addr as *mut AnyObject;
            // [view window]
            let window: *mut AnyObject = msg_send![view, window];
            if !window.is_null() {
                // [window collectionBehavior] → OR in the bits → setCollectionBehavior:
                let behavior: usize = msg_send![window, collectionBehavior];
                let new_behavior = behavior | CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY;
                let _: () = msg_send![window, setCollectionBehavior: new_behavior];
            }
            // Balance the retain taken before dispatch.
            let _: () = msg_send![view, release];
        }
    });
}

#[cfg(not(target_os = "macos"))]
pub fn set_window_visible_on_all_spaces(_ns_view: *mut std::ffi::c_void) {}
