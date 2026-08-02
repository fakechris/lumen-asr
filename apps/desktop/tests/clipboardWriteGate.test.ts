import assert from "node:assert/strict";
import test from "node:test";
import { ClipboardWriteGate } from "../src/clipboardWriteGate.ts";

function deferred() {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<void>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

test("trims copied text and ignores empty input", async () => {
  const gate = new ClipboardWriteGate();
  const writes: string[] = [];
  const writeText = async (value: string) => {
    writes.push(value);
  };

  assert.equal(await gate.write("  保留文本  ", writeText), "copied");
  assert.equal(await gate.write("   ", writeText), "empty");
  assert.deepEqual(writes, ["保留文本"]);
});

test("a pending clipboard write rejects overlapping writes", async () => {
  const gate = new ClipboardWriteGate();
  const first = deferred();

  const firstResult = gate.write("first", () => first.promise);
  assert.equal(await gate.write("second", async () => {}), "busy");
  first.resolve();
  assert.equal(await firstResult, "copied");
});

test("cancelled or stale requests cannot surface feedback or errors", async () => {
  const gate = new ClipboardWriteGate();
  const cancelled = deferred();
  const cancelledResult = gate.write("cancelled", () => cancelled.promise);
  gate.cancelPending();
  cancelled.resolve();
  assert.equal(await cancelledResult, "stale");

  await assert.rejects(
    gate.write("current", async () => {
      throw new Error("clipboard denied");
    }),
    /clipboard denied/,
  );
});
