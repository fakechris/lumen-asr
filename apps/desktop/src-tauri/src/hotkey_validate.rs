//! Soft hotkey validation for onboarding Stage E.

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyValidation {
    pub ok: bool,
    pub shortcut: String,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[tauri::command]
pub fn validate_hotkey(shortcut: String) -> HotkeyValidation {
    let s = shortcut.trim().to_string();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    if s.is_empty() {
        errors.push("快捷键不能为空".into());
        return HotkeyValidation {
            ok: false,
            shortcut: s,
            warnings,
            errors,
        };
    }

    let upper = s.to_ascii_uppercase();
    let parts: Vec<&str> = upper
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        errors.push("无法解析快捷键".into());
    }

    // Count known modifier tokens vs non-modifier key.
    let mut mc = 0u8;
    let mut has_key = false;
    let mut has_fn = false;
    for p in &parts {
        match *p {
            "ALT" | "OPTION" | "SHIFT" | "CONTROL" | "CTRL" | "COMMAND" | "CMD" | "META"
            | "SUPER" | "COMMANDORCONTROL" | "COMMANDORCTRL" => mc += 1,
            "FN" | "FUNCTION" | "GLOBE" => {
                mc += 1;
                has_fn = true;
            }
            _ => has_key = true,
        }
    }

    if mc == 0 {
        errors.push("至少需要一个修饰键（如 Fn / Alt / Shift / Control）".into());
    }
    if !has_key && mc < 2 && !has_fn {
        errors.push("除 Fn 外，纯修饰键组合至少需要两个修饰键（如 Alt+Shift）".into());
    }

    // Soft conflicts
    if upper == "COMMAND+SPACE" || upper == "CMD+SPACE" || upper == "META+SPACE" {
        warnings.push("可能与 Spotlight（⌘Space）冲突".into());
    }
    if upper == "CONTROL+SPACE" || upper == "CTRL+SPACE" {
        warnings.push("可能与输入法切换冲突".into());
    }
    if upper.contains("COMMAND") && upper.contains("TAB") {
        warnings.push("可能与应用切换冲突".into());
    }
    if has_fn {
        warnings.push(
            "若 macOS 已将 Fn/🌐 设为听写或切换输入法，建议在系统设置中关闭该动作以免冲突".into(),
        );
    }

    // Try parse with HotkeySpec if available
    #[cfg(target_os = "macos")]
    {
        use lumen_platform_macos::{HotkeyMode, HotkeySpec};
        if let Err(e) = HotkeySpec::parse(&s, HotkeyMode::Hold) {
            // global-shortcut format might still work for some chords
            if !s.contains("CommandOrControl") {
                warnings.push(format!("EventTap 解析提示: {e}"));
            }
        }
    }

    HotkeyValidation {
        ok: errors.is_empty(),
        shortcut: s,
        warnings,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_fn_with_system_conflict_hint() {
        let result = validate_hotkey("Fn".into());
        assert!(result.ok);
        assert!(result.errors.is_empty());
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.contains("macOS")));
    }
}
