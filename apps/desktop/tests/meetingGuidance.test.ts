import assert from "node:assert/strict";
import test from "node:test";

import { diarGuidance, isNoLlmMarker } from "../src/meetingGuidance.ts";

test("no reason yields no guidance", () => {
  assert.equal(diarGuidance(null), null);
  assert.equal(diarGuidance(undefined), null);
  assert.equal(diarGuidance(""), null);
});

test("unsupported-platform reason maps to macos_only", () => {
  assert.equal(
    diarGuidance(
      "transcribe: unsupported: offline diarization requires macOS built with the `diarize` feature (diar-rs)",
    ),
    "macos_only",
  );
});

test("missing diar models reason maps to install_models", () => {
  assert.equal(
    diarGuidance(
      "transcribe: diar models not found: missing segmentation at /models/diar/seg.onnx",
    ),
    "install_models",
  );
});

test("an unrelated failure has no specific diar guidance", () => {
  assert.equal(diarGuidance("recorder stop failed: device disconnected"), null);
});

test("detects the no-LLM sentinel summary", () => {
  assert.equal(isNoLlmMarker('{"skipped_no_llm":true}'), true);
  assert.equal(isNoLlmMarker('{"one_liner":"real minutes"}'), false);
  assert.equal(isNoLlmMarker("not json"), false);
});
