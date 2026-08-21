// Pure mapping from dictation phase to sound cue, kept side-effect free so
// node:test can exercise it without the Tauri event API or audio assets.

export type SoundKind = "start" | "done" | "error";

// Map one dictation phase to the sound it should trigger, if any.
export function soundForPhase(phase: string): SoundKind | null {
  switch (phase) {
    case "listening":
      return "start";
    case "done":
      return "done";
    case "error":
      return "error";
    default:
      return null;
  }
}
