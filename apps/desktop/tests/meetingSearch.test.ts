import assert from "node:assert/strict";
import test from "node:test";

import {
  detailEscapeGoesBack,
  libraryKeyAction,
  moveCursor,
  searchFieldLabel,
  splitSnippet,
  type LibraryKeyInput,
} from "../src/meetingSearch.ts";

const key = (overrides: Partial<LibraryKeyInput>): LibraryKeyInput => ({
  key: "",
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  inSearch: false,
  inOtherField: false,
  overlayOpen: false,
  hasQuery: false,
  hasCursor: false,
  ...overrides,
});

test("snippet marks become highlighted runs", () => {
  assert.deepEqual(splitSnippet("…聊\u0002预算\u0003评审…"), [
    { text: "…聊", hit: false },
    { text: "预算", hit: true },
    { text: "评审…", hit: false },
  ]);
  assert.deepEqual(splitSnippet("plain"), [{ text: "plain", hit: false }]);
  assert.deepEqual(splitSnippet("\u0002open"), [{ text: "open", hit: true }]);
  assert.deepEqual(splitSnippet(""), []);
});

test("field labels name where the hit matched", () => {
  assert.equal(searchFieldLabel("segment"), "逐字稿");
  assert.equal(searchFieldLabel("summary"), "纪要");
  assert.equal(searchFieldLabel("notes"), "笔记");
});

test("cursor moves and clamps within the visible rows", () => {
  const ids = ["a", "b", "c"];
  assert.equal(moveCursor(ids, null, 1), "a");
  assert.equal(moveCursor(ids, null, -1), "c");
  assert.equal(moveCursor(ids, "a", 1), "b");
  assert.equal(moveCursor(ids, "c", 1), "c");
  assert.equal(moveCursor(ids, "a", -1), "a");
  assert.equal(moveCursor(ids, "gone", 1), "a");
  assert.equal(moveCursor([], "a", 1), null);
});

test("cmd/ctrl+F focuses search unless a dialog owns the keyboard", () => {
  assert.equal(
    libraryKeyAction(key({ key: "f", metaKey: true })),
    "focus-search",
  );
  assert.equal(
    libraryKeyAction(key({ key: "F", ctrlKey: true })),
    "focus-search",
  );
  assert.equal(
    libraryKeyAction(key({ key: "f", metaKey: true, inOtherField: true })),
    "focus-search",
  );
  assert.equal(
    libraryKeyAction(key({ key: "f", metaKey: true, overlayOpen: true })),
    null,
  );
  assert.equal(libraryKeyAction(key({ key: "f" })), null);
});

test("arrows and Enter navigate from the list and the search box", () => {
  assert.equal(libraryKeyAction(key({ key: "ArrowDown" })), "down");
  assert.equal(libraryKeyAction(key({ key: "ArrowUp", inSearch: true })), "up");
  assert.equal(
    libraryKeyAction(key({ key: "Enter", inSearch: true, hasCursor: true })),
    "open",
  );
  assert.equal(libraryKeyAction(key({ key: "Enter" })), null);
  // Other text fields (rename, title draft) keep their keys.
  assert.equal(
    libraryKeyAction(key({ key: "ArrowDown", inOtherField: true })),
    null,
  );
  assert.equal(
    libraryKeyAction(key({ key: "Enter", inOtherField: true, hasCursor: true })),
    null,
  );
  assert.equal(
    libraryKeyAction(key({ key: "ArrowDown", overlayOpen: true })),
    null,
  );
  assert.equal(
    libraryKeyAction(key({ key: "ArrowDown", metaKey: true })),
    null,
  );
});

test("Esc clears the query first, then leaves the search box", () => {
  assert.equal(
    libraryKeyAction(key({ key: "Escape", inSearch: true, hasQuery: true })),
    "clear-search",
  );
  assert.equal(
    libraryKeyAction(key({ key: "Escape", hasQuery: true })),
    "clear-search",
  );
  assert.equal(
    libraryKeyAction(key({ key: "Escape", inSearch: true })),
    "blur-search",
  );
  assert.equal(libraryKeyAction(key({ key: "Escape" })), null);
  assert.equal(
    libraryKeyAction(key({ key: "Escape", hasQuery: true, overlayOpen: true })),
    null,
  );
});

test("Esc in the detail view goes back only when nothing else wants it", () => {
  const base = {
    key: "Escape",
    defaultPrevented: false,
    inField: false,
    overlayOpen: false,
  };
  assert.equal(detailEscapeGoesBack(base), true);
  assert.equal(detailEscapeGoesBack({ ...base, inField: true }), false);
  assert.equal(detailEscapeGoesBack({ ...base, overlayOpen: true }), false);
  assert.equal(detailEscapeGoesBack({ ...base, defaultPrevented: true }), false);
  assert.equal(detailEscapeGoesBack({ ...base, key: "Enter" }), false);
});
