# Implementation Plan: Global Hotkey (Event Tap) + Focus Cache

## Goals

1. Reliable push-to-talk (press → record, release → stop/insert), including modifier-only chords such as `Alt+Shift`.
2. Capture the typing target at record start (app name + bundle id) and reuse it at insert time without forcing `open -a` when the target is already frontmost.

## Design

### A. Keyboard monitor (macOS)

- Dedicated thread running `CGEventTap` + `CFRunLoop`.
- Events: `KeyDown`, `KeyUp`, `FlagsChanged` (+ re-enable on tap timeout).
- Chord parser supports `Alt+Shift`, `Alt+Space`, `Control+Shift+Space`, etc.
- Hold mode: rising edge starts dictation; falling edge stops.
- Toggle mode: rising edge toggles start/stop.
- Optional key swallowing while the chord is active (config later).
- Pause/resume for settings “record shortcut” UI.

### B. Focus cache

- On dictation start, capture frontmost app (name + bundle id) **before** showing UI.
- Prefer fast native path (`NSWorkspace` frontmost application); fall back to existing capture if needed.
- Store in process-local cache used by insert.
- On insert: hide capsule; only re-activate cached target if frontmost is our process.

### C. Integration

- Desktop `hotkey` module uses the EventTap monitor on macOS.
- Keep lifecycle phase machine (idle → recording → processing).
- Insert path remains type-first, paste fallback.

## Out of scope

- Windows EventTap equivalent (architecture-ready only).
- Post-paste AX edit learning rewrite.

## Success criteria

- Hold `Alt+Shift`: capsule shows, release triggers ASR + insert into the app that had focus at press.
- No stacked concurrent sessions from flag flicker.
- Settings can pause/resume monitoring while recording a new shortcut.

## Status

| Stage | Item | Status |
|-------|------|--------|
| 1 | CGEventTap monitor (`hotkey_tap`) | Done |
| 1 | Wire desktop hotkey to EventTap (+ fallbacks) | Done |
| 2 | NSWorkspace frontmost snapshot | Done |
| 2 | Sync focus cache at dictation start | Done |
| 2 | Insert: activate only if self frontmost | Done |
