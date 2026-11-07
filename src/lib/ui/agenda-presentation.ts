import { listen } from "@tauri-apps/api/event";
import { expirePresentedRemoteDelete, getAgendaPresentation, observePresentedCreates } from "../ipc/client";
import type { AgendaPresentationSnapshot, CalendarAgendaItem, CalendarSettingsView, LocalCalendar, LocalTodo, PresentedCreate } from "../ipc/contracts";
import { withinCalendarDisplayRange } from "./calendar-display-range";
export const emptyPresentation = (): AgendaPresentationSnapshot => ({ version: -1, creates: [], remoteDeletes: [] });
export function deleteHidden(snapshot: AgendaPresentationSnapshot, resourceId: string): boolean {
  return snapshot.remoteDeletes.some(d => d.item.resourceId === resourceId && (d.phase === "undoable" || d.phase === "admitting"));
}
export function presentedCalendars(items: CalendarAgendaItem[], snapshot: AgendaPresentationSnapshot): CalendarAgendaItem[] {
  return items.filter(item => !deleteHidden(snapshot, item.resourceId)).map(item => {
    const deletion = snapshot.remoteDeletes.find(d => d.phase === "admitted" && d.item.resourceId === item.resourceId);
    if (!deletion?.remote || item.pendingRemote) return item;
    return { ...item, remoteEditEligible: false, pendingRemote: { operationId: deletion.remote.receipt.operationId, accountId: deletion.remote.receipt.accountId, kind: "delete", status: deletion.failure ? "retryNeeded" : "pending" } };
  });
}
export function createDestinationVisible(c: PresentedCreate, local: LocalCalendar[], remote: CalendarSettingsView[], hidden: Set<string>): boolean {
  if (c.phase === "failed" || c.failure === "localMetadata") return true;
  if (hidden.has(c.draft.calendarId)) return false;
  const calendar = c.source === "local" ? local.find(calendar => calendar.calendarId === c.draft.calendarId && !calendar.hidden) : remote.find(calendar => calendar.calendarId === c.draft.calendarId && calendar.visible);
  return !!calendar && withinCalendarDisplayRange(c.draft.startAtUtcMs, c.draft.dueAtUtcMs, calendar.displayPastDays ?? null, calendar.displayFutureDays ?? null);
}
export function hasReplacement(c: PresentedCreate, todos: LocalTodo[], calendars: CalendarAgendaItem[]): boolean {
  return c.localItem ? todos.some(item => item.todoId === c.localItem!.todoId && item.occurrenceKey === c.localItem!.occurrenceKey) : !!c.remote && calendars.some(item => item.resourceId === c.remote!.receipt.resourceId && item.calendarId === c.remote!.receipt.calendarId);
}
export function visibleCreates(snapshot: AgendaPresentationSnapshot, todos: LocalTodo[], calendars: CalendarAgendaItem[], owner: string): PresentedCreate[] {
  return snapshot.creates.filter(c => !(c.remote?.receipt.resourceId && deleteHidden(snapshot, c.remote.receipt.resourceId)) && (c.phase === "failed" || c.failure === "localMetadata" || !c.observed.includes(owner) && !hasReplacement(c, todos, calendars)));
}
/** One subscription and keyed deadlines per WebView; Rust owns every transition. */
export class AgendaPresentation {
  snapshot = emptyPresentation();
  private disposed = false;
  private stop?: () => void;
  private timers = new Map<string, ReturnType<typeof setTimeout>>();
  private observing = new Set<string>();
  constructor(private owner: "main" | "desktop", private changed: (snapshot: AgendaPresentationSnapshot) => void, private feedback: (action: "saveAction" | "deleteAction", failed: boolean) => void, private failure: () => void) {}
  async start() {
    try {
      const stop = await listen<AgendaPresentationSnapshot>("calendar-todo://agenda-presentation-changed", event => this.accept(event.payload));
      if (this.disposed) { stop(); return; } this.stop = stop;
      this.accept(await getAgendaPresentation(), true);
    } catch { if (!this.disposed) this.failure(); }
  }
  accept(next: AgendaPresentationSnapshot, initial = false) {
    if (this.disposed || next.version <= this.snapshot.version) return;
    const previous = this.snapshot;
    this.snapshot = next;
    this.arm(); this.changed(next);
    if (initial && previous.version < 0) return;
    for (const c of next.creates) {
      const old = previous.creates.find(old => old.presentationId === c.presentationId);
      if (c.owner === this.owner && c.feedback > (old?.feedback ?? 0)) this.feedback("saveAction", !!c.failure);
    }
    for (const d of next.remoteDeletes) {
      const old = previous.remoteDeletes.find(old => old.deletionId === d.deletionId);
      if (d.owner === this.owner && d.feedback > (old?.feedback ?? 0)) this.feedback("deleteAction", true);
    }
  }
  reconcile(todos: LocalTodo[], calendars: CalendarAgendaItem[], excludedIds: Set<string>) {
    const ids = this.snapshot.creates.filter(c => c.phase === "committed" && c.reconciliationReady && !c.failure && !c.observed.includes(this.owner) && !this.observing.has(c.presentationId) && (hasReplacement(c, todos, calendars) || excludedIds.has(c.presentationId))).map(c => ({ presentationId: c.presentationId, revision: c.revision }));
    if (!ids.length) return;
    for (const id of ids) this.observing.add(id.presentationId);
    void observePresentedCreates(ids).then(snapshot => this.accept(snapshot)).catch(() => { if (!this.disposed) this.failure(); }).finally(() => { for (const id of ids) this.observing.delete(id.presentationId); });
  }
  private arm() {
    const current = new Set(this.snapshot.remoteDeletes.filter(d => d.phase === "undoable").map(d => d.deletionId));
    for (const [id, timer] of this.timers) if (!current.has(id)) { clearTimeout(timer); this.timers.delete(id); }
    for (const d of this.snapshot.remoteDeletes) {
      if (d.phase !== "undoable" || this.timers.has(d.deletionId)) continue;
      this.schedule(d.deletionId, Math.max(0, d.expiresAtMs - Date.now()));
    }
  }
  private schedule(id: string, delayMs: number) {
    this.timers.set(id, setTimeout(() => {
      this.timers.delete(id);
      void expirePresentedRemoteDelete(id).then(next => {
        this.accept(next);
        if (!this.disposed && next.remainingMs !== null && this.snapshot.remoteDeletes.some(d => d.deletionId === id && d.phase === "undoable")) {
          const timer = this.timers.get(id); if (timer) clearTimeout(timer);
          this.schedule(id, next.remainingMs);
        }
      }).catch(() => { if (!this.disposed) this.failure(); });
    }, delayMs + 10));
  }
  dispose() { this.disposed = true; this.stop?.(); for (const timer of this.timers.values()) clearTimeout(timer); this.timers.clear(); }
}
