export type LatestRequestOutcome<T> =
  | { status: "current"; value: T }
  | { status: "stale" };

export class LatestRequestGate<T> {
  private revision = 0;
  private cycle: Promise<LatestRequestOutcome<T>> | null = null;
  private cycleRevision = 0;
  private rerunRequested = false;
  private nextRequest: (() => Promise<T>) | null = null;

  run(request: () => Promise<T>): Promise<LatestRequestOutcome<T>> {
    this.nextRequest = request;
    if (this.cycle && this.cycleRevision === this.revision) {
      this.rerunRequested = true;
      return this.cycle;
    }

    const revision = ++this.revision;
    this.cycleRevision = revision;
    let tracked!: Promise<LatestRequestOutcome<T>>;
    tracked = this.runCycle(revision).finally(() => {
      if (this.cycle === tracked) this.cycle = null;
    });
    this.cycle = tracked;
    return tracked;
  }

  private async runCycle(revision: number): Promise<LatestRequestOutcome<T>> {
    while (true) {
      this.rerunRequested = false;
      const request = this.nextRequest!;
      try {
        const value = await request();
        if (revision !== this.revision) return { status: "stale" };
        if (!this.rerunRequested) return { status: "current", value };
      } catch (error) {
        if (revision !== this.revision) return { status: "stale" };
        if (!this.rerunRequested) throw error;
      }
    }
  }

  cancelPending() {
    this.revision += 1;
    this.rerunRequested = false;
    this.nextRequest = null;
  }
}
