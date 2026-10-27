export class CompletionEventGate<T extends { completed: boolean }> {
  private readonly pending = new Map<string, { expectedCompleted: boolean; queued: T | null }>();

  begin(key: string, expectedCompleted: boolean): void {
    this.pending.set(key, { expectedCompleted, queued: null });
  }

  receive(key: string, change: T): T | null {
    const pending = this.pending.get(key);
    if (!pending) return change;
    pending.queued = change.completed === pending.expectedCompleted ? null : change;
    return null;
  }

  finish(key: string): T | null {
    const queued = this.pending.get(key)?.queued ?? null;
    this.pending.delete(key);
    return queued;
  }
}
