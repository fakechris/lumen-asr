import { test } from "node:test";
import assert from "node:assert/strict";
import { historyExportFilename } from "../src/historyExport.ts";

test("history export filename is dated with the chosen extension", () => {
  const name = historyExportFilename("jsonl");
  assert.match(name, /^lumen-history-\d{8}\.jsonl$/);
  assert.match(historyExportFilename("md"), /^lumen-history-\d{8}\.md$/);
});
