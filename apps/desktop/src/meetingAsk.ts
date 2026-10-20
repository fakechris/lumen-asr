// Pure helpers for "问会议" (ask past meetings): citation-marker parsing,
// chip labels and the question box's key handling.
//
// Framework-free so they are unit-testable without a DOM; `MeetingPanel.tsx`
// imports these and `tests/meetingAsk.test.ts` exercises them directly.

/** A validated citation returned by `ask_meetings`. */
export type AskCitationRef = {
  meeting_id: string;
  title?: string | null;
  created_at: string;
  ms: number;
};

export type AnswerPart =
  | { kind: "text"; text: string }
  | { kind: "cite"; citation: AskCitationRef };

/** `[[m:<uuid>@<ms>]]`, the marker format the backend validates. */
const MARKER = /\[\[m:([0-9a-fA-F-]{36})@(\d+)\]\]/g;

/** Split an answer into text runs and citation chips. A marker is shown only
 * when it matches a returned citation; unknown markers are dropped, and a
 * repeat of the citation just before it (only whitespace between) collapses. */
export function splitAnswer(
  answer: string,
  citations: readonly AskCitationRef[],
): AnswerPart[] {
  const parts: AnswerPart[] = [];
  const pushText = (text: string) => {
    if (!text) return;
    const last = parts[parts.length - 1];
    if (last?.kind === "text") last.text += text;
    else parts.push({ kind: "text", text });
  };
  let last = 0;
  for (const match of answer.matchAll(MARKER)) {
    const start = match.index ?? 0;
    const between = answer.slice(last, start);
    last = start + match[0].length;
    const id = match[1].toLowerCase();
    const ms = Number(match[2]);
    const citation = citations.find(
      (c) => c.meeting_id.toLowerCase() === id && c.ms === ms,
    );
    const prev = parts[parts.length - 1];
    if (
      citation &&
      between.trim() === "" &&
      prev?.kind === "cite" &&
      prev.citation === citation
    ) {
      continue;
    }
    pushText(between);
    if (citation) parts.push({ kind: "cite", citation });
  }
  pushText(answer.slice(last));
  return parts;
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/** mm:ss (h:mm:ss past an hour) for a citation offset in milliseconds. */
export function formatCitationTime(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}:${pad2(m)}:${pad2(s % 60)}` : `${pad2(m)}:${pad2(s % 60)}`;
}

/** Local `M月D日`, with the year when it is not the current one. */
export function formatMeetingDate(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const day = `${d.getMonth() + 1}月${d.getDate()}日`;
  return d.getFullYear() === now.getFullYear() ? day : `${d.getFullYear()}年${day}`;
}

/** Chip label: `标题 · 日期 · mm:ss`. */
export function citationLabel(c: AskCitationRef, now: Date = new Date()): string {
  const title = c.title?.trim() || "未命名会议";
  return `${title} · ${formatMeetingDate(c.created_at, now)} · ${formatCitationTime(c.ms)}`;
}

export type AskKeyAction = "submit" | "cancel" | "clear" | "exit" | null;

/** Keys in the question box: Enter asks; Esc cancels a running question,
 * else clears the question and answer, else leaves the ask mode. */
export function askKeyAction(input: {
  key: string;
  isComposing: boolean;
  running: boolean;
  hasText: boolean;
  hasResult: boolean;
}): AskKeyAction {
  if (input.isComposing) return null;
  if (input.key === "Enter") {
    return !input.running && input.hasText ? "submit" : null;
  }
  if (input.key === "Escape") {
    if (input.running) return "cancel";
    return input.hasText || input.hasResult ? "clear" : "exit";
  }
  return null;
}
