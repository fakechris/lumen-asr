import assert from "node:assert/strict";
import test from "node:test";

import { isMeaningfulMeetingTrim } from "../src/meetingTrim.ts";

test("matches the backend unchanged-range tolerance on both edges", () => {
  assert.equal(isMeaningfulMeetingTrim(0, 60, 60), false);
  assert.equal(isMeaningfulMeetingTrim(0.25, 59.75, 60), false);
  assert.equal(isMeaningfulMeetingTrim(0.3, 60, 60), true);
  assert.equal(isMeaningfulMeetingTrim(0, 59.7, 60), true);
});
