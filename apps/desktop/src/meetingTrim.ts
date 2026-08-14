/** Must match the backend's tolerance for treating a selection as unchanged. */
export const MEETING_TRIM_EPSILON_SECONDS = 0.25;

/** True when at least one side removes more than the backend tolerance. */
export function isMeaningfulMeetingTrim(
  startSeconds: number,
  endSeconds: number,
  durationSeconds: number,
): boolean {
  return (
    startSeconds > MEETING_TRIM_EPSILON_SECONDS ||
    durationSeconds - endSeconds > MEETING_TRIM_EPSILON_SECONDS
  );
}
