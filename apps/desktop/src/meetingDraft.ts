// Pure helpers for choosing which minutes to show: final minutes, draft
// minutes (generated from the live transcript right after a meeting stops,
// while the offline pipeline is still running), or the local recap (keyword
// extraction, written when no LLM is configured).
//
// Framework-free so the "which minutes do we show, and how is it labelled"
// decision is unit-testable without a DOM; `MeetingPanel.tsx` imports these and
// `tests/meetingDraft.test.ts` exercises them directly.

import { isNoLlmMarker } from "./meetingGuidance.ts";

/** Storage kind of the final structured minutes row. */
export const FINAL_MINUTES_KIND = "summary";
/** Storage kind of the draft minutes row (same JSON shape as the final one). */
export const DRAFT_MINUTES_KIND = "minutes_draft";
/** Storage kind of the local recap row (same JSON shape as the final one). */
export const LOCAL_RECAP_KIND = "local_recap";

/** Label shown above a local recap, so it never passes for AI minutes. */
export const LOCAL_RECAP_LABEL = "本地摘要（未配置 AI）";

export type MinutesOrigin = "final" | "draft" | "local_recap";

export type MinutesRowChoice = {
  content: string;
  origin: MinutesOrigin;
};

/**
 * Pick the minutes row to render, in order of authority:
 * 1. final minutes (the backend deletes the draft and the local recap when
 *    they are saved, but a stale read must still prefer them);
 * 2. the live-transcript draft;
 * 3. the local recap.
 * The no-LLM sentinel is not minutes; a meeting carrying it shows only its
 * local recap, if any.
 */
export function pickMinutesRow(
  summaries: ReadonlyArray<{ kind: string; content: string }>,
): MinutesRowChoice | null {
  const final = summaries.find((s) => s.kind === FINAL_MINUTES_KIND);
  if (final && !isNoLlmMarker(final.content)) {
    return { content: final.content, origin: "final" };
  }
  const draft = summaries.find((s) => s.kind === DRAFT_MINUTES_KIND);
  if (draft && !final) return { content: draft.content, origin: "draft" };
  const recap = summaries.find((s) => s.kind === LOCAL_RECAP_KIND);
  return recap ? { content: recap.content, origin: "local_recap" } : null;
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

/** The label above the shown minutes; null for final minutes. */
export function minutesOriginLabel(
  origin: MinutesOrigin,
  inProgress: boolean,
): string | null {
  switch (origin) {
    case "final":
      return null;
    case "draft":
      return draftMinutesLabel(inProgress);
    case "local_recap":
      return LOCAL_RECAP_LABEL;
  }
}
