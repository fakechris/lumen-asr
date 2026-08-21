#!/usr/bin/env node
// Synthesize the dictation sound cues (start / done / error) as 16-bit PCM
// WAV. Generated artifacts live in src/assets/sounds/ (gitignored) — the PR
// gate rejects binary blobs, so sounds ship as code, like the icon regen
// pipeline. Runs from predev/prebuild.

import { mkdirSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SR = 44100;
const outDir = join(dirname(fileURLToPath(import.meta.url)), "../src/assets/sounds");

function writeWav(name, samples) {
  const data = Buffer.alloc(samples.length * 2);
  samples.forEach((s, i) => {
    const clamped = Math.max(-1, Math.min(1, s));
    data.writeInt16LE(Math.round(clamped * 32767 * 0.5), i * 2);
  });
  const header = Buffer.alloc(44);
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + data.length, 4);
  header.write("WAVEfmt ", 8);
  header.writeUInt32LE(16, 16); // fmt chunk size
  header.writeUInt16LE(1, 20); // PCM
  header.writeUInt16LE(1, 22); // mono
  header.writeUInt32LE(SR, 24);
  header.writeUInt32LE(SR * 2, 28); // byte rate
  header.writeUInt16LE(2, 32); // block align
  header.writeUInt16LE(16, 34); // bits per sample
  header.write("data", 36);
  header.writeUInt32LE(data.length, 40);
  writeFileSync(join(outDir, name), Buffer.concat([header, data]));
}

function tone(freq, dur, fade = 0.02) {
  const n = Math.floor(SR * dur);
  const f = Math.floor(SR * fade);
  const out = new Array(n);
  for (let i = 0; i < n; i++) {
    let env = 1;
    if (i < f) env = i / f;
    if (i > n - f) env = (n - i) / f;
    out[i] = Math.sin(2 * Math.PI * freq * (i / SR)) * env;
  }
  return out;
}

function sweep(f0, f1, dur) {
  const n = Math.floor(SR * dur);
  const out = new Array(n);
  for (let i = 0; i < n; i++) {
    const f = f0 + (f1 - f0) * (i / n);
    const env = Math.min(1, i / (SR * 0.015), (n - i) / (SR * 0.03));
    out[i] = Math.sin(2 * Math.PI * f * (i / SR)) * env;
  }
  return out;
}

mkdirSync(outDir, { recursive: true });
writeWav("start.wav", sweep(880, 1320, 0.13));
writeWav("done.wav", [...tone(988, 0.11), ...tone(1319, 0.12)]);
writeWav("error.wav", [
  ...tone(220, 0.1),
  ...new Array(Math.floor(SR * 0.03)).fill(0),
  ...tone(196, 0.13),
]);
for (const name of ["start.wav", "done.wav", "error.wav"]) {
  if (!existsSync(join(outDir, name))) throw new Error(`missing ${name}`);
}
console.log("sound cues generated →", outDir);
