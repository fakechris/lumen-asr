import assert from "node:assert/strict";
import test from "node:test";

import { draftMinutesLabel, pickMinutesRow } from "../src/meetingDraft.ts";

const finalRow = { kind: "summary", content: '{"one_liner":"final"}' };
const draftRow = { kind: "minutes_draft", content: '{"one_liner":"draft"}' };

test("final minutes win over a draft", () => {
  assert.deepEqual(pickMinutesRow([draftRow, finalRow]), {
    content: finalRow.content,
    draft: false,
  });
});

test("a draft is shown while there are no final minutes", () => {
  assert.deepEqual(pickMinutesRow([draftRow]), {
    content: draftRow.content,
    draft: true,
  });
});

test("the no-LLM sentinel shows no minutes at all", () => {
  assert.equal(
    pickMinutesRow([{ kind: "summary", content: '{"skipped_no_llm":true}' }]),
    null,
  );
});

test("no summary rows means no minutes", () => {
  assert.equal(pickMinutesRow([{ kind: "decisions", content: "[]" }]), null);
});

test("draft label reflects whether the final minutes are still coming", () => {
  assert.match(draftMinutesLabel(true), /生成中/);
  assert.match(draftMinutesLabel(false), /未生成/);
});
