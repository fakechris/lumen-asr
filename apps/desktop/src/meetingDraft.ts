// Pure helpers for draft minutes (generated from the live transcript right
// after a meeting stops, while the offline pipeline is still running).
//
// Framework-free so the "which minutes do we show, and how is it labelled"
// decision is unit-testable without a DOM; `MeetingPanel.tsx` imports these and
// `tests/meetingDraft.test.ts` exercises them directly.

import { isNoLlmMarker } from "./meetingGuidance.ts";

/** Storage kind of the final structured minutes row. */
export const FINAL_MINUTES_KIND = "summary";
/** Storage kind of the draft minutes row (same JSON shape as the final one). */
export const DRAFT_MINUTES_KIND = "minutes_draft";

export type MinutesRowChoice = {
  content: string;
  draft: boolean;
};

/**
 * Pick the minutes row to render: the final minutes whenever they exist (the
 * backend deletes the draft when they are saved, but a stale read must still
 * prefer them), otherwise the draft. The no-LLM sentinel is not minutes, and a
 * meeting carrying it shows neither.
 */
export function pickMinutesRow(
  summaries: ReadonlyArray<{ kind: string; content: string }>,
): MinutesRowChoice | null {
  const final = summaries.find((s) => s.kind === FINAL_MINUTES_KIND);
  if (final) {
    return isNoLlmMarker(final.content)
      ? null
      : { content: final.content, draft: false };
  }
  const draft = summaries.find((s) => s.kind === DRAFT_MINUTES_KIND);
  return draft ? { content: draft.content, draft: true } : null;
}

/**
 * Label shown above draft minutes: while the meeting is still processing the
 * final minutes are on their way; once it has settled without them (minutes
 * step failed, or the whole run failed) the draft is all there is.
 */
export function draftMinutesLabel(inProgress: boolean): string {
  return inProgress
    ? "草稿（基于实时转写）· 正式纪要生成中"
    : "草稿（基于实时转写）· 正式纪要未生成";
}
