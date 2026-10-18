import assert from "node:assert/strict";
import test from "node:test";

import {
  draftMinutesLabel,
  LOCAL_RECAP_LABEL,
  minutesOriginLabel,
  pickMinutesRow,
} from "../src/meetingDraft.ts";

const finalRow = { kind: "summary", content: '{"one_liner":"final"}' };
const draftRow = { kind: "minutes_draft", content: '{"one_liner":"draft"}' };
const recapRow = { kind: "local_recap", content: '{"decisions":[]}' };
const noLlmRow = { kind: "summary", content: '{"skipped_no_llm":true}' };

test("final minutes win over a draft", () => {
  assert.deepEqual(pickMinutesRow([draftRow, finalRow]), {
    content: finalRow.content,
    origin: "final",
  });
});

test("a draft is shown while there are no final minutes", () => {
  assert.deepEqual(pickMinutesRow([draftRow]), {
    content: draftRow.content,
    origin: "draft",
  });
});

test("the no-LLM sentinel alone shows no minutes at all", () => {
  assert.equal(pickMinutesRow([noLlmRow]), null);
});

test("the no-LLM sentinel never surfaces a draft", () => {
  assert.equal(pickMinutesRow([noLlmRow, draftRow]), null);
});

test("no summary rows means no minutes", () => {
  assert.equal(pickMinutesRow([{ kind: "decisions", content: "[]" }]), null);
});

test("a local recap is shown next to the no-LLM sentinel", () => {
  assert.deepEqual(pickMinutesRow([noLlmRow, recapRow]), {
    content: recapRow.content,
    origin: "local_recap",
  });
});

test("final minutes and drafts win over a local recap", () => {
  assert.equal(pickMinutesRow([recapRow, finalRow])?.origin, "final");
  assert.equal(pickMinutesRow([recapRow, draftRow])?.origin, "draft");
});

test("draft label reflects whether the final minutes are still coming", () => {
  assert.match(draftMinutesLabel(true), /生成中/);
  assert.match(draftMinutesLabel(false), /未生成/);
});

test("origin labels: none for final minutes, explicit for the others", () => {
  assert.equal(minutesOriginLabel("final", false), null);
  assert.equal(minutesOriginLabel("draft", true), draftMinutesLabel(true));
  assert.equal(minutesOriginLabel("local_recap", false), LOCAL_RECAP_LABEL);
  assert.equal(LOCAL_RECAP_LABEL, "本地摘要（未配置 AI）");
});
