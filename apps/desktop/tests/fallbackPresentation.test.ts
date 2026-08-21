import assert from "node:assert/strict";
import test from "node:test";

import {
  correctorFallbackNotice,
  correctorFallbackReasonLabel,
  dictationDoneNotice,
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
    "识别完成，校对未应用：AI 服务响应超时",
  );
});

test("done notice keeps insert failure separate from ASR failure", () => {
  assert.equal(dictationDoneNotice({}), null);
  assert.equal(
    dictationDoneNotice({
      insertNotice: "未能插入到当前窗口，已复制到剪贴板，请手动粘贴。",
    }),
    "未能插入到当前窗口，已复制到剪贴板，请手动粘贴。",
  );
  assert.equal(
    dictationDoneNotice({
      fallbackReason: "timeout",
      insertNotice: "未能插入到当前窗口，已复制到剪贴板，请手动粘贴。",
    }),
    "识别完成，校对未应用：AI 服务响应超时 未能插入到当前窗口，已复制到剪贴板，请手动粘贴。",
  );
});
