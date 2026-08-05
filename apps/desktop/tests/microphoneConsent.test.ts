import assert from "node:assert/strict";
import test from "node:test";

import {
  acknowledgeWindowsMicrophoneNotice,
  hasAcknowledgedWindowsMicrophoneNotice,
} from "../src/microphoneConsent.ts";

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  };
}

test("requires acknowledgement before first Windows microphone use", () => {
  const storage = memoryStorage();
  assert.equal(hasAcknowledgedWindowsMicrophoneNotice(storage), false);

  acknowledgeWindowsMicrophoneNotice(storage);
  assert.equal(hasAcknowledgedWindowsMicrophoneNotice(storage), true);
});

test("storage failures leave the notice unacknowledged", () => {
  const storage = {
    getItem() {
      throw new Error("storage unavailable");
    },
    setItem() {
      throw new Error("storage unavailable");
    },
  };

  acknowledgeWindowsMicrophoneNotice(storage);
  assert.equal(hasAcknowledgedWindowsMicrophoneNotice(storage), false);
});
