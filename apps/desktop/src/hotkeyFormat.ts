/** Pretty-print Tauri / internal shortcut strings for UI (⌘⌥⌃⇧). */
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

const MOD_CODES = new Set([
  "ControlLeft",
  "ControlRight",
  "ShiftLeft",
  "ShiftRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
  "OSLeft",
  "OSRight",
]);

const MOD_KEYS = new Set([
  "Control",
  "Shift",
  "Alt",
  "Meta",
  "OS",
  "AltGraph",
]);

export function isModifierEvent(e: KeyboardEvent): boolean {
  return MOD_KEYS.has(e.key) || MOD_CODES.has(e.code);
}

export type ChordState = {
  /** True if corresponding modifier is part of the chord. */
  command: boolean;
  control: boolean;
  alt: boolean;
  shift: boolean;
  /** Non-modifier key code token, e.g. "Space", "A", "F5". */
  key: string | null;
};

export function emptyChord(): ChordState {
  return {
    command: false,
    control: false,
    alt: false,
    shift: false,
    key: null,
  };
}

/** Merge a keydown into the accumulating chord (natural whole-combo capture). */
export function absorbKeyDown(chord: ChordState, e: KeyboardEvent): ChordState {
  const next = { ...chord };
  if (e.code === "MetaLeft" || e.code === "MetaRight" || e.key === "Meta") {
    next.command = true;
  } else if (
    e.code === "ControlLeft" ||
    e.code === "ControlRight" ||
    e.key === "Control"
  ) {
    next.control = true;
  } else if (e.code === "AltLeft" || e.code === "AltRight" || e.key === "Alt") {
    next.alt = true;
  } else if (
    e.code === "ShiftLeft" ||
    e.code === "ShiftRight" ||
    e.key === "Shift"
  ) {
    next.shift = true;
  } else {
    const k = codeToShortcutKey(e.code, e.key);
    if (k) next.key = k;
  }
  // Also trust event modifier flags (covers sticky / out-of-order OS events).
  if (e.metaKey) next.command = true;
  if (e.ctrlKey) next.control = true;
  if (e.altKey) next.alt = true;
  if (e.shiftKey) next.shift = true;
  return next;
}

export function chordModCount(c: ChordState): number {
  return (
    (c.command ? 1 : 0) +
    (c.control ? 1 : 0) +
    (c.alt ? 1 : 0) +
    (c.shift ? 1 : 0)
  );
}

/**
 * Whether this chord is a valid dictation hotkey:
 * - modifiers + key (e.g. ⌥Space)
 * - 2+ modifiers only (e.g. ⌥⇧) — registered via macOS mod watcher
 * - F-keys alone
 */
export function isValidChord(c: ChordState): boolean {
  if (c.key) {
    const isFunctionKey = /^F\d{1,2}$/i.test(c.key);
    return chordModCount(c) > 0 || isFunctionKey;
  }
  return chordModCount(c) >= 2;
}

/** Serialize to config / global-shortcut string. Order: Command Control Alt Shift Key */
export function chordToShortcut(c: ChordState): string | null {
  if (!isValidChord(c)) return null;
  const parts: string[] = [];
  if (c.command) parts.push("Command");
  if (c.control) parts.push("Control");
  if (c.alt) parts.push("Alt");
  if (c.shift) parts.push("Shift");
  if (c.key) parts.push(c.key);
  return parts.join("+");
}

export function formatChordLive(c: ChordState): string {
  const sc = chordToShortcut(c);
  if (sc) return formatHotkeyLabel(sc);
  // Incomplete preview while holding
  const parts: string[] = [];
  if (c.command) parts.push("⌘");
  if (c.control) parts.push("⌃");
  if (c.alt) parts.push("⌥");
  if (c.shift) parts.push("⇧");
  if (c.key) parts.push(c.key);
  return parts.length ? parts.join("") : "…";
}

/**
 * @deprecated Prefer absorbKeyDown + chordToShortcut for natural multi-key capture.
 * Kept for simple one-shot conversion.
 */
export function eventToShortcut(e: KeyboardEvent): string | null {
  return chordToShortcut(absorbKeyDown(emptyChord(), e));
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
  if (key.length === 1) {
    return /[a-z]/i.test(key) ? key.toUpperCase() : key;
  }
  return null;
}

/** Common presets (safe defaults + modifier-only). */
export const HOTKEY_PRESETS: { label: string; value: string }[] = [
  { label: "⌥Space", value: "Alt+Space" },
  { label: "⌥⇧", value: "Alt+Shift" },
  { label: "⌃⇧", value: "Control+Shift" },
  { label: "⌃⇧Space", value: "Control+Shift+Space" },
  { label: "⌘⇧D", value: "Command+Shift+D" },
];
