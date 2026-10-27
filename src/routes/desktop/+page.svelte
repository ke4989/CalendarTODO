<script lang="ts">
  import { onMount, onDestroy, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import ActionToast from "$lib/components/ActionToast.svelte";
  import DateTimeInput from "$lib/components/DateTimeInput.svelte";
  import EventContextMenu from "$lib/components/EventContextMenu.svelte";
  import TagInput from "$lib/components/TagInput.svelte";
  import ReorderMenu from "$lib/components/ReorderMenu.svelte";
  import UndoToast from "$lib/components/UndoToast.svelte";
  import ToastViewport from "$lib/components/ToastViewport.svelte";
  import { DeletionUndoQueue, type UndoEntry } from "$lib/ui/deletion-undo";
  import RecurrenceDeleteScope from "$lib/components/RecurrenceDeleteScope.svelte";
  import RecurrenceSelector from "$lib/components/RecurrenceSelector.svelte";
  import { copyRecurrenceOptions, defaultRecurrenceOptions, recurrenceChanged, recurrenceInput, recurrenceSummary } from "$lib/ui/recurrence";
  import { controlText, resolveLanguage, setActiveLanguage, translate, withoutTerminalPunctuation, type MessageKey } from "$lib/i18n";
  import { fitOneLine } from "$lib/ui/fit-one-line";
  import { collectTagSuggestions } from "$lib/ui/tag-input";
  import { startReorderAutoScroll, type DragPoint } from "$lib/ui/reorder-auto-scroll";
  import { reorderAtPointer } from "$lib/ui/reorder-preview";
  import { reorderKeys, type ReorderAction } from "$lib/ui/reorder-menu";
  import { createBackdropClickGuard, dialogFocus } from "$lib/ui/dialog-focus";
  import { animateCompletionChange, reducedCompletionMotion, type CompletionDirection } from "$lib/ui/completion-motion";
  import { CompletionEventGate } from "$lib/ui/completion-events";
  import { withinCalendarDisplayRange } from "$lib/ui/calendar-display-range";
  import { applyRemoteDraft, draftRequest, initialDraft, selectedBase, retryBackgroundRemoteWrite, startBackgroundRemoteWrite, utcInput, projectionCandidate, preparationHint, type PreparationState, type RemoteDraft } from "$lib/ui/remote-crud-preparation";
  import { applyTheme } from "$lib/theme/apply";
  import { remoteCalendarBlockedReason } from "$lib/ui/remote-calendar-availability";
  import { confirmedRemoteEvent, listenConfirmedEvent, prepareRemoteCrud, listCalendarSettings, listAttachmentStates } from "$lib/ipc/client";
  import { deleteLocalTodo, deleteLocalTodoOccurrence, desktopCollapsed, getAppSettings, getDesktopItemOrder, listCalendarAgendaItems, listLocalCalendars, listLocalTodos, recordUiError, restoreRecentlyDeletedTodo, restoreRecentlyDeletedTodoOccurrence, saveLocalTodo, setCalendarCompleted, setDesktopCollapsed, setDesktopItemOrder, setDesktopPinned, setLocalTodoCompleted, startDesktopDrag, updateCalendarLocalMetadata } from "$lib/ipc/client";
  import type { AppSettings, AttachmentView, CalendarSettingsView, CalendarAgendaItem, CalendarCompletionChanged, LocalCalendar, LocalTodo, LocalTodoCompletionChanged, Priority, RecurrenceFrequency, RecurrenceOptions } from "$lib/ipc/contracts";

  type Entry = { key: string; kind: "todo"; item: LocalTodo } | { key: string; kind: "calendar"; item: CalendarAgendaItem };
  const todoEntryKey = (item: LocalTodo) => `todo:${item.todoId}:${item.occurrenceKey}`;
  const calendarEntryKey = (item: CalendarAgendaItem) => `calendar:${item.seriesId}:${item.recurrenceKey}`;

  let todos = $state<LocalTodo[]>([]);
  let calendarItems = $state<CalendarAgendaItem[]>([]);
  const tagSuggestions = $derived(collectTagSuggestions([...todos, ...calendarItems]));
  let settings = $state<AppSettings | null>(null);
  const language = $derived(resolveLanguage(settings?.language ?? "system"));
  const text = (key: MessageKey, values: Record<string, string | number> = {}) => Object.entries(values).reduce((message, [name, value]) => message.replaceAll(`{${name}}`, String(value)), translate(language, key) as string);
  $effect(() => { setActiveLanguage(language); document.documentElement.lang = language; });
  $effect(() => { applyTheme(settings?.themePalette ?? "quietDaybook", settings?.colorMode ?? "light", false); });
  let localCalendars = $state<LocalCalendar[]>([]); let calendarId = $state("");
  const visibleLocalCalendars = $derived(localCalendars.filter((calendar) => !calendar.hidden));
  let modal = $state<"create" | "todo" | "calendar" | null>(null);
  const modalBackdropClick = createBackdropClickGuard();
  let viewingCalendar = $state<CalendarAgendaItem | null>(null);
  let calendarSettings = $state<CalendarSettingsView[]>([]);
  let attachmentStates = $state<Map<string, AttachmentView>>(new Map());
  const remoteDestinations = $derived(calendarSettings.filter((calendar) => calendar.visible));
  function preferredCreateCalendarId(): string {
    const preferred = settings?.defaultCalendarId;
    if (preferred && (visibleLocalCalendars.some((calendar) => calendar.calendarId === preferred) || remoteDestinations.some((calendar) => calendar.calendarId === preferred && !remoteBlockedReason(calendar)))) return preferred;
    return visibleLocalCalendars[0]?.calendarId ?? remoteDestinations.find((calendar) => !remoteBlockedReason(calendar))?.calendarId ?? "";
  }
  const remoteCreateCalendar = $derived(modal === "create" ? calendarSettings.find((calendar) => calendar.calendarId === calendarId) : undefined);
  const remoteActiveCalendar = $derived(modal === "calendar" ? calendarSettings.find((calendar) => calendar.calendarId === viewingCalendar?.calendarId) : remoteCreateCalendar);
  const remoteReason = $derived(modal === "calendar" || remoteCreateCalendar ? remoteBlockedReason(remoteActiveCalendar, modal === "calendar" ? viewingCalendar ?? undefined : undefined) : null);
  let remoteView = $state<PreparationState | null>(null);
  let actionToast = $state<{ id: number; message: string; variant: "info" | "success" | "error" } | null>(null);
  let actionToastId = 0;
  let actionToastTimer: ReturnType<typeof setTimeout> | undefined;
  let desktopScrollbarVisible = $state(false);
  let desktopScrollbarTimer: ReturnType<typeof setTimeout> | undefined;
  const backgroundStops = new Set<() => void>();
  function resetRemoteEditor() { remoteView = null; }
  function showActionToast(message: string, variant: "info" | "success" | "error") {
    actionToast = { id: ++actionToastId, message, variant };
    if (actionToastTimer) clearTimeout(actionToastTimer);
    actionToastTimer = setTimeout(() => { actionToast = null; actionToastTimer = undefined; }, 5_000);
  }
  function diagnosticFrom(error: unknown): string {
    if (typeof error === "object" && error !== null && "message" in error && typeof error.message === "string") return error.message;
    return typeof error === "string" ? error : "unknown error";
  }
  async function reportActionFailure(action: "saveAction" | "deleteAction", code: string, diagnostic: string, sync = false) {
    try { await recordUiError(`${action}-event`, code, diagnostic); } catch { /* The original failure remains visible. */ }
    showActionToast(sync ? translate(language, "syncFailedSeeLog") : text("actionFailedSeeLog", { action: translate(language, action) }), "error");
  }
  function showActionFailure(action: "saveAction" | "deleteAction", reason: string) { void reportActionFailure(action, "localSave", reason); }
  onDestroy(() => { backgroundStops.forEach((stop) => stop()); if (actionToastTimer) clearTimeout(actionToastTimer); if (desktopScrollbarTimer) clearTimeout(desktopScrollbarTimer); });
  function revealDesktopScrollbar() {
    desktopScrollbarVisible = true;
    if (desktopScrollbarTimer) clearTimeout(desktopScrollbarTimer);
    desktopScrollbarTimer = setTimeout(() => { desktopScrollbarVisible = false; desktopScrollbarTimer = undefined; }, 3_000);
  }
  $effect(() => { if (!modal) resetRemoteEditor(); });
  function remoteBlockedReason(calendar: CalendarSettingsView | undefined, item?: CalendarAgendaItem) { return remoteCalendarBlockedReason(calendar, attachmentStates, item); }
  function editorDraft(item?: CalendarAgendaItem): RemoteDraft {
    const startAt = fromInput(start), endAt = fromInput(due);
    const draft: RemoteDraft = { ...initialDraft(item), title, selectTitle: !item || title !== item.title, description: notes, descriptionAction: !item || notes !== (item.notes ?? "") ? notes.trim() ? "set" : "clear" : "retain", location, locationAction: !item || location !== (item.location ?? "") ? location.trim() ? "set" : "clear" : "retain", selectTime: !item || startAt !== item.startAtUtcMs || endAt !== item.endAtUtcMs, start: utcInput(startAt), end: utcInput(endAt) };
    if (item) draft.base = selectedBase(item, draft);
    return draft;
  }
  function remoteWriteContext(calendar: CalendarSettingsView, item?: CalendarAgendaItem) {
    return { accountId: calendar.accountId, calendarId: calendar.calendarId, resourceId: item?.resourceId, candidate: !remoteBlockedReason(calendar, item) && (!item || projectionCandidate(item)) };
  }
  function validateRemoteWrite(kind: "createUtcV2" | "updateEventV3" | "delete", calendar: CalendarSettingsView, item: CalendarAgendaItem | undefined, draft: RemoteDraft): boolean {
    const built = draftRequest(kind, remoteWriteContext(calendar, item), draft, "validation");
    if (built.request && built.request.action !== "status" && built.request.action !== "resolveConflict") return true;
    remoteView = { phase: "rejected", message: "", errors: built.errors, receipt: null, operations: [], busy: false };
    return false;
  }
  async function beginRemoteWrite(kind: "createUtcV2" | "updateEventV3" | "delete", calendar: CalendarSettingsView, item: CalendarAgendaItem | undefined, draft: RemoteDraft, action: "saveAction" | "deleteAction", settled?: (succeeded: boolean) => void) {
    let stop = () => {};
    let completed = false;
    let terminalFailed = false;
    const result = await startBackgroundRemoteWrite(kind, remoteWriteContext(calendar, item), draft, confirmedRemoteEvent, listenConfirmedEvent, {
      applied: () => { completed = true; backgroundStops.delete(stop); void refresh().finally(() => settled?.(true)); },
      failed: (diagnostic) => { completed = true; terminalFailed = true; backgroundStops.delete(stop); void reportActionFailure(action, "remoteBackgroundSync", diagnostic, true); void refresh().finally(() => settled?.(false)); },
    });
    if (!result.started) {
      void reportActionFailure(action, "remoteAdmission", result.message);
      return result;
    }
    stop = result.stop;
    if (!completed) backgroundStops.add(stop);
    void refresh();
    return { ...result, terminalFailed };
  }
  async function keepPendingRemote(item: CalendarAgendaItem) {
    const pending = item.pendingRemote;
    if (!pending || pending.status !== "conflict" || operationBusy) return;
    operationBusy=true;
    try { await prepareRemoteCrud({action:"resolveConflict",input:{operationId:pending.operationId,kind:pending.kind,choice:"keepRemote"}}); await refresh(); }
    catch (failure) { void reportActionFailure("deleteAction","remoteConflictResolution",diagnosticFrom(failure)); }
    finally { operationBusy=false; }
  }
  async function retryPendingRemote(item: CalendarAgendaItem) {
    const pendingRemote = item.pendingRemote;
    if (!pendingRemote || busy) return;
    operationBusy = true;
    let stop = () => {};
    let completed = false;
    const result = await retryBackgroundRemoteWrite(
      { kind: pendingRemote.kind, operationId: pendingRemote.operationId, accountId: pendingRemote.accountId, calendarId: item.calendarId, resourceId: item.resourceId },
      confirmedRemoteEvent,
      listenConfirmedEvent,
      {
        applied: () => { completed = true; backgroundStops.delete(stop); operationBusy = false; void refresh(); },
        failed: (diagnostic) => { completed = true; backgroundStops.delete(stop); operationBusy = false; void reportActionFailure(pendingRemote.kind === "delete" ? "deleteAction" : "saveAction", "remoteBackgroundSync", diagnostic, true); void refresh(); },
      },
    );
    if (!result.started) {
      operationBusy = false;
      void reportActionFailure("saveAction", "remoteRetry", "exact operation retry was not accepted", true);
      return;
    }
    stop = result.stop;
    if (!completed) backgroundStops.add(stop);
    await refresh();
  }
  async function commitRemoteDeletion(item: CalendarAgendaItem, calendar: CalendarSettingsView) {
    const draft = { ...initialDraft(item), confirmed: true };
    const result = await beginRemoteWrite("delete", calendar, item, draft, "deleteAction", () => finishStagedRemoteDeletion(item));
    finishStagedRemoteDeletion(item);
  }
  async function removeRemoteEvent(selected?: CalendarAgendaItem) {
    const item = selected ?? viewingCalendar;
    if (!item || operationBusy || item.pendingRemote || savingEditors.has(`calendar:${item.resourceId}:${item.recurrenceKey}`)) return;
    const calendar = calendarSettings.find((calendar) => calendar.calendarId === item.calendarId);
    const reason = remoteBlockedReason(calendar, item);
    if (reason || !calendar) { error = reason ?? translate(language, "calendarUnavailableRefresh"); return; }
    const draft = { ...initialDraft(item), confirmed: true };
    if (!validateRemoteWrite("delete", calendar, item, draft)) return;
    stageRemoteDeletion(item, calendar);
    if (modal === "calendar" && viewingCalendar?.resourceId === item.resourceId) { modal = null; viewingCalendar = null; }
  }
  let error = $state("");
  let order = $state<string[]>([]);
  let title = $state("");
  let start = $state(""); let due = $state("");
  let dueEdited = $state(false);
  let priority = $state<Priority>("normal"); let notes = $state(""); let location = $state("");
  let important = $state(false); let tags = $state<string[]>([]);
  let recurrence = $state<"none" | RecurrenceFrequency>("none"); let interval = $state(1);
  let recurrenceOptions = $state<RecurrenceOptions>(defaultRecurrenceOptions());
  let editingTodo = $state<LocalTodo | null>(null);
  let dragging = $state<string | null>(null);
  let dragOverKey = $state<string | null>(null);
  let previewOrder = $state<string[] | null>(null);
  let dragPointer: number | null = null;
  let dragHandle: HTMLElement | null = null;
  let reorderMenu = $state<{ key: string; x: number; y: number; anchor: HTMLButtonElement } | null>(null);
  let deleteMenu = $state<{ entry: Entry; x: number; y: number; opener: HTMLElement | null } | null>(null);
  let recurrenceDeleteTodo = $state<LocalTodo | null>(null);
  let reorderAnnouncement = $state("");
  let dragScroll: ReturnType<typeof startReorderAutoScroll> | null = null;
  let dragSession = 0;
  let dragPreviewRevision = 0;
  let titleInput = $state<HTMLInputElement>();
  let collapsed = $state(false);
  let folding = $state(false);
  let pinned = $state(false);
  let pinSaving = $state(false);
  let pinRevision = 0;
  let operationBusy = $state(false);
  let savingEditors = $state<Map<string, ReturnType<typeof captureEditor>>>(new Map());
  const failedEditors = new Map<string, ReturnType<typeof captureEditor>>();
  const busy = $derived(operationBusy || savingEditors.has(editorIdentity()));
  function editorIdentity() { return modal === "calendar" && viewingCalendar ? `calendar:${viewingCalendar.resourceId}:${viewingCalendar.recurrenceKey}` : modal === "todo" && editingTodo ? `todo:${editingTodo.todoId}:${editingTodo.occurrenceKey}` : "create"; }
  function captureEditor() { return { title, notes, location, start, due, calendarId, priority, important, tags: [...tags], recurrence, interval, recurrenceOptions: copyRecurrenceOptions(recurrenceOptions), editingTodo, viewingCalendar }; }
  function restoreSavingEditor() {
    const saved = savingEditors.get(editorIdentity()) ?? failedEditors.get(editorIdentity());
    if (!saved) return;
    ({ title, notes, location, start, due, calendarId, priority, important, tags, recurrence, interval, recurrenceOptions, editingTodo, viewingCalendar } = saved);
  }
  let initialLoading = $state(true);
  let completionChanges = $state<Map<string, CompletionDirection>>(new Map());
  const localCompletionEvents = new CompletionEventGate<LocalTodoCompletionChanged>();
  const calendarCompletionEvents = new CompletionEventGate<CalendarCompletionChanged>();
  type UndoDeletion =
    | { kind: "local"; id: string; title: string }
    | { kind: "localOccurrence"; id: string; occurrenceKey: string; title: string }
    | { kind: "remote"; id: string; title: string; item: CalendarAgendaItem; calendar: CalendarSettingsView };
  let undoItems = $state<UndoEntry<UndoDeletion>[]>([]);
  const undoQueue = new DeletionUndoQueue<UndoDeletion>((entries) => undoItems = entries, (item) => {
    if (item.kind === "remote") void commitRemoteDeletion(item.item, item.calendar);
  });
  onDestroy(() => undoQueue.dispose());
  let stagedRemoteDeleteKeys = $state<Set<string>>(new Set());
  const displayedTodos = $derived(todos.filter((item) => {
    const calendar = localCalendars.find((candidate) => candidate.calendarId === item.calendarId);
    return withinCalendarDisplayRange(item.startAtUtcMs, item.dueAtUtcMs, calendar?.displayPastDays ?? null, calendar?.displayFutureDays ?? null);
  }));
  const pending = $derived(displayedTodos.filter((item) => !item.completed).length + calendarItems.filter((item) => item.status === "ready" && !item.completed).length);
  const entries = $derived.by(() => {
    const all: Entry[] = [...calendarItems.map((item): Entry => ({ key: calendarEntryKey(item), kind: "calendar", item })), ...displayedTodos.map((item): Entry => ({ key: todoEntryKey(item), kind: "todo", item }))];
    const rank = new Map((previewOrder ?? order).map((key, index) => [key, index]));
    return all.sort((a, b) => Number(a.item.completed) - Number(b.item.completed) || (a.item.completed && b.item.completed ? (a.item.completedAtMs ?? 0) - (b.item.completedAtMs ?? 0) : 0) || Number(b.item.important) - Number(a.item.important) || (rank.has(a.key) || rank.has(b.key) ? (rank.get(a.key) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.key) ?? Number.MAX_SAFE_INTEGER) : (desktopTimeValue(a) ?? Number.MAX_SAFE_INTEGER) - (desktopTimeValue(b) ?? Number.MAX_SAFE_INTEGER)) || a.key.localeCompare(b.key));
  });
  function desktopTimeValue(entry: Entry): number | null { return entry.kind === "calendar" ? entry.item.startAtUtcMs : entry.item.startAtUtcMs ?? entry.item.dueAtUtcMs; }
  function desktopTime(entry: Entry): string | null {
    const start = desktopTimeValue(entry);
    if (start === null) return null;
    if (entry.kind === "calendar" && entry.item.allDay) return translate(language, "allDay");
    return new Intl.DateTimeFormat(language, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }).format(start);
  }

  async function refresh() {
    const currentPinRevision = pinRevision;
    try {
      const loaded = await Promise.all([listLocalTodos(), listCalendarAgendaItems(), getAppSettings(), getDesktopItemOrder(), listLocalCalendars(), listCalendarSettings(), listAttachmentStates()]);
      [todos, calendarItems, settings, order, localCalendars, calendarSettings] = loaded;
      if (currentPinRevision === pinRevision && !pinSaving) pinned = loaded[2].desktopPinned;
      calendarItems = calendarItems.filter((item) => !stagedRemoteDeleteKeys.has(remoteDeleteKey(item)));
      if (viewingCalendar) {
        const selected = viewingCalendar;
        viewingCalendar = calendarItems.find((item) => item.resourceId === selected.resourceId && item.seriesId === selected.seriesId && item.recurrenceKey === selected.recurrenceKey) ?? null;
      }
      attachmentStates = new Map(loaded[6].map((state) => [state.accountId, state]));
      if (!calendarId || localCalendars.find((calendar) => calendar.calendarId === calendarId)?.hidden) calendarId = preferredCreateCalendarId();
      error = "";
    } catch { error = translate(language, "listRefreshFailed"); }
    finally { initialLoading = false; }
  }

  onMount(() => {
    let disposed = false;
    const stops: (() => void)[] = [];
    void listen<LocalTodoCompletionChanged | null>("calendar-todo://local-todo-changed", (event) => {
      const change = event.payload;
      if (change?.item) {
        const key = `todo:${change.todoId}:${change.previousOccurrenceKey}`;
        const accepted = localCompletionEvents.receive(key, change);
        if (accepted) void applyLocalCompletionChange(accepted);
      } else void refresh();
    }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    void listen<CalendarCompletionChanged | null>("calendar-todo://agenda-data-changed", (event) => {
      const change = event.payload;
      if (change?.item) {
        const key = `calendar:${change.seriesId}:${change.previousRecurrenceKey}`;
        const accepted = calendarCompletionEvents.receive(key, change);
        if (accepted) void applyCalendarCompletionChange(accepted);
      } else void refresh();
    }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    for (const name of ["calendar-todo://agenda-refresh", "calendar-todo://settings-changed", "calendar-todo://source-mode-changed"]) void listen(name, () => { void refresh(); }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    void listen<AttachmentView>("calendar-todo://attachment-state", (event) => { attachmentStates = new Map(attachmentStates).set(event.payload.accountId, event.payload); void refresh(); }).then((stop) => { if (disposed) stop(); else stops.push(stop); });
    void refresh();
    void desktopCollapsed().then((value) => { collapsed = value; });
    return () => { cancelReorder(); disposed = true; stops.forEach((stop) => stop()); };
  });

  const toInput = (value: number | null, seconds = false) => value === null ? "" : new Date(value - new Date(value).getTimezoneOffset() * 60_000).toISOString().slice(0, seconds ? 19 : 16);
  const fromInput = (value: string) => value ? new Date(value).getTime() : null;
  async function openCreate() {
    if (busy || remoteView?.busy) return;
    resetRemoteEditor();
    if (collapsed) void fold();
    editingTodo = null; viewingCalendar = null; modal = "create";
    const initialStart = Date.now(); title = ""; start = toInput(initialStart); due = toInput(initialStart + 3_600_000); dueEdited = false; calendarId = preferredCreateCalendarId(); priority = "normal"; notes = ""; location = ""; important = false; tags = []; recurrence = "none"; interval = 1; recurrenceOptions = defaultRecurrenceOptions(); error = "";
    await tick();
    titleInput?.focus();
  }
  function updateStart(value: string) {
    start = value;
    const startAt = fromInput(value), dueAt = fromInput(due);
    if (startAt !== null && (!dueEdited || dueAt === null || startAt >= dueAt)) due = toInput(startAt + 3_600_000);
  }
  function openEntry(entry: Entry) {
    if (operationBusy || modal === "create" && busy) return;
    const identity = entry.kind === "calendar" ? `calendar:${entry.item.resourceId}:${entry.item.recurrenceKey}` : `todo:${entry.item.todoId}:${entry.item.occurrenceKey}`;
    if (!failedEditors.has(identity) && !savingEditors.has(identity) && failedEditors.size + savingEditors.size >= 32) { void reportActionFailure("saveAction", "retainedDraftLimit", "resolve retained failed event edits before opening another editor"); return; }
    resetRemoteEditor();
    if (entry.kind === "calendar") {
      const item = entry.item; editingTodo = null; viewingCalendar = item; modal = "calendar";
      title = item.title; notes = item.notes ?? ""; location = item.location ?? ""; start = toInput(item.startAtUtcMs, true); due = toInput(item.endAtUtcMs, true); dueEdited = true; calendarId = item.calendarId; important = item.important; tags = item.tags.map((tag) => tag.name); recurrence = item.recurrence?.frequency ?? "none"; interval = item.recurrence?.interval ?? 1; recurrenceOptions = copyRecurrenceOptions(item.recurrence?.options ?? defaultRecurrenceOptions()); error = "";
      restoreSavingEditor();
      return;
    }
    const item = entry.item;
    editingTodo = item; viewingCalendar = null; modal = "todo";
    title = item.title; start = toInput(item.startAtUtcMs); due = toInput(item.dueAtUtcMs); dueEdited = true; calendarId = item.calendarId; priority = item.priority;
    notes = item.notes ?? ""; location = item.location ?? ""; important = item.important;
    tags = item.tags.map((tag) => tag.name); recurrence = item.recurrence?.frequency ?? "none"; interval = item.recurrence?.interval ?? 1; recurrenceOptions = copyRecurrenceOptions(item.recurrence?.options ?? defaultRecurrenceOptions()); error = "";
    restoreSavingEditor();
  }
  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!title.trim() || busy || remoteView?.busy) return;
    const identity = editorIdentity();
    failedEditors.delete(identity);
    updateStart(start);
    const snapshot = captureEditor();
    savingEditors = new Map(savingEditors).set(identity, snapshot);
    const current = () => editorIdentity() === identity;
    let accepted = false;
    try {
    if (modal === "calendar" || remoteCreateCalendar) {
      const item = modal === "calendar" ? viewingCalendar ?? undefined : undefined;
      const calendar = item ? calendarSettings.find((calendar) => calendar.calendarId === item.calendarId) : remoteCreateCalendar;
      if (item) {
        const draft = editorDraft(item);
        const remoteChanged = draft.selectTitle || draft.descriptionAction !== "retain" || draft.locationAction !== "retain" || draft.selectTime;
        const pinAtSubmit = important;
        const pinChanged = pinAtSubmit !== item.important;
        const tagNames = [...tags];
        if (tagNames.length > 32 || tagNames.some((name) => Array.from(name).length > 64)) { error = translate(language, "tagLimitExceededNoPeriod"); return; }
        const localRecurrence = recurrenceInput(recurrence, interval, recurrenceOptions);
        const metadataChanged = tagNames.join("\u0000") !== item.tags.map((tag) => tag.name).join("\u0000") || recurrenceChanged(localRecurrence, item.recurrence);
        if (!remoteChanged && !pinChanged && !metadataChanged) { modal = null; viewingCalendar = null; return; }
        let remoteRequest: ReturnType<typeof beginRemoteWrite> | null = null;
        if (remoteChanged) {
          const reason = remoteBlockedReason(calendar, item);
          if (reason || !calendar) { error = reason ?? translate(language, "calendarUnavailableRefresh"); return; }
          if (!validateRemoteWrite("updateEventV3", calendar, item, draft)) return;
          if (pinChanged || metadataChanged) draft.metadata = { resourceId: item.resourceId, tagNames, recurrence: localRecurrence, important: pinChanged ? pinAtSubmit : undefined };
          const optimistic = { ...applyRemoteDraft(item, draft), pendingRemote: { operationId: `optimistic:${crypto.randomUUID()}`, accountId: calendar.accountId, kind: "updateEventV3" as const, status: "pending" as const } };
          calendarItems = calendarItems.map((current) => current.seriesId === item.seriesId && current.recurrenceKey === item.recurrenceKey ? optimistic : current);
          remoteRequest = beginRemoteWrite("updateEventV3", calendar, item, draft, "saveAction");
        }
        if (remoteRequest) {
          const result = await remoteRequest;
          if (!result.started) { calendarItems = calendarItems.map((current) => current.seriesId === item.seriesId && current.recurrenceKey === item.recurrenceKey ? item : current); return; }
          accepted = true;
          if (current()) { modal = null; viewingCalendar = null; }
          if (result.terminalFailed) return;
          showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
          return;
        }
        if (await persistMetadata()) {
          accepted = true;
          if (current()) { modal = null; viewingCalendar = null; }
          showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
        }
        async function persistMetadata() {
          try {
            await updateCalendarLocalMetadata({ resourceId: item!.resourceId, tagNames, recurrence: localRecurrence, important: pinChanged ? pinAtSubmit : undefined });
            await refresh();
            return true;
          } catch (failure) { void reportActionFailure("saveAction", "localMetadata", diagnosticFrom(failure)); return false; }
        }
      } else {
        const reason = remoteBlockedReason(calendar);
        if (reason || !calendar) { error = reason ?? translate(language, "calendarUnavailableRefresh"); return; }
        const tagNames = tags;
        const pinAtSubmit = important;
        if (tagNames.length > 32 || tagNames.some((name) => Array.from(name).length > 64)) { error = translate(language, "tagLimitExceededNoPeriod"); return; }
        const localRecurrence = recurrenceInput(recurrence, interval, recurrenceOptions);
        const draft = editorDraft();
        if (!validateRemoteWrite("createUtcV2", calendar, undefined, draft)) return;
        let createdResourceId = "";
        let earlySettlement: boolean | undefined;
        const finishCreate = (succeeded: boolean) => {
          if (!createdResourceId) { earlySettlement = succeeded; return; }
          if (!succeeded) return;
          void (async () => {
            try {
              if (tagNames.length || localRecurrence || pinAtSubmit) {
                await updateCalendarLocalMetadata({ resourceId: createdResourceId, tagNames, recurrence: localRecurrence, important: pinAtSubmit ? true : undefined });
                await refresh();
              }
            } catch (failure) { void reportActionFailure("saveAction", "localMetadata", diagnosticFrom(failure)); }
          })();
        };
        const request = beginRemoteWrite("createUtcV2", calendar, undefined, draft, "saveAction", finishCreate);
        const provisionalId = `optimistic:${crypto.randomUUID()}`;
        const provisional: CalendarAgendaItem = {
          seriesId: provisionalId, resourceId: provisionalId, recurrenceKey: "SINGLETON", calendarId: calendar.calendarId,
          calendarName: calendar.displayName, calendarColor: calendar.effectiveColor,
          title: title.trim(), notes: notes.trim() || null, location: location.trim() || null,
          startAtUtcMs: fromInput(start), endAtUtcMs: fromInput(due), allDay: false, status: "ready", unsupportedReason: null,
          workItemId: null, completed: false, important: pinAtSubmit, tags: tagNames.map((name) => ({ tagId: `optimistic:${name}`, name })), recurrence: localRecurrence,
          subtasks: [], pendingRemote: { operationId: provisionalId, accountId: calendar.accountId, kind: "createUtcV2", status: "pending" }, remoteEditEligible: false, remoteEditBlockedReason: translate(language, "pendingRemoteSave"), remoteTimeEditEligible: false
        };
        calendarItems = [...calendarItems, provisional];
        const result = await request;
        calendarItems = calendarItems.filter((current) => current.resourceId !== provisionalId);
        if (!result.started) { modal = "create"; return; }
        modal = null;
        if (result.terminalFailed) return;
        createdResourceId = result.receipt.resourceId;
        showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
        await refresh();
        if (earlySettlement !== undefined) finishCreate(earlySettlement);
      }
      return;
    }
    if (!visibleLocalCalendars.some((calendar) => calendar.calendarId === calendarId)) { error = translate(language, "selectedCalendarUnavailable"); return; }
    const startAtUtcMs = fromInput(start), dueAtUtcMs = fromInput(due);
    const tagNames = tags;
    if (recurrence !== "none" && dueAtUtcMs === null) { error = translate(language, "recurrenceDueRequired"); return; }
    if (tagNames.length > 32) { error = translate(language, "maxTags"); return; }
    if (tagNames.some((name) => Array.from(name).length > 64)) { error = translate(language, "maxTagLength"); return; }
    const todoId = editingTodo?.todoId ?? null;
    const previous = editingTodo;
    const localCalendar = visibleLocalCalendars.find((item) => item.calendarId === calendarId)!;
    const optimisticId = todoId ?? `optimistic:${crypto.randomUUID()}`;
    const localRecurrence = recurrenceInput(recurrence, interval, recurrenceOptions);
    const optimistic: LocalTodo = {
      ...(previous ?? {
        todoId: optimisticId, workItemId: null, occurrenceKey: "SINGLETON", completed: false,
        backlogCount: 0, subtasks: [], createdAtMs: Date.now(), updatedAtMs: Date.now(),
      }),
      todoId: optimisticId, calendarId, calendarName: localCalendar.name, calendarColor: localCalendar.color,
      title: title.trim(), notes: notes.trim() || null, location: location.trim() || null,
      startAtUtcMs, dueAtUtcMs, priority, important,
      tags: tagNames.map((name) => previous?.tags.find((tag) => tag.name === name) ?? { tagId: `optimistic:${name}`, name }),
      recurrence: localRecurrence ? { seriesId: previous?.recurrence?.seriesId ?? optimisticId, ...localRecurrence } : null,
      updatedAtMs: Date.now(),
    };
    todos = previous
      ? todos.map((item) => item.todoId === previous.todoId ? optimistic : item)
      : [...todos, optimistic];
    try {
      const saved = await saveLocalTodo({ todoId, selectedOccurrenceKey: previous?.occurrenceKey ?? null, calendarId, title: title.trim(), notes: notes.trim() || null, location: location.trim() || null, startAtUtcMs, dueAtUtcMs, priority, important, tagNames, recurrence: localRecurrence });
      todos = todos.map((item) => item.todoId === optimisticId ? saved : item);
      accepted = true;
      if (current()) { modal = null; editingTodo = null; }
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
    } catch (failure) {
      todos = previous ? todos.map((item) => item.todoId === previous.todoId ? previous : item) : todos.filter((item) => item.todoId !== optimisticId);
      void reportActionFailure("saveAction", "localSave", diagnosticFrom(failure));
    }
    } finally {
      if (accepted) failedEditors.delete(identity); else if (identity !== "create") failedEditors.set(identity,snapshot);
      const next = new Map(savingEditors); next.delete(identity); savingEditors = next;
    }
  }
  async function removeTodo(item: LocalTodo) {
    if (busy) return;
    operationBusy = true;
    try {
      await deleteLocalTodo(item.todoId);
      if (editingTodo?.todoId === item.todoId) { modal = null; editingTodo = null; }
      await refresh();
      scheduleUndoDeletion({ kind: "local", id: item.todoId, title: item.title });
    }
    catch { error = translate(language, "deleteFailedRetry"); }
    finally { operationBusy = false; }
  }
  async function removeOccurrence(item: LocalTodo) {
    if (busy) return; operationBusy=true;
    try { await deleteLocalTodoOccurrence(item.todoId,item.occurrenceKey); await refresh(); scheduleUndoDeletion({ kind:"localOccurrence",id:item.todoId,occurrenceKey:item.occurrenceKey,title:item.title }); }
    catch { error=translate(language,"deleteFailedRetry"); }
    finally { operationBusy=false; }
  }
  function chooseRecurrenceDelete(scope:"occurrence"|"series") { const item=recurrenceDeleteTodo; recurrenceDeleteTodo=null; if (!item) return; if (scope==="occurrence") void removeOccurrence(item); else void removeTodo(item); }
  async function removeEditingTodo() {
    if (!editingTodo) return;
    if (editingTodo.recurrence) recurrenceDeleteTodo=editingTodo; else await removeTodo(editingTodo);
  }
  async function undoDelete(token: number) {
    const item = undoQueue.undo(token);
    if (!item) return;
    if (item.kind === "remote") {
      const next = new Set(stagedRemoteDeleteKeys); next.delete(remoteDeleteKey(item.item)); stagedRemoteDeleteKeys = next;
      await refresh();
      return;
    }
    try { if (item.kind === "localOccurrence") await restoreRecentlyDeletedTodoOccurrence(item.id,item.occurrenceKey); else await restoreRecentlyDeletedTodo(item.id); await refresh(); }
    catch { error = translate(language, "undoFailedRetry"); }
  }
  function remoteDeleteKey(item: CalendarAgendaItem): string { return `${item.seriesId}:${item.recurrenceKey}`; }
  function scheduleUndoDeletion(item: UndoDeletion) {
    undoQueue.enqueue(item);
  }
  function stageRemoteDeletion(item: CalendarAgendaItem, calendar: CalendarSettingsView) {
    const displayedItem = calendarItems.find((current) => current.resourceId === item.resourceId && remoteDeleteKey(current) === remoteDeleteKey(item));
    const next = new Set(stagedRemoteDeleteKeys); next.add(remoteDeleteKey(item)); stagedRemoteDeleteKeys = next;
    calendarItems = calendarItems.filter((current) => remoteDeleteKey(current) !== remoteDeleteKey(item));
    scheduleUndoDeletion({ kind: "remote", id: remoteDeleteKey(item), title: displayedItem?.title ?? item.title, item, calendar });
  }
  function finishStagedRemoteDeletion(item: CalendarAgendaItem) {
    const next = new Set(stagedRemoteDeleteKeys); next.delete(remoteDeleteKey(item)); stagedRemoteDeleteKeys = next;
    void refresh();
  }

  async function applyLocalCompletionChange(change: LocalTodoCompletionChanged) {
    const current = todos.find((item) => item.todoId === change.todoId && item.occurrenceKey === change.previousOccurrenceKey);
    if (!current) { await refresh(); return; }
    const fromKey = todoEntryKey(current);
    localCompletionEvents.begin(fromKey, change.completed);
    completionChanges = new Map(completionChanges).set(fromKey, change.completed ? "complete" : "restore");
    try {
      await animateCompletionChange(document, fromKey, todoEntryKey(change.item), async () => {
        todos = todos.map((item) => item.todoId === change.todoId && item.occurrenceKey === change.previousOccurrenceKey ? change.item : item);
        await tick();
      });
    } finally {
      const queued = localCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      if (queued) await applyLocalCompletionChange(queued);
    }
  }

  async function applyCalendarCompletionChange(change: CalendarCompletionChanged) {
    const current = calendarItems.find((item) => item.seriesId === change.seriesId && item.recurrenceKey === change.previousRecurrenceKey);
    if (!current) { await refresh(); return; }
    const fromKey = calendarEntryKey(current);
    calendarCompletionEvents.begin(fromKey, change.completed);
    completionChanges = new Map(completionChanges).set(fromKey, change.completed ? "complete" : "restore");
    try {
      await animateCompletionChange(document, fromKey, calendarEntryKey(change.item), async () => {
        calendarItems = calendarItems.map((item) => item.seriesId === change.seriesId && item.recurrenceKey === change.previousRecurrenceKey ? change.item : item);
        await tick();
      });
    } finally {
      const queued = calendarCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      if (queued) await applyCalendarCompletionChange(queued);
    }
  }

  async function toggleTodo(item: LocalTodo) {
    const fromKey = todoEntryKey(item);
    if (completionChanges.has(fromKey)) return;
    const direction: CompletionDirection = item.completed ? "restore" : "complete";
    completionChanges = new Map(completionChanges).set(fromKey, direction);
    localCompletionEvents.begin(fromKey, !item.completed);
    try {
      const updated = await setLocalTodoCompleted(item.todoId, item.occurrenceKey, !item.completed);
      await animateCompletionChange(document, fromKey, todoEntryKey(updated), async () => {
        todos = todos.map((current) => current.todoId === item.todoId && current.occurrenceKey === item.occurrenceKey ? updated : current);
        await tick();
      });
    }
    catch { error = translate(language, "updateFailedRetry"); }
    finally {
      const queued = localCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      if (queued) await applyLocalCompletionChange(queued);
    }
  }

  async function toggleCalendar(item: CalendarAgendaItem) {
    if (item.status !== "ready" || item.pendingRemote) return;
    const fromKey = calendarEntryKey(item);
    if (completionChanges.has(fromKey)) return;
    const direction: CompletionDirection = item.completed ? "restore" : "complete";
    completionChanges = new Map(completionChanges).set(fromKey, direction);
    calendarCompletionEvents.begin(fromKey, !item.completed);
    try {
      const updated = await setCalendarCompleted(item.seriesId, item.recurrenceKey, !item.completed);
      await animateCompletionChange(document, fromKey, calendarEntryKey(updated), async () => {
        calendarItems = calendarItems.map((current) => current.seriesId === item.seriesId && current.recurrenceKey === item.recurrenceKey ? updated : current);
        await tick();
      });
    }
    catch { error = translate(language, "updateFailedRetry"); }
    finally {
      const queued = calendarCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      if (queued) await applyCalendarCompletionChange(queued);
    }
  }

  async function fold() {
    if (folding) return;
    folding = true;
    const next = !collapsed;
    try { await setDesktopCollapsed(next); collapsed = next; }
    catch { error = translate(language, "resizeListFailed"); }
    finally { folding = false; }
  }

  function drag(event: PointerEvent) {
    if (event.button !== 0 || (event.target as HTMLElement).closest("button")) return;
    void startDesktopDrag().catch(() => { error = translate(language, "moveListFailed"); });
  }

  function cancelReorder() {
    dragScroll?.stop(); dragScroll = null;
    dragSession++;
    dragging = null; dragOverKey = null; previewOrder = null;
    for (const row of document.querySelectorAll<HTMLElement>(".list-scroll li[data-key]")) row.getAnimations({ subtree: true }).forEach((animation) => animation.cancel());
    const handle = dragHandle, pointer = dragPointer;
    dragPointer = null; dragHandle = null;
    if (pointer !== null) { try { handle?.releasePointerCapture(pointer); } catch { /* already released */ } }
  }

  function closeReorderMenu(returnFocus = true) {
    const anchor = reorderMenu?.anchor;
    reorderMenu = null;
    if (returnFocus) queueMicrotask(() => anchor?.isConnected && anchor.focus());
  }
  function openDeleteMenu(event: MouseEvent, entry: Entry) {
    event.preventDefault(); event.stopPropagation();
    if (operationBusy || entry.kind === "calendar" && (entry.item.pendingRemote || savingEditors.has(`calendar:${entry.item.resourceId}:${entry.item.recurrenceKey}`))) return;
    closeReorderMenu(false);
    const row = event.currentTarget as HTMLElement;
    const target = event.target as HTMLElement;
    const opener = target.closest<HTMLElement>("button, [tabindex]") ?? row;
    deleteMenu = { entry, x: event.clientX, y: event.clientY, opener };
  }
  function closeDeleteMenu() { deleteMenu = null; }
  function chooseDeleteEntry() {
    const entry = deleteMenu?.entry;
    deleteMenu = null;
    if (!entry) return;
    if (entry.kind === "calendar") void removeRemoteEvent(entry.item);
    else if (entry.item.recurrence) recurrenceDeleteTodo=entry.item;
    else void removeTodo(entry.item);
  }
  function openReorderMenu(event: MouseEvent | KeyboardEvent, key: string) {
    event.preventDefault(); event.stopPropagation(); cancelReorder();
    deleteMenu = null;
    const anchor = event.currentTarget as HTMLButtonElement;
    const rect = anchor.getBoundingClientRect();
    reorderMenu = {
      key,
      x: event instanceof MouseEvent && event.clientX ? event.clientX : rect.right,
      y: event instanceof MouseEvent && event.clientY ? event.clientY : rect.bottom,
      anchor
    };
  }
  function reorderMenuKey(event: KeyboardEvent, key: string) {
    if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) openReorderMenu(event, key);
  }
  function reorderMenuDisabled(key: string): Record<ReorderAction, boolean> {
    const entry = entries.find((item) => item.key === key);
    const group = entries.filter((item) => item.item.important === entry?.item.important).map((item) => item.key);
    const index = group.indexOf(key);
    return { up: index <= 0, top: index <= 0, down: index < 0 || index >= group.length - 1, bottom: index < 0 || index >= group.length - 1 };
  }
  function chooseReorderAction(action: ReorderAction) {
    if (!reorderMenu) return;
    const key = reorderMenu.key;
    const entry = entries.find((item) => item.key === key);
    const group = entries.filter((item) => item.item.important === entry?.item.important).map((item) => item.key);
    const keys = reorderKeys(entries.map((item) => item.key), group, key, action);
    closeReorderMenu();
    if (keys.every((item, index) => item === entries[index]?.key)) return;
    order = keys;
    reorderAnnouncement = text("reorderPosition", { title: entry?.item.title ?? "", position: keys.indexOf(key) + 1, count: keys.length });
    void persistOrder(keys);
  }

  function lostReorderCapture(event: PointerEvent) {
    if (event.pointerId === dragPointer && !dragHandle?.hasPointerCapture(event.pointerId)) cancelReorder();
  }

  function startReorder(event: PointerEvent, key: string) {
    if (event.button !== 0 || busy || dragPointer !== null) return;
    event.preventDefault(); event.stopPropagation();
    cancelReorder();
    dragging = key; dragOverKey = key; previewOrder = entries.map((entry) => entry.key);
    const list = document.querySelector<HTMLElement>(".list-scroll");
    if (!list) { cancelReorder(); return; }
    // Keyed rows move during preview; capture on the stable list, not a moving button.
    dragPointer = event.pointerId; dragHandle = list;
    try { dragHandle.setPointerCapture(event.pointerId); } catch { /* best effort; window events remain active */ }
    const scroller = list;
    if (list && scroller) dragScroll = startReorderAutoScroll(scroller, list, event, (point) => { void previewReorder(point, false); });
  }

  function moveReorder(event: PointerEvent) {
    if (!dragging || event.pointerId !== dragPointer) return;
    if (event.buttons === 0) { cancelReorder(); return; }
    dragScroll?.update(event);
    void previewReorder(event);
  }

  async function previewReorder(point: DragPoint, animate = true) {
    if (!dragging || !previewOrder || point.pointerId !== dragPointer) return;
    const list = document.querySelector<HTMLElement>(".list-scroll");
    if (!list) { cancelReorder(); return; }
    const revision = ++dragPreviewRevision;
    // Scrolling must hit-test untransformed rows, even while the pointer is stationary.
    if (!animate) for (const row of list.querySelectorAll<HTMLElement>("li[data-key]")) row.getAnimations({ subtree: true }).forEach((animation) => animation.cancel());
    const rows = [...list.querySelectorAll<HTMLElement>("li[data-key]")];
    const before = new Map(rows.map((row) => [row.dataset.key, row.getBoundingClientRect()]));
    const keys = reorderAtPointer(previewOrder, dragging, point.clientY, rows.flatMap((row) => {
      const rect = before.get(row.dataset.key);
      return row.dataset.key && rect ? [{ key: row.dataset.key, midpointY: rect.top + rect.height / 2 }] : [];
    }));
    if (keys.every((key, index) => key === previewOrder?.[index])) return;
    dragOverKey = keys[keys.indexOf(dragging) + 1] ?? "end"; previewOrder = keys;
    const session = dragSession;
    await tick();
    if (!animate || !dragging || point.pointerId !== dragPointer || session !== dragSession || revision !== dragPreviewRevision || reducedCompletionMotion()) return;
    for (const row of list.querySelectorAll<HTMLElement>("li[data-key]")) {
      const previous = before.get(row.dataset.key);
      if (!previous) continue;
      const delta = previous.top - row.getBoundingClientRect().top;
      // Keep separators at their layout boundaries while the content moves.
      if (delta) for (const child of row.children) child.animate([{ transform: `translateY(${delta}px)` }, { transform: "translateY(0)" }], { duration: 170, easing: "ease-out" });
    }
  }

  function endReorder(event: PointerEvent) {
    if (!dragging || event.pointerId !== dragPointer) return;
    const insideList = !!(document.elementFromPoint(event.clientX, event.clientY) as HTMLElement | null)?.closest(".list-scroll");
    const keys = event.type === "pointercancel" || !insideList ? null : previewOrder;
    const changed = !!keys && keys.some((key, index) => key !== order[index]);
    if (keys) order = keys;
    cancelReorder();
    if (keys && changed) void persistOrder(keys);
  }

  async function persistOrder(keys: string[]) {
    try { await setDesktopItemOrder(keys); error = ""; }
    catch { error = translate(language, "orderSaveFailed"); await refresh(); }
  }

  async function togglePin() {
    if (pinSaving) return;
    pinSaving = true; pinRevision++;
    try { pinned = await setDesktopPinned(!pinned); }
    catch { error = translate(language, "pinWindowFailed"); }
    finally { pinSaving = false; pinRevision++; }
  }
</script>

<svelte:head><title>{translate(language, "desktopTitle")}</title></svelte:head>
<svelte:window oncontextmenu={(event) => { event.preventDefault(); deleteMenu = null; }} onlostpointercapture={lostReorderCapture} onblur={() => { cancelReorder(); deleteMenu = null; reorderMenu = null; }} onkeydown={(event) => { if (event.key === "Escape") { cancelReorder(); deleteMenu = null; reorderMenu = null; } }} onpointermove={moveReorder} onpointerup={endReorder} onpointercancel={endReorder} />
<main class:collapsed class:metadata-hidden={!settings?.desktopShowCalendarName && !settings?.desktopShowTime && !settings?.desktopShowTags} aria-busy={initialLoading} style={`--opacity: ${settings?.desktopOpacityPercent ?? 82}%`}>
  <ToastViewport>
    {#each undoItems as entry (entry.token)}<UndoToast title={entry.payload.title} undo={() => void undoDelete(entry.token)} {language} stacked />{/each}
    {#if actionToast}{#key actionToast.id}<ActionToast message={actionToast.message} variant={actionToast.variant} stacked />{/key}{/if}
  </ToastViewport>
  {#if reorderMenu}<ReorderMenu x={reorderMenu.x} y={reorderMenu.y} disabled={reorderMenuDisabled(reorderMenu.key)} choose={chooseReorderAction} close={closeReorderMenu} {language} />{/if}
  {#if deleteMenu}<EventContextMenu opener={deleteMenu.opener} x={deleteMenu.x} y={deleteMenu.y} label={translate(language, "delete")} choose={chooseDeleteEntry} close={closeDeleteMenu} />{/if}
  {#if recurrenceDeleteTodo}<RecurrenceDeleteScope {language} title={recurrenceDeleteTodo.title} close={() => recurrenceDeleteTodo=null} choose={chooseRecurrenceDelete} />{/if}
  <p class="sr-only" aria-live="polite">{reorderAnnouncement}</p>
  <header role="toolbar" tabindex="0" aria-label={translate(language, "desktopTitlebar")} onpointerdown={drag}>
    <div class="overview">
      <strong>{translate(language, "overview")}</strong><small>{text("pendingItems", { count: pending })}</small>
    </div>
    <div class="actions">
      <button type="button" aria-label={translate(language, "addEvent")} title={translate(language, "addEvent")} onclick={openCreate}>＋</button>
      <button type="button" class:pinned disabled={pinSaving || initialLoading} aria-pressed={pinned} aria-label={translate(language, pinned ? "unpinWindow" : "pinWindow")} title={translate(language, pinned ? "unpin" : "pinPosition")} onclick={() => void togglePin()}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 4h8l-1 5 3 3v2H6v-2l3-3-1-5ZM12 14v7" /></svg></button>
      <button type="button" disabled={folding} aria-label={translate(language, collapsed ? "expandList" : "collapseList")} title={translate(language, collapsed ? "expand" : "collapse")} onclick={() => void fold()}><svg class:left={collapsed} viewBox="0 0 24 24" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg></button>
    </div>
  </header>
  {#if error && !modal}<p class="error" role="alert">{error}</p>{/if}
  {#if !collapsed}
    {#if initialLoading}<p class="empty" role="status">{translate(language, "loadingAgenda")}</p>
    {:else}
    <div class="list-scroll" class:desktop-scrollbar-visible={desktopScrollbarVisible} role="region" aria-label={translate(language, "agendaList")} onscroll={revealDesktopScrollbar} onpointermove={(event) => { const bounds = event.currentTarget.getBoundingClientRect(); if (event.clientX >= bounds.right - 12) revealDesktopScrollbar(); }}>
    <ul aria-label={translate(language, "agendaList")}>
      {#each entries as entry (entry.key)}
        <li tabindex="-1" data-key={entry.key} data-completion-motion-id={`desktop:${entry.key}`} data-completion-key={entry.key} class:completion-finishing={completionChanges.get(entry.key) === "complete"} class:completion-restoring={completionChanges.get(entry.key) === "restore"} class:dragging={dragging === entry.key} class:drop-target={!!dragging && dragging !== entry.key && dragOverKey === entry.key} oncontextmenu={(event) => openDeleteMenu(event, entry)} ondblclick={() => openEntry(entry)}>
          <button class="check" style:--completion-color={entry.item.calendarColor ?? undefined} type="button" aria-label={text(entry.item.completed ? "restoreItem" : "completeItem", { title: entry.item.title })} aria-pressed={entry.item.completed} disabled={completionChanges.has(entry.key) || (entry.kind === "calendar" && (entry.item.status !== "ready" || !!entry.item.pendingRemote))} onclick={() => entry.kind === "todo" ? toggleTodo(entry.item) : toggleCalendar(entry.item)} ondblclick={(event) => event.stopPropagation()}><span aria-hidden="true">{entry.item.completed ? "✓" : ""}</span></button>
          <div class="item-text"><div class="item-title-line"><strong class:done={entry.item.completed} use:fitOneLine>{entry.item.title}</strong>{#if settings?.desktopShowTags && entry.item.tags.length}<span class="item-tags" use:fitOneLine>{entry.item.tags.map((tag) => `#${tag.name}`).join(" ")}</span>{/if}</div>{#if settings?.desktopShowCalendarName || settings?.desktopShowTime || entry.kind === "calendar" && entry.item.pendingRemote}<small class="item-metadata" use:fitOneLine>{#if settings?.desktopShowCalendarName}<b>{entry.item.calendarName}</b>{/if}{#if settings?.desktopShowTime}<time>{desktopTime(entry) ?? ""}</time>{/if}{#if entry.kind === "calendar" && entry.item.pendingRemote}<span>{translate(language, entry.item.pendingRemote.status === "conflict" ? "syncConflict" : entry.item.pendingRemote.status === "retryNeeded" ? "remoteRetryNeeded" : "pendingRemoteSave")}</span>{/if}</small>{/if}{#if entry.item.recurrence}<small class="recurrence-summary" use:fitOneLine>{recurrenceSummary(language, entry.item.recurrence.frequency, entry.item.recurrence.interval)}{entry.kind === "calendar" ? translate(language, "localOnlySuffix") : ""}</small>{/if}</div>
          <button class="reorder" type="button" aria-haspopup="menu" aria-label={text("reorderItem", { title: entry.item.title })} title={translate(language, "holdToReorder")} onpointerdown={(event) => startReorder(event, entry.key)} oncontextmenu={(event) => openReorderMenu(event, entry.key)} onkeydown={(event) => reorderMenuKey(event, entry.key)} onclick={(event) => { if (event.detail === 0) openReorderMenu(event, entry.key); }} ondblclick={(event) => event.stopPropagation()}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 7h14M5 12h14M5 17h14" /></svg></button>
        </li>
      {/each}
    </ul>
    {#if !entries.length && !error}<p class="empty">{translate(language, "emptyAgendaDesktop")}</p>{/if}
    </div>
    {/if}
  {/if}
  {#if modal}
    <div class="scrim" role="presentation" onpointerdown={(event) => modalBackdropClick.pointerDown(event.target, event.currentTarget)} onpointerup={(event) => modalBackdropClick.pointerUp(event.target, event.currentTarget)} onpointercancel={modalBackdropClick.pointerCancel} onclick={(event) => { if (modalBackdropClick.click(event.target, event.currentTarget) && !busy && !remoteView?.busy) modal = null; }}>
      <div class="dialog" class:creating={modal === "create"} role="dialog" tabindex="-1" aria-modal="true" aria-label={translate(language, modal === "create" ? "newEvent" : modal === "todo" ? "editEvent" : "calendarEventDetails")} use:dialogFocus={{ close: () => modal = null, disabled: busy || !!remoteView?.busy }}>
        <div class="dialog-head"><h2>{translate(language, modal === "create" ? "newEvent" : "editEvent")}</h2><button type="button" aria-label={translate(language, "close")} disabled={busy || remoteView?.busy} onclick={() => { modal = null; }}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18" /></svg></button></div>
          <form autocomplete="off" onsubmit={save} oninput={() => { if (remoteView) remoteView = null; }} aria-busy={busy || remoteView?.busy || false}>
            <div class="dialog-body" class:read-only={modal === "calendar"}>
            {#if error}<p class="error" role="alert">{error}</p>{/if}
            {#if remoteReason}<p role="alert">{remoteReason}</p>{:else if viewingCalendar && !projectionCandidate(viewingCalendar)}<p role="alert">{preparationHint(viewingCalendar)}</p>{/if}
            <fieldset class="editor-fields" disabled={busy || remoteView?.busy || modal === "calendar" && (!!remoteReason || !viewingCalendar || !projectionCandidate(viewingCalendar))}>
            <label>{translate(language, "title")}<input bind:this={titleInput} data-autofocus bind:value={title} maxlength="512" required /></label>
            <label>{translate(language, "calendar")}<select bind:value={calendarId} disabled={modal === "calendar"} onchange={resetRemoteEditor}>{#if modal === "calendar"}<option value={viewingCalendar?.calendarId}>{viewingCalendar?.calendarName}</option>{:else}{#each visibleLocalCalendars as calendar}<option value={calendar.calendarId}>{calendar.name}</option>{/each}{#if modal === "create"}{#each remoteDestinations as calendar}<option value={calendar.calendarId} disabled={!!remoteBlockedReason(calendar)}>{calendar.displayName} · iCloud{remoteBlockedReason(calendar) ? ` · ${remoteBlockedReason(calendar)}` : ""}</option>{/each}{/if}{/if}</select></label>
            {#if modal === "calendar"}<p>{translate(language, "remoteMoveUnsupported")}</p>{/if}
            <div class="desktop-time-fields"><span class="field-label">{translate(language, "time")}</span><label for="desktop-event-start">{translate(language, "start")}</label><DateTimeInput disabled={busy} id="desktop-event-start" step={modal === "calendar" || remoteCreateCalendar ? 1 : 60} value={start} onValue={updateStart} {language} /><label for="desktop-event-due">{translate(language, "due")}</label><DateTimeInput disabled={busy} id="desktop-event-due" step={modal === "calendar" || remoteCreateCalendar ? 1 : 60} value={due} onValue={(value) => { due = value; dueEdited = true; }} min={start || undefined} {language} /></div>
            <label>{translate(language, "notes")}<textarea bind:value={notes} maxlength="10000" rows="2"></textarea></label>
            <label>{translate(language, "location")}<input bind:value={location} maxlength="512" /></label>
            </fieldset>
            <fieldset class="editor-fields" disabled={busy || remoteView?.busy}>
            <div class="desktop-tag-field"><label for="desktop-tags">{translate(language, "tags")}</label><TagInput id="desktop-tags" {tags} {language} suggestions={tagSuggestions} onTags={(value) => tags = value} disabled={busy || remoteView?.busy} /></div>
            <div class="desktop-create-options"><div class="desktop-recurrence-fields"><RecurrenceSelector {language} frequency={recurrence} {interval} options={recurrenceOptions} anchorAt={fromInput(due)} disabled={busy || !!remoteView?.busy} change={(frequency, nextInterval, options) => { recurrence = frequency; interval = nextInterval; recurrenceOptions = options; }} /></div><label class="inline important-toggle"><input type="checkbox" bind:checked={important} disabled={busy || remoteView?.busy || modal === "calendar" && !viewingCalendar?.workItemId} /><span>{translate(language, "important")}{modal === "calendar" || remoteCreateCalendar ? ` (${translate(language, "localOnly")})` : ""}</span></label></div>
            </fieldset>
            {#if modal === "calendar" || remoteCreateCalendar}<small>{translate(language, "remoteTimeHelp")}{modal === "calendar" ? ` ${translate(language, "localMetadataHelp")}` : ""}</small>{/if}
            {#if remoteView}<p role="status" aria-live="polite">{remoteView.message}</p>{#each Object.values(remoteView.errors) as message}<p class="error" role="alert">{message}</p>{/each}{/if}
            </div>
            <div class="form-actions">{#if modal === "todo"}<button class="delete-action" type="button" onclick={removeEditingTodo} disabled={busy}>{translate(language, "delete")}</button>{:else if modal === "calendar" && !viewingCalendar?.pendingRemote}<button class="delete-action" type="button" onclick={() => void removeRemoteEvent()} disabled={busy || remoteView?.busy || !!remoteReason || !viewingCalendar || !projectionCandidate(viewingCalendar)}>{translate(language, "delete")}</button>{/if}<button type="button" onclick={() => { modal = null; }} disabled={busy || remoteView?.busy}>{translate(language, "cancel")}</button>{#if modal === "calendar" && viewingCalendar?.pendingRemote?.kind === "delete" && viewingCalendar.pendingRemote.status === "conflict"}<button type="button" disabled={busy} onclick={() => viewingCalendar && keepPendingRemote(viewingCalendar)}>{translate(language,"keepRemoteVersion")}</button>{:else if modal === "calendar" && viewingCalendar?.pendingRemote}<button type="button" disabled={busy || viewingCalendar.pendingRemote.status === "conflict"} onclick={() => viewingCalendar && retryPendingRemote(viewingCalendar)}>{controlText(language, busy ? "processing" : viewingCalendar.pendingRemote.kind === "delete" ? "retryDelete" : "retrySave")}</button>{:else}<button type="submit" disabled={busy || remoteView?.busy || !title.trim()}>{controlText(language, busy || remoteView?.busy ? "saving" : "save")}</button>{/if}</div>
          </form>
      </div>
    </div>
  {/if}
</main>

<style>
  :global(html),:global(body){margin:0;min-width:0;background:transparent;font-family:var(--font-ui);color:var(--color-text)}main{margin:8px;padding:12px;border:1px solid var(--color-border);border-radius:15px;background:color-mix(in srgb,var(--color-surface) var(--opacity),transparent);box-shadow:var(--shadow-widget);max-height:calc(100vh - 16px);overflow:auto}main.collapsed{height:50px;padding:6px 10px;overflow:hidden}header,.actions{display:flex;align-items:center;justify-content:space-between;gap:8px}header{min-height:32px}.overview{display:flex;align-items:baseline;gap:7px;min-width:0;cursor:grab;user-select:none}.overview strong{font-size:var(--font-size-body)}.overview small{font-size:var(--font-size-caption);color:var(--color-text-muted);white-space:nowrap}.actions{gap:4px}.actions button{width:32px;height:32px;display:grid;place-items:center;border:1px solid var(--color-border);border-radius:var(--radius-control);background:color-mix(in srgb,var(--color-surface) 86%,transparent);color:var(--color-text);font-size:1.2rem;cursor:pointer}.actions button.pinned{border-color:color-mix(in srgb,var(--color-primary) 62%,var(--color-border));color:var(--color-primary);background:color-mix(in srgb,var(--color-primary) 18%,transparent)}form{display:flex;gap:6px;margin:10px 0}input{min-width:0;flex:1;padding:8px;border:1px solid var(--color-border);border-radius:var(--radius-control);background:color-mix(in srgb,var(--color-surface) 94%,transparent)}form button{padding:6px 11px;border:1px solid var(--color-primary);border-radius:var(--radius-control);color:var(--color-on-primary);background:var(--color-primary)}ul{display:grid;gap:12px;margin:12px 0 0;padding:0;list-style:none}li{display:flex;align-items:center;gap:10px;padding:9px 10px;border:1px solid var(--color-border);border-radius:var(--radius-row);background:color-mix(in srgb,var(--color-surface) 88%,transparent)}li>div{min-width:0}li strong,li small{display:block;overflow-wrap:anywhere}li strong{font-size:.86rem}li small{margin-top:3px;color:var(--color-text-muted);font-size:var(--font-size-caption)}.done{text-decoration:line-through;color:var(--color-text-muted)}.check{width:24px;height:24px;flex:0 0 24px;border:1.5px solid var(--color-border-strong);border-radius:6px;background:color-mix(in srgb,var(--color-surface) 94%,transparent);color:var(--color-on-primary);cursor:pointer}.check[aria-pressed="true"]{background:var(--color-primary);border-color:var(--color-primary)}.check:disabled{opacity:.5;cursor:not-allowed}.empty,.error{font-size:.77rem}.error{color:var(--color-danger)}
  .collapsed .overview small{display:none}
  .actions svg{width:18px;height:18px;display:block;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}.actions svg.left{transform:rotate(90deg)}
  main,main.collapsed{height:calc(100vh - 16px);max-height:none;display:flex;flex-direction:column;overflow:hidden;padding:12px}header{height:32px;min-height:32px;cursor:grab;user-select:none}.overview{pointer-events:none;cursor:inherit}.collapsed .overview small{display:inline}.list-scroll{min-height:0;overflow-y:auto;overscroll-behavior:contain}.item-text{flex:1}.check{width:20px;height:20px;flex-basis:20px;border-radius:5px}.reorder{margin-left:auto;padding:3px 5px;border:0;background:transparent;color:var(--color-text-muted);font-size:18px;line-height:1;cursor:grab}.scrim{position:fixed;inset:0;z-index:5;display:grid;place-items:center;padding:16px;background:var(--color-scrim)}.dialog{width:min(100%,390px);max-height:calc(100vh - 32px);overflow-y:auto;padding:16px;border:1px solid var(--color-border);border-radius:14px;background:var(--color-surface);color:var(--color-text);box-shadow:var(--shadow-dialog)}.dialog-head{display:flex;align-items:center;justify-content:space-between}.dialog-head h2{margin:0 0 12px;font-size:1rem}.dialog-head button{border:0;background:transparent;font-size:1.5rem;cursor:pointer}.dialog form{display:grid;gap:10px;margin:0}.dialog form label,.dialog fieldset{display:grid;gap:5px;font-size:.78rem}.dialog fieldset{margin:0;padding:9px;border:1px solid var(--color-border);border-radius:8px}.dialog input:not([type="checkbox"]),.dialog select,.dialog textarea{width:100%;min-width:0;padding:7px;border:1px solid var(--color-border);border-radius:8px;background:var(--color-surface-raised);color:var(--color-text)}.dialog .inline{display:flex;align-items:center;gap:8px}.form-actions{display:flex;justify-content:flex-end;gap:8px}.form-actions button{padding:7px 13px;border:1px solid var(--color-primary);border-radius:8px;background:var(--color-surface);color:var(--color-primary);cursor:pointer}.form-actions button[type="submit"]{background:var(--color-primary);color:var(--color-on-primary)}.read-only{font-size:.83rem}.read-only small{color:var(--color-text-muted)}
  /* The card fills the window, so the visible translucent border is the resize target. */
  :global(html),:global(body){width:100%;height:100%;overflow:hidden}
  main{margin:0;width:100vw;height:100vh;padding:12px 8px;box-shadow:none;overflow-x:hidden}
  main.collapsed{margin:0;width:100vw;height:100vh;padding:12px 8px}
  .check{display:grid;place-items:center;padding:0;font-size:12px;font-weight:700;line-height:1}
  li.completion-finishing,li.completion-finishing strong{color:var(--color-text-muted)}li.completion-finishing strong{text-decoration:line-through}li.completion-restoring,li.completion-restoring strong{color:var(--color-text)}li.completion-restoring strong{text-decoration:none}
  .item-text{flex:1;min-width:0}
  .list-scroll{width:100%;min-width:0;padding-left:7px;overflow-x:hidden}
  .list-scroll ul,.list-scroll li{min-width:0}
  main.collapsed{padding-top:12px;padding-bottom:12px}
  .reorder{margin-left:auto;padding:2px 5px;border-radius:6px;cursor:grab;touch-action:none;user-select:none}
  .reorder:active{background:var(--color-surface-soft);cursor:grabbing}
  .reorder svg{display:block;width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round}
  li.dragging{opacity:1!important;color:var(--color-text)!important;background:color-mix(in srgb,var(--color-surface) 96%,var(--color-primary))!important}li.dragging .item-text,li.dragging strong,li.dragging small{color:var(--color-text)!important}li.drop-target{border-color:transparent!important;background:transparent!important}
  .scrim{padding:28px}
  .dialog{display:flex;flex-direction:column;max-height:calc(100vh - 56px);overflow:hidden;border-radius:16px;padding:0}
  .dialog-head{flex:0 0 auto;padding:12px 16px;border-bottom:1px solid var(--color-border)}
  .dialog-head h2{margin:0}
  .dialog-head button{width:28px;height:28px;display:grid;place-items:center;border-radius:8px;font-size:1.25rem;line-height:1}
  .dialog form{display:flex;flex-direction:column;min-height:0}
  .dialog-body{flex:1 1 auto;display:grid;gap:10px;min-height:0;padding:14px 16px 6px;overflow-y:auto}
  .dialog fieldset{border-radius:10px}
  .dialog input:not([type="checkbox"]),.dialog select,.dialog textarea{border-radius:9px}
  .form-actions{position:relative;z-index:2;flex:0 0 auto;padding:7px 16px;border-top:1px solid var(--color-border);background:var(--color-surface-soft)}
  .form-actions button{padding:5px 15px;border:1px solid var(--color-border-strong);border-radius:9px;background:var(--color-surface-soft);color:var(--color-text);font-weight:600}
  .form-actions button[type="submit"]{border-color:var(--color-primary);background:var(--color-primary);color:var(--color-on-primary)}
  .form-actions button:disabled{opacity:.55}
  .form-actions .delete-action{margin-right:auto;border-color:var(--color-danger);background:var(--color-danger);color:var(--color-on-danger)}
  .sr-only{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}.actions button{width:36px;height:36px}.check{position:relative;width:36px;height:36px;flex:0 0 36px;margin-inline:-2px;display:grid;place-items:center;padding:0;border:0;border-radius:0;background:transparent!important;color:var(--completion-color,var(--color-primary));font-size:13px;font-weight:750;line-height:1}.check::before{content:"";position:absolute;width:18px;height:18px;border:1.5px solid var(--completion-color,var(--color-border-strong));border-radius:5px;background:transparent}.check[aria-pressed="true"]::before{background:transparent}.check span{position:relative}.reorder{width:36px;height:36px;display:grid;place-items:center;padding:0}.dialog-head button{width:36px;height:36px}.actions button:focus-visible,.check:focus-visible,.reorder:focus-visible,.dialog button:focus-visible,.dialog input:focus-visible,.dialog select:focus-visible,.dialog textarea:focus-visible{outline:2px solid var(--color-focus);outline-offset:2px}
  ul{gap:0;margin-top:8px}li{min-height:46px;padding:5px 4px;border:0;border-bottom:1px solid color-mix(in srgb,var(--color-border) 76%,transparent);border-radius:0;background:transparent}.metadata-hidden li{min-height:38px;padding-block:3px}.metadata-hidden ul{margin-top:5px}.item-text strong{font-size:.84rem;line-height:1.25}.item-text small{margin-top:2px}.item-text small:nth-of-type(n+2){display:none}
  .dialog fieldset.editor-fields{display:grid;gap:10px;margin:0;padding:0;border:0}.desktop-time-fields{display:grid;grid-template-columns:52px minmax(0,1fr);align-items:center;gap:6px 8px}.desktop-time-fields .field-label{grid-column:1/-1;font-size:.78rem;font-weight:600}.desktop-time-fields label{margin:0;color:var(--color-text-muted)}.desktop-tag-field{display:grid;gap:5px}.desktop-tag-field :global(.tag-input .controls){background:var(--color-surface-raised)}.desktop-tag-field :global(.tag-input .controls input){padding-inline:8px}.desktop-create-options{display:flex;align-items:end;gap:8px;flex-wrap:wrap}.desktop-recurrence-fields{min-width:min(100%,210px);flex:1 1 210px;display:grid;grid-template-columns:minmax(0,1fr);gap:8px}.dialog .important-toggle{min-height:36px;display:grid;grid-template-columns:20px minmax(0,1fr);align-items:center;gap:8px}.dialog .important-toggle input{width:18px;height:18px;margin:0}.dialog .important-toggle span{min-width:0;line-height:1.35}.dialog-head button svg{width:18px;height:18px;display:block;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round}.form-actions{flex-wrap:wrap}.form-actions button{min-width:0;height:auto;white-space:normal;overflow-wrap:anywhere;text-align:center}
  main,main.collapsed{padding:12px}.actions button{border-color:var(--color-border-strong);background:transparent}.overview{align-self:stretch;align-items:center}.item-title-line{min-width:0;display:flex;align-items:baseline;gap:10px}.item-title-line strong{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.item-tags{flex:0 1 auto;min-width:0;color:var(--color-text-muted);font-size:var(--font-size-caption);font-weight:400;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.item-metadata{display:flex!important;align-items:baseline;gap:10px}.item-metadata>*{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.desktop-tag-field :global(.tag-input .controls){border:1px solid var(--color-border);background:var(--color-surface-raised);box-shadow:none}.desktop-tag-field :global(.tag-input .controls:focus-within){border-color:var(--color-border-strong);box-shadow:none}.desktop-tag-field :global(.tag-input .controls input),.desktop-tag-field :global(.tag-input .controls input:focus){border:0;outline:0;box-shadow:none}
  .desktop-tag-field :global(.tag-input .controls:focus-within){border-color:var(--color-border);background:color-mix(in srgb,var(--color-primary) 9%,var(--color-surface));box-shadow:none}.desktop-tag-field :global(.tag-input .controls input),.desktop-tag-field :global(.tag-input .controls input:focus),.desktop-tag-field :global(.tag-input .controls input:focus-visible){border:0!important;outline:0!important;background:transparent!important;box-shadow:none!important}
  .dialog input:not([type="checkbox"]),.dialog select,.dialog textarea,.dialog :global(.date-time-input>input),.dialog :global(.tag-input .controls){border:0!important;outline:0!important;background:var(--color-field-idle)!important;box-shadow:none!important}.dialog input:not([type="checkbox"]):focus,.dialog select:focus,.dialog textarea:focus,.dialog :global(.date-time-input>input:focus),.dialog :global(.tag-input .controls:focus-within){outline:0!important;background:var(--color-field-idle)!important;box-shadow:var(--focus-ring)!important}
  .dialog :global(.tag-input .controls input),.dialog :global(.tag-input .controls input:focus){background:transparent!important;box-shadow:none!important}
  .dialog{--focus-ring:var(--field-focus-ring);--color-surface:var(--color-detail-surface);--color-field-idle:var(--color-detail-field)}
  .dialog.creating{--color-surface:var(--color-create-surface);--color-field-idle:var(--color-create-field)}
  .dialog input:disabled,.dialog select:disabled,.dialog textarea:disabled,.dialog :global(.date-time-input>input:disabled){opacity:1!important;color:var(--color-text-muted)!important;background:var(--color-field-disabled)!important}
  @media(max-width:380px){.scrim{padding:14px}.dialog{max-height:calc(100vh - 28px)}.desktop-time-fields{grid-template-columns:minmax(0,1fr)}.desktop-time-fields .field-label{grid-column:auto}.desktop-time-fields label{margin-top:2px}.desktop-time-fields :global(.date-time-input>input){padding-right:40px}.desktop-time-fields :global(.date-time-input>.icon-button){width:34px;min-width:34px}.desktop-recurrence-fields{grid-template-columns:minmax(0,1fr)}}
  @media(max-width:380px){.desktop-time-fields :global(.date-time-input>.picker-trigger>.icon-button){width:34px;min-width:34px}}
  @media(max-height:180px){.scrim{padding:4px}.dialog{max-height:calc(100vh - 8px);overflow-y:auto}.dialog form,.dialog-body{flex-shrink:0}.dialog-body{overflow:visible}}
  .item-text small.recurrence-summary{display:block}
  .list-scroll{width:100%;margin-right:0;padding-right:0;scrollbar-gutter:stable}
  .list-scroll::-webkit-scrollbar-thumb{background:transparent}.list-scroll.desktop-scrollbar-visible::-webkit-scrollbar-thumb{background:color-mix(in srgb,var(--color-border-strong) 62%,transparent);background-clip:padding-box}.list-scroll.desktop-scrollbar-visible::-webkit-scrollbar-thumb:vertical{border-width:0 1px 0 5px}.list-scroll.desktop-scrollbar-visible::-webkit-scrollbar-thumb:hover{border-width:0 1px 0 1px;background:color-mix(in srgb,var(--color-border-strong) 84%,transparent);background-clip:padding-box}
</style>
