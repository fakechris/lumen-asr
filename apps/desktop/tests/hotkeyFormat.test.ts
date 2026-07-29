import assert from "node:assert/strict";
import test from "node:test";

import {
  HOTKEY_PRESETS,
  absorbKeyDown,
  chordToShortcut,
  emptyChord,
  formatHotkeyLabel,
  isValidChord,
} from "../src/hotkeyFormat.ts";

test("supports Fn as a standalone hold-to-record chord", () => {
  const event = {
    key: "Fn",
    code: "Fn",
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
  } as KeyboardEvent;

  const chord = absorbKeyDown(emptyChord(), event);
  assert.equal((chord as typeof chord & { fn: boolean }).fn, true);
  assert.equal(isValidChord(chord), true);
  assert.equal(chordToShortcut(chord), "Fn");
  assert.equal(formatHotkeyLabel("Fn"), "fn");
  assert.ok(HOTKEY_PRESETS.some((preset) => preset.value === "Fn"));
});
