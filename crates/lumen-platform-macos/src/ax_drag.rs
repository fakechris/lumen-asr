//! Accessibility grant helper: a floating panel whose app icon can be dragged
//! into System Settings → Privacy → Accessibility.
//!
//! macOS will not grant Accessibility from an in-app toggle. The Settings list
//! accepts a dropped `.app` (or the running binary). Showing that file as a
//! draggable icon removes the Finder “+” hunt.

use std::path::{Path, PathBuf};

/// File the user should drop onto the Accessibility list: the `.app` bundle
/// when running packaged, otherwise the current executable.
pub fn drag_payload_path(exe: &Path) -> PathBuf {
    app_bundle_root(exe).unwrap_or_else(|| exe.to_path_buf())
}

fn app_bundle_root(path: &Path) -> Option<PathBuf> {
    if path.extension().and_then(|e| e.to_str()) == Some("app") {
        return Some(path.to_path_buf());
    }
    let macos = path.parent()?;
    if macos.file_name()?.to_string_lossy() != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_string_lossy() != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension().and_then(|e| e.to_str()) == Some("app") {
        Some(app.to_path_buf())
    } else {
        None
    }
}

/// On-screen window used to park the overlay under System Settings.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayWindow {
    pub owner: String,
    pub layer: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

const SETTINGS_OWNERS: &[&str] = &[
    "System Settings",
    "System Preferences",
    "系统设置",
    "系统偏好设置",
];

/// Largest ordinary Settings window, if any.
pub fn pick_settings_window(windows: &[OverlayWindow]) -> Option<&OverlayWindow> {
    windows
        .iter()
        .filter(|w| w.layer == 0 && w.width >= 400.0 && w.height >= 280.0)
        .filter(|w| SETTINGS_OWNERS.iter().any(|n| w.owner == *n))
        .max_by(|a, b| {
            a.width
                .partial_cmp(&b.width)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.height
                        .partial_cmp(&b.height)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        })
}

/// Panel frame in CG/AppKit screen coords (origin bottom-left), docked near
/// the bottom of the Settings window.
pub fn overlay_frame_in(settings: (f64, f64, f64, f64), panel: (f64, f64)) -> (f64, f64, f64, f64) {
    let (sx, sy, sw, _sh) = settings;
    let (pw, ph) = panel;
    let width = pw.min(sw - 24.0).max(320.0);
    let x = sx + (sw - width) / 2.0;
    let y = sy + 18.0;
    (x, y, width, ph)
}

pub fn present_accessibility_drag_overlay() {
    #[cfg(target_os = "macos")]
    native::present();
}

pub fn dismiss_accessibility_drag_overlay() {
    #[cfg(target_os = "macos")]
    native::dismiss();
}

#[cfg(target_os = "macos")]
mod native {
    use super::{drag_payload_path, overlay_frame_in, pick_settings_window, OverlayWindow};
    use crate::run_on_main;
    use core_foundation::base::{TCFType, ToVoid};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
        kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowOwnerName,
    };
    use lumen_platform_suite_macos::is_accessibility_trusted;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Sel};
    use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
    use objc2_app_kit::{
        NSBackingStoreType, NSButton, NSColor, NSEvent, NSFloatingWindowLevel, NSFont, NSImage,
        NSImageView, NSPanel, NSScreen, NSTextField, NSView, NSWindowCollectionBehavior,
        NSWindowStyleMask, NSWorkspace,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const PANEL_W: f64 = 460.0;
    const PANEL_H: f64 = 128.0;

    static CANCEL_POLL: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static PANEL: RefCell<Option<Retained<NSPanel>>> = const { RefCell::new(None) };
        static CLOSER: RefCell<Option<Retained<OverlayCloser>>> = const { RefCell::new(None) };
    }

    #[derive(Default)]
    struct DragIvars {
        path: RefCell<Option<Retained<NSString>>>,
    }

    #[derive(Default)]
    struct CloserIvars;

    define_class!(
        #[unsafe(super(NSView))]
        #[thread_kind = MainThreadOnly]
        #[name = "LumenAxDragSourceView"]
        #[ivars = DragIvars]
        struct AxDragView;

        impl AxDragView {
            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, event: &NSEvent) {
                let Some(path) = self.ivars().path.borrow().clone() else {
                    return;
                };
                let bounds = self.bounds();
                #[allow(deprecated)]
                let _ = self.dragFile_fromRect_slideBack_event(&path, bounds, true, event);
            }
        }
    );

    define_class!(
        #[unsafe(super(objc2::runtime::NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "LumenAxDragCloser"]
        #[ivars = CloserIvars]
        struct OverlayCloser;

        impl OverlayCloser {
            #[unsafe(method(close:))]
            fn close(&self, _sender: Option<&AnyObject>) {
                dismiss_on_main();
            }
        }
    );

    pub fn present() {
        if is_accessibility_trusted() {
            dismiss();
            return;
        }
        let path = std::env::current_exe()
            .ok()
            .map(|p| drag_payload_path(&p))
            .filter(|p| p.exists());
        let Some(path) = path else {
            tracing::warn!("accessibility drag overlay: no payload path");
            return;
        };
        CANCEL_POLL.store(false, Ordering::SeqCst);
        let path_str = path.to_string_lossy().into_owned();
        run_on_main(move || show_panel(&path_str));
        std::thread::spawn(|| {
            for _ in 0..120 {
                std::thread::sleep(Duration::from_millis(500));
                if CANCEL_POLL.load(Ordering::SeqCst) {
                    return;
                }
                if is_accessibility_trusted() {
                    run_on_main(dismiss_on_main);
                    return;
                }
            }
        });
    }

    pub fn dismiss() {
        CANCEL_POLL.store(true, Ordering::SeqCst);
        run_on_main(dismiss_on_main);
    }

    fn dismiss_on_main() {
        CANCEL_POLL.store(true, Ordering::SeqCst);
        PANEL.with(|p| {
            if let Some(panel) = p.borrow_mut().take() {
                panel.orderOut(None);
                panel.close();
            }
        });
        CLOSER.with(|c| {
            c.borrow_mut().take();
        });
    }

    fn show_panel(path: &str) {
        dismiss_on_main();
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("accessibility drag overlay requires the main thread");
            return;
        };
        let ns_path = NSString::from_str(path);
        let ws = NSWorkspace::sharedWorkspace();
        let icon = ws.iconForFile(&ns_path);
        icon.setSize(NSSize {
            width: 36.0,
            height: 36.0,
        });
        let name = display_name(path);
        let frame = panel_screen_frame(mtm);
        let style = NSWindowStyleMask::Borderless
            | NSWindowStyleMask::NonactivatingPanel
            | NSWindowStyleMask::UtilityWindow;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSPanel>(),
            NSRect::new(
                NSPoint {
                    x: frame.0,
                    y: frame.1,
                },
                NSSize {
                    width: frame.2,
                    height: frame.3,
                },
            ),
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setWorksWhenModal(true);
        panel.setLevel(NSFloatingWindowLevel);
        panel.setHidesOnDeactivate(false);
        unsafe {
            panel.setReleasedWhenClosed(false);
        }
        panel.setHasShadow(true);
        panel.setOpaque(true);
        panel.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
        panel.setMovableByWindowBackground(true);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Transient
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setTitle(&NSString::from_str("Lumen ASR"));

        let content = panel.contentView().unwrap_or_else(|| {
            let view = NSView::initWithFrame(mtm.alloc::<NSView>(), panel.frame());
            panel.setContentView(Some(&view));
            view
        });
        build_contents(mtm, &content, &ns_path, &icon, &name, content.bounds().size);
        panel.orderFrontRegardless();
        PANEL.with(|p| *p.borrow_mut() = Some(panel));
    }

    fn build_contents(
        mtm: MainThreadMarker,
        content: &NSView,
        path: &NSString,
        icon: &NSImage,
        name: &str,
        size: NSSize,
    ) {
        let pad = 16.0;
        let row_h = 56.0;
        let row_y = 16.0;
        let row_w = size.width - pad * 2.0;

        let hint = NSTextField::labelWithString(
            &NSString::from_str("把 Lumen ASR 拖到上方列表，即可开启辅助功能"),
            mtm,
        );
        hint.setFrame(NSRect::new(
            NSPoint {
                x: pad,
                y: row_y + row_h + 10.0,
            },
            NSSize {
                width: row_w - 56.0,
                height: 36.0,
            },
        ));
        hint.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        hint.setTextColor(Some(&NSColor::labelColor()));
        content.addSubview(&hint);

        let closer_alloc = OverlayCloser::alloc(mtm).set_ivars(CloserIvars);
        let closer: Retained<OverlayCloser> = unsafe { msg_send![super(closer_alloc), init] };
        let close = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("关闭"),
                Some(&*closer as &AnyObject),
                Some(Sel::register(c"close:")),
                mtm,
            )
        };
        close.setFrame(NSRect::new(
            NSPoint {
                x: size.width - pad - 52.0,
                y: row_y + row_h + 14.0,
            },
            NSSize {
                width: 52.0,
                height: 28.0,
            },
        ));
        content.addSubview(&close);
        CLOSER.with(|c| *c.borrow_mut() = Some(closer));

        let row_alloc = AxDragView::alloc(mtm).set_ivars(DragIvars {
            path: RefCell::new(Some(path.retain())),
        });
        let row_frame = NSRect::new(
            NSPoint { x: pad, y: row_y },
            NSSize {
                width: row_w,
                height: row_h,
            },
        );
        let row: Retained<AxDragView> =
            unsafe { msg_send![super(row_alloc), initWithFrame: row_frame] };

        let image_view = NSImageView::initWithFrame(
            mtm.alloc::<NSImageView>(),
            NSRect::new(
                NSPoint { x: 12.0, y: 10.0 },
                NSSize {
                    width: 36.0,
                    height: 36.0,
                },
            ),
        );
        image_view.setImage(Some(icon));
        row.addSubview(&image_view);

        let label = NSTextField::labelWithString(&NSString::from_str(name), mtm);
        label.setFrame(NSRect::new(
            NSPoint { x: 58.0, y: 16.0 },
            NSSize {
                width: row_w - 110.0,
                height: 24.0,
            },
        ));
        label.setFont(Some(&NSFont::systemFontOfSize(15.0)));
        label.setTextColor(Some(&NSColor::labelColor()));
        row.addSubview(&label);

        let grip = NSTextField::labelWithString(&NSString::from_str("⋮⋮"), mtm);
        grip.setFrame(NSRect::new(
            NSPoint {
                x: row_w - 40.0,
                y: 16.0,
            },
            NSSize {
                width: 28.0,
                height: 24.0,
            },
        ));
        grip.setFont(Some(&NSFont::systemFontOfSize(16.0)));
        grip.setTextColor(Some(&NSColor::secondaryLabelColor()));
        row.addSubview(&grip);

        content.addSubview(&row);
    }

    fn display_name(path: &str) -> String {
        let p = std::path::Path::new(path);
        if p.extension().and_then(|e| e.to_str()) == Some("app") {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Lumen ASR".into())
        } else {
            p.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Lumen ASR".into())
        }
    }

    fn panel_screen_frame(mtm: MainThreadMarker) -> (f64, f64, f64, f64) {
        if let Some(win) = live_settings_window() {
            return overlay_frame_in((win.x, win.y, win.width, win.height), (PANEL_W, PANEL_H));
        }
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let vis = screen.visibleFrame();
            let width = PANEL_W.min(vis.size.width - 40.0);
            let x = vis.origin.x + (vis.size.width - width) / 2.0;
            let y = vis.origin.y + 48.0;
            return (x, y, width, PANEL_H);
        }
        (80.0, 80.0, PANEL_W, PANEL_H)
    }

    fn live_settings_window() -> Option<OverlayWindow> {
        let array = copy_window_info(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )?;
        let mut windows = Vec::new();
        for ptr in array.get_all_values() {
            if ptr.is_null() {
                continue;
            }
            let dict: CFDictionary = unsafe { TCFType::wrap_under_get_rule(ptr as *const _) };
            let Some(owner) = dict_string(&dict, unsafe { kCGWindowOwnerName }) else {
                continue;
            };
            let layer = dict_i64(&dict, unsafe { kCGWindowLayer }).unwrap_or(0);
            let Some((x, y, w, h)) = dict_rect(&dict, unsafe { kCGWindowBounds }) else {
                continue;
            };
            windows.push(OverlayWindow {
                owner,
                layer,
                x,
                y,
                width: w,
                height: h,
            });
        }
        pick_settings_window(&windows).cloned()
    }

    fn dict_string(
        dict: &CFDictionary,
        key: core_foundation::string::CFStringRef,
    ) -> Option<String> {
        let key = unsafe { CFString::wrap_under_get_rule(key) };
        let value = dict.find(key.to_void())?;
        if (*value).is_null() {
            return None;
        }
        let s: CFString = unsafe { TCFType::wrap_under_get_rule(*value as *const _) };
        Some(s.to_string())
    }

    fn dict_i64(dict: &CFDictionary, key: core_foundation::string::CFStringRef) -> Option<i64> {
        let key = unsafe { CFString::wrap_under_get_rule(key) };
        let value = dict.find(key.to_void())?;
        if (*value).is_null() {
            return None;
        }
        let n: CFNumber = unsafe { TCFType::wrap_under_get_rule(*value as *const _) };
        n.to_i64()
    }

    fn dict_rect(
        dict: &CFDictionary,
        key: core_foundation::string::CFStringRef,
    ) -> Option<(f64, f64, f64, f64)> {
        let key = unsafe { CFString::wrap_under_get_rule(key) };
        let value = dict.find(key.to_void())?;
        if (*value).is_null() {
            return None;
        }
        let bounds: CFDictionary = unsafe { TCFType::wrap_under_get_rule(*value as *const _) };
        let x = dict_named_f64(&bounds, "X")?;
        let y = dict_named_f64(&bounds, "Y")?;
        let w = dict_named_f64(&bounds, "Width")?;
        let h = dict_named_f64(&bounds, "Height")?;
        Some((x, y, w, h))
    }

    fn dict_named_f64(dict: &CFDictionary, key: &str) -> Option<f64> {
        let k = CFString::new(key);
        let value = dict.find(k.to_void())?;
        if (*value).is_null() {
            return None;
        }
        let n: CFNumber = unsafe { TCFType::wrap_under_get_rule(*value as *const _) };
        n.to_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn payload_prefers_app_bundle_over_binary() {
        let exe = PathBuf::from("/Applications/Lumen ASR.app/Contents/MacOS/lumen-asr-desktop");
        assert_eq!(
            drag_payload_path(&exe),
            PathBuf::from("/Applications/Lumen ASR.app")
        );
    }

    #[test]
    fn payload_keeps_bare_binary() {
        let exe = PathBuf::from("/tmp/target/debug/lumen-asr-desktop");
        assert_eq!(drag_payload_path(&exe), exe);
    }

    #[test]
    fn payload_keeps_app_path() {
        let app = PathBuf::from("/Applications/Lumen ASR.app");
        assert_eq!(drag_payload_path(&app), app);
    }

    #[test]
    fn picks_largest_system_settings_window() {
        let windows = vec![
            OverlayWindow {
                owner: "Finder".into(),
                layer: 0,
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            OverlayWindow {
                owner: "System Settings".into(),
                layer: 0,
                x: 100.0,
                y: 80.0,
                width: 760.0,
                height: 640.0,
            },
            OverlayWindow {
                owner: "System Settings".into(),
                layer: 0,
                x: 120.0,
                y: 90.0,
                width: 420.0,
                height: 300.0,
            },
            OverlayWindow {
                owner: "System Settings".into(),
                layer: 25,
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 40.0,
            },
        ];
        let picked = pick_settings_window(&windows).expect("settings");
        assert_eq!(picked.width, 760.0);
        assert_eq!(picked.x, 100.0);
    }

    #[test]
    fn overlay_docks_near_bottom_of_settings() {
        let (x, y, w, h) = overlay_frame_in((40.0, 60.0, 800.0, 640.0), (460.0, 128.0));
        assert!((w - 460.0).abs() < f64::EPSILON);
        assert!((h - 128.0).abs() < f64::EPSILON);
        assert!((x - (40.0 + (800.0 - 460.0) / 2.0)).abs() < 0.01);
        assert!((y - 78.0).abs() < 0.01);
    }
}
