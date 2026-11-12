/** INV-1804: pure helpers for history batch export. */

/** Default export filename: `lumen-history-YYYYMMDD.<ext>`. */
export function historyExportFilename(ext: "md" | "jsonl"): string {
  const now = new Date();
  const stamp = [
    now.getFullYear(),
    String(now.getMonth() + 1).padStart(2, "0"),
    String(now.getDate()).padStart(2, "0"),
  ].join("");
  return `lumen-history-${stamp}.${ext}`;
}
