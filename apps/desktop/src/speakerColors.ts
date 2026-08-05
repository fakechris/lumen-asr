// Per-speaker colors for the meeting transcript (live + offline).
//
// Goal: give each speaker a stable, distinguishable color so a reader can tell
// people apart at a glance — while keeping color a *secondary* cue (the name
// text is always present, so nothing depends on color perception alone).
//
// Model:
//  - A speaker's color is derived from a normalized identity key (offline: the
//    display name, or the engine cluster label when unnamed; live: the annotated
//    / voiceprint display name). Normalizing by name means the *same person*
//    tends to get the *same color* across the live preview and the offline final
//    transcript, and across meetings.
//  - The base color is a stable hash of the key into a fixed palette
//    (`--spk-1`..`--spk-N`, defined per theme in styles.css so both light and
//    dark stay readable). Hashing alone can collide, so a per-view pass
//    (`buildSpeakerColorMap`) de-collides the speakers actually shown together:
//    each distinct speaker keeps its hashed slot unless it is already taken in
//    this view, in which case it probes forward to the next free slot. Within a
//    single transcript no two speakers share a color until the palette is
//    exhausted, while the hash bias keeps colors consistent across views.
//  - "我" (the self identity) is pinned to one fixed color (`--spk-self`).
//  - An unassigned / unknown speaker gets no color (null) — callers fall back to
//    a neutral token.

export const SPEAKER_PALETTE_SIZE = 10;

/** Fixed color for the self identity ("我"). */
export const SELF_COLOR_VAR = "var(--spk-self)";

const SELF_KEYS = new Set(["我", "me", "自己"]);

/** Trim + lowercase a speaker name into a stable coloring key, or null when it
 * carries no identity (empty / unassigned). */
export function normalizeSpeakerKey(
  name: string | null | undefined,
): string | null {
  const t = (name ?? "").trim().toLowerCase();
  return t.length > 0 ? t : null;
}

/** FNV-1a 32-bit hash — small, stable, and well-distributed for short strings. */
function hashKey(key: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < key.length; i += 1) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/** The palette slot a key hashes to (0-based). */
function baseSlot(key: string): number {
  return hashKey(key) % SPEAKER_PALETTE_SIZE;
}

function slotVar(slot: number): string {
  return `var(--spk-${slot + 1})`;
}

/**
 * Build a `normalizedKey → cssColorVar` map for a set of speakers shown
 * together, in appearance order. Each distinct speaker is biased to its hashed
 * palette slot and only nudged forward when that slot is already used in this
 * view, so distinct speakers stay visually distinct (up to the palette size)
 * while colors remain stable across views. Self and empty keys are skipped
 * (self has a fixed color; empty is neutral) — resolve those via
 * `colorForSpeaker`.
 */
export function buildSpeakerColorMap(
  keysInOrder: (string | null | undefined)[],
): Map<string, string> {
  const map = new Map<string, string>();
  const used = new Set<number>();
  for (const raw of keysInOrder) {
    const norm = normalizeSpeakerKey(raw);
    if (!norm || SELF_KEYS.has(norm) || map.has(norm)) continue;
    let slot = baseSlot(norm);
    if (used.size < SPEAKER_PALETTE_SIZE) {
      let probe = 0;
      while (used.has(slot) && probe < SPEAKER_PALETTE_SIZE) {
        slot = (slot + 1) % SPEAKER_PALETTE_SIZE;
        probe += 1;
      }
    }
    used.add(slot);
    map.set(norm, slotVar(slot));
  }
  return map;
}

/**
 * Resolve a speaker's color: the fixed self color when it is the self identity,
 * the per-view mapped color when present, otherwise its stable hashed color;
 * null for an unassigned / unknown speaker (callers use a neutral token).
 */
export function colorForSpeaker(
  map: Map<string, string>,
  key: string | null | undefined,
  opts?: { self?: boolean },
): string | null {
  const norm = normalizeSpeakerKey(key);
  if (opts?.self || (norm != null && SELF_KEYS.has(norm))) return SELF_COLOR_VAR;
  if (!norm) return null;
  return map.get(norm) ?? slotVar(baseSlot(norm));
}
