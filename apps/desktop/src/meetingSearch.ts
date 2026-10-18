// Pure helpers for the meeting-library search and keyboard navigation.
//
// Framework-free so snippet parsing and the key → action mapping are
// unit-testable without a DOM; `MeetingPanel.tsx` imports these and
// `tests/meetingSearch.test.ts` exercises them directly.

/** Start/end of a highlighted match inside a search snippet (must match
 * `SEARCH_SNIPPET_MARK_START` / `_END` in `lumen-store`). */
export const SNIPPET_MARK_START = "\u0002";
export const SNIPPET_MARK_END = "\u0003";

export type SnippetPart = { text: string; hit: boolean };

/** Split a backend snippet into plain and highlighted runs. Marks are stripped;
 * an unterminated highlight runs to the end. Empty runs are dropped. */
export function splitSnippet(snippet: string): SnippetPart[] {
  const parts: SnippetPart[] = [];
  let hit = false;
  let text = "";
  const flush = () => {
    if (text) parts.push({ text, hit });
    text = "";
  };
  for (const ch of snippet) {
    if (ch === SNIPPET_MARK_START) {
      flush();
      hit = true;
    } else if (ch === SNIPPET_MARK_END) {
      flush();
      hit = false;
    } else {
      text += ch;
    }
  }
  flush();
  return parts;
}

export type MeetingSearchField = "title" | "notes" | "segment" | "summary";

const FIELD_LABEL: Record<MeetingSearchField, string> = {
  title: "标题",
  notes: "笔记",
  segment: "逐字稿",
  summary: "纪要",
};

/** Short Chinese label naming where a search hit matched. */
export function searchFieldLabel(field: MeetingSearchField): string {
  return FIELD_LABEL[field] ?? field;
}

/** Move the list cursor by `delta` rows within `ids`, clamped to the ends.
 * With no (or a vanished) cursor, ↓ lands on the first row and ↑ on the last. */
export function moveCursor(
  ids: readonly string[],
  current: string | null,
  delta: 1 | -1,
): string | null {
  if (ids.length === 0) return null;
  const index = current === null ? -1 : ids.indexOf(current);
  if (index < 0) return delta > 0 ? ids[0] : ids[ids.length - 1];
  const next = Math.min(ids.length - 1, Math.max(0, index + delta));
  return ids[next];
}

export type LibraryKeyAction =
  | "up"
  | "down"
  | "open"
  | "focus-search"
  | "clear-search"
  | "blur-search"
  | null;

export type LibraryKeyInput = {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  /** Focus is in the library search box. */
  inSearch: boolean;
  /** Focus is in some other text field (rename, title draft, …). */
  inOtherField: boolean;
  /** A dialog or menu is open and owns the keyboard. */
  overlayOpen: boolean;
  /** The search box currently holds text. */
  hasQuery: boolean;
  /** A row is highlighted by the keyboard cursor. */
  hasCursor: boolean;
};

/** Map one keydown in the meeting library to an action.
 *
 * - ⌘F / Ctrl+F focuses the search box (from anywhere but an open dialog).
 * - ↑ / ↓ move the row cursor; they also work from the search box so the user
 *   can type and walk the results without reaching for the mouse.
 * - Enter opens the highlighted row (from the search box or the list).
 * - Esc in the search box clears the query, or leaves the box when it is
 *   already empty; Esc in the list clears an active query.
 * Other text fields keep every key to themselves. */
export function libraryKeyAction(input: LibraryKeyInput): LibraryKeyAction {
  if (input.overlayOpen) return null;
  if (
    (input.metaKey || input.ctrlKey) &&
    !input.altKey &&
    input.key.toLowerCase() === "f"
  ) {
    return "focus-search";
  }
  if (input.inOtherField) return null;
  if (input.metaKey || input.ctrlKey || input.altKey) return null;
  switch (input.key) {
    case "ArrowDown":
      return "down";
    case "ArrowUp":
      return "up";
    case "Enter":
      return input.hasCursor ? "open" : null;
    case "Escape":
      if (input.hasQuery) return "clear-search";
      return input.inSearch ? "blur-search" : null;
    default:
      return null;
  }
}

export type DetailKeyInput = {
  key: string;
  defaultPrevented: boolean;
  /** Focus is in a text field (title, notes, segment editor, …). */
  inField: boolean;
  /** A dialog or menu is open and handles Esc itself. */
  overlayOpen: boolean;
};

/** True when Esc in the meeting detail view should return to the library:
 * only when nothing else (a field, dialog or menu) has a use for it. */
export function detailEscapeGoesBack(input: DetailKeyInput): boolean {
  return (
    input.key === "Escape" &&
    !input.defaultPrevented &&
    !input.inField &&
    !input.overlayOpen
  );
}
