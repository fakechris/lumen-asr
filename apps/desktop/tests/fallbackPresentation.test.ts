import assert from "node:assert/strict";
import test from "node:test";

import {
  correctorFallbackNotice,
  correctorFallbackReasonLabel,
} from "../src/fallbackPresentation.ts";

test("explains granular context-integrity fallback reasons", () => {
  assert.equal(
    correctorFallbackReasonLabel("context_protected_token_mismatch"),
    "数字或编号保护检查未通过",
  );
  assert.equal(
    correctorFallbackReasonLabel("context_safety_marker_mismatch"),
    "关键动作或否定含义检查未通过",
  );
});

test("fallback notice says that the model revision was not used", () => {
  assert.equal(
    correctorFallbackNotice("timeout"),
    "AI 修订未采用，已使用基础整理文本：AI 服务响应超时",
  );
});
