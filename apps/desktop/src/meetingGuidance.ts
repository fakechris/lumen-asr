// Pure helpers for meeting failure / no-LLM guidance.
//
// Kept framework-free (no React) so the branching logic that decides *which*
// guidance to show is unit-testable without a DOM. `MeetingPanel.tsx` imports
// these; `tests/meetingGuidance.test.ts` exercises them directly.

/**
 * Which actionable hint to show for a failed meeting, derived from its
 * `failure_reason`:
 *  - `"install_models"` — speaker-diarization models are missing.
 *  - `"macos_only"` — offline diarization is unsupported on this platform.
 *  - `null` — no specific guidance (show the raw reason only).
 */
export type DiarGuidance = "install_models" | "macos_only" | null;

export function diarGuidance(reason?: string | null): DiarGuidance {
  if (!reason) return null;
  const r = reason.toLowerCase();
  // Order matters: the unsupported-platform message also mentions
  // "diarization", so match the platform case first.
  if (r.includes("requires macos") || r.includes("unsupported")) {
    return "macos_only";
  }
  if (r.includes("diar models not found") || r.includes("missing")) {
    return "install_models";
  }
  return null;
}

/**
 * True when a `summary`-kind row's `content` is the "LLM was not configured, so
 * minutes were skipped" sentinel the backend writes on the transcript-only path.
 * Such a meeting is `ready` with a real transcript but no generated minutes.
 */
export function isNoLlmMarker(content: string): boolean {
  try {
    const raw = JSON.parse(content) as { skipped_no_llm?: unknown };
    return raw?.skipped_no_llm === true;
  } catch {
    return false;
  }
}
