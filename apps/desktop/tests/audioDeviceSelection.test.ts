import assert from "node:assert/strict";
import test from "node:test";

import { chooseAudioDevice } from "../src/audioDeviceSelection.ts";

const devices = [
  { name: "Sound Blaster E5", is_default: true },
  { name: "MacBook Pro Microphone", is_default: false },
];

test("keeps a persisted microphone instead of overwriting it with the system default", () => {
  assert.equal(
    chooseAudioDevice(devices, "MacBook Pro Microphone"),
    "MacBook Pro Microphone",
  );
});

test("falls back to the system default when the persisted microphone disappeared", () => {
  assert.equal(
    chooseAudioDevice(devices, "Disconnected Microphone"),
    "Sound Blaster E5",
  );
});

test("returns an empty selection when no input devices exist", () => {
  assert.equal(chooseAudioDevice([], "MacBook Pro Microphone"), "");
});
