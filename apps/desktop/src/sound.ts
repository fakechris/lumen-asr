// Dictation sound feedback (start / done / error).
//
// Subscribes to the backend "dictation" event stream once and plays a short
// cue on phase transitions. Playback is best-effort: any audio failure is
// swallowed so dictation is never blocked by sound.

import { listen } from "@tauri-apps/api/event";
import startUrl from "./assets/sounds/start.wav";
import doneUrl from "./assets/sounds/done.wav";
import errorUrl from "./assets/sounds/error.wav";
import { soundForPhase, type SoundKind } from "./soundMap";

const SOURCES: Record<SoundKind, string> = {
  start: startUrl,
  done: doneUrl,
  error: errorUrl,
};

let enabled = true;
let started = false;

export function setSoundsEnabled(value: boolean): void {
  enabled = value;
}

export function playSound(kind: SoundKind): void {
  if (!enabled) return;
  try {
    const audio = new Audio(SOURCES[kind]);
    void audio.play().catch(() => {});
  } catch {
    // Audio unavailable (e.g. no output device) — never surface this.
  }
}

// Start listening to dictation events. Idempotent; call once at app startup.
export function initSoundFeedback(): void {
  if (started) return;
  started = true;
  let lastPhase = "";
  void listen<{ phase: string }>("dictation", (event) => {
    const phase = event.payload.phase;
    if (phase === lastPhase) return;
    lastPhase = phase;
    const kind = soundForPhase(phase);
    if (kind) playSound(kind);
  }).catch(() => {
    started = false;
  });
}
