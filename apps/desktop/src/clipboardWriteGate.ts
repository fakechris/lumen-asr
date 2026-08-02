export type ClipboardWriteOutcome = "busy" | "copied" | "empty" | "stale";

/** Keeps clipboard feedback aligned with the latest user action. */
export class ClipboardWriteGate {
  private revision = 0;
  private pending = false;

  isPending() {
    return this.pending;
  }

  async write(
    text: string,
    writeText: (value: string) => Promise<void>,
  ): Promise<ClipboardWriteOutcome> {
    const normalized = text.trim();
    if (!normalized) return "empty";
    if (this.pending) return "busy";

    const revision = ++this.revision;
    this.pending = true;
    try {
      // Invoke immediately inside the click handler's user-activation scope.
      await writeText(normalized);
    } catch (error) {
      if (revision !== this.revision) return "stale";
      throw error;
    } finally {
      this.pending = false;
    }
    return revision === this.revision ? "copied" : "stale";
  }

  cancelPending() {
    this.revision += 1;
  }
}
