// Fetch the bundled IBM Plex woff2 into public/fonts/ at build time.
//
// The public repo forbids committing binaries, so the fonts are pulled on
// demand (pinned jsdelivr URLs) into a gitignored dir. This is best-effort:
// any failure just leaves the font missing, and styles.css falls back to the
// system font stack — the build never breaks offline.
//
// Runs automatically via the `predev` / `prebuild` npm hooks.
import { mkdir, writeFile, access } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, "..", "public", "fonts");
const CDN = "https://cdn.jsdelivr.net/npm";

// [package@version, path-within-package, local filename]
const FONTS = [
  ["@ibm/plex-sans@1.1.0", "fonts/complete/woff2/IBMPlexSans-Regular.woff2", "IBMPlexSans-Regular.woff2"],
  ["@ibm/plex-sans@1.1.0", "fonts/complete/woff2/IBMPlexSans-Medium.woff2", "IBMPlexSans-Medium.woff2"],
  ["@ibm/plex-sans@1.1.0", "fonts/complete/woff2/IBMPlexSans-SemiBold.woff2", "IBMPlexSans-SemiBold.woff2"],
  ["@ibm/plex-sans@1.1.0", "fonts/complete/woff2/IBMPlexSans-Bold.woff2", "IBMPlexSans-Bold.woff2"],
  ["@ibm/plex-mono@1.1.0", "fonts/complete/woff2/IBMPlexMono-Regular.woff2", "IBMPlexMono-Regular.woff2"],
  ["@ibm/plex-mono@1.1.0", "fonts/complete/woff2/IBMPlexMono-Medium.woff2", "IBMPlexMono-Medium.woff2"],
  ["@ibm/plex-mono@1.1.0", "fonts/complete/woff2/IBMPlexMono-SemiBold.woff2", "IBMPlexMono-SemiBold.woff2"],
  ["@ibm/plex-sans-sc@1.1.0", "fonts/complete/woff2/hinted/IBMPlexSansSC-Regular.woff2", "IBMPlexSansSC-Regular.woff2"],
  ["@ibm/plex-sans-sc@1.1.0", "fonts/complete/woff2/hinted/IBMPlexSansSC-SemiBold.woff2", "IBMPlexSansSC-SemiBold.woff2"],
];

async function exists(p) {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

async function main() {
  if (typeof fetch !== "function") {
    console.warn("[fonts] global fetch unavailable (need Node 18+); skipping — UI uses system fonts.");
    return;
  }
  await mkdir(OUT, { recursive: true });
  let got = 0;
  let skipped = 0;
  const failed = [];
  for (const [pkg, path, name] of FONTS) {
    const dest = join(OUT, name);
    if (await exists(dest)) {
      skipped++;
      continue;
    }
    const url = `${CDN}/${pkg}/${path}`;
    try {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const buf = Buffer.from(await res.arrayBuffer());
      if (buf.length < 1024) throw new Error(`suspiciously small (${buf.length}b)`);
      await writeFile(dest, buf);
      got++;
    } catch (e) {
      failed.push(`${name} (${e.message})`);
    }
  }
  console.log(`[fonts] IBM Plex: ${got} fetched, ${skipped} cached, ${failed.length} failed`);
  if (failed.length) {
    console.warn(`[fonts] missing → system-font fallback: ${failed.join(", ")}`);
  }
}

main().catch((e) => {
  // Never fail the build on a font issue.
  console.warn(`[fonts] skipped (${e.message}); UI uses system fonts.`);
});
