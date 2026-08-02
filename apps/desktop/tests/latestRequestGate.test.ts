import assert from "node:assert/strict";
import test from "node:test";
import { LatestRequestGate } from "../src/latestRequestGate.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

test("overlapping refreshes coalesce into one latest rerun", async () => {
  const gate = new LatestRequestGate<string>();
  const first = deferred<string>();
  const rerun = deferred<string>();
  let calls = 0;
  const firstResult = gate.run(() => {
    calls += 1;
    return first.promise;
  });
  const latestResult = gate.run(() => {
    calls += 1;
    return rerun.promise;
  });
  const duplicateResult = gate.run(() => {
    calls += 1;
    return rerun.promise;
  });

  assert.equal(calls, 1);
  first.resolve("old snapshot");
  await Promise.resolve();
  assert.equal(calls, 2);
  rerun.resolve("new snapshot");
  const expected = {
    status: "current",
    value: "new snapshot",
  } as const;
  assert.deepEqual(await firstResult, expected);
  assert.deepEqual(await latestResult, expected);
  assert.deepEqual(await duplicateResult, expected);
});

test("cancelled requests suppress both values and errors", async () => {
  const gate = new LatestRequestGate<string>();
  const pending = deferred<string>();
  const result = gate.run(() => pending.promise);
  gate.cancelPending();
  pending.resolve("too late");
  assert.deepEqual(await result, { status: "stale" });

  await assert.rejects(
    gate.run(async () => {
      throw new Error("current failure");
    }),
    /current failure/,
  );
});

test("a fresh request starts immediately after cancelling an older cycle", async () => {
  const gate = new LatestRequestGate<string>();
  const oldRequest = deferred<string>();
  const freshRequest = deferred<string>();
  let calls = 0;
  const oldResult = gate.run(() => {
    calls += 1;
    return oldRequest.promise;
  });
  gate.cancelPending();
  const freshResult = gate.run(() => {
    calls += 1;
    return freshRequest.promise;
  });

  assert.equal(calls, 2);
  freshRequest.resolve("fresh snapshot");
  assert.deepEqual(await freshResult, {
    status: "current",
    value: "fresh snapshot",
  });
  oldRequest.resolve("cancelled snapshot");
  assert.deepEqual(await oldResult, { status: "stale" });
});
