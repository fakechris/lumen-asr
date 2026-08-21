import assert from "node:assert/strict";
import test from "node:test";

import { soundForPhase } from "../src/soundMap.ts";

test("plays a cue for listening, done and error phases", () => {
  assert.equal(soundForPhase("listening"), "start");
  assert.equal(soundForPhase("done"), "done");
  assert.equal(soundForPhase("error"), "error");
});

test("stays silent for processing, idle, notice, cancelled and unknown phases", () => {
  assert.equal(soundForPhase("processing"), null);
  assert.equal(soundForPhase("idle"), null);
  assert.equal(soundForPhase("notice"), null);
  assert.equal(soundForPhase("cancelled"), null);
  assert.equal(soundForPhase(""), null);
});
