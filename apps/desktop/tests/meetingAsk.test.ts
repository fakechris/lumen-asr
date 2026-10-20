import assert from "node:assert/strict";
import test from "node:test";

import {
  askKeyAction,
  citationLabel,
  formatCitationTime,
  formatMeetingDate,
  splitAnswer,
  type AskCitationRef,
} from "../src/meetingAsk.ts";

const ID_A = "0b6a7a3e-6f1c-4a55-9d0e-1a2b3c4d5e6f";
const ID_B = "9f8e7d6c-5b4a-4321-8fed-cba987654321";

const cite = (meeting_id: string, ms: number, title = "周会"): AskCitationRef => ({
  meeting_id,
  title,
  created_at: "2026-10-13T02:00:00+00:00",
  ms,
});

test("answer markers become citation chips; unknown markers are dropped", () => {
  const a = cite(ID_A, 120000);
  const b = cite(ID_B, 5000, "招聘同步");
  const parts = splitAnswer(
    `预算砍半[[m:${ID_A}@120000]]，招聘推迟[[m:${ID_B}@5000]]。编造[[m:${ID_B}@1]]结束`,
    [a, b],
  );
  assert.deepEqual(parts, [
    { kind: "text", text: "预算砍半" },
    { kind: "cite", citation: a },
    { kind: "text", text: "，招聘推迟" },
    { kind: "cite", citation: b },
    { kind: "text", text: "。编造结束" },
  ]);
});

test("adjacent repeats collapse and malformed markers stay as text", () => {
  const a = cite(ID_A, 0);
  assert.deepEqual(
    splitAnswer(`x[[m:${ID_A}@0]] [[m:${ID_A.toUpperCase()}@0]]y`, [a]),
    [
      { kind: "text", text: "x" },
      { kind: "cite", citation: a },
      { kind: "text", text: "y" },
    ],
  );
  assert.deepEqual(splitAnswer("[[m:nope@1]]", [a]), [
    { kind: "text", text: "[[m:nope@1]]" },
  ]);
  assert.deepEqual(splitAnswer("", [a]), []);
});

test("citation labels read 标题 · 日期 · mm:ss", () => {
  const now = new Date(2026, 9, 17);
  const local = new Date(2026, 9, 13, 10, 0).toISOString();
  assert.equal(
    citationLabel({ ...cite(ID_A, 125_400), created_at: local }, now),
    "周会 · 10月13日 · 02:05",
  );
  assert.equal(
    citationLabel({ ...cite(ID_A, 0, "  "), created_at: local }, now),
    "未命名会议 · 10月13日 · 00:00",
  );
  assert.equal(formatCitationTime(3_725_000), "1:02:05");
  assert.equal(formatMeetingDate(new Date(2025, 0, 2).toISOString(), now), "2025年1月2日");
  assert.equal(formatMeetingDate("bogus", now), "bogus");
});

test("question box keys: Enter asks, Esc cancels, clears, then exits", () => {
  const base = { isComposing: false, running: false, hasText: true, hasResult: false };
  assert.equal(askKeyAction({ ...base, key: "Enter" }), "submit");
  assert.equal(askKeyAction({ ...base, key: "Enter", hasText: false }), null);
  assert.equal(askKeyAction({ ...base, key: "Enter", running: true }), null);
  assert.equal(askKeyAction({ ...base, key: "Enter", isComposing: true }), null);
  assert.equal(askKeyAction({ ...base, key: "Escape", running: true }), "cancel");
  assert.equal(askKeyAction({ ...base, key: "Escape" }), "clear");
  assert.equal(
    askKeyAction({ ...base, key: "Escape", hasText: false, hasResult: true }),
    "clear",
  );
  assert.equal(askKeyAction({ ...base, key: "Escape", hasText: false }), "exit");
  assert.equal(askKeyAction({ ...base, key: "a" }), null);
});
