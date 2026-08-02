import assert from "node:assert/strict";
import test from "node:test";
import { firstNonBlankText } from "../src/sessionText.ts";

test("returns the first field containing non-whitespace text", () => {
  assert.equal(firstNonBlankText("  ", " pasted ", "raw"), "pasted");
  assert.equal(firstNonBlankText(null, "\t\n", " raw "), "raw");
  assert.equal(firstNonBlankText("\u00a0\ufeff", undefined, ""), "");
});
