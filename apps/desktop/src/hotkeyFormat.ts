/** Pretty-print Tauri shortcut strings for UI (⌘⌥⌃⇧). */
export function formatHotkeyLabel(raw: string): string {
  if (!raw) return "—";
  return raw
    .replace(/CommandOrControl/gi, "⌘")
    .replace(/CmdOrCtrl/gi, "⌘")
    .replace(/Command/gi, "⌘")
    .replace(/Control/gi, "⌃")
    .replace(/Ctrl/gi, "⌃")
    .replace(/Option|Alt/gi, "⌥")
    .replace(/Shift/gi, "⇧")
    .replace(/Super|Meta/gi, "⌘")
    .replace(/Space/gi, "Space")
    .replace(/\+/g, "");
}

/**
 * Convert a browser KeyboardEvent into a Tauri global-shortcut string.
 * Returns null for pure modifiers / incomplete presses.
 */
export function eventToShortcut(e: KeyboardEvent): string | null {
  if (e.repeat) return null;

  const pureMods = new Set([
    "Control",
    "Shift",
    "Alt",
    "Meta",
    "OS",
    "AltGraph",
  ]);
  if (pureMods.has(e.key)) return null;

  const mods: string[] = [];
  // Order matches common Tauri / keyboard-types parsing expectations.
  if (e.metaKey) mods.push("Command");
  if (e.ctrlKey) mods.push("Control");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");

  const key = codeToShortcutKey(e.code, e.key);
  if (!key) return null;

  // Require a modifier unless it's an F-key (F5, F13, …).
  const isFunctionKey = /^F\d{1,2}$/i.test(key);
  if (mods.length === 0 && !isFunctionKey) {
    return null;
  }

  return [...mods, key].join("+");
}

function codeToShortcutKey(code: string, key: string): string | null {
  if (code === "Space") return "Space";
  if (code.startsWith("Key") && code.length === 4) {
    return code.slice(3).toUpperCase();
  }
  if (code.startsWith("Digit") && code.length === 6) {
    return code.slice(5);
  }
  if (/^F\d{1,2}$/.test(code)) return code;
  if (code === "ArrowUp") return "Up";
  if (code === "ArrowDown") return "Down";
  if (code === "ArrowLeft") return "Left";
  if (code === "ArrowRight") return "Right";
  if (code === "Escape") return "Escape";
  if (code === "Tab") return "Tab";
  if (code === "Enter" || code === "NumpadEnter") return "Enter";
  if (code === "Backspace") return "Backspace";
  if (code === "Delete") return "Delete";
  if (code === "Home") return "Home";
  if (code === "End") return "End";
  if (code === "PageUp") return "PageUp";
  if (code === "PageDown") return "PageDown";
  if (code.startsWith("Numpad") && code.length > 6) {
    const rest = code.slice(6);
    if (/^\d$/.test(rest)) return rest;
  }
  // Single printable character
  if (key.length === 1) {
    return /[a-z]/i.test(key) ? key.toUpperCase() : key;
  }
  return null;
}

/** Common presets shown next to the recorder (safe defaults). */
export const HOTKEY_PRESETS: { label: string; value: string }[] = [
  { label: "⌥Space", value: "Alt+Space" },
  { label: "⌃⇧Space", value: "Control+Shift+Space" },
  { label: "⌘⇧D", value: "Command+Shift+D" },
  { label: "⌃⌥Space", value: "Control+Alt+Space" },
];
