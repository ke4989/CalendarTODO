export const DETAIL_AUTOSAVE_DELAY_MS = 1_500;

/** One stable editor identity. Leaving a selection detaches its timer, never its commits. */
export class DetailAutosave<T> {
  private session = 0;
  private revision = 0;
  private admitted = 0;
  private scheduled = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private running = false;
  private queued: { session: number; revision: number; snapshot: T; explicit: boolean } | null = null;
  private latest: T | null = null;
  private frozen = false;
  private alive = true;
  constructor(private readonly capture: () => T | null,
    private readonly commit: (snapshot: T, explicit: boolean, current: () => boolean) => Promise<boolean>,
    private readonly freeze: (frozen: boolean) => void) {}
  get snapshot() { return this.latest; }
  get saving() { return this.frozen; }
  get dirty() { return this.revision > this.admitted; }
  get pending() { return this.running || this.queued !== null; }
  reset() {
    this.session++; this.revision = 0; this.admitted = 0; this.scheduled = 0; this.queued = null; this.latest = null;
    this.clearTimer();
    this.frozen = false; this.freeze(false);
  }
  private clearTimer() {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }
  changed() {
    if (this.frozen || !this.alive) return;
    this.revision++;
    this.latest = this.capture();
    this.clearTimer();
    this.timer = setTimeout(() => { this.timer = undefined; this.enqueue(false); }, DETAIL_AUTOSAVE_DELAY_MS);
  }
  /** False means invalid dirty input must remain reachable in the visible editor. */
  leave() {
    this.clearTimer();
    if (this.dirty && this.latest === null) return false;
    if (!this.frozen) this.enqueue(false);
    return true;
  }
  save() {
    if (this.frozen || !this.alive) return;
    this.clearTimer();
    const snapshot = this.capture();
    if (snapshot === null) return;
    this.latest = snapshot;
    this.revision++;
    this.frozen = true; this.freeze(true);
    this.enqueue(true, snapshot);
  }
  private enqueue(explicit: boolean, snapshot = this.latest) {
    if (!this.alive || snapshot === null || !explicit && (this.revision <= this.admitted || this.revision <= this.scheduled)) return;
    this.queued = { session: this.session, revision: this.revision, snapshot, explicit };
    this.scheduled = this.revision;
    void this.drain();
  }
  private async drain() {
    if (this.running) return;
    this.running = true;
    try {
      while (this.queued) {
        const job = this.queued;
        this.queued = null;
        const current = () => this.alive && this.session === job.session && this.revision === job.revision;
        try {
          const success = await this.commit(job.snapshot, job.explicit, current);
          if (this.session === job.session) {
            if (success) { this.admitted = Math.max(this.admitted, job.revision); if (this.revision === job.revision) this.latest = null; }
            else if (!this.queued) this.scheduled = this.admitted;
          }
        } finally {
          if (job.explicit && this.session === job.session) { this.frozen = false; if (this.alive) this.freeze(false); }
        }
      }
    } finally { this.running = false; }
  }
  dispose() { this.leave(); this.alive = false; }
}
