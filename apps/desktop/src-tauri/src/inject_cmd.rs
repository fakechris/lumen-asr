//! Text insert IPC (M4).

use crate::config::InjectConfig;
use crate::AppState;
use lumen_core::InsertStrategy;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use lumen_inject::InsertOutcome;
#[cfg(target_os = "windows")]
use lumen_inject::{InjectError, TextInjectorBackend};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use lumen_inject::{InsertOutcome, TextInjector};
#[cfg(target_os = "macos")]
use lumen_platform_macos::MacTextInjectorBackend;
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectConfigDto {
    pub mode: String,
    pub preserve_clipboard: bool,
    pub auto_insert: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectConfigInput {
    pub mode: Option<String>,
    pub preserve_clipboard: Option<bool>,
    pub auto_insert: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertTextOutcome {
    pub strategy: String,
    pub restored_clipboard: bool,
}

fn strategy_str(s: InsertStrategy) -> &'static str {
    match s {
        InsertStrategy::Paste => "paste",
        InsertStrategy::Ax => "ax",
        InsertStrategy::Type => "type",
        InsertStrategy::CopyOnly => "copy_only",
        InsertStrategy::None => "none",
    }
}

#[tauri::command]
pub fn get_inject_config(state: State<'_, AppState>) -> Result<InjectConfigDto, String> {
    let cfg = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    Ok(InjectConfigDto {
        mode: cfg.inject.mode.clone(),
        preserve_clipboard: cfg.inject.preserve_clipboard,
        auto_insert: cfg.inject.auto_insert,
    })
}

#[tauri::command]
pub fn save_inject_config(
    state: State<'_, AppState>,
    input: InjectConfigInput,
) -> Result<InjectConfigDto, String> {
    let mut guard = state
        .config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?;
    if let Some(m) = input.mode {
        guard.inject.mode = m;
    }
    if let Some(v) = input.preserve_clipboard {
        guard.inject.preserve_clipboard = v;
    }
    if let Some(v) = input.auto_insert {
        guard.inject.auto_insert = v;
    }
    guard.save()?;
    Ok(InjectConfigDto {
        mode: guard.inject.mode.clone(),
        preserve_clipboard: guard.inject.preserve_clipboard,
        auto_insert: guard.inject.auto_insert,
    })
}

/// Insert text into the frontmost app using configured policy.
#[tauri::command]
pub async fn insert_text(
    state: State<'_, AppState>,
    text: String,
) -> Result<InsertTextOutcome, String> {
    let policy = {
        let cfg = state
            .config
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        cfg.inject.to_policy()
    };

    // Small delay so the user can refocus the target app after clicking our UI.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    #[cfg(target_os = "macos")]
    let out: InsertOutcome = {
        let injector = TextInjector::new(MacTextInjectorBackend, policy);
        injector.insert(&text).await.map_err(|e| e.to_string())?
    };
    #[cfg(target_os = "windows")]
    let out: InsertOutcome = {
        let injector = TextInjector::new(WindowsTextInjectorBackend, policy);
        injector.insert(&text).await.map_err(|e| e.to_string())?
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let out: InsertOutcome = {
        let _ = policy;
        return Err("text insertion is not implemented on this platform".into());
    };
    Ok(InsertTextOutcome {
        strategy: strategy_str(out.strategy).into(),
        restored_clipboard: out.restored_clipboard,
    })
}

pub async fn insert_with_config(cfg: &InjectConfig, text: &str) -> Result<InsertOutcome, String> {
    if text.is_empty() {
        return Ok(InsertOutcome {
            strategy: InsertStrategy::None,
            restored_clipboard: false,
        });
    }
    #[cfg(target_os = "windows")]
    {
        let injector = TextInjector::new(WindowsTextInjectorBackend, cfg.to_policy());
        return injector.insert(text).await.map_err(|e| e.to_string());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = cfg;
        return Err("text insertion is not implemented on this platform".into());
    }
    #[cfg(target_os = "macos")]
    if !lumen_platform_macos::is_accessibility_trusted() {
        return Err(
            "Accessibility permission required to insert into other apps (System Settings → Privacy & Security → Accessibility)"
                .into(),
        );
    }
    #[cfg(target_os = "macos")]
    let injector = TextInjector::new(MacTextInjectorBackend, cfg.to_policy());
    #[cfg(target_os = "macos")]
    injector.insert(text).await.map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
struct WindowsTextInjectorBackend;

#[cfg(target_os = "windows")]
#[async_trait::async_trait]
impl TextInjectorBackend for WindowsTextInjectorBackend {
    async fn paste_with_restore(&self, text: &str, preserve: bool) -> Result<(), InjectError> {
        if preserve {
            // Windows cannot losslessly snapshot every clipboard format. Keep
            // the clipboard untouched and use Unicode input instead.
            return windows_clipboard::type_unicode(text).map_err(InjectError::Other);
        }
        windows_clipboard::paste(text).map_err(|error| match error {
            windows_clipboard::PasteError::NoInput(message) => InjectError::Other(message),
            windows_clipboard::PasteError::PartialInput(message) => {
                InjectError::PartialInput(message)
            }
        })
    }

    async fn ax_insert(&self, _text: &str) -> Result<(), InjectError> {
        Err(InjectError::NotSupported(
            "Windows uses focused-window keyboard input instead of Accessibility insertion".into(),
        ))
    }

    async fn type_unicode(&self, text: &str) -> Result<(), InjectError> {
        windows_clipboard::type_unicode(text).map_err(InjectError::Other)
    }

    async fn copy_only(&self, text: &str) -> Result<(), InjectError> {
        windows_clipboard::set_unicode_text(text).map_err(InjectError::Other)
    }
}

pub async fn copy_only(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use lumen_inject::TextInjectorBackend;
        MacTextInjectorBackend
            .copy_only(text)
            .await
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        windows_clipboard::set_unicode_text(text)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = text;
        Err("clipboard copy is not implemented on this platform".into())
    }
}

#[cfg(target_os = "windows")]
mod windows_clipboard {
    use std::ffi::c_void;
    use std::ptr;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_V,
    };

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, memory: *mut c_void) -> *mut c_void;
        fn GetAsyncKeyState(v_key: i32) -> i16;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalFree(memory: *mut c_void) -> *mut c_void;
        fn GlobalLock(memory: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(memory: *mut c_void) -> i32;
    }

    struct ClipboardGuard;

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    fn open_clipboard() -> Result<ClipboardGuard, String> {
        (0..10)
            .find_map(|_| {
                if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                    Some(ClipboardGuard)
                } else {
                    thread::sleep(Duration::from_millis(20));
                    None
                }
            })
            .ok_or_else(|| "Windows clipboard is busy".to_string())
    }

    pub fn set_unicode_text(text: &str) -> Result<(), String> {
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);

        let _guard = open_clipboard()?;

        if unsafe { EmptyClipboard() } == 0 {
            return Err("could not clear Windows clipboard".into());
        }

        let bytes = utf16.len() * std::mem::size_of::<u16>();
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if memory.is_null() {
            return Err("could not allocate Windows clipboard memory".into());
        }
        let target = unsafe { GlobalLock(memory) } as *mut u16;
        if target.is_null() {
            unsafe {
                GlobalFree(memory);
            }
            return Err("could not lock Windows clipboard memory".into());
        }
        unsafe {
            ptr::copy_nonoverlapping(utf16.as_ptr(), target, utf16.len());
            GlobalUnlock(memory);
        }
        if unsafe { SetClipboardData(CF_UNICODETEXT, memory) }.is_null() {
            unsafe {
                GlobalFree(memory);
            }
            return Err("could not publish Unicode text to Windows clipboard".into());
        }
        // SetClipboardData transfers ownership of `memory` to the OS.
        Ok(())
    }

    pub(super) enum PasteError {
        NoInput(String),
        PartialInput(String),
    }

    pub fn paste(text: &str) -> Result<(), PasteError> {
        if text.is_empty() {
            return Ok(());
        }
        wait_hotkey_modifiers_clear(Duration::from_millis(800)).map_err(PasteError::NoInput)?;

        set_unicode_text(text).map_err(PasteError::NoInput)?;
        thread::sleep(Duration::from_millis(40));
        match send_ctrl_v() {
            InputDelivery::Complete => Ok(()),
            InputDelivery::None => Err(PasteError::NoInput(
                "Windows blocked simulated keyboard input; elevated apps cannot receive input from a non-elevated Lumen process"
                    .into(),
            )),
            InputDelivery::Partial(sent) => Err(PasteError::PartialInput(format!(
                "Windows accepted only {sent} of 4 simulated paste events"
            ))),
        }
    }

    pub fn type_unicode(text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        wait_hotkey_modifiers_clear(Duration::from_millis(800))?;
        for chunk in text.encode_utf16().collect::<Vec<_>>().chunks(128) {
            let mut inputs = Vec::with_capacity(chunk.len() * 2);
            for unit in chunk {
                inputs.push(keyboard_input(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE));
                inputs.push(keyboard_input(
                    VIRTUAL_KEY(0),
                    *unit,
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                ));
            }
            send_inputs(&inputs)?;
        }
        Ok(())
    }

    fn wait_hotkey_modifiers_clear(timeout: Duration) -> Result<(), String> {
        const VK_SHIFT: i32 = 0x10;
        const VK_CONTROL_CODE: i32 = 0x11;
        const VK_MENU: i32 = 0x12;
        const VK_LWIN: i32 = 0x5B;
        const VK_RWIN: i32 = 0x5C;
        const MODIFIERS: [i32; 5] = [VK_SHIFT, VK_CONTROL_CODE, VK_MENU, VK_LWIN, VK_RWIN];

        let started = Instant::now();
        loop {
            let any_down = MODIFIERS
                .iter()
                .any(|key| unsafe { GetAsyncKeyState(*key) } < 0);
            if !any_down {
                thread::sleep(Duration::from_millis(20));
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err("release the dictation shortcut before Windows text insertion".into());
            }
            thread::sleep(Duration::from_millis(12));
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InputDelivery {
        None,
        Partial(u32),
        Complete,
    }

    fn send_ctrl_v() -> InputDelivery {
        let inputs = [
            keyboard_input(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
            keyboard_input(VK_V, 0, KEYBD_EVENT_FLAGS(0)),
            keyboard_input(VK_V, 0, KEYEVENTF_KEYUP),
            keyboard_input(VK_CONTROL, 0, KEYEVENTF_KEYUP),
        ];
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == inputs.len() as u32 {
            return InputDelivery::Complete;
        }
        if sent > 0 {
            release_paste_keys();
            InputDelivery::Partial(sent)
        } else {
            InputDelivery::None
        }
    }

    fn keyboard_input(key: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == inputs.len() as u32 {
            Ok(())
        } else {
            release_paste_keys();
            Err(
                "Windows blocked simulated keyboard input; elevated apps cannot receive input from a non-elevated Lumen process"
                    .into(),
            )
        }
    }

    fn release_paste_keys() {
        // Best effort: if Windows accepted only part of a key sequence, ensure
        // the synthetic Ctrl/V keys do not remain logically down.
        let releases = [
            keyboard_input(VK_V, 0, KEYEVENTF_KEYUP),
            keyboard_input(VK_CONTROL, 0, KEYEVENTF_KEYUP),
        ];
        let _ = unsafe { SendInput(&releases, std::mem::size_of::<INPUT>() as i32) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ctrl_v_sequence_releases_every_key() {
            let inputs = [
                keyboard_input(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
                keyboard_input(VK_V, 0, KEYBD_EVENT_FLAGS(0)),
                keyboard_input(VK_V, 0, KEYEVENTF_KEYUP),
                keyboard_input(VK_CONTROL, 0, KEYEVENTF_KEYUP),
            ];

            assert_eq!(inputs.len(), 4);
            unsafe {
                assert_eq!(inputs[0].Anonymous.ki.wVk, VK_CONTROL);
                assert_eq!(inputs[1].Anonymous.ki.wVk, VK_V);
                assert_eq!(inputs[2].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
                assert_eq!(inputs[3].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
            }
        }
    }
}
