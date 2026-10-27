export interface UndoEntry<T> { token: number; payload: T; expiresAtMs: number }

/** Each presentation-only deletion owns its own deadline. Disposal never admits work. */
export class DeletionUndoQueue<T> {
  private entries = new Map<number, { entry: UndoEntry<T>; timer: ReturnType<typeof setTimeout> }>();
  private sequence = 0;
  constructor(private changed: (entries: UndoEntry<T>[]) => void, private expired: (payload: T) => void,
    private clock = { now: () => Date.now(), set: (callback: () => void, ms: number) => setTimeout(callback, ms), clear: (timer: ReturnType<typeof setTimeout>) => clearTimeout(timer) }) {}
  private publish() { this.changed([...this.entries.values()].map(({ entry }) => entry)); }
  enqueue(payload: T) {
    const token = ++this.sequence;
    const entry = { token, payload, expiresAtMs: this.clock.now() + 5_000 };
    const timer = this.clock.set(() => {
      if (!this.entries.delete(token)) return;
      this.publish();
      this.expired(payload);
    }, 5_000);
    this.entries.set(token, { entry, timer });
    this.publish();
    return token;
  }
  undo(token: number): T | undefined {
    const pending = this.entries.get(token);
    if (!pending) return;
    this.clock.clear(pending.timer);
    this.entries.delete(token);
    this.publish();
    return pending.entry.payload;
  }
  dispose() {
    for (const { timer } of this.entries.values()) this.clock.clear(timer);
    this.entries.clear();
    this.publish();
  }
}
