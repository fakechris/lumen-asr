import assert from "node:assert/strict";
import test from "node:test";

import {
  copyToastLabel,
  correctorFallbackNotice,
  correctorFallbackReasonLabel,
  dictationDoneNotice,
  formatAsrEngineLabel,
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
  assert.equal(dictationDoneNotice({ insertNotice: "已复制" }), "已复制");
  assert.equal(
    dictationDoneNotice({
      fallbackReason: "timeout",
      insertNotice: "已复制",
    }),
    "已复制 · 识别完成，校对未应用：AI 服务响应超时",
  );
  assert.equal(
    dictationDoneNotice({
      insertNotice: "未能插入，也无法写入剪贴板。请从历史记录复制结果。",
    }),
    "未能插入，也无法写入剪贴板。请从历史记录复制结果。",
  );
});

test("copy toast is the short confirmation, not the full insert help", () => {
  assert.equal(copyToastLabel(null), null);
  assert.equal(copyToastLabel("已复制"), "已复制");
  assert.equal(copyToastLabel("已复制 · 请开启辅助功能后可自动插入"), "已复制");
  assert.equal(copyToastLabel("未能插入，也无法写入剪贴板。请从历史记录复制结果。"), null);
});

test("history labels the local engine when cloud ASR timed out", () => {
  assert.equal(formatAsrEngineLabel("sensevoice"), "sensevoice");
  assert.equal(
    formatAsrEngineLabel("openai_audio→sensevoice"),
    "sensevoice（在线超时，改用本地）",
  );
});
