//! Text insert IPC (M4).

use crate::config::InjectConfig;
use crate::AppState;
use lumen_core::InsertStrategy;
#[cfg(not(target_os = "macos"))]
use lumen_inject::InsertOutcome;
#[cfg(target_os = "macos")]
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
        let _ = policy;
        copy_only(&text).await?;
        InsertOutcome {
            strategy: InsertStrategy::CopyOnly,
            restored_clipboard: false,
        }
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
        let _ = cfg;
        copy_only(text).await?;
        return Ok(InsertOutcome {
            strategy: InsertStrategy::CopyOnly,
            restored_clipboard: false,
        });
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
    use std::time::Duration;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, memory: *mut c_void) -> *mut c_void;
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

    pub fn set_unicode_text(text: &str) -> Result<(), String> {
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);

        let _guard = (0..10)
            .find_map(|_| {
                if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                    Some(ClipboardGuard)
                } else {
                    thread::sleep(Duration::from_millis(20));
                    None
                }
            })
            .ok_or_else(|| "Windows clipboard is busy".to_string())?;

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
}
