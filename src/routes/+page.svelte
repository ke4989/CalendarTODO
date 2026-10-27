<script lang="ts">
  import { onMount, onDestroy, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import ActionToast from "$lib/components/ActionToast.svelte";
  import Button from "$lib/components/Button.svelte";
  import DateTimeInput from "$lib/components/DateTimeInput.svelte";
  import DialogShell from "$lib/components/DialogShell.svelte";
  import LicenseViewer from "$lib/components/LicenseViewer.svelte";
  import InlineStatus from "$lib/components/InlineStatus.svelte";
  import IconButton from "$lib/components/IconButton.svelte";
  import SettingsRow from "$lib/components/SettingsRow.svelte";
  import Switch from "$lib/components/Switch.svelte";
  import TagInput from "$lib/components/TagInput.svelte";
  import EventContextMenu from "$lib/components/EventContextMenu.svelte";
  import ReorderMenu from "$lib/components/ReorderMenu.svelte";
  import UndoToast from "$lib/components/UndoToast.svelte";
  import ToastViewport from "$lib/components/ToastViewport.svelte";
  import { DetailAutosave } from "$lib/ui/detail-autosave";
  import { DeletionUndoQueue, type UndoEntry } from "$lib/ui/deletion-undo";
  import RecurrenceDeleteScope from "$lib/components/RecurrenceDeleteScope.svelte";
  import RecurrenceSelector from "$lib/components/RecurrenceSelector.svelte";
  import { copyRecurrenceOptions, defaultRecurrenceOptions, recurrenceChanged, recurrenceInput } from "$lib/ui/recurrence";
  import { controlText, resolveLanguage, setActiveLanguage, translate, withoutTerminalPunctuation, type MessageKey } from "$lib/i18n";
  import { applyRemoteDraft, draftRequest, initialDraft, selectedBase, retryBackgroundRemoteWrite, startBackgroundRemoteWrite, utcInput, projectionCandidate, preparationHint, type PreparationState, type RemoteDraft } from "$lib/ui/remote-crud-preparation";
  import { applyTheme } from "$lib/theme/apply";
  import { confirmedRemoteEvent, listenConfirmedEvent, prepareRemoteCrud } from "$lib/ipc/client";
  import { fitOneLine } from "$lib/ui/fit-one-line";
  import { remoteCalendarBlockedReason } from "$lib/ui/remote-calendar-availability";
  import { startReorderAutoScroll, type DragPoint } from "$lib/ui/reorder-auto-scroll";
  import { mergeVisibleOrder, reorderAtPointer } from "$lib/ui/reorder-preview";
  import { reorderKeys, type ReorderAction } from "$lib/ui/reorder-menu";
  import { createBackdropClickGuard, dialogFocus } from "$lib/ui/dialog-focus";
  import { collectTagSuggestions } from "$lib/ui/tag-input";
  import { selectTimelineEntries, timelineFocusTime, timelineNowPosition } from "$lib/ui/timeline-selection.js";
  import { withinCalendarDisplayRange } from "$lib/ui/calendar-display-range";
  import { animateCompletionChange, reducedCompletionMotion, type CompletionDirection } from "$lib/ui/completion-motion";
  import { CompletionEventGate } from "$lib/ui/completion-events";
  import {
    attachAccountSource, deleteLocalCalendar, deleteLocalTodo, deleteLocalTodoOccurrence, discoverRemoteCalendars, getBootstrapState, restoreRecentlyDeletedTodo, restoreRecentlyDeletedTodoOccurrence, setLocalCalendarHidden,
    checkForUpdate, clearErrorLog, enrollAccountCredential, getAppSettings, getAppVersion, getDesktopItemOrder, getRemoteMutationView, openLatestRelease, openProductRepository, readErrorLog, recordUiError, resetDesktopItemOrder, setDesktopItemOrder, listAccountProfiles, listAccountSyncStates, listAttachmentStates, listCalendarAgendaItems, listCalendarSettings, listLocalCalendars, listLocalTodos, purgeExpiredRemoteCache, registerAccountProfile, removeAccountProfile, resolveRemoteMutationConflict, saveLocalCalendar, saveLocalTodo, setAccountProfileEnabled, setAccountRemoteEditingEnabled, setCalendarColorOverride, setCalendarDisplayRange, setCalendarVisible, setCalendarCompleted, setCalendarImportant, setLocalTodoCompleted, triggerAccountSync, updateAppSettings, updateCalendarLocalMetadata
  } from "$lib/ipc/client";
  import type { AgendaRefreshNotice, AppLanguage, AppSettings, AttachmentView, BootstrapState, CalendarAgendaItem, CalendarCompletionChanged, CalendarSettingsView, ColorMode, ConflictResolutionChoice, IpcError, LocalCalendar, LocalTodo, LocalTodoCompletionChanged, MutationApplicationView, Priority, RecurrenceFrequency, RecurrenceOptions, RuntimeAccountProfile, SyncExecutionView, ThemePalette, WindowMode } from "$lib/ipc/contracts";

  let bootstrap = $state<BootstrapState | null>(null);
  let todos = $state<LocalTodo[]>([]);
  let calendarItems = $state<CalendarAgendaItem[]>([]);
  const tagSuggestions = $derived(collectTagSuggestions([...todos, ...calendarItems]));
  let calendarSettings = $state<CalendarSettingsView[]>([]);
  let localCalendars = $state<LocalCalendar[]>([]);
  const visibleLocalCalendars = $derived(localCalendars.filter((calendar) => !calendar.hidden));
  let createCalendarId = $state(""); let editCalendarId = $state("");
  const remoteCreateCalendar = $derived(calendarSettings.find((calendar) => calendar.calendarId === createCalendarId));
  const remoteDestinations = $derived(calendarSettings.filter((calendar) => calendar.visible));
  function preferredCreateCalendarId(): string {
    const preferred = settings?.defaultCalendarId;
    if (preferred && (visibleLocalCalendars.some((calendar) => calendar.calendarId === preferred) || remoteDestinations.some((calendar) => calendar.calendarId === preferred && !remoteBlockedReason(calendar)))) return preferred;
    return visibleLocalCalendars[0]?.calendarId ?? remoteDestinations.find((calendar) => !remoteBlockedReason(calendar))?.calendarId ?? "";
  }
  let remoteView = $state<PreparationState | null>(null);
  let actionToast = $state<{ id: number; message: string; variant: "info" | "success" | "error" } | null>(null);
  let actionToastId = 0;
  let actionToastTimer: ReturnType<typeof setTimeout> | undefined;
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
  onDestroy(() => { backgroundStops.forEach((stop) => stop()); if (actionToastTimer) clearTimeout(actionToastTimer); detailSessions.forEach((session) => session.saver.dispose()); });
  function remoteBlockedReason(calendar: CalendarSettingsView | undefined, item?: CalendarAgendaItem) { return item && item.pendingRemote?.kind === "updateEventV3" && item.pendingRemote.status !== "conflict" ? remoteCalendarBlockedReason(calendar, attachmentStates) : remoteCalendarBlockedReason(calendar, attachmentStates, item); }
  function editorDraft(item: CalendarAgendaItem | undefined, values: { title: string; notes: string; location: string; start: string; end: string }): RemoteDraft {
    const start = fromLocalInput(values.start), end = fromLocalInput(values.end);
    return { ...initialDraft(item), title: values.title, selectTitle: !item || values.title !== item.title, description: values.notes, descriptionAction: !item || values.notes !== (item.notes ?? "") ? values.notes.trim() ? "set" : "clear" : "retain", location: values.location, locationAction: !item || values.location !== (item.location ?? "") ? values.location.trim() ? "set" : "clear" : "retain", selectTime: !item || start !== item.startAtUtcMs || end !== item.endAtUtcMs, start: utcInput(start), end: utcInput(end) };
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
      applied: () => { completed = true; backgroundStops.delete(stop); void refreshCalendar().finally(() => settled?.(true)); },
      failed: (diagnostic) => { completed = true; terminalFailed = true; backgroundStops.delete(stop); void reportActionFailure(action, "remoteBackgroundSync", diagnostic, true); void refreshCalendar().finally(() => settled?.(false)); },
    });
    if (!result.started) {
      void reportActionFailure(action, "remoteAdmission", result.message);
      return result;
    }
    stop = result.stop;
    if (!completed) backgroundStops.add(stop);
    void refreshCalendar();
    return { ...result, terminalFailed, terminalComplete: completed };
  }
  async function keepPendingRemote(item: CalendarAgendaItem) {
    const pending = item.pendingRemote;
    if (!pending || pending.status !== "conflict" || busyIds.has(pending.operationId)) return;
    markBusy(pending.operationId,true);
    try { await prepareRemoteCrud({action:"resolveConflict",input:{operationId:pending.operationId,kind:pending.kind,choice:"keepRemote"}}); await refreshCalendar(); }
    catch (failure) { void reportActionFailure("deleteAction","remoteConflictResolution",diagnosticFrom(failure)); }
    finally { markBusy(pending.operationId,false); }
  }
  async function retryPendingRemote(item: CalendarAgendaItem) {
    const pending = item.pendingRemote;
    if (!pending || busyIds.has(pending.operationId)) return;
    markBusy(pending.operationId, true);
    let stop = () => {};
    let completed = false;
    const result = await retryBackgroundRemoteWrite(
      { kind: pending.kind, operationId: pending.operationId, accountId: pending.accountId, calendarId: item.calendarId, resourceId: item.resourceId },
      confirmedRemoteEvent,
      listenConfirmedEvent,
      {
        applied: () => { completed = true; backgroundStops.delete(stop); markBusy(pending.operationId, false); void refreshCalendar(); },
        failed: (diagnostic) => { completed = true; backgroundStops.delete(stop); markBusy(pending.operationId, false); void reportActionFailure(pending.kind === "delete" ? "deleteAction" : "saveAction", "remoteBackgroundSync", diagnostic, true); void refreshCalendar(); },
      },
    );
    if (!result.started) {
      markBusy(pending.operationId, false);
      void reportActionFailure("saveAction", "remoteRetry", "exact operation retry was not accepted", true);
      return;
    }
    stop = result.stop;
    if (!completed) backgroundStops.add(stop);
    await refreshCalendar();
  }
  type RemoteDetailValues = { title: string; notes: string; location: string; start: string; due: string; important: boolean; tags: string[]; recurrence: "none" | RecurrenceFrequency; recurrenceInterval: number; options: RecurrenceOptions };
  function captureRemoteDetailValues(): RemoteDetailValues {
    updateEditStart(editStart);
    return { title: editTitle, notes: editNotes, location: editLocation, start: editStart, due: editDue, important: editImportant, tags: [...editTags], recurrence: editRecurrence, recurrenceInterval: Number(editRecurrenceInterval), options: structuredClone($state.snapshot(editRecurrenceOptions)) };
  }
  function applyRemoteDetailValues(values: RemoteDetailValues) {
    editTitle = values.title; editNotes = values.notes; editLocation = values.location; editStart = values.start; editDue = values.due;
    editImportant = values.important; editTags = [...values.tags]; editRecurrence = values.recurrence; editRecurrenceInterval = values.recurrenceInterval;
  }
  async function saveRemoteDetails(item: CalendarAgendaItem, closeAfter = false, values = captureRemoteDetailValues(), current = () => true, session?: DetailSession): Promise<boolean> {
    if (!values.title.trim() || !Number.isInteger(values.recurrenceInterval) || values.recurrenceInterval < 1 || values.recurrenceInterval > 365) return false;
    const draft = editorDraft(item, { title: values.title, notes: values.notes, location: values.location, start: values.start, end: values.due });
    const remoteChanged = draft.selectTitle || draft.descriptionAction !== "retain" || draft.locationAction !== "retain" || draft.selectTime;
    const pinChanged = values.important !== item.important;
    const tagNames = values.tags;
    if (tagNames.length > 32 || tagNames.some((tag) => Array.from(tag).length > 64)) { errorMessage = translate(language, "tagLimitExceeded"); return false; }
    const recurrence = recurrenceInput(values.recurrence, values.recurrenceInterval, values.options);
    const metadataChanged = tagNames.join("\u0000") !== item.tags.map((tag) => tag.name).join("\u0000") || recurrenceChanged(recurrence, item.recurrence);
    if (!remoteChanged && !pinChanged && !metadataChanged) { if (closeAfter && current()) detailEditing = false; return true; }
    const calendar = calendarSettings.find((calendar) => calendar.calendarId === item.calendarId);
    let remoteRequest: ReturnType<typeof beginRemoteWrite> | null = null;
    if (remoteChanged) {
      const reason = remoteBlockedReason(calendar, item);
      if (reason || !calendar) { errorMessage = reason ?? translate(language, "calendarUnavailableRefresh"); return false; }
      if (!validateRemoteWrite("updateEventV3", calendar, item, draft)) return false;
      draft.base = selectedBase(item, draft);
      if (pinChanged || metadataChanged) draft.metadata = { resourceId: item.resourceId, tagNames, recurrence, important: pinChanged ? values.important : undefined };
      remoteRequest = beginRemoteWrite("updateEventV3", calendar, item, draft, "saveAction");
    }
    if (remoteRequest) {
      const result = await remoteRequest;
      if (!result.started) {
        return false;
      }
      const accepted = { ...applyRemoteDraft(item, draft), important: values.important, tags: tagNames.map((name) => ({ tagId: name, name })), recurrence };
      if (session) session.calendar = accepted;
      if (current()) {
        submittedCalendar = accepted;
        if (closeAfter) detailEditing = false;
      }
      if (!result.terminalFailed) showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
      return true;
    }
    const saved = await persistMetadata();
    const accepted = { ...item, important: values.important, tags: tagNames.map((name) => ({ tagId: name, name })), recurrence };
    if (saved && session) session.calendar = accepted;
    if (saved && current()) {
      submittedCalendar = accepted;
      if (closeAfter) detailEditing = false;
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
    }
    return saved;

    async function persistMetadata() {
      try {
        await updateCalendarLocalMetadata({ resourceId: item.resourceId, tagNames, recurrence, important: pinChanged ? values.important : undefined });
        await refreshCalendar();
        return true;
      } catch (failure) { void reportActionFailure("saveAction", "localMetadata", diagnosticFrom(failure)); return false; }
    }
  }
  async function commitRemoteDeletion(item: CalendarAgendaItem, calendar: CalendarSettingsView) {
    const draft = { ...initialDraft(item), confirmed: true };
    const result = await beginRemoteWrite("delete", calendar, item, draft, "deleteAction", () => finishStagedRemoteDeletion(item));
    finishStagedRemoteDeletion(item);
  }
  async function removeRemoteEvent(item: CalendarAgendaItem) {
    const calendar = calendarSettings.find((calendar) => calendar.calendarId === item.calendarId);
    const reason = remoteBlockedReason(calendar, item);
    if (reason || !calendar || item.pendingRemote || detailSessions.get(`calendar:${item.resourceId}:${item.recurrenceKey}`)?.saver.pending) { errorMessage = reason ?? translate(language, "pendingRemoteSave"); return; }
    const draft = { ...initialDraft(item), confirmed: true };
    if (!validateRemoteWrite("delete", calendar, item, draft)) return;
    stageRemoteDeletion(item, calendar);
    if (detailModal === "calendar" && selectedCalendarResourceId === item.resourceId) { cancelQueuedDetailSave(); detailModal = null; detailEditing = false; }
  }
  $effect(() => { if (!createModal && detailModal !== "calendar") resetRemoteEditor(); });
  let newLocalCalendarName = $state(""); let newLocalCalendarColor = $state("#466FA8");
  let settings = $state<AppSettings | null>(null);
  let accounts = $state<RuntimeAccountProfile[]>([]);
  let syncStates = $state<Map<string, SyncExecutionView>>(new Map());
  let attachmentStates = $state<Map<string, AttachmentView>>(new Map());
  let mutationViews = $state<Map<string, MutationApplicationView>>(new Map());
  let discoveringIds = $state<Set<string>>(new Set());
  let syncingIds = $state<Set<string>>(new Set());
  let currentView = $state<"agenda" | "settings">("agenda");
  let settingsTab = $state<"general" | "calendars" | "desktop" | "about">("general");
  const settingsTabs = ["general", "calendars", "desktop", "about"] as const;
  let selectedSettingsAccountId = $state("local");
  let selectedSettingsCalendarId = $state<string | null>(null);
  let accountModal = $state(false);
  let settingsLanguage = $state<AppLanguage>("system");
  let settingsThemePalette = $state<ThemePalette>("quietDaybook");
  let settingsColorMode = $state<ColorMode>("light");
  let appVersion = $state("…");
  let openingProductPage = $state(false);
  let aboutMessage = $state("");
  let checkingUpdate = $state(false);
  let availableUpdateVersion = $state<string | null>(null);
  let licensesOpen = $state(false);
  let errorLogModal = $state(false);
  let errorLogContent = $state("");
  let errorLogLoading = $state(false);
  let errorLogClearing = $state(false);
  let errorLogReadError = $state("");
  const language = $derived(resolveLanguage(settingsLanguage));
  const text = (key: MessageKey, values: Record<string, string | number> = {}) => Object.entries(values).reduce((message, [name, value]) => message.replaceAll(`{${name}}`, String(value)), translate(language, key) as string);
  let currentTime = $state(Date.now());
  const today = $derived(new Date(currentTime));
  const todayDayNumber = $derived(String(today.getDate()));
  const todayWeekday = $derived(new Intl.DateTimeFormat(language, { weekday: "long" }).format(today));
  const todayMonthYear = $derived(new Intl.DateTimeFormat(language, { year: "numeric", month: "long" }).format(today));
  $effect(() => { setActiveLanguage(language); document.documentElement.lang = language; });
  // Palette and color mode are presentation only. Draft changes apply
  // immediately; system mode remains live through the media-query listener.
  $effect(() => { applyTheme(settingsThemePalette, settingsColorMode); });
  let settingsMode = $state<WindowMode>("desktopCompanion");
  let settingsSyncInterval = $state(15);
  let settingsSyncOnStartup = $state(true);
  let settingsDesktopOpacity = $state(82);
  let settingsDesktopShowCalendarName = $state(true);
  let settingsDesktopShowTime = $state(true);
  let settingsDesktopShowTags = $state(true);
  let settingsMainShowCalendarName = $state(true);
  let settingsMainShowTime = $state(true);
  let settingsMainShowTags = $state(true);
  let settingsStartHiddenInTray = $state(false);
  let settingsLaunchAtLogin = $state(false);
  let settingsLocalTodoColor = $state("#2F7D68");
  let settingsLocalDisplayPastDays = $state<number | undefined>(undefined);
  let settingsLocalDisplayFutureDays = $state<number | undefined>(undefined);
  let settingsDefaultCalendarId = $state("");
  let settingsReminderNotifications = $state(true);
  let savingSettings = $state(false);
  let settingsSaveRequested = false;
  let cleaningRemoteCache = $state(false);
  let accountLabel = $state("");
  let accountEmail = $state("");
  let accountPassword = $state("");
  let accountRegion = $state("icloud-cn");
  let savingAccount = $state(false);
  let credentialAccountId = $state<string | null>(null);
  let credentialUsername = $state("");
  let credentialPassword = $state("");
  function moveSettingsTab(event: KeyboardEvent) {
    const current = settingsTabs.indexOf(settingsTab);
    const next = event.key === "Home" ? 0 : event.key === "End" ? settingsTabs.length - 1 : event.key === "ArrowRight" || event.key === "ArrowDown" ? (current + 1) % settingsTabs.length : event.key === "ArrowLeft" || event.key === "ArrowUp" ? (current - 1 + settingsTabs.length) % settingsTabs.length : -1;
    if (next < 0) return;
    event.preventDefault(); settingsTab = settingsTabs[next];
    requestAnimationFrame(() => document.getElementById(`settings-tab-${settingsTab}`)?.focus());
  }
  async function openAboutPage(kind: "releases" | "repository") {
    if (openingProductPage) return;
    openingProductPage = true;
    try {
      await (kind === "releases" ? openLatestRelease() : openProductRepository());
      showActionToast(translate(language, kind === "releases" ? "releasePageOpened" : "repositoryOpened"), "success");
    } catch {
      showActionToast(translate(language, "externalPageOpenFailed"), "error");
    } finally { openingProductPage = false; }
  }
  let updateCheckInFlight: ReturnType<typeof checkForUpdate> | null = null;
  async function checkAboutUpdate(interactive = true) {
    if (interactive && checkingUpdate) return;
    if (interactive) { checkingUpdate = true; aboutMessage = ""; }
    const request = updateCheckInFlight ??= checkForUpdate();
    try {
      const result = await request;
      availableUpdateVersion = result.status === "available" ? result.version : null;
      if (interactive) aboutMessage = result.status === "available" ? text("updateAvailable", { version: result.version }) : translate(language, "updateUpToDate");
    } catch (failure) {
      if (interactive) {
        aboutMessage = translate(language, "updateCheckFailed");
        try { await recordUiError("checkForUpdate", typeof failure === "object" && failure !== null && "code" in failure && typeof failure.code === "string" ? failure.code : "updateCheckFailed", diagnosticFrom(failure)); aboutMessage += ` ${translate(language, "errorSavedToLog")}`; } catch { /* The failure remains visible when logging is unavailable. */ }
      }
    } finally { if (updateCheckInFlight === request) updateCheckInFlight = null; if (interactive) checkingUpdate = false; }
  }
  function useUpdateAction() {
    if (availableUpdateVersion) void openAboutPage("releases");
    else void checkAboutUpdate();
  }
  let title = $state("");
  let priority = $state<Priority>("normal");
  let createStart = $state(""); let createDue = $state("");
  let createDueEdited = $state(false);
  let createNotes = $state(""); let createLocation = $state("");
  let createImportant = $state(false); let createTags = $state<string[]>([]);
  let createRecurrence = $state<"none" | RecurrenceFrequency>("none"); let createInterval = $state(1);
  let createRecurrenceOptions = $state<RecurrenceOptions>(defaultRecurrenceOptions());
  let itemOrder = $state<string[]>([]);
  let previewOrder = $state<string[] | null>(null);
  let draggingKey = $state<string | null>(null);
  let dragTargetKey = $state<string | null>(null);
  let dragPointer: number | null = null;
  let dragHandle: HTMLElement | null = null;
  let reorderMenu = $state<{ key: string; x: number; y: number; anchor: HTMLButtonElement } | null>(null);
  let deleteMenu = $state<{ entry: AgendaEntry; x: number; y: number; opener: HTMLElement | null } | null>(null);
  let recurrenceDeleteTodo = $state<LocalTodo | null>(null);
  let dragScroll: ReturnType<typeof startReorderAutoScroll> | null = null;
  let dragSession = 0;
  let dragPreviewRevision = 0;
  let createModal = $state(false);
  const createBackdropClick = createBackdropClickGuard();
  let detailModal = $state<"todo" | "calendar" | null>(null);
  let detailEditing = $state(false);
  let detailSaving = $state(false);
  let submittedTodo = $state<LocalTodo | null>(null);
  let submittedCalendar = $state<CalendarAgendaItem | null>(null);
  let activeDetailField = $state<"title" | "calendar" | "time" | "location" | "notes" | "metadata">("title");
  let agendaFilter = $state<"all" | "today" | "incomplete">("all");
  let railPinnedOpen = $state(false);
  let railHoverOpen = $state(false);
  let railHoverSuppressed = $state(false);
  let railHoverKey = $state("");
  const railOpen = $derived(railPinnedOpen || railHoverOpen);
  let hiddenCalendarFilters = $state<Set<string>>(new Set());
  let selectedTodoId = $state<string | null>(null);
  let selectedTodoOccurrenceKey = $state<string | null>(null);
  let selectedCalendarKey = $state<string | null>(null);
  let selectedCalendarResourceId = $state<string | null>(null);
  let createTitleInput = $state<HTMLInputElement>();
  const selectedTodo = $derived(detailEditing && submittedTodo ? submittedTodo : todos.find((item) => item.todoId === selectedTodoId && item.occurrenceKey === selectedTodoOccurrenceKey) ?? null);
  const selectedCalendar = $derived(detailEditing && submittedCalendar ? submittedCalendar : calendarItems.find((item) => item.resourceId === selectedCalendarResourceId && `${item.seriesId}:${item.recurrenceKey}` === selectedCalendarKey) ?? null);
  type AgendaEntry = { key: string; kind: "calendar"; item: CalendarAgendaItem } | { key: string; kind: "todo"; item: LocalTodo };
  const todoEntryKey = (item: LocalTodo) => `todo:${item.todoId}:${item.occurrenceKey}`;
  const calendarEntryKey = (item: CalendarAgendaItem) => `calendar:${item.seriesId}:${item.recurrenceKey}`;
  const displayedTodos = $derived(todos.filter((item) => {
    const calendar = localCalendars.find((candidate) => candidate.calendarId === item.calendarId);
    return withinCalendarDisplayRange(item.startAtUtcMs, item.dueAtUtcMs, calendar?.displayPastDays ?? null, calendar?.displayFutureDays ?? null);
  }));
  const agendaEntries = $derived.by(() => {
    const all: AgendaEntry[] = [...calendarItems.map((item): AgendaEntry => ({ key: calendarEntryKey(item), kind: "calendar", item })), ...displayedTodos.map((item): AgendaEntry => ({ key: todoEntryKey(item), kind: "todo", item }))];
    const ranks = new Map((previewOrder ?? itemOrder).map((key, index) => [key, index]));
    return all.sort((a, b) => Number(b.item.important) - Number(a.item.important) || (ranks.has(a.key) || ranks.has(b.key) ? (ranks.get(a.key) ?? Number.MAX_SAFE_INTEGER) - (ranks.get(b.key) ?? Number.MAX_SAFE_INTEGER) : (entryTime(a) ?? Number.MAX_SAFE_INTEGER) - (entryTime(b) ?? Number.MAX_SAFE_INTEGER)) || a.key.localeCompare(b.key));
  });
  const sidebarCalendars = $derived([
    ...visibleLocalCalendars.map((calendar) => ({ id: calendar.calendarId, name: calendar.name, color: calendar.color })),
    ...calendarSettings.filter((calendar) => calendar.visible && calendar.available).map((calendar) => ({ id: calendar.calendarId, name: calendar.displayName, color: calendar.effectiveColor ?? "#2F7D68" }))
  ]);
  function entryTime(entry: AgendaEntry): number | null { return entry.kind === "calendar" ? entry.item.startAtUtcMs : entry.item.startAtUtcMs ?? entry.item.dueAtUtcMs ?? null; }
  function isToday(timestamp: number | null): boolean {
    if (timestamp === null) return true;
    const value = new Date(timestamp);
    return value.getFullYear() === today.getFullYear() && value.getMonth() === today.getMonth() && value.getDate() === today.getDate();
  }
  const filteredAgendaEntries = $derived.by(() => agendaEntries.filter((entry) => {
    if (hiddenCalendarFilters.has(entry.item.calendarId)) return false;
    const timestamp = entryTime(entry);
    if (agendaFilter === "today") return isToday(timestamp);
    if (agendaFilter === "incomplete") return !entry.item.completed;
    return true;
  }).sort((a, b) => Number(a.item.completed) - Number(b.item.completed) || (a.item.completed && b.item.completed ? (a.item.completedAtMs ?? 0) - (b.item.completedAtMs ?? 0) : 0)));
  const timelineEntries = $derived.by(() => {
    const now = currentTime;
    const timed = agendaEntries
      .filter((entry) => !hiddenCalendarFilters.has(entry.item.calendarId) && !entry.item.completed && timelineFocusTime(entry, now) !== null)
      .map((entry) => ({ ...entry, time: timelineFocusTime(entry, now)!, end: entry.kind === "calendar" ? entry.item.endAtUtcMs : entry.item.startAtUtcMs !== null ? entry.item.dueAtUtcMs : null }));
    return selectTimelineEntries(timed, now, isToday, 5, { todayOnly: agendaFilter === "today" });
  });
  const currentTimePosition = $derived(timelineNowPosition(timelineEntries, currentTime));
  const nearestEntry = $derived.by(() => {
    const now = currentTime;
    return [...filteredAgendaEntries].filter((entry) => !entry.item.completed).sort((a, b) => {
      const aTime = entryTime(a), bTime = entryTime(b);
      const aRank = aTime !== null && aTime >= now ? aTime - now : Number.MAX_SAFE_INTEGER;
      const bRank = bTime !== null && bTime >= now ? bTime - now : Number.MAX_SAFE_INTEGER;
      return aRank - bRank;
    })[0] ?? filteredAgendaEntries[0] ?? null;
  });
  const activeDetailEntry = $derived(
    detailModal === "todo" && selectedTodo ? ({ key: todoEntryKey(selectedTodo), kind: "todo", item: selectedTodo } satisfies AgendaEntry) :
    detailModal === "calendar" && selectedCalendar ? ({ key: calendarEntryKey(selectedCalendar), kind: "calendar", item: selectedCalendar } satisfies AgendaEntry) : nearestEntry
  );
  let loading = $state(true);
  let submitting = $state(false);
  let busyIds = $state<Set<string>>(new Set());
  let completionChanges = $state<Map<string, CompletionDirection>>(new Map());
  const localCompletionEvents = new CompletionEventGate<LocalTodoCompletionChanged>();
  const calendarCompletionEvents = new CompletionEventGate<CalendarCompletionChanged>();
  let errorMessage = $state("");
  let errorLogAvailable = $state(false);
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
  let editingId = $state<string | null>(null);
  let editTitle = $state("");
  let editNotes = $state("");
  let editLocation = $state("");
  let editStart = $state("");
  let editDue = $state("");
  let editPriority = $state<Priority>("normal");
  let editImportant = $state(false);
  let editTags = $state<string[]>([]);
  let editRecurrence = $state<"none" | RecurrenceFrequency>("none");
  let editRecurrenceInterval = $state(1);
  let editRecurrenceOptions = $state<RecurrenceOptions>(defaultRecurrenceOptions());
  let calendarEditingId = $state<string | null>(null);
  const calendarColorPresets = [
    { value: "#2F7D68", label: "pineGreen" }, { value: "#2F7F8F", label: "lakeBlue" },
    { value: "#466FA8", label: "skyBlue" }, { value: "#6657A8", label: "irisPurple" },
    { value: "#A8526B", label: "berryRed" }, { value: "#B8644C", label: "terracotta" },
    { value: "#A9782F", label: "amber" }, { value: "#68747D", label: "mistGray" }
  ] as const;
  const pendingCount = $derived(todos.filter((todo) => !todo.completed).length + calendarItems.filter((item) => item.status === "ready" && !item.completed).length);

  onMount(() => {
    void getAppVersion().then((value) => appVersion = value);
    const updateTimer = setTimeout(() => { void checkAboutUpdate(false); }, 3_000);
    const clockTimer = setInterval(() => { currentTime = Date.now(); }, 30_000);
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let unlistenSync: (() => void) | undefined;
    let unlistenAttachment: (() => void) | undefined;
    let unlistenLocal: (() => void) | undefined;
    let unlistenSettings: (() => void) | undefined;
    let unlistenSource: (() => void) | undefined;
    let unlistenSourceRequired: (() => void) | undefined;
    let unlistenAgendaData: (() => void) | undefined;
    let unlistenMutations: (() => void) | undefined;
    void listen<CalendarCompletionChanged | null>("calendar-todo://agenda-data-changed", (event) => {
      const change = event.payload;
      if (change?.item) {
        const key = `calendar:${change.seriesId}:${change.previousRecurrenceKey}`;
        const accepted = calendarCompletionEvents.receive(key, change);
        if (accepted) void applyCalendarCompletionChange(accepted);
      } else void Promise.all([refresh(), refreshCalendar(), refreshCalendarSettings(), refreshOrder()]);
    }).then((stop) => { if (disposed) stop(); else unlistenAgendaData = stop; });
    void listen<LocalTodoCompletionChanged | null>("calendar-todo://local-todo-changed", (event) => {
      const change = event.payload;
      if (change?.item) {
        const key = `todo:${change.todoId}:${change.previousOccurrenceKey}`;
        const accepted = localCompletionEvents.receive(key, change);
        if (accepted) void applyLocalCompletionChange(accepted);
      } else void Promise.all([refresh(), refreshLocalCalendars()]);
    }).then((stop) => { if (disposed) stop(); else unlistenLocal = stop; });
    void listen("calendar-todo://settings-changed", () => { void getAppSettings().then((value) => { settings = value; if (!savingSettings) restoreSettingsDraft(value); }); }).then((stop) => { if (disposed) stop(); else unlistenSettings = stop; });
    void listen("calendar-todo://source-mode-changed", () => { void Promise.all([refreshCalendarSettings(), listAttachmentStates(), refreshAccountRuntime(), refreshCalendar()]).then(([, attachments, [profiles, states]]) => { attachmentStates = new Map(attachments.map((state) => [state.accountId, state])); accounts = profiles; syncStates = states; }).catch(() => { errorMessage = translate(language, "accountConnectionRefreshFailed"); }); }).then((stop) => { if (disposed) stop(); else unlistenSource = stop; });
    void listen("calendar-todo://source-selection-required", () => { currentView = "settings"; settingsTab = "calendars"; showActionToast(translate(language, "selectIcloudAccount"), "info"); }).then((stop) => { if (disposed) stop(); else unlistenSourceRequired = stop; });
    void listen<AgendaRefreshNotice>("calendar-todo://agenda-refresh", () => {
      void Promise.all([refreshCalendar(), refreshCalendarSettings()]).catch(() => { errorMessage = translate(language, "calendarRangeReloadFailed"); });
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; });
    void listen<SyncExecutionView>("calendar-todo://sync-state", (event) => {
      syncStates = new Map(syncStates).set(event.payload.accountId, event.payload);
    }).then((stop) => { if (disposed) stop(); else unlistenSync = stop; });
    void listen<AttachmentView>("calendar-todo://attachment-state", (event) => {
      attachmentStates = new Map(attachmentStates).set(event.payload.accountId, event.payload);
      if (event.payload.state !== "attaching") {
        void Promise.all([refreshCalendarSettings(), refreshCalendar()]).catch(() => { errorMessage = translate(language, "accountStatusCalendarReloadFailed"); });
        void refreshAccountRuntime().then(([profiles, states]) => { accounts = profiles; syncStates = states; });
      }
    }).then((stop) => { if (disposed) stop(); else unlistenAttachment = stop; });
    void listen<MutationApplicationView>("calendar-todo://remote-mutation-application", (event) => {
      mutationViews = new Map(mutationViews).set(event.payload.accountId, event.payload);
    }).then((stop) => { if (disposed) stop(); else unlistenMutations = stop; });
    void load();
    return () => { cancelReorder(); disposed = true; clearTimeout(updateTimer); clearInterval(clockTimer); unlisten?.(); unlistenSync?.(); unlistenAttachment?.(); unlistenLocal?.(); unlistenSettings?.(); unlistenSource?.(); unlistenSourceRequired?.(); unlistenAgendaData?.(); unlistenMutations?.(); };
  });

  async function load() {
    loading = true; errorMessage = "";
    try {
      const loaded = await Promise.all([getBootstrapState(), listLocalTodos(), listCalendarAgendaItems(), listCalendarSettings(), getAppSettings(), listAccountProfiles(), listAccountSyncStates(), listAttachmentStates(), getDesktopItemOrder(), listLocalCalendars()]);
      [bootstrap, todos, calendarItems, calendarSettings, settings, accounts] = loaded;
      selectedSettingsAccountId = accounts[0]?.accountId ?? "local";
      syncStates = new Map(loaded[6].map((state) => [state.accountId, state]));
      attachmentStates = new Map(loaded[7].map((state) => [state.accountId, state]));
      mutationViews = new Map((await Promise.all(accounts.map((account) => getRemoteMutationView(account.accountId)))).map((view) => [view.accountId, view]));
      itemOrder = loaded[8]; localCalendars = loaded[9]; selectedSettingsCalendarId = null; createCalendarId = preferredCreateCalendarId(); const initialStart = Date.now(); createStart = toLocalInput(initialStart); createDue = toLocalInput(initialStart + 3_600_000);
      settingsLanguage = settings.language; settingsThemePalette = settings.themePalette; settingsColorMode = settings.colorMode; settingsMode = settings.windowMode; settingsSyncInterval = settings.syncIntervalMinutes; settingsSyncOnStartup = settings.syncOnStartup; settingsDesktopOpacity = settings.desktopOpacityPercent; settingsDesktopShowCalendarName = settings.desktopShowCalendarName; settingsDesktopShowTime = settings.desktopShowTime; settingsDesktopShowTags = settings.desktopShowTags; settingsMainShowCalendarName = settings.mainShowCalendarName; settingsMainShowTime = settings.mainShowTime; settingsMainShowTags = settings.mainShowTags; settingsStartHiddenInTray = settings.startHiddenInTray; settingsLaunchAtLogin = settings.launchAtLogin; settingsLocalTodoColor = settings.localTodoColor; settingsLocalDisplayPastDays = settings.localDisplayPastDays ?? undefined; settingsLocalDisplayFutureDays = settings.localDisplayFutureDays ?? undefined; settingsDefaultCalendarId = settings.defaultCalendarId ?? preferredCreateCalendarId(); settingsReminderNotifications = settings.reminderNotificationsEnabled;
    }
    catch (error) { errorMessage = messageFrom(error, translate(language, "agendaLoadFailed")); }
    finally { loading = false; }
  }

  async function refresh() { todos = await listLocalTodos(); }
  async function refreshLocalCalendars() { localCalendars = await listLocalCalendars(); if (!createCalendarId || localCalendars.find((calendar) => calendar.calendarId === createCalendarId)?.hidden) createCalendarId = visibleLocalCalendars[0]?.calendarId ?? ""; }
  async function persistLocalCalendar(calendar: LocalCalendar, name = calendar.name, color = calendar.color, displayPastDays = calendar.displayPastDays, displayFutureDays = calendar.displayFutureDays) { try { await saveLocalCalendar({ calendarId: calendar.calendarId, name, color, displayPastDays, displayFutureDays }); await Promise.all([refreshLocalCalendars(), refresh()]); } catch (error) { await reportFailure("save-local-calendar", error, translate(language, "localCalendarSaveFailed")); } }
  async function addLocalCalendar() { if (!newLocalCalendarName.trim()) return; try { const created = await saveLocalCalendar({ calendarId: null, name: newLocalCalendarName, color: newLocalCalendarColor, displayPastDays: null, displayFutureDays: null }); newLocalCalendarName = ""; await refreshLocalCalendars(); createCalendarId = created.calendarId; } catch (error) { await reportFailure("create-local-calendar", error, translate(language, "localCalendarCreateFailed")); } }
  async function toggleLocalCalendarHidden(calendar: LocalCalendar, hidden: boolean) { try { await setLocalCalendarHidden({ calendarId: calendar.calendarId, hidden }); await Promise.all([refreshLocalCalendars(), refresh()]); } catch (error) { await refreshLocalCalendars(); await reportFailure("toggle-local-calendar", error, translate(language, "localCalendarVisibilityFailed")); } }
  async function removeLocalCalendar(calendar: LocalCalendar) { if (!window.confirm(text("confirmDeleteLocalCalendar", { name: calendar.name }))) return; try { const result = await deleteLocalCalendar({ calendarId: calendar.calendarId }); await Promise.all([refreshLocalCalendars(), refresh()]); showActionToast(result.movedTodos ? text("localCalendarDeletedMoved", { count: result.movedTodos }) : translate(language, "localCalendarDeleted"), "success"); } catch (error) { await reportFailure("delete-local-calendar", error, translate(language, "localCalendarDeleteFailed")); } }
  function remoteDeleteKey(item: CalendarAgendaItem): string { return `${item.seriesId}:${item.recurrenceKey}`; }
  async function refreshCalendar() { calendarItems = (await listCalendarAgendaItems()).filter((item) => !stagedRemoteDeleteKeys.has(remoteDeleteKey(item))); }
  async function refreshCalendarSettings() { calendarSettings = await listCalendarSettings(); }
  async function saveCalendarColor(calendarId: string, color: string | null) {
    clearMessages();
    try { await setCalendarColorOverride(calendarId, color); await Promise.all([refreshCalendar(), refreshCalendarSettings()]); }
    catch (error) { await reportFailure("save-calendar-color", error, translate(language, "calendarColorSaveFailed")); }
  }
  async function saveCalendarVisibility(calendar: CalendarSettingsView, visible: boolean) { try { await setCalendarVisible(calendar.accountId, calendar.calendarId, visible); await Promise.all([refreshCalendar(), refreshCalendarSettings()]); } catch (error) { await refreshCalendarSettings(); await reportFailure("toggle-calendar-sync", error, translate(language, "calendarVisibilityFailed")); } }
  async function saveCalendarRange(form: HTMLFormElement, calendar: CalendarSettingsView) {
    clearMessages();
    const data = new FormData(form);
    const parseBound = (name: string) => { const raw = String(data.get(name) ?? "").trim(); return raw === "" ? null : Number(raw); };
    const displayPastDays = parseBound("displayPastDays");
    const displayFutureDays = parseBound("displayFutureDays");
    if (displayPastDays !== null && (!Number.isInteger(displayPastDays) || displayPastDays < 0 || displayPastDays > 3650) || displayFutureDays !== null && (!Number.isInteger(displayFutureDays) || displayFutureDays < 0 || displayFutureDays > 3650)) { errorMessage = translate(language, "displayRangeInvalid"); return; }
    try { await setCalendarDisplayRange(calendar.accountId, calendar.calendarId, displayPastDays, displayFutureDays); await Promise.all([refreshCalendar(), refreshCalendarSettings()]); }
    catch (error) { await reportFailure("save-calendar-range", error, translate(language, "displayRangeSaveFailed")); }
  }
  async function saveLocalCalendarRange(form: HTMLFormElement, calendar: LocalCalendar) {
    const data = new FormData(form);
    const parseBound = (name: string) => { const raw = String(data.get(name) ?? "").trim(); return raw === "" ? null : Number(raw); };
    const displayPastDays = parseBound("displayPastDays");
    const displayFutureDays = parseBound("displayFutureDays");
    if (displayPastDays !== null && (!Number.isInteger(displayPastDays) || displayPastDays < 0 || displayPastDays > 3650) || displayFutureDays !== null && (!Number.isInteger(displayFutureDays) || displayFutureDays < 0 || displayFutureDays > 3650)) { errorMessage = translate(language, "displayRangeInvalid"); return; }
    await persistLocalCalendar(calendar, calendar.name, calendar.color, displayPastDays, displayFutureDays);
  }
  async function discoverCalendars(accountId: string): Promise<boolean> {
    discoveringIds = new Set(discoveringIds).add(accountId);
    try {
      calendarSettings = [...calendarSettings.filter((calendar) => calendar.accountId !== accountId), ...await discoverRemoteCalendars(accountId)];
      return true;
    } catch (error) {
      await reportFailure("discover-calendars", error, translate(language, "remoteCalendarsRefreshFailed"));
      return false;
    } finally {
      const next = new Set(discoveringIds); next.delete(accountId); discoveringIds = next;
    }
  }
  let resettingOrder = $state(false);
  let orderRevision = 0;
  async function refreshOrder() {
    const revision = ++orderRevision;
    const order = await getDesktopItemOrder();
    if (revision === orderRevision) itemOrder = order;
  }
  async function resetOrder() {
    if (resettingOrder) return;
    resettingOrder = true;
    cancelReorder(); closeReorderMenu();
    ++orderRevision;
    try { await resetDesktopItemOrder(); await refreshOrder(); }
    catch (error) { await reportFailure("reset-order", error, translate(language, "orderSaveFailedWithPeriod")); }
    finally { resettingOrder = false; }
  }

  async function toggleRemoteEditing(account: RuntimeAccountProfile, enabled: boolean) {
    markBusy(account.accountId, true); clearMessages();
    try {
      await setAccountRemoteEditingEnabled(account.accountId, enabled);
      accounts = await listAccountProfiles();
      const view = await getRemoteMutationView(account.accountId);
      mutationViews = new Map(mutationViews).set(account.accountId, view);
      await Promise.all([refreshCalendarSettings(), refreshCalendar()]);
    } catch (error) { accounts = await listAccountProfiles().catch(() => accounts); await reportFailure("toggle-remote-editing", error, translate(language, "remoteEditingUpdateFailed")); }
    finally { markBusy(account.accountId, false); }
  }

  async function resolveMutationConflict(accountId: string, operationId: string, choice: ConflictResolutionChoice) {
    markBusy(operationId, true); clearMessages();
    try {
      const view = await resolveRemoteMutationConflict(accountId, operationId, choice);
      mutationViews = new Map(mutationViews).set(accountId, view);
      showActionToast(translate(language, choice === "keepRemote" ? "remoteVersionKept" : "localRetryCreated"), "success");
    } catch (error) { await reportFailure("resolve-remote-conflict", error, translate(language, "remoteConflictResolveFailed")); }
    finally { markBusy(operationId, false); }
  }

  async function openCreateModal() {
    if (submitting || !leaveDetail()) return;
    detailEditing = false; detailModal = null;
    resetRemoteEditor();
    const initialStart = Date.now(); title = ""; priority = "normal"; createStart = toLocalInput(initialStart); createDue = toLocalInput(initialStart + 3_600_000); createDueEdited = false; createCalendarId = preferredCreateCalendarId(); createNotes = ""; createLocation = ""; createImportant = false; createTags = []; createRecurrence = "none"; createInterval = 1; createRecurrenceOptions = defaultRecurrenceOptions();
    createModal = true; clearMessages();
    await tick(); createTitleInput?.focus();
  }
  function openCalendarModal(item: CalendarAgendaItem) {
    if (!canOpenDetail(`calendar:${item.resourceId}:${item.recurrenceKey}`) || !leaveDetail()) return;
    resetRemoteEditor(); editTitle = item.title; editNotes = item.notes ?? ""; editLocation = item.location ?? ""; editStart = toLocalInput(item.startAtUtcMs, true); editDue = toLocalInput(item.endAtUtcMs, true);
    editImportant = item.important;
    editTags = item.tags.map((tag) => tag.name);
    editRecurrence = item.recurrence?.frequency ?? "none";
    editRecurrenceInterval = item.recurrence?.interval ?? 1;
    editRecurrenceOptions = copyRecurrenceOptions(item.recurrence?.options ?? defaultRecurrenceOptions());
    selectedCalendarKey = `${item.seriesId}:${item.recurrenceKey}`;
    selectedCalendarResourceId = item.resourceId;
    calendarEditingId = selectedCalendarKey;
    detailModal = "calendar"; detailEditing = false; clearMessages();
    activateDetail(`calendar:${item.resourceId}:${item.recurrenceKey}`);
  }
  function openTodoModal(todo: LocalTodo) {
    if (!canOpenDetail(`todo:${todo.todoId}:${todo.occurrenceKey}`) || !leaveDetail()) return;
    selectedTodoId = todo.todoId; selectedTodoOccurrenceKey = todo.occurrenceKey; beginEdit(todo); detailModal = "todo"; detailEditing = false;
    activateDetail(`todo:${todo.todoId}:${todo.occurrenceKey}`);
  }
  async function editActiveDetail(field: typeof activeDetailField = "title") {
    if (!activeDetailEntry || detailSaving) return;
    if (activeDetailEntry.kind === "calendar" && activeDetailEntry.item.pendingRemote && (activeDetailEntry.item.pendingRemote.kind !== "updateEventV3" || activeDetailEntry.item.pendingRemote.status === "conflict")) return;
    if (activeDetailEntry.kind === "calendar") openCalendarModal(activeDetailEntry.item);
    else openTodoModal(activeDetailEntry.item);
    activeDetailField = field;
    submittedTodo = activeDetailEntry.kind === "todo" ? activeDetailEntry.item : null;
    submittedCalendar = activeDetailEntry.kind === "calendar" ? activeDetailEntry.item : null;
    detailEditing = true;
    await tick();
    document.querySelector<HTMLElement>(`[data-detail-field="${field}"] input, [data-detail-field="${field}"] textarea, [data-detail-field="${field}"] select`)?.focus();
  }
  function resetDetailSelection() { if (!leaveDetail()) return; detailModal = null; selectedTodoId = null; selectedTodoOccurrenceKey = null; selectedCalendarKey = null; selectedCalendarResourceId = null; detailEditing = false; }
  function toggleCalendarFilter(calendarId: string) {
    const next = new Set(hiddenCalendarFilters);
    if (next.has(calendarId)) next.delete(calendarId); else next.add(calendarId);
    hiddenCalendarFilters = next;
  }

  function cancelReorder() {
    dragScroll?.stop(); dragScroll = null;
    dragSession++;
    draggingKey = null; dragTargetKey = null; previewOrder = null;
    for (const row of document.querySelectorAll<HTMLElement>(".todo-items li[data-key]")) row.getAnimations({ subtree: true }).forEach((animation) => animation.cancel());
    const handle = dragHandle, pointer = dragPointer;
    dragPointer = null; dragHandle = null;
    if (pointer !== null) { try { handle?.releasePointerCapture(pointer); } catch { /* already released */ } }
  }

  function closeReorderMenu(returnFocus = true) {
    const anchor = reorderMenu?.anchor;
    reorderMenu = null;
    if (returnFocus) queueMicrotask(() => anchor?.isConnected && anchor.focus());
  }
  function openDeleteMenu(event: MouseEvent, entry: AgendaEntry) {
    event.preventDefault(); event.stopPropagation();
    if (submitting || entry.kind === "calendar" && (entry.item.pendingRemote || detailSessions.get(`calendar:${entry.item.resourceId}:${entry.item.recurrenceKey}`)?.saver.pending)) return;
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
    else if (entry.item.recurrence) recurrenceDeleteTodo = entry.item;
    else void remove(entry.item);
  }
  function openReorderMenu(event: MouseEvent | KeyboardEvent, key: string) {
    event.preventDefault(); event.stopPropagation(); cancelReorder();
    const anchor = event.currentTarget as HTMLButtonElement;
    const rect = anchor.getBoundingClientRect();
    const x = event instanceof MouseEvent && event.clientX ? event.clientX : rect.right;
    const y = event instanceof MouseEvent && event.clientY ? event.clientY : rect.bottom;
    reorderMenu = { key, x, y, anchor };
  }
  function reorderMenuKey(event: KeyboardEvent, key: string) {
    if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) openReorderMenu(event, key);
  }
  function reorderMenuDisabled(key: string): Record<ReorderAction, boolean> {
    const entry = filteredAgendaEntries.find((item) => item.key === key);
    const group = filteredAgendaEntries.filter((item) => item.item.important === entry?.item.important).map((item) => item.key);
    const index = group.indexOf(key);
    return { up: index <= 0, top: index <= 0, down: index < 0 || index >= group.length - 1, bottom: index < 0 || index >= group.length - 1 };
  }
  async function chooseReorderAction(action: ReorderAction) {
    if (!reorderMenu || resettingOrder) return;
    const key = reorderMenu.key;
    const entry = agendaEntries.find((item) => item.key === key);
    const visible = filteredAgendaEntries;
    const group = visible.filter((item) => item.item.important === entry?.item.important).map((item) => item.key);
    const keys = reorderKeys(visible.map((item) => item.key), group, key, action);
    const merged = mergeVisibleOrder(itemOrder, agendaEntries.map((item) => item.key), visible.map((item) => item.key), keys);
    closeReorderMenu();
    if (merged.every((item, index) => item === itemOrder[index])) return;
    itemOrder = merged;
    try { await setDesktopItemOrder(merged); showActionToast(text("reorderPosition", { title: entry?.item.title ?? "", position: keys.indexOf(key) + 1, count: keys.length }), "success"); }
    catch (error) { await refreshOrder(); errorMessage = messageFrom(error, translate(language, "orderSaveFailedWithPeriod")); }
  }

  function lostReorderCapture(event: PointerEvent) {
    if (event.pointerId === dragPointer && !dragHandle?.hasPointerCapture(event.pointerId)) cancelReorder();
  }

  function startReorder(event: PointerEvent, key: string) {
    if (event.button !== 0 || dragPointer !== null || resettingOrder) return;
    event.preventDefault(); event.stopPropagation();
    cancelReorder();
    const visibleKeys = filteredAgendaEntries.map((entry) => entry.key);
    draggingKey = key; dragTargetKey = key; previewOrder = visibleKeys;
    const list = document.querySelector<HTMLElement>(".todo-items");
    if (!list) { cancelReorder(); return; }
    // Keyed rows move during preview; capture on the stable list, not a moving button.
    dragPointer = event.pointerId; dragHandle = list;
    try { dragHandle.setPointerCapture(event.pointerId); } catch { /* best effort; window events remain active */ }
    const scroller = document.scrollingElement as HTMLElement | null;
    if (list && scroller) dragScroll = startReorderAutoScroll(scroller, list, event, (point) => { void previewReorder(point, false); });
  }

  function moveReorder(event: PointerEvent) {
    if (!draggingKey || event.pointerId !== dragPointer) return;
    if (event.buttons === 0) { cancelReorder(); return; }
    dragScroll?.update(event);
    void previewReorder(event);
  }

  async function previewReorder(point: DragPoint, animate = true) {
    if (!draggingKey || !previewOrder || point.pointerId !== dragPointer) return;
    const list = document.querySelector<HTMLElement>(".todo-items");
    if (!list) { cancelReorder(); return; }
    const revision = ++dragPreviewRevision;
    // Scrolling must hit-test untransformed rows, even while the pointer is stationary.
    if (!animate) for (const row of list.querySelectorAll<HTMLElement>("li[data-key]")) row.getAnimations({ subtree: true }).forEach((animation) => animation.cancel());
    const rows = [...list.querySelectorAll<HTMLElement>("li[data-key]")];
    const before = new Map(rows.map((row) => [row.dataset.key, row.getBoundingClientRect()]));
    const keys = reorderAtPointer(previewOrder, draggingKey, point.clientY, rows.flatMap((row) => {
      const rect = before.get(row.dataset.key);
      return row.dataset.key && rect ? [{ key: row.dataset.key, midpointY: rect.top + rect.height / 2 }] : [];
    }));
    if (keys.every((key, index) => key === previewOrder?.[index])) return;
    dragTargetKey = keys[keys.indexOf(draggingKey) + 1] ?? "end"; previewOrder = keys;
    const session = dragSession;
    await tick();
    if (!animate || !draggingKey || point.pointerId !== dragPointer || session !== dragSession || revision !== dragPreviewRevision || reducedCompletionMotion()) return;
    for (const row of list.querySelectorAll<HTMLElement>("li[data-key]")) {
      const previous = before.get(row.dataset.key);
      if (!previous) continue;
      const delta = previous.top - row.getBoundingClientRect().top;
      // Move the row's contents, not the bordered row itself: transforming the
      // separator's own box leaves a second line behind while the preview runs.
      if (delta) for (const child of row.children) child.animate([{ transform: `translateY(${delta}px)` }, { transform: "translateY(0)" }], { duration: 170, easing: "ease-out" });
    }
  }

  async function endReorder(event: PointerEvent) {
    if (!draggingKey || event.pointerId !== dragPointer) return;
    const insideList = !!(document.elementFromPoint(event.clientX, event.clientY) as HTMLElement | null)?.closest(".todo-items");
    const keys = event.type === "pointercancel" || !insideList ? null : previewOrder;
    const mergedKeys = keys ? mergeVisibleOrder(itemOrder, agendaEntries.map((entry) => entry.key), filteredAgendaEntries.map((entry) => entry.key), keys) : null;
    const changed = !!mergedKeys && mergedKeys.some((key, index) => key !== itemOrder[index]);
    if (mergedKeys) itemOrder = mergedKeys;
    cancelReorder();
    if (changed && mergedKeys) {
      try { await setDesktopItemOrder(mergedKeys); } catch { await refreshOrder(); errorMessage = translate(language, "orderSaveFailedWithPeriod"); }
    }
  }

  function toggleCalendarDetails(item: CalendarAgendaItem) {
    const itemId = `${item.seriesId}:${item.recurrenceKey}`;
    if (calendarEditingId === itemId) { calendarEditingId = null; return; }
    calendarEditingId = itemId; clearMessages();
  }

  async function applyCalendarCompletionChange(change: CalendarCompletionChanged) {
    const current = calendarItems.find((item) => item.seriesId === change.seriesId && item.recurrenceKey === change.previousRecurrenceKey);
    if (!current) { await refreshCalendar(); return; }
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

  async function toggleCalendar(item: CalendarAgendaItem) {
    if (item.pendingRemote) return;
    const itemId = `${item.seriesId}:${item.recurrenceKey}`;
    if (busyIds.has(itemId)) return;
    const fromKey = calendarEntryKey(item);
    if (completionChanges.has(fromKey)) return;
    const direction: CompletionDirection = item.completed ? "restore" : "complete";
    markBusy(itemId, true); clearMessages();
    completionChanges = new Map(completionChanges).set(fromKey, direction);
    calendarCompletionEvents.begin(fromKey, !item.completed);
    try {
      const updated = await setCalendarCompleted(item.seriesId, item.recurrenceKey, !item.completed);
      await animateCompletionChange(document, fromKey, calendarEntryKey(updated), async () => {
        calendarItems = calendarItems.map((current) => current.seriesId === item.seriesId && current.recurrenceKey === item.recurrenceKey ? updated : current);
        await tick();
      });
      showActionToast(translate(language, item.completed ? "restored" : "completed"), "success");
    }
    catch (error) { errorMessage = messageFrom(error, translate(language, "calendarItemUpdateFailed")); }
    finally {
      const queued = calendarCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      markBusy(itemId, false);
      if (queued) await applyCalendarCompletionChange(queued);
    }
  }

  function restoreSettingsDraft(value: AppSettings) {
    settingsLanguage = value.language; settingsThemePalette = value.themePalette; settingsColorMode = value.colorMode; settingsMode = value.windowMode; settingsSyncInterval = value.syncIntervalMinutes; settingsSyncOnStartup = value.syncOnStartup; settingsDesktopOpacity = value.desktopOpacityPercent; settingsDesktopShowCalendarName = value.desktopShowCalendarName; settingsDesktopShowTime = value.desktopShowTime; settingsDesktopShowTags = value.desktopShowTags; settingsMainShowCalendarName = value.mainShowCalendarName; settingsMainShowTime = value.mainShowTime; settingsMainShowTags = value.mainShowTags; settingsStartHiddenInTray = value.startHiddenInTray; settingsLaunchAtLogin = value.launchAtLogin; settingsLocalTodoColor = value.localTodoColor; settingsLocalDisplayPastDays = value.localDisplayPastDays ?? undefined; settingsLocalDisplayFutureDays = value.localDisplayFutureDays ?? undefined; settingsDefaultCalendarId = value.defaultCalendarId ?? ""; settingsReminderNotifications = value.reminderNotificationsEnabled;
  }
  async function autoSaveSettings() {
    if (!settings) return;
    settingsSaveRequested = true;
    if (savingSettings) return;
    const previous = settings;
    savingSettings = true; clearMessages();
    try {
      while (settingsSaveRequested) {
        settingsSaveRequested = false;
        settings = await updateAppSettings({ language: settingsLanguage, themePalette: settingsThemePalette, colorMode: settingsColorMode, windowMode: settingsMode, syncIntervalMinutes: settingsSyncInterval, syncOnStartup: settingsSyncOnStartup, desktopListMode: settings.desktopListMode, desktopOpacityPercent: settingsDesktopOpacity, desktopShowCalendarName: settingsDesktopShowCalendarName, desktopShowTime: settingsDesktopShowTime, desktopShowTags: settingsDesktopShowTags, mainShowCalendarName: settingsMainShowCalendarName, mainShowTime: settingsMainShowTime, mainShowTags: settingsMainShowTags, startHiddenInTray: settingsStartHiddenInTray, launchAtLogin: settingsLaunchAtLogin, localTodoColor: settingsLocalTodoColor, localDisplayPastDays: settingsLocalDisplayPastDays ?? null, localDisplayFutureDays: settingsLocalDisplayFutureDays ?? null, defaultCalendarId: settingsDefaultCalendarId || null, reminderNotificationsEnabled: settingsReminderNotifications });
      }
      restoreSettingsDraft(settings);
    } catch (error) { settingsSaveRequested = false; restoreSettingsDraft(previous); await reportFailure("save-settings", error, translate(language, "settingsSaveFailed")); }
    finally { savingSettings = false; }
  }

  async function setDesktopListMode(enabled: boolean) {
    if (!settings) return;
    clearMessages();
    try {
      settings = await updateAppSettings({ language: settings.language, themePalette: settings.themePalette, colorMode: settings.colorMode, windowMode: settings.windowMode, syncIntervalMinutes: settings.syncIntervalMinutes, syncOnStartup: settings.syncOnStartup, desktopListMode: enabled, desktopOpacityPercent: settings.desktopOpacityPercent, desktopShowCalendarName: settings.desktopShowCalendarName, desktopShowTime: settings.desktopShowTime, desktopShowTags: settings.desktopShowTags, mainShowCalendarName: settings.mainShowCalendarName, mainShowTime: settings.mainShowTime, mainShowTags: settings.mainShowTags, startHiddenInTray: settings.startHiddenInTray, launchAtLogin: settings.launchAtLogin, localTodoColor: settings.localTodoColor, localDisplayPastDays: settings.localDisplayPastDays, localDisplayFutureDays: settings.localDisplayFutureDays, defaultCalendarId: settings.defaultCalendarId, reminderNotificationsEnabled: settings.reminderNotificationsEnabled });
      editingId = null; calendarEditingId = null;
    } catch (error) { await reportFailure("toggle-desktop-list", error, translate(language, "desktopToggleFailed")); }
  }

  async function cleanExpiredRemoteCache() {
    if (!window.confirm(translate(language, "confirmCleanCache"))) return;
    cleaningRemoteCache = true; clearMessages();
    try {
      const removed = await purgeExpiredRemoteCache();
      showActionToast(removed > 0 ? text("cacheCleaned", { count: removed }) : translate(language, "noCacheToClean"), "success");
    } catch (error) { await reportFailure("clean-remote-cache", error, translate(language, "cacheCleanFailed")); }
    finally { cleaningRemoteCache = false; }
  }

  async function addAccount(event: SubmitEvent) {
    event.preventDefault(); if (!accountEmail.trim() || !accountPassword || !accountRegion.trim()) return;
    savingAccount = true; clearMessages();
    const username = accountEmail.trim();
    let created: RuntimeAccountProfile | null = null;
    let step: "profile" | "credential" | "connection" = "profile";
    try {
      created = await registerAccountProfile({ userLabel: accountLabel.trim() || username, regionProfile: accountRegion, enabled: true });
      accountLabel = ""; accountEmail = "";
      step = "credential";
      await enrollAccountCredential({ accountId: created.accountId, username, password: accountPassword });
      step = "connection";
      const state = await attachAccountSource(created.accountId, "icloudChinaReadOnly");
      attachmentStates = new Map(attachmentStates).set(created.accountId, state);
      [accounts, syncStates] = await refreshAccountRuntime();
      selectedSettingsAccountId = created.accountId; accountModal = false;
    } catch (error) {
      accounts = await listAccountProfiles().catch(() => accounts);
      if (created && !accounts.find((account) => account.accountId === created?.accountId)?.credentialConfigured) {
        credentialAccountId = created.accountId;
        credentialUsername = username;
      }
      const message = step === "profile" ? translate(language, "accountConfigSaveFailed") : step === "credential" ? translate(language, "credentialsSaveFailed") : translate(language, "icloudConnectFailed");
      await reportFailure(`connect-icloud-${step}`, error, message);
    }
    finally { accountPassword = ""; savingAccount = false; }
  }

  async function toggleAccount(account: RuntimeAccountProfile) {
    markBusy(account.accountId, true); clearMessages();
    try {
      await setAccountProfileEnabled(account.accountId, !account.enabled);
      [accounts, syncStates] = await refreshAccountRuntime();
      attachmentStates = new Map((await listAttachmentStates()).map((state) => [state.accountId, state]));
    } catch (error) { await reportFailure("toggle-account", error, translate(language, "accountStatusUpdateFailed")); }
    finally { markBusy(account.accountId, false); }
  }

  async function removeAccount(account: RuntimeAccountProfile) {
    if (!window.confirm(text("confirmRemoveAccount", { name: account.userLabel }))) return;
    markBusy(account.accountId, true); clearMessages();
    try {
      await removeAccountProfile(account.accountId);
      [accounts, syncStates] = await refreshAccountRuntime();
      attachmentStates = new Map((await listAttachmentStates()).map((state) => [state.accountId, state]));
      cancelCredential();
      showActionToast(translate(language, "accountRemoved"), "success");
    } catch (error) { await reportFailure("remove-account", error, translate(language, "accountRemoveFailed")); }
    finally { markBusy(account.accountId, false); }
  }

  function editCredential(accountId: string, username = "") {
    credentialAccountId = accountId; credentialUsername = username; credentialPassword = ""; clearMessages();
  }

  function cancelCredential() {
    credentialAccountId = null; credentialUsername = ""; credentialPassword = "";
  }

  async function saveCredential(event: SubmitEvent, account: RuntimeAccountProfile) {
    event.preventDefault(); if (!credentialUsername || !credentialPassword) return;
    markBusy(account.accountId, true); clearMessages();
    let credentialSaved = false;
    try {
      await enrollAccountCredential({ accountId: account.accountId, username: credentialUsername, password: credentialPassword });
      credentialSaved = true;
      const state = await attachAccountSource(account.accountId, "icloudChinaReadOnly");
      attachmentStates = new Map(attachmentStates).set(account.accountId, state);
      [accounts, syncStates] = await refreshAccountRuntime(); credentialAccountId = null;
    } catch (error) { await reportFailure(credentialSaved ? "connect-icloud" : "save-credentials", error, translate(language, credentialSaved ? "icloudConnectFailed" : "credentialsSaveFailed")); }
    finally { credentialUsername = ""; credentialPassword = ""; markBusy(account.accountId, false); }
  }

  async function attachSource(account: RuntimeAccountProfile, mode: "synthetic" | "icloudChinaReadOnly") {
    markBusy(account.accountId, true); clearMessages();
    try {
      const state = await attachAccountSource(account.accountId, mode);
      attachmentStates = new Map(attachmentStates).set(account.accountId, state);
    } catch (error) { await reportFailure("attach-calendar-source", error, translate(language, mode === "synthetic" ? "sampleCalendarOpenFailed" : "icloudConnectFailed")); }
    finally { markBusy(account.accountId, false); }
  }

  async function syncAccount(account: RuntimeAccountProfile) {
    syncingIds = new Set(syncingIds).add(account.accountId); clearMessages();
    try {
      const state = await triggerAccountSync(account.accountId);
      syncStates = new Map(syncStates).set(account.accountId, state);
    } catch (error) { await reportFailure("sync-account", error, translate(language, "calendarRefreshFailed")); }
    finally { const next = new Set(syncingIds); next.delete(account.accountId); syncingIds = next; }
  }

  async function refreshAccountRuntime(): Promise<[RuntimeAccountProfile[], Map<string, SyncExecutionView>]> {
    const [profiles, states] = await Promise.all([listAccountProfiles(), listAccountSyncStates()]);
    return [profiles, new Map(states.map((state) => [state.accountId, state]))];
  }

  function syncStateLabel(accountId: string): string {
    const state = syncStates.get(accountId);
    if (!state || state.state === "detached" || state.state === "idle") return translate(language, "notSynced");
    if (state.state === "running") return translate(language, state.trigger === "startup" ? "startupSyncRunning" : "manualSyncRunning");
    if (state.state === "succeeded") return translate(language, "updated");
    if (state.state === "actionRequired") return translate(language, "loginUpdateRequired");
    const reasons = {
      transient: translate(language, "syncTransient"),
      unauthorized: translate(language, "syncUnauthorized"),
      incomplete: translate(language, "syncIncomplete"),
      unsafeData: translate(language, "syncUnsafeData"),
      validation: translate(language, "syncValidation"),
      persistence: translate(language, "syncPersistence")
    };
    const reason = state.failure ? reasons[state.failure] : undefined;
    const label = reason ? text("syncFailedReason", { reason }) : translate(language, "syncFailed");
    return state.nextRetryDelayMs ? text("syncRetryDelay", { label, seconds: Math.ceil(state.nextRetryDelayMs / 1000) }) : label;
  }

  function attachmentStateLabel(account: RuntimeAccountProfile): string {
    if (account.status === "disabled") return translate(language, "paused");
    const attachment = attachmentStates.get(account.accountId);
    if (attachment?.state === "attaching") return translate(language, "connectingCalendar");
    if (attachment?.state === "actionRequired") return translate(language, "loginReconfigureRequired");
    if (attachment?.state === "failed") {
      if (attachment.failure === "unavailable") return translate(language, "connectionUnavailable");
      return translate(language, "connectionFailed");
    }
    if (account.status === "ready") return text("calendarReady", { status: syncStateLabel(account.accountId) });
    return translate(language, account.actionRequired === "sourceAttachment" ? "loginSavedAwaitingConnection" : "loginConfigRequired");
  }

  async function create(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    try {
    updateCreateStart(createStart);
    const tagNames = createTags;
    const pinAtSubmit = createImportant;
    if (tagNames.length > 32 || tagNames.some((tag) => Array.from(tag).length > 64)) { errorMessage = translate(language, "tagLimitExceeded"); return; }
    const recurrence = recurrenceInput(createRecurrence, createInterval, createRecurrenceOptions);
    if (remoteCreateCalendar) {
      const reason = remoteBlockedReason(remoteCreateCalendar);
      if (reason) { errorMessage = reason; return; }
      const draft = editorDraft(undefined, { title, notes: createNotes, location: createLocation, start: createStart, end: createDue });
      if (!validateRemoteWrite("createUtcV2", remoteCreateCalendar, undefined, draft)) return;
      let createdResourceId = "";
      let earlySettlement: boolean | undefined;
      const finishCreate = (succeeded: boolean) => {
        if (!createdResourceId) { earlySettlement = succeeded; return; }
        if (!succeeded) return;
        void (async () => {
          try {
            if (tagNames.length || recurrence || pinAtSubmit) {
              await updateCalendarLocalMetadata({ resourceId: createdResourceId, tagNames, recurrence, important: pinAtSubmit ? true : undefined });
              await refreshCalendar();
            }
          } catch (failure) { void reportActionFailure("saveAction", "localMetadata", diagnosticFrom(failure)); }
        })();
      };
      const request = beginRemoteWrite("createUtcV2", remoteCreateCalendar, undefined, draft, "saveAction", finishCreate);
      const provisionalId = `optimistic:${crypto.randomUUID()}`;
      const provisional: CalendarAgendaItem = {
        seriesId: provisionalId, resourceId: provisionalId, recurrenceKey: "SINGLETON", calendarId: remoteCreateCalendar.calendarId,
        calendarName: remoteCreateCalendar.displayName, calendarColor: remoteCreateCalendar.effectiveColor,
        title: title.trim(), notes: createNotes.trim() || null, location: createLocation.trim() || null,
        startAtUtcMs: fromLocalInput(createStart), endAtUtcMs: fromLocalInput(createDue), allDay: false, status: "ready", unsupportedReason: null,
        workItemId: null, completed: false, important: pinAtSubmit, tags: tagNames.map((name) => ({ tagId: `optimistic:${name}`, name })), recurrence,
        subtasks: [], pendingRemote: { operationId: provisionalId, accountId: remoteCreateCalendar.accountId, kind: "createUtcV2", status: "pending" }, remoteEditEligible: false, remoteEditBlockedReason: translate(language, "pendingRemoteSave"), remoteTimeEditEligible: false
      };
      calendarItems = [...calendarItems, provisional];
      const result = await request;
      calendarItems = calendarItems.filter((item) => item.resourceId !== provisionalId);
      if (!result.started) { createModal = true; return; }
      createModal = false;
      if (result.terminalFailed) return;
      createdResourceId = result.receipt.resourceId;
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
      void refreshCalendar();
      if (earlySettlement !== undefined) finishCreate(earlySettlement);
      return;
    }
    if (!visibleLocalCalendars.some((calendar) => calendar.calendarId === createCalendarId)) { errorMessage = translate(language, "selectedCalendarUnavailable"); return; }
    if (!title.trim()) { errorMessage = translate(language, "enterTodoTitle"); return; }
    const startAtUtcMs = fromLocalInput(createStart), dueAtUtcMs = fromLocalInput(createDue);
    if (createRecurrence !== "none" && dueAtUtcMs === null) { errorMessage = translate(language, "recurrenceDueRequiredWithPeriod"); return; }
    clearMessages();
    const calendar = visibleLocalCalendars.find((item) => item.calendarId === createCalendarId)!;
    const optimisticId = `optimistic:${crypto.randomUUID()}`;
    const optimisticTodo: LocalTodo = {
      todoId: optimisticId, calendarId: calendar.calendarId, calendarName: calendar.name,
      calendarColor: calendar.color, workItemId: null, occurrenceKey: "SINGLETON",
      title: title.trim(), notes: createNotes.trim() || null, location: createLocation.trim() || null,
      startAtUtcMs, dueAtUtcMs, priority, important: pinAtSubmit, completed: false,
      recurrence: recurrence ? { seriesId: optimisticId, ...recurrence } : null, backlogCount: 0,
      tags: tagNames.map((name) => ({ tagId: `optimistic:${name}`, name })), subtasks: [],
      createdAtMs: Date.now(), updatedAtMs: Date.now(),
    };
    todos = [...todos, optimisticTodo];
    try {
      const saved = await saveLocalTodo({ todoId: null, calendarId: createCalendarId, title: title.trim(), notes: createNotes.trim() || null, location: createLocation.trim() || null, startAtUtcMs, dueAtUtcMs, priority, important: pinAtSubmit, tagNames, recurrence });
      todos = todos.map((item) => item.todoId === optimisticId ? saved : item);
      createModal = false;
      title = ""; priority = "normal";
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
    } catch (failure) { todos = todos.filter((item) => item.todoId !== optimisticId); void reportActionFailure("saveAction", "localSave", diagnosticFrom(failure)); }
    } finally { submitting = false; }
  }
  function updateCreateStart(value: string) {
    createStart = value;
    const start = fromLocalInput(value), due = fromLocalInput(createDue);
    if (start !== null && (!createDueEdited || due === null || start >= due)) createDue = toLocalInput(start + 3_600_000);
  }
  function updateEditStart(value: string) {
    editStart = value;
    const start = fromLocalInput(value), due = fromLocalInput(editDue);
    if (start !== null && due !== null && start >= due) editDue = toLocalInput(start + 3_600_000, value.length > 16);
  }

  function beginEdit(todo: LocalTodo) {
    editingId = todo.todoId; editTitle = todo.title; editNotes = todo.notes ?? ""; editLocation = todo.location ?? "";
    editStart = toLocalInput(todo.startAtUtcMs); editDue = toLocalInput(todo.dueAtUtcMs); editCalendarId = todo.calendarId;
    editPriority = todo.priority; editImportant = todo.important; editTags = todo.tags.map((tag) => tag.name);
    editRecurrence = todo.recurrence?.frequency ?? "none"; editRecurrenceInterval = todo.recurrence?.interval ?? 1; editRecurrenceOptions = copyRecurrenceOptions(todo.recurrence?.options ?? defaultRecurrenceOptions());
    clearMessages();
  }

  async function saveDetails(todo: LocalTodo, closeAfter = false) {
    if (busyIds.has(todo.todoId)) return;
    if (!editTitle.trim() || !Number.isInteger(Number(editRecurrenceInterval)) || Number(editRecurrenceInterval) < 1 || Number(editRecurrenceInterval) > 365) return;
    updateEditStart(editStart);
    const start = fromLocalInput(editStart); const due = fromLocalInput(editDue);
    if (editRecurrence !== "none" && due === null) { errorMessage = translate(language, "fixedRecurrenceDueRequired"); return; }
    markBusy(todo.todoId, true); clearMessages();
    const calendar = visibleLocalCalendars.find((item) => item.calendarId === editCalendarId);
    const recurrence = recurrenceInput(editRecurrence, editRecurrenceInterval, editRecurrenceOptions);
    const optimistic: LocalTodo = {
      ...todo,
      calendarId: editCalendarId,
      calendarName: calendar?.name ?? todo.calendarName,
      calendarColor: calendar?.color ?? todo.calendarColor,
      title: editTitle,
      notes: editNotes || null,
      location: editLocation || null,
      startAtUtcMs: start,
      dueAtUtcMs: due,
      priority: editPriority,
      important: editImportant,
      tags: editTags.map((name) => todo.tags.find((tag) => tag.name === name) ?? { tagId: `optimistic:${name}`, name }),
      recurrence: recurrence ? { seriesId: todo.recurrence?.seriesId ?? todo.todoId, ...recurrence } : null,
      updatedAtMs: Date.now(),
    };
    todos = todos.map((item) => item.todoId === todo.todoId && item.occurrenceKey === todo.occurrenceKey ? optimistic : item);
    try {
      const saved = await saveLocalTodo({
        todoId: todo.todoId, selectedOccurrenceKey: todo.occurrenceKey, calendarId: editCalendarId, title: editTitle, notes: editNotes || null, location: editLocation || null,
        startAtUtcMs: start, dueAtUtcMs: due, priority: editPriority, important: editImportant,
        tagNames: editTags,
        recurrence
      });
      todos = todos.map((item) => item.todoId === todo.todoId && item.occurrenceKey === todo.occurrenceKey ? saved : item);
      if (closeAfter) { editingId = null; detailEditing = false; }
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
    } catch (failure) { todos = todos.map((item) => item.todoId === todo.todoId && item.occurrenceKey === todo.occurrenceKey ? todo : item); void reportActionFailure("saveAction", "localSave", diagnosticFrom(failure)); }
    finally { markBusy(todo.todoId, false); }
  }

  type DetailSnapshot = { todo: LocalTodo | null; calendar: CalendarAgendaItem | null; values: RemoteDetailValues; calendarId: string; priority: Priority; options: RecurrenceOptions };
  function captureDetailSnapshot(): DetailSnapshot | null {
    if (!detailEditing || !editTitle.trim() || !Number.isInteger(Number(editRecurrenceInterval)) || Number(editRecurrenceInterval) < 1 || Number(editRecurrenceInterval) > 365 || editTags.length > 32 || editTags.some((tag) => [...tag].length > 64)) return null;
    const values = captureRemoteDetailValues();
    if (values.recurrence !== "none" && fromLocalInput(values.due) === null) return null;
    if (values.start && fromLocalInput(values.start) === null || values.due && fromLocalInput(values.due) === null) return null;
    return { todo: detailModal === "todo" ? selectedTodo : null, calendar: detailModal === "calendar" ? selectedCalendar : null, values, calendarId: editCalendarId, priority: editPriority, options: structuredClone($state.snapshot(editRecurrenceOptions)) };
  }
  type DetailSession = { saver: DetailAutosave<DetailSnapshot>; todo: LocalTodo | null; calendar: CalendarAgendaItem | null };
  const detailSessions = new Map<string, DetailSession>();
  let activeDetailIdentity: string | null = null;
  function canOpenDetail(identity: string) {
    if (detailSessions.has(identity) || detailSessions.size < 32) return true;
    for (const [key, session] of detailSessions) if (!session.saver.dirty && !session.saver.pending) detailSessions.delete(key);
    if (detailSessions.size < 32) return true;
    void reportActionFailure("saveAction", "retainedDraftLimit", "resolve retained unsaved event edits before opening another editor"); return false;
  }
  function activateDetail(identity: string) {
    activeDetailIdentity = identity;
    const session = detailSessions.get(identity);
    detailSaving = session?.saver.saving ?? false;
    submittedTodo = session?.todo ?? null; submittedCalendar = session?.calendar ?? null;
    const snapshot = session?.saver.snapshot;
    if (snapshot && (session.saver.dirty || session.saver.pending)) {
      applyRemoteDetailValues(snapshot.values); editRecurrenceOptions = snapshot.options;
      editCalendarId = snapshot.calendarId; editPriority = snapshot.priority; detailEditing = true;
    }
  }
  function currentDetailSaver() {
    const identity = activeDetailIdentity;
    if (!identity) return null;
    let session = detailSessions.get(identity);
    if (!session) {
      session = { todo: submittedTodo, calendar: submittedCalendar, saver: new DetailAutosave<DetailSnapshot>(captureDetailSnapshot, async (snapshot, explicit, latest) => {
        const current = () => activeDetailIdentity === identity && latest();
        if (snapshot.todo) return saveDetailTodo(snapshot, explicit, current, session!);
        if (snapshot.calendar) return saveRemoteDetails(session!.calendar ?? snapshot.calendar, explicit, snapshot.values, current, session);
        return false;
      }, (frozen) => { if (activeDetailIdentity === identity) detailSaving = frozen; }) };
      detailSessions.set(identity, session);
    }
    return session.saver;
  }
  function leaveDetail() {
    if (detailEditing && !currentDetailSaver()?.leave()) {
      void reportActionFailure("saveAction", "invalidDetailDraft", "cannot leave invalid unsaved event fields"); return false;
    }
    activeDetailIdentity = null; detailSaving = false; submittedTodo = null; submittedCalendar = null;
    for (const [identity, session] of detailSessions) if (!session.saver.dirty && !session.saver.pending) detailSessions.delete(identity);
    return true;
  }
  async function saveDetailTodo(snapshot: DetailSnapshot, explicit: boolean, current: () => boolean, session: DetailSession): Promise<boolean> {
    const todo = snapshot.todo!, v = snapshot.values;
    try {
      const saved = await saveLocalTodo({ todoId: todo.todoId, selectedOccurrenceKey: todo.occurrenceKey, calendarId: snapshot.calendarId, title: v.title, notes: v.notes || null, location: v.location || null, startAtUtcMs: fromLocalInput(v.start), dueAtUtcMs: fromLocalInput(v.due), priority: snapshot.priority, important: v.important, tagNames: v.tags, recurrence: recurrenceInput(v.recurrence, v.recurrenceInterval, snapshot.options) });
      todos = todos.map((item) => item.todoId === todo.todoId && item.occurrenceKey === todo.occurrenceKey ? saved : item);
      session.todo = saved;
      if (current()) {
        submittedTodo = saved;
        if (explicit) { editingId = null; detailEditing = false; }
      }
      showActionToast(withoutTerminalPunctuation(text("actionSucceeded", { action: translate(language, "saveAction") })), "success");
      return true;
    } catch (failure) { void reportActionFailure("saveAction", "localSave", diagnosticFrom(failure)); return false; }
  }
  function cancelQueuedDetailSave() { if (activeDetailIdentity) { const session = detailSessions.get(activeDetailIdentity); session?.saver.reset(); if (!session?.saver.pending) detailSessions.delete(activeDetailIdentity); } activeDetailIdentity = null; detailSaving = false; submittedTodo = null; submittedCalendar = null; }
  function cancelDetailEditing() {
    if (detailSaving) return;
    cancelQueuedDetailSave();
    editingId = null;
    detailEditing = false;
  }
  function scheduleDetailAutoSave() {
    if (!detailEditing || detailSaving) return;
    currentDetailSaver()?.changed();
  }
  async function persistActiveDetail(closeAfter: boolean) {
    if (detailSaving) return;
    if (closeAfter) currentDetailSaver()?.save(); else currentDetailSaver()?.changed();
  }
  function submitDetail(event: SubmitEvent) { event.preventDefault(); void persistActiveDetail(true); }

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

  async function toggle(todo: LocalTodo) {
    const fromKey = todoEntryKey(todo);
    if (completionChanges.has(fromKey)) return;
    const direction: CompletionDirection = todo.completed ? "restore" : "complete";
    clearMessages();
    completionChanges = new Map(completionChanges).set(fromKey, direction);
    localCompletionEvents.begin(fromKey, !todo.completed);
    try {
      const updated = await setLocalTodoCompleted(todo.todoId, todo.occurrenceKey, !todo.completed);
      await animateCompletionChange(document, fromKey, todoEntryKey(updated), async () => {
        todos = todos.map((current) => current.todoId === todo.todoId && current.occurrenceKey === todo.occurrenceKey ? updated : current);
        await tick();
      });
      showActionToast(translate(language, todo.completed ? "restored" : "completed"), "success");
    }
    catch (error) { errorMessage = messageFrom(error, translate(language, "completionUpdateFailed")); }
    finally {
      const queued = localCompletionEvents.finish(fromKey);
      const next = new Map(completionChanges); next.delete(fromKey); completionChanges = next;
      if (queued) await applyLocalCompletionChange(queued);
    }
  }

  async function remove(todo: LocalTodo) {
    if (busyIds.has(todo.todoId)) return;
    markBusy(todo.todoId, true); clearMessages();
    try {
      await deleteLocalTodo(todo.todoId);
      todos = todos.filter((item) => item.todoId !== todo.todoId); editingId = null; resetDetailSelection();
      scheduleUndoDeletion({ kind: "local", id: todo.todoId, title: todo.title });
    }
    catch (error) { errorMessage = messageFrom(error, translate(language, "localTodoDeleteFailed")); }
    finally { markBusy(todo.todoId, false); }
  }
  async function removeOccurrence(todo: LocalTodo) {
    if (busyIds.has(todo.todoId)) return;
    markBusy(todo.todoId,true); clearMessages();
    try {
      await deleteLocalTodoOccurrence(todo.todoId,todo.occurrenceKey);
      todos = todos.filter((item) => todoEntryKey(item) !== todoEntryKey(todo));
      scheduleUndoDeletion({ kind:"localOccurrence", id:todo.todoId, occurrenceKey:todo.occurrenceKey, title:todo.title });
    } catch (error) { errorMessage = messageFrom(error,translate(language,"localTodoDeleteFailed")); }
    finally { markBusy(todo.todoId,false); }
  }
  function chooseRecurrenceDelete(scope: "occurrence" | "series") {
    const todo=recurrenceDeleteTodo; recurrenceDeleteTodo=null; if (!todo) return;
    if (scope === "occurrence") void removeOccurrence(todo); else void remove(todo);
  }
  function requestTodoDelete(todo: LocalTodo) { if (todo.recurrence) recurrenceDeleteTodo=todo; else void remove(todo); }
  async function undoDelete(token: number) {
    const item = undoQueue.undo(token);
    if (!item) return;
    if (item.kind === "remote") {
      const next = new Set(stagedRemoteDeleteKeys); next.delete(remoteDeleteKey(item.item)); stagedRemoteDeleteKeys = next;
      await refreshCalendar();
      return;
    }
    try {
      if (item.kind === "localOccurrence") await restoreRecentlyDeletedTodoOccurrence(item.id,item.occurrenceKey);
      else await restoreRecentlyDeletedTodo(item.id);
      await refresh();
    }
    catch (error) { errorMessage = messageFrom(error, translate(language, "undoFailedRetryWithPeriod")); }
  }
  function scheduleUndoDeletion(item: UndoDeletion) {
    undoQueue.enqueue(item);
  }
  function stageRemoteDeletion(item: CalendarAgendaItem, calendar: CalendarSettingsView) {
    const displayedItem = calendarItems.find((current) => current.resourceId === item.resourceId && remoteDeleteKey(current) === remoteDeleteKey(item));
    const next = new Set(stagedRemoteDeleteKeys); next.add(remoteDeleteKey(item)); stagedRemoteDeleteKeys = next;
    calendarItems = calendarItems.filter((current) => remoteDeleteKey(current) !== remoteDeleteKey(item));
    resetDetailSelection();
    scheduleUndoDeletion({ kind: "remote", id: remoteDeleteKey(item), title: displayedItem?.title ?? item.title, item, calendar });
  }
  function finishStagedRemoteDeletion(item: CalendarAgendaItem) {
    const next = new Set(stagedRemoteDeleteKeys); next.delete(remoteDeleteKey(item)); stagedRemoteDeleteKeys = next;
    void refreshCalendar();
  }

  function clearMessages() { errorMessage = ""; errorLogAvailable = false; }
  function markBusy(todoId: string, busy: boolean) { const next = new Set(busyIds); busy ? next.add(todoId) : next.delete(todoId); busyIds = next; }
  function messageFrom(error: unknown, fallback: string): string {
    if (language === "en") return fallback;
    if (typeof error === "object" && error !== null && "message" in error) { const message = (error as IpcError).message; if (typeof message === "string") return message; }
    return fallback;
  }
  async function reportFailure(action: string, error: unknown, fallback: string) {
    const message = messageFrom(error, fallback);
    const code = typeof error === "object" && error !== null && "code" in error && typeof (error as IpcError).code === "string" ? (error as IpcError).code : "unknown";
    const diagnostic = typeof error === "object" && error !== null && "message" in error && typeof (error as IpcError).message === "string" ? (error as IpcError).message : message;
    try {
      await recordUiError(action, code, diagnostic);
      errorLogAvailable = true;
      errorMessage = `${message} ${translate(language, "errorSavedToLog")}`;
    } catch {
      errorLogAvailable = false;
      errorMessage = message;
    }
  }
  async function clearDisplayedErrorLog() {
    if (errorLogClearing || errorLogLoading) return;
    errorLogClearing = true;
    try { await clearErrorLog(); errorLogContent = await readErrorLog(); errorLogReadError = ""; }
    catch (failure) { void reportActionFailure("saveAction", "errorLogClear", diagnosticFrom(failure)); }
    finally { errorLogClearing = false; }
  }
  async function showErrorLog() {
    errorLogModal = true; errorLogLoading = true; errorLogReadError = "";
    try { errorLogContent = await readErrorLog(); }
    catch { errorLogContent = ""; errorLogReadError = translate(language, "errorLogReadFailed"); }
    finally { errorLogLoading = false; }
  }
  function minimizeMainWindow() { void getCurrentWindow().minimize(); }
  function toggleMainWindowMaximize() { void getCurrentWindow().toggleMaximize(); }
  function closeMainWindow() { void getCurrentWindow().close(); }
  let chromeDrag: { pointerId: number; x: number; y: number; target: HTMLElement } | null = null;
  function startMainWindowDrag(event: PointerEvent) {
    if (event.button !== 0 || (event.target as HTMLElement).closest("button")) return;
    chromeDrag = { pointerId: event.pointerId, x: event.clientX, y: event.clientY, target: event.currentTarget as HTMLElement };
    chromeDrag.target.setPointerCapture(event.pointerId);
  }
  async function moveMainWindowDrag(event: PointerEvent) {
    if (!chromeDrag || event.pointerId !== chromeDrag.pointerId || Math.hypot(event.clientX - chromeDrag.x, event.clientY - chromeDrag.y) < 4) return;
    const drag = chromeDrag;
    chromeDrag = null;
    if (drag.target.hasPointerCapture(event.pointerId)) drag.target.releasePointerCapture(event.pointerId);
    await getCurrentWindow().startDragging();
  }
  function endMainWindowDrag(event: PointerEvent) {
    if (!chromeDrag || event.pointerId !== chromeDrag.pointerId) return;
    if (chromeDrag.target.hasPointerCapture(event.pointerId)) chromeDrag.target.releasePointerCapture(event.pointerId);
    chromeDrag = null;
  }
  function handleChromeDoubleClick(event: MouseEvent) {
    if (!(event.target as HTMLElement).closest("button")) toggleMainWindowMaximize();
  }
  function togglePinnedRail() {
    if (railPinnedOpen) { railPinnedOpen = false; railHoverOpen = false; railHoverSuppressed = true; }
    else { railPinnedOpen = true; railHoverOpen = false; railHoverSuppressed = false; }
  }
  function enterRail() { if (!railPinnedOpen && !railHoverSuppressed) railHoverOpen = true; }
  function leaveRail() { if (!railPinnedOpen) railHoverOpen = false; railHoverSuppressed = false; railHoverKey = ""; }
  function leaveRailFocus(event: FocusEvent) {
    if (!(event.currentTarget as HTMLElement).contains(event.relatedTarget as Node | null)) leaveRail();
  }
  function enterRailItem(key: string) {
    if (!railPinnedOpen && railHoverSuppressed && railHoverKey && railHoverKey !== key) { railHoverSuppressed = false; railHoverOpen = true; }
    railHoverKey = key;
  }
  function activateRailItem(action: () => void) {
    if (submitting || !leaveDetail()) return;
    detailEditing = false; detailModal = null;
    action();
    if (!railPinnedOpen) { railHoverOpen = false; railHoverSuppressed = true; }
  }
  function toLocalInput(value: number | null, preserveSeconds = false): string {
    if (value === null) return ""; const date = new Date(value); const offset = date.getTimezoneOffset() * 60_000;
    return new Date(value - offset).toISOString().slice(0, preserveSeconds ? 19 : 16);
  }
  function fromLocalInput(value: string): number | null { return value ? new Date(value).getTime() : null; }
  function formatTime(value: number | null): string { return value === null ? "" : new Intl.DateTimeFormat(language, { dateStyle: "short", timeStyle: "short" }).format(value); }
  function agendaAxisDate(value: number | null): string {
    return value === null ? "—" : new Intl.DateTimeFormat(language, { month: "2-digit", day: "2-digit" }).format(value);
  }
  function agendaAxisTime(value: number | null, allDay = false): string {
    if (value === null) return "";
    return allDay ? translate(language, "allDay") : new Intl.DateTimeFormat(language, { hour: "2-digit", minute: "2-digit" }).format(value);
  }
  function agendaMetadataTime(entry: AgendaEntry): string {
    const value = entryTime(entry);
    if (value === null) return "";
    if (entry.kind === "calendar" && entry.item.allDay) return `${new Intl.DateTimeFormat(language, { dateStyle: "short", timeZone: "UTC" }).format(value)} · ${translate(language, "allDay")}`;
    return formatTime(value);
  }
  function agendaDateTime(value: number | null): string | undefined {
    return value === null ? undefined : new Date(value).toISOString();
  }
  function formatCalendarRange(item: CalendarAgendaItem): string {
    if (item.status === "unsupported" || item.startAtUtcMs === null) return translate(language, "unsupportedTimeOrRecurrence");
    if (item.allDay) return `${new Intl.DateTimeFormat(language, { dateStyle: "medium", timeZone: "UTC" }).format(item.startAtUtcMs)} · ${translate(language, "allDay")}`;
    const start = formatTime(item.startAtUtcMs);
    const end = item.endAtUtcMs === null ? "" : formatTime(item.endAtUtcMs);
    return end && end !== start ? `${start} –\n${end}` : start;
  }
</script>

<svelte:head><meta name="description" content="Calendar TODO local agenda." /></svelte:head>
<svelte:window oncontextmenu={(event) => event.preventDefault()} onlostpointercapture={lostReorderCapture} onblur={() => { cancelReorder(); deleteMenu = null; reorderMenu = null; }} onkeydown={(event) => { if (event.key === "Escape") { cancelReorder(); deleteMenu = null; reorderMenu = null; } }} onpointermove={moveReorder} onpointerup={endReorder} onpointercancel={endReorder} />
<a class="skip-link" href="#agenda-list">{translate(language, "skipToAgenda")}</a>

<div class="window-chrome" class:rail-pinned={railPinnedOpen} role="group" aria-label={translate(language, "windowControls")} onpointerdown={startMainWindowDrag} onpointermove={moveMainWindowDrag} onpointerup={endMainWindowDrag} onpointercancel={endMainWindowDrag} ondblclick={handleChromeDoubleClick}>
  <span class="chrome-drag-space" aria-hidden="true"></span>
  <div class="window-controls">
    <button type="button" onpointerdown={(event) => event.stopPropagation()} aria-label={translate(language, "minimizeWindow")} title={translate(language, "minimizeWindow")} onclick={minimizeMainWindow}><svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M3 8h10" /></svg></button>
    <button type="button" onpointerdown={(event) => event.stopPropagation()} aria-label={translate(language, "maximizeWindow")} title={translate(language, "maximizeWindow")} onclick={toggleMainWindowMaximize}><svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><rect x="3.25" y="3.25" width="9.5" height="9.5" rx=".75" /></svg></button>
    <button class="window-close" type="button" onpointerdown={(event) => event.stopPropagation()} aria-label={translate(language, "closeWindow")} title={translate(language, "closeWindow")} onclick={closeMainWindow}><svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="m4 4 8 8m0-8-8 8" /></svg></button>
  </div>
</div>

<main class="shell" class:settings-view={currentView === "settings"} class:detail-open={detailModal !== null} class:rail-pinned={railPinnedOpen}>
  <ToastViewport>
    {#each undoItems as entry (entry.token)}<UndoToast title={entry.payload.title} undo={() => void undoDelete(entry.token)} {language} stacked />{/each}
    {#if actionToast}{#key actionToast.id}<ActionToast message={actionToast.message} variant={actionToast.variant} stacked />{/key}{/if}
  </ToastViewport>
  {#if reorderMenu}<ReorderMenu x={reorderMenu.x} y={reorderMenu.y} disabled={reorderMenuDisabled(reorderMenu.key)} choose={chooseReorderAction} close={closeReorderMenu} {language} />{/if}
  {#if deleteMenu}<EventContextMenu opener={deleteMenu.opener} x={deleteMenu.x} y={deleteMenu.y} label={translate(language, "delete")} choose={chooseDeleteEntry} close={closeDeleteMenu} />{/if}
  {#if recurrenceDeleteTodo}<RecurrenceDeleteScope {language} title={recurrenceDeleteTodo.title} close={() => recurrenceDeleteTodo=null} choose={chooseRecurrenceDelete} />{/if}
    <div class="titlebar" data-navigation-rail role="banner" class:rail-open={railOpen} class:rail-pinned={railPinnedOpen} class:rail-preview={railHoverOpen && !railPinnedOpen} onpointerenter={enterRail} onpointerleave={leaveRail} onfocusin={enterRail} onfocusout={leaveRailFocus}>
    <div class="app-brand" class:expanded={railOpen}>{#if railOpen}<strong>Calendar TODO</strong>{/if}<button class="rail-toggle" type="button" aria-label={railPinnedOpen ? translate(language, "collapse") : translate(language, "expand")} title={railPinnedOpen ? translate(language, "collapse") : translate(language, "expand")} onclick={togglePinnedRail}><svg viewBox="0 0 24 24" aria-hidden="true">{#if railPinnedOpen}<path d="m14 7-5 5 5 5M19 7l-5 5 5 5" />{:else}<path d="m10 7 5 5-5 5M5 7l5 5-5 5" />{/if}</svg></button></div>
    <nav class="primary-navigation" aria-label="Calendar TODO">
      {#each [["all", "agenda"], ["today", "today"], ["incomplete", "incomplete"]] as option}
        <button type="button" onpointerenter={() => enterRailItem(option[0])} aria-label={translate(language, option[1] as MessageKey)} title={translate(language, option[1] as MessageKey)} aria-current={currentView === "agenda" && agendaFilter === option[0] ? "page" : undefined} onclick={() => activateRailItem(() => { currentView = "agenda"; agendaFilter = option[0] as typeof agendaFilter; })}><svg viewBox="0 0 24 24" aria-hidden="true">{#if option[0] === "all"}<path d="M5 6h14M5 12h14M5 18h10" />{:else if option[0] === "today"}<path d="M5 5h14v14H5zM8 3v4M16 3v4M5 9h14" />{:else}<circle cx="12" cy="12" r="8"/><path d="M12 8v4l3 2" />{/if}</svg><span>{translate(language, option[1] as MessageKey)}</span></button>
      {/each}
    </nav>
    <div class="sidebar-calendars">
      <h2><span class="calendar-stack-icon" aria-hidden="true"></span><span>{translate(language, "calendarFilters")}</span></h2>
      {#each sidebarCalendars as calendar (calendar.id)}
        <button type="button" onpointerenter={() => enterRailItem(`calendar:${calendar.id}`)} aria-pressed={!hiddenCalendarFilters.has(calendar.id)} title={calendar.name} onclick={() => activateRailItem(() => toggleCalendarFilter(calendar.id))}><span class="filter-dot" class:hollow={hiddenCalendarFilters.has(calendar.id)} style:--calendar-color={calendar.color}></span><span>{calendar.name}</span></button>
      {/each}
    </div>
    <div class="rail-bottom">
      <button class="desktop-toggle" type="button" onpointerenter={() => enterRailItem("desktop")} aria-pressed={settings?.desktopListMode ?? false} aria-label={translate(language, "showDesktop")} title={translate(language, "showDesktop")} onclick={() => activateRailItem(() => void setDesktopListMode(!settings?.desktopListMode))}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5h16v11H4zM8 20h8M12 16v4" /></svg><span>{translate(language, "showDesktop")}</span></button>
      <button class="settings-toggle" type="button" onpointerenter={() => enterRailItem("settings")} aria-label={translate(language, "settings")} title={translate(language, "settings")} aria-current={currentView === "settings" ? "page" : undefined} onclick={() => activateRailItem(() => currentView = "settings")}><svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="3"/><path d="M19 12a7 7 0 0 0-.1-1l2-1.5-2-3.4-2.4 1A7 7 0 0 0 15 6l-.3-2.6h-4L10.4 6A7 7 0 0 0 8 7.1l-2.4-1-2 3.4 2 1.5a7 7 0 0 0 0 2l-2 1.5 2 3.4 2.4-1A7 7 0 0 0 10.4 18l.3 2.6h4L15 18a7 7 0 0 0 1.6-1.1l2.4 1 2-3.4-2-1.5a7 7 0 0 0 0-1z"/></svg><span>{translate(language, "settings")}</span></button>
    </div>
  </div>

  <div class="page-heading">
    {#if currentView === "agenda"}<div class="day-anchor"><span class="day-number">{todayDayNumber}</span><span class="day-copy"><strong>{todayWeekday}</strong><span>{todayMonthYear}</span></span></div>{:else}<h1>{translate(language, "settings")}</h1>{/if}
    {#if currentView === "agenda"}<Button class="new-event-action" variant="primary" type="button" onclick={openCreateModal}>{translate(language, "newEvent")}</Button>{/if}
  </div>

  {#if errorMessage}<InlineStatus variant="error"><span>{errorMessage}</span>{#if errorLogAvailable}{#snippet action()}<Button variant="quiet" size="compact" type="button" onclick={showErrorLog}>{translate(language, "viewErrorLog")}</Button>{/snippet}{/if}</InlineStatus>{/if}

  {#if currentView === "settings"}
  <section class="settings-card" aria-labelledby="settings-title">
    <h2 id="settings-title" class="sr-only">{translate(language, "settings")}</h2>
    <div class="settings-tabs" role="tablist" tabindex="-1" aria-label={translate(language, "settings")} onkeydown={moveSettingsTab}>
      {#each settingsTabs as tab}
        <button id={`settings-tab-${tab}`} type="button" role="tab" aria-selected={settingsTab === tab} aria-controls={`settings-panel-${tab}`} tabindex={settingsTab === tab ? 0 : -1} onclick={() => settingsTab = tab}>{translate(language, tab)}</button>
      {/each}
    </div>
    {#if settingsTab === "general"}<div id="settings-panel-general" role="tabpanel" aria-labelledby="settings-tab-general"><div class="settings-form">
        <div><label for="app-language">{translate(language, "language")}</label><select id="app-language" bind:value={settingsLanguage} onchange={() => void autoSaveSettings()}><option value="system">{translate(language, "systemLanguage")}</option><option value="zh-CN">{translate(language, "chinese")}</option><option value="en">{translate(language, "english")}</option></select></div>
        <div><label for="theme-palette">{translate(language, "themePalette")}</label><select id="theme-palette" bind:value={settingsThemePalette} onchange={() => void autoSaveSettings()}><option value="quietDaybook">{translate(language, "themeQuietDaybook")}</option><option value="dawnWheat">{translate(language, "themeDawnWheat")}</option></select></div>
        <div><label for="color-mode">{translate(language, "colorMode")}</label><select id="color-mode" bind:value={settingsColorMode} onchange={() => void autoSaveSettings()}><option value="system">{translate(language, "colorModeSystem")}</option><option value="light">{translate(language, "colorModeLight")}</option><option value="dark">{translate(language, "colorModeDark")}</option></select><p class="field-help">{translate(language, "themeHelp")}</p></div>
        <div class="wide"><label for="default-calendar">{translate(language, "defaultCalendar")}</label><select id="default-calendar" bind:value={settingsDefaultCalendarId} onchange={() => void autoSaveSettings()}>{#each visibleLocalCalendars as calendar}<option value={calendar.calendarId}>{calendar.name} · {translate(language, "local")}</option>{/each}{#each remoteDestinations as calendar}<option value={calendar.calendarId}>{calendar.displayName} · iCloud</option>{/each}</select><p class="field-help">{translate(language, "defaultCalendarHelp")}</p></div>
        <SettingsRow label={translate(language, "desktopShowCalendarName")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowCalendarName")} labelVisible={false} checked={settingsMainShowCalendarName} onChange={(checked) => { settingsMainShowCalendarName = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "desktopShowTime")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowTime")} labelVisible={false} checked={settingsMainShowTime} onChange={(checked) => { settingsMainShowTime = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "desktopShowTags")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowTags")} labelVisible={false} checked={settingsMainShowTags} onChange={(checked) => { settingsMainShowTags = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "startHiddenInTray")} wide>{#snippet control()}<Switch label={translate(language, "startHiddenInTray")} labelVisible={false} checked={settingsStartHiddenInTray} onChange={(checked) => { settingsStartHiddenInTray = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "launchAtLogin")} wide>{#snippet control()}<Switch label={translate(language, "launchAtLogin")} labelVisible={false} checked={settingsLaunchAtLogin} onChange={(checked) => { settingsLaunchAtLogin = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "reminderNotifications")} wide>{#snippet control()}<Switch label={translate(language, "reminderNotifications")} labelVisible={false} checked={settingsReminderNotifications} onChange={(checked) => { settingsReminderNotifications = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
      </div></div>
    {:else if settingsTab === "desktop"}<div id="settings-panel-desktop" role="tabpanel" aria-labelledby="settings-tab-desktop"><div class="settings-form">
        <SettingsRow label={translate(language, "showDesktop")} wide>{#snippet control()}<Switch label={translate(language, "showDesktop")} labelVisible={false} checked={settings?.desktopListMode ?? false} onChange={(checked) => void setDesktopListMode(checked)} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "desktopShowCalendarName")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowCalendarName")} labelVisible={false} checked={settingsDesktopShowCalendarName} onChange={(checked) => { settingsDesktopShowCalendarName = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "desktopShowTime")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowTime")} labelVisible={false} checked={settingsDesktopShowTime} onChange={(checked) => { settingsDesktopShowTime = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <SettingsRow label={translate(language, "desktopShowTags")} wide>{#snippet control()}<Switch label={translate(language, "desktopShowTags")} labelVisible={false} checked={settingsDesktopShowTags} onChange={(checked) => { settingsDesktopShowTags = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow>
        <div><label for="window-mode">{translate(language, "desktopWindowMode")}</label><select id="window-mode" bind:value={settingsMode} onchange={() => void autoSaveSettings()}><option value="desktopCompanion">{translate(language, "companion")}</option><option value="alwaysOnTop">{translate(language, "topmost")}</option></select></div>
        <div class="wide"><label for="desktop-opacity">{text("desktopOpacity", { percent: settingsDesktopOpacity })}</label><input id="desktop-opacity" type="range" min="20" max="100" step="5" bind:value={settingsDesktopOpacity} onchange={() => void autoSaveSettings()} /><p class="field-help">{translate(language, "desktopOpacityHelp")}</p></div>
      </div></div>
    {:else if settingsTab === "calendars"}<div id="settings-panel-calendars" role="tabpanel" aria-labelledby="settings-tab-calendars">
      <div class="settings-form sync-preferences-row"><div><label for="sync-interval">{translate(language, "syncIntervalMinutes")}</label><input id="sync-interval" type="number" min="5" max="1440" bind:value={settingsSyncInterval} disabled={savingSettings} onchange={() => { if (settingsSyncInterval >= 5 && settingsSyncInterval <= 1440) void autoSaveSettings(); }} /></div><SettingsRow label={translate(language, "syncOnStartup")}>{#snippet control()}<Switch label={translate(language, "syncOnStartup")} labelVisible={false} checked={settingsSyncOnStartup} disabled={savingSettings} onChange={(checked) => { settingsSyncOnStartup = checked; void autoSaveSettings(); }} />{/snippet}</SettingsRow></div>
      <div class="account-layout">
      <aside class="account-master" aria-label={translate(language, "calendarAccounts")}>{#each accounts as account (account.accountId)}<button type="button" class:selected={selectedSettingsAccountId === account.accountId} onclick={() => { selectedSettingsAccountId = account.accountId; selectedSettingsCalendarId = null; }}><strong>{account.userLabel}</strong><span>{attachmentStateLabel(account)}</span></button>{/each}<button type="button" class:selected={selectedSettingsAccountId === "local"} onclick={() => { selectedSettingsAccountId = "local"; selectedSettingsCalendarId = null; }}><strong>{translate(language, "localAccount")}</strong><span>{localCalendars.length}</span></button><Button variant="secondary" type="button" onclick={() => accountModal = true}>{translate(language, "addCalendarAccount")}</Button></aside>
      <div class="account-detail">
      {#if selectedSettingsAccountId === "local"}<section class="account-settings local-account"><div class="calendar-settings-heading"><h3>{translate(language, "localAccount")}</h3><span class="calendar-settings-hint">{translate(language, "calendarEditHint")}</span></div>
        {#each localCalendars as calendar (calendar.calendarId)}
          <div class="local-calendar-row calendar-setting-row" class:hidden-calendar={calendar.hidden}><span class="calendar-color-dot" style:background={calendar.color} aria-hidden="true"></span><button class="calendar-setting-select" type="button" aria-expanded={selectedSettingsCalendarId === calendar.calendarId} onclick={() => selectedSettingsCalendarId = selectedSettingsCalendarId === calendar.calendarId ? null : calendar.calendarId}><strong>{calendar.name}</strong></button><div class="calendar-visibility" class:disabled-copy={calendar.hidden}><Switch label={translate(language, "show")} checked={!calendar.hidden} onChange={(checked) => toggleLocalCalendarHidden(calendar, !checked)} /></div>
            {#if selectedSettingsCalendarId === calendar.calendarId}<div class="calendar-setting-detail"><input aria-label={translate(language, "localCalendarName")} value={calendar.name} maxlength="128" onchange={(event) => persistLocalCalendar(calendar, event.currentTarget.value, calendar.color)} style:color={calendar.color} />
            <div class="calendar-palette" role="group" aria-label={text("changeCalendarColor", { name: calendar.name })}>{#each calendarColorPresets as preset (preset.value)}<button class="color-swatch" class:selected={calendar.color.toUpperCase() === preset.value} type="button" title={translate(language, preset.label as MessageKey)} aria-label={translate(language, preset.label as MessageKey)} style:--swatch={preset.value} onclick={() => persistLocalCalendar(calendar, calendar.name, preset.value)}></button>{/each}<label class="custom-color" title={translate(language, "customColor")}><span aria-hidden="true">＋</span><input aria-label={text("customCalendarColor", { name: calendar.name })} type="color" value={calendar.color} onchange={(event) => persistLocalCalendar(calendar, calendar.name, event.currentTarget.value)} /></label></div>
            <form class="calendar-display-range" aria-label={text("calendarDisplayRange", { name: calendar.name })} onchange={(event) => void saveLocalCalendarRange(event.currentTarget, calendar)} onsubmit={(event) => event.preventDefault()}>
              <label>{translate(language, "past")}<input name="displayPastDays" type="number" min="0" max="3650" step="1" value={calendar.displayPastDays ?? ""} />{translate(language, "days")}</label>
              <label>{translate(language, "future")}<input name="displayFutureDays" type="number" min="0" max="3650" step="1" value={calendar.displayFutureDays ?? ""} />{translate(language, "days")}</label>
            </form>
            <p class="field-help">{translate(language, "blankRangeShowsAll")}</p>
            <Button variant="secondary" type="button" disabled={localCalendars.length < 2} title={translate(language, localCalendars.length < 2 ? "keepOneLocalCalendar" : "deleteLocalCalendar")} onclick={() => removeLocalCalendar(calendar)}>{translate(language, "delete")}</Button></div>{/if}
          </div>
        {/each}
        <div class="new-local-calendar"><input aria-label={translate(language, "newCalendarName")} bind:value={newLocalCalendarName} maxlength="128" placeholder={translate(language, "newCalendarName")} /><input aria-label={translate(language, "newCalendarColor")} type="color" bind:value={newLocalCalendarColor} /><Button variant="secondary" type="button" onclick={addLocalCalendar}>{translate(language, "createCalendar")}</Button></div>
      </section>
      {:else}
      <section class="account-settings wide" aria-label={translate(language, "calendarAccounts")}>
        {#if accounts.length === 0}<p class="muted">{translate(language, "noAccounts")}</p>{:else}<ul class="account-list">
          {#each accounts.filter((account) => account.accountId === selectedSettingsAccountId) as account (account.accountId)}
            {@const mutationView = mutationViews.get(account.accountId)}
            <li>
              <div class="account-heading"><div class="account-summary"><strong>{account.userLabel}</strong><span>{attachmentStateLabel(account)}</span></div></div>
              <div class="account-primary-action">
                {#if !account.enabled}<button class="primary-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => toggleAccount(account)}>{translate(language, "enableAccount")}</button>
                {:else if !account.credentialConfigured}<button class="primary-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => editCredential(account.accountId)}>{translate(language, "configureLogin")}</button>
                {:else if attachmentStates.get(account.accountId)?.state === "attaching"}<button class="primary-action" type="button" disabled>{controlText(language, "connectingIcloud")}</button>
                {:else if attachmentStates.get(account.accountId)?.state === "ready" && attachmentStates.get(account.accountId)?.sourceMode === "icloudChinaReadOnly"}<button class="primary-action" type="button" disabled={syncingIds.has(account.accountId) || syncStates.get(account.accountId)?.state === "running"} onclick={() => syncAccount(account)}>{controlText(language, syncingIds.has(account.accountId) || syncStates.get(account.accountId)?.state === "running" ? "syncing" : "syncNow")}</button>
                {:else}<button class="primary-action" type="button" disabled={busyIds.has(account.accountId) || account.regionProfile !== "icloud-cn"} onclick={() => attachSource(account, "icloudChinaReadOnly")}>{translate(language, "connectIcloud")}</button>{/if}
              </div>
              {#if credentialAccountId === account.accountId}<form class="credential-form" onsubmit={(event) => saveCredential(event, account)}><div><label for={`credential-username-${account.accountId}`}>Apple ID</label><input id={`credential-username-${account.accountId}`} bind:value={credentialUsername} autocomplete="username" maxlength="512" required /></div><div><label for={`credential-password-${account.accountId}`}>{translate(language, "appSpecificPassword")}</label><input id={`credential-password-${account.accountId}`} type="password" bind:value={credentialPassword} autocomplete="new-password" maxlength="2560" required /></div><div class="actions"><button class="primary-action" type="submit" disabled={busyIds.has(account.accountId)}>{translate(language, "saveAndConnect")}</button><button class="secondary-action" type="button" onclick={cancelCredential}>{translate(language, "cancel")}</button></div><p class="field-help wide">{translate(language, "credentialStorageHelp")}</p></form>{/if}
              <details class="account-advanced"><summary>{translate(language, "moreActions")}</summary><div class="account-actions">{#if account.enabled && account.credentialConfigured}{#if import.meta.env.DEV}<button class="secondary-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => attachSource(account, "synthetic")}>{translate(language, "trySampleCalendar")}</button>{/if}<button class="secondary-action" type="button" disabled={discoveringIds.has(account.accountId) || attachmentStates.get(account.accountId)?.state === "attaching"} onclick={() => discoverCalendars(account.accountId)}>{controlText(language, discoveringIds.has(account.accountId) ? "refreshing" : "refreshCalendars")}</button>{/if}<button class="secondary-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => editCredential(account.accountId)}>{translate(language, account.credentialConfigured ? "changeLogin" : "configureLogin")}</button>{#if account.enabled}<button class="secondary-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => toggleAccount(account)}>{translate(language, "pauseAccount")}</button>{/if}<button class="secondary-action danger-action" type="button" disabled={busyIds.has(account.accountId)} onclick={() => removeAccount(account)}>{translate(language, "deleteAccount")}</button></div>
              <section class="mutation-settings" aria-label={text("remoteEditingSettings", { name: account.userLabel })}>
                <Switch label={translate(language, "allowRemoteIntent")} checked={account.remoteEditingEnabled} disabled={!account.enabled || busyIds.has(account.accountId)} onChange={(checked) => toggleRemoteEditing(account, checked)} />
                <p class="field-help">{translate(language, "remoteIntentHelp")}</p>
                {#if mutationView}
                  {#each mutationView.conflicts.filter((conflict) => conflict.resolutionState === "unresolved") as conflict (conflict.operationId)}
                    <div class="mutation-conflict"><span>{translate(language, "remoteTitleConflict")}</span><div><button class="secondary-action compact-action" type="button" disabled={busyIds.has(conflict.operationId) || !mutationView.authorization.canKeepRemote} onclick={() => resolveMutationConflict(account.accountId, conflict.operationId, "keepRemote")}>{translate(language, "keepRemoteVersion")}</button><button class="secondary-action compact-action" type="button" disabled={busyIds.has(conflict.operationId) || !mutationView.authorization.canRetryLocal} onclick={() => resolveMutationConflict(account.accountId, conflict.operationId, "retryLocal")}>{translate(language, "retryLocalTitle")}</button></div></div>
                  {/each}
                {/if}
              </section></details>
              {#if calendarSettings.some((calendar) => calendar.accountId === account.accountId && calendar.available)}
                <section class="calendar-settings" aria-label={text("accountCalendars", { name: account.userLabel })}>
                  <div class="calendar-settings-heading"><h4>{translate(language, "remoteCalendars")}</h4><span class="calendar-settings-hint">{translate(language, "calendarEditHint")}</span></div>
                  {#each calendarSettings.filter((calendar) => calendar.accountId === account.accountId && calendar.available) as calendar (calendar.calendarId)}
                    <div class="calendar-setting-row remote-calendar-row">
                      <span class="calendar-color-dot" style:background={calendar.effectiveColor ?? "#17765a"} aria-hidden="true"></span>
                       <button class="calendar-setting-select" type="button" aria-expanded={selectedSettingsCalendarId === calendar.calendarId} onclick={() => selectedSettingsCalendarId = selectedSettingsCalendarId === calendar.calendarId ? null : calendar.calendarId}><strong>{calendar.displayName}</strong></button>
                       <div class="calendar-visibility" class:disabled-copy={!calendar.visible}><Switch label={translate(language, "displayAndSync")} checked={calendar.visible} onChange={(checked) => saveCalendarVisibility(calendar, checked)} /></div>
                      {#if selectedSettingsCalendarId === calendar.calendarId}<div class="calendar-setting-detail">
                      <button class="secondary-action" type="button" disabled={!calendar.colorOverride} onclick={() => saveCalendarColor(calendar.calendarId, null)}>{translate(language, "restoreServerColor")}</button>
                      <div class="calendar-palette" role="group" aria-label={text("changeLocalCalendarColor", { name: calendar.displayName })}>
                        {#each calendarColorPresets as preset (preset.value)}
                          <button class="color-swatch" class:selected={calendar.colorOverride?.toUpperCase() === preset.value} type="button" title={translate(language, preset.label as MessageKey)} aria-label={translate(language, preset.label as MessageKey)} aria-pressed={calendar.colorOverride?.toUpperCase() === preset.value} style:--swatch={preset.value} onclick={() => saveCalendarColor(calendar.calendarId, preset.value)}></button>
                        {/each}
                        <label class="custom-color" title={translate(language, "customColor")}><span aria-hidden="true">＋</span><span class="sr-only">{text("customCalendarColor", { name: calendar.displayName })}</span><input type="color" value={calendar.effectiveColor?.slice(0, 7) ?? "#2F7D68"} onchange={(event) => saveCalendarColor(calendar.calendarId, event.currentTarget.value)} /></label>
                      </div>
                      <form class="calendar-display-range" aria-label={text("calendarDisplayRange", { name: calendar.displayName })} onchange={(event) => void saveCalendarRange(event.currentTarget, calendar)} onsubmit={(event) => event.preventDefault()}>
                        <label>{translate(language, "past")}<input name="displayPastDays" type="number" min="0" max="3650" step="1" value={calendar.displayPastDays ?? ""} />{translate(language, "days")}</label>
                        <label>{translate(language, "future")}<input name="displayFutureDays" type="number" min="0" max="3650" step="1" value={calendar.displayFutureDays ?? ""} />{translate(language, "days")}</label>
                      </form>
                      <p class="field-help">{translate(language, "blankRangeShowsAll")}</p>
                      <p class="field-help">{translate(language, "calendarSyncScopeBrief")}</p>
                      </div>{/if}
                    </div>
                  {/each}
                </section>
              {/if}
            </li>
          {/each}
        </ul>{/if}
      </section>{/if}
      </div></div>
    </div>{:else}<div id="settings-panel-about" class="about-panel" role="tabpanel" aria-labelledby="settings-tab-about"><span class="about-brand-mark" aria-hidden="true"></span><h2>Calendar TODO</h2><p>{translate(language, "version")} {appVersion}</p><div class="about-actions"><button class="secondary-action update-action" type="button" disabled={checkingUpdate || openingProductPage} aria-busy={checkingUpdate} aria-label={availableUpdateVersion ? text("getUpdateLabel", { version: availableUpdateVersion }) : controlText(language, checkingUpdate ? "checkingUpdate" : "update")} onclick={useUpdateAction}>{controlText(language, checkingUpdate ? "checkingUpdate" : availableUpdateVersion ? "getUpdate" : "update")}{#if availableUpdateVersion}<span class="update-dot" aria-hidden="true"></span>{/if}</button><button class="secondary-action" type="button" disabled={openingProductPage} onclick={() => void openAboutPage("repository")}>{translate(language, "github")}</button><button class="secondary-action" type="button" onclick={showErrorLog}>{translate(language, "errorLog")}</button></div><p class="field-help">{translate(language, "releasePageHint")}</p>{#if aboutMessage}<p class="field-help" role="status">{aboutMessage}</p>{/if}<details class="maintenance-tools"><summary>{translate(language, "moreActions")}</summary><Button variant="secondary" type="button" disabled={cleaningRemoteCache} onclick={cleanExpiredRemoteCache}>{controlText(language, cleaningRemoteCache ? "cleaning" : "cleanRemoteCache")}</Button><p class="field-help">{translate(language, "cleanRemoteCacheHelp")}</p></details><button class="license-link" type="button" onclick={() => licensesOpen = true}>{translate(language, "licenses")}</button></div>{/if}
  </section>
  {:else}



  <div class="today-workspace">
  <section id="agenda-list" class="agenda" class:hide-time={!settingsMainShowTime} aria-labelledby="agenda-title" aria-busy={loading}>
    <div class="agenda-heading"><h2 id="agenda-title">{translate(language, "agenda")}</h2><span class="agenda-summary" aria-label={text("agendaSummaryAria", { pending: pendingCount, calendars: calendarItems.length, todos: todos.length })}>{text("timelineCount", { count: timelineEntries.length })}</span></div>
    {#if loading}<p class="loading-state">{translate(language, "loadingAgenda")}</p>
    {:else if todos.length === 0 && calendarItems.length === 0}<div class="empty-state"><h3>{translate(language, "nothingToday")}</h3><p>{translate(language, "emptyAgendaHelp")}</p></div>
    {:else}
      <ul class="agenda-items timeline-items">
        {#snippet nowMarker()}
          <div class="now-line" aria-label={agendaAxisTime(currentTime)}><time datetime={agendaDateTime(currentTime)}>{agendaAxisTime(currentTime)}</time><span class="now-marker" aria-hidden="true"></span></div>
        {/snippet}
        {#each timelineEntries as entry (entry.key)}
          {#if currentTimePosition.key === entry.key && currentTimePosition.placement === "before"}<li class="now-slot">{@render nowMarker()}</li>{/if}
          <li tabindex="-1" class="timeline-entry" data-completion-motion-id={`main-timeline:${entry.key}`} data-completion-key={entry.key} class:completion-finishing={completionChanges.get(entry.key) === "complete"} class:completion-restoring={completionChanges.get(entry.key) === "restore"} oncontextmenu={(event) => openDeleteMenu(event, entry)}>
            {#if currentTimePosition.key === entry.key && currentTimePosition.placement === "inside"}{@render nowMarker()}{/if}
            <div class="timeline-content">
            {#if settingsMainShowTime}<time class="agenda-time" datetime={agendaDateTime(entry.time)}><span>{agendaAxisDate(entry.time)}</span><span>{agendaAxisTime(entry.time, entry.kind === "calendar" && entry.item.allDay)}</span></time>{/if}
            <span class="timeline-dot" style:--completion-color={entry.item.calendarColor ?? undefined} aria-hidden="true"></span>
            <button class="calendar-copy row-open" type="button" onclick={() => entry.kind === "calendar" ? openCalendarModal(entry.item) : openTodoModal(entry.item)}>
              <p class="entry-title-line"><span class="entry-title" use:fitOneLine>{entry.item.title}</span>{#if settingsMainShowCalendarName}<span class="timeline-calendar" use:fitOneLine>{entry.item.calendarName}</span>{/if}</p>
            </button>
            </div>
            {#if currentTimePosition.key === entry.key && currentTimePosition.placement === "after"}{@render nowMarker()}{/if}
          </li>
        {/each}
        {#if timelineEntries.length === 0}<li class="now-slot">{@render nowMarker()}</li>{/if}
      </ul>
      {#if timelineEntries.length === 0}<p class="timeline-empty">{translate(language, "noTimelineItems")}</p>{/if}
      <div class="todo-heading agenda-heading"><h2>{text("todoCount", { count: filteredAgendaEntries.length })}</h2><IconButton class="reset-order-button" label={translate(language, "resetOrder")} size="compact" disabled={resettingOrder} onclick={resetOrder}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 12a9 9 0 0 1-15.64 6.08L3 16" /><path d="M3 21v-5h5" /><path d="M3 12a9 9 0 0 1 15.64-6.08L21 8" /><path d="M21 3v5h-5" /></svg></IconButton></div>
      <ul class="agenda-items todo-items">
        {#each filteredAgendaEntries as entry (entry.key)}
        <li tabindex="-1" data-key={entry.key} data-completion-motion-id={`main-list:${entry.key}`} data-completion-key={entry.key} class:dragging={draggingKey === entry.key} class:drop-target={!!draggingKey && dragTargetKey === entry.key && draggingKey !== entry.key} class:completed-entry={entry.item.completed} class:completion-finishing={completionChanges.get(entry.key) === "complete"} class:completion-restoring={completionChanges.get(entry.key) === "restore"} class:unsupported-event={entry.kind === "calendar" && entry.item.status === "unsupported"} oncontextmenu={(event) => openDeleteMenu(event, entry)} ondblclick={() => entry.kind === "calendar" ? openCalendarModal(entry.item) : openTodoModal(entry.item)}>
          <div class="todo-row">
            <button class="complete-button" style:--completion-color={entry.item.calendarColor ?? undefined} type="button" aria-label={text(entry.item.completed ? "restoreItem" : "completeItem", { title: entry.item.title })} aria-pressed={entry.item.completed} disabled={completionChanges.has(entry.key) || (entry.kind === "calendar" && (entry.item.status !== "ready" || !!entry.item.pendingRemote))} onclick={() => entry.kind === "calendar" ? toggleCalendar(entry.item) : toggle(entry.item)}><span aria-hidden="true">{entry.item.completed ? "✓" : ""}</span></button>
            <button class="todo-copy row-open" type="button" onclick={() => entry.kind === "calendar" ? openCalendarModal(entry.item) : openTodoModal(entry.item)}>
              <p class="entry-title-line"><span class="entry-title" use:fitOneLine>{entry.item.title}</span>{#if settingsMainShowTags && entry.item.tags.length}<span class="entry-tags" use:fitOneLine>{entry.item.tags.map((tag) => `#${tag.name}`).join(" ")}</span>{/if}</p>{#if settingsMainShowCalendarName || settingsMainShowTime || entry.item.important || entry.item.completed || entry.kind === "calendar" && entry.item.pendingRemote}<span class="entry-metadata" use:fitOneLine>{#if settingsMainShowCalendarName}<b class="calendar-name" style:color={entry.item.calendarColor ?? undefined}>{entry.item.calendarName}</b>{/if}{#if settingsMainShowTime}<time datetime={agendaDateTime(entryTime(entry))}>{agendaMetadataTime(entry)}</time>{/if}{#if entry.item.important}<span>{translate(language, "pinned")}</span>{/if}{#if entry.item.completed}<span>{translate(language, "completed")}</span>{/if}{#if entry.kind === "calendar" && entry.item.pendingRemote}<span>{translate(language, entry.item.pendingRemote.status === "conflict" ? "syncConflict" : entry.item.pendingRemote.status === "retryNeeded" ? "remoteRetryNeeded" : "pendingRemoteSave")}</span>{/if}</span>{/if}
            </button>
            <button class="agenda-reorder" type="button" aria-haspopup="menu" aria-label={text("reorderItem", { title: entry.item.title })} title={translate(language, "holdToReorder")} onpointerdown={(event) => startReorder(event, entry.key)} oncontextmenu={(event) => openReorderMenu(event, entry.key)} onkeydown={(event) => reorderMenuKey(event, entry.key)} onclick={(event) => { if (event.detail === 0) openReorderMenu(event, entry.key); }} ondblclick={(event) => event.stopPropagation()}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 7h14M5 12h14M5 17h14" /></svg></button>
          </div>
        </li>
        {/each}
      </ul>{/if}
      <button class="workspace-blank" type="button" aria-label={translate(language, "eventDetails")} onclick={resetDetailSelection}></button>
  </section>
  </div>
  <aside class="event-detail-pane" aria-busy={detailSaving} class:detail-editing={detailEditing} aria-labelledby="event-detail-title">
    <div class="detail-pane-heading"><h2 id="event-detail-title">{translate(language, "eventDetails")}</h2></div>
    {#if detailEditing && detailModal === "todo" && selectedTodo}
      {@const todo = selectedTodo}
      {#if errorMessage}<p class="message error" role="alert">{errorMessage}</p>{/if}
      <form id="todo-detail-form" class="inline-detail-form" autocomplete="off" oninput={scheduleDetailAutoSave} onchange={scheduleDetailAutoSave} onsubmit={submitDetail} aria-label={text("editItem", { title: todo.title })} aria-busy={detailSaving}>
        <fieldset disabled={detailSaving}>
        <div class="detail-title detail-field detail-title-editor" class:active-edit-field={activeDetailField === "title"} data-detail-field="title"><span class="detail-color" style:background={todo.calendarColor ?? undefined} aria-hidden="true"></span><label class="sr-only" for={`edit-title-${todo.todoId}`}>{translate(language, "title")}</label><input id={`edit-title-${todo.todoId}`} bind:value={editTitle} maxlength="512" required /></div>
        <div class="detail-field" class:active-edit-field={activeDetailField === "calendar"} data-detail-field="calendar"><label for={`edit-calendar-${todo.todoId}`}>{translate(language, "calendar")}</label><select id={`edit-calendar-${todo.todoId}`} bind:value={editCalendarId}>{#each visibleLocalCalendars as calendar}<option value={calendar.calendarId}>{calendar.name}</option>{/each}</select></div>
        <div class="detail-field detail-time-fields" class:active-edit-field={activeDetailField === "time"} data-detail-field="time"><div><label for={`edit-start-${todo.todoId}`}>{translate(language, "startOptional")}</label><DateTimeInput disabled={detailSaving} id={`edit-start-${todo.todoId}`} value={editStart} onValue={(value) => { updateEditStart(value); scheduleDetailAutoSave(); }} {language} /></div><div><label for={`edit-due-${todo.todoId}`}>{translate(language, "dueOptional")}</label><DateTimeInput disabled={detailSaving} id={`edit-due-${todo.todoId}`} value={editDue} onValue={(value) => { editDue = value; scheduleDetailAutoSave(); }} min={editStart || undefined} {language} /></div></div>
        <div class="detail-field" class:active-edit-field={activeDetailField === "location"} data-detail-field="location"><label for={`edit-location-${todo.todoId}`}>{translate(language, "location")}</label><input id={`edit-location-${todo.todoId}`} bind:value={editLocation} maxlength="512" /></div>
        <div class="detail-field" class:active-edit-field={activeDetailField === "notes"} data-detail-field="notes"><label for={`edit-notes-${todo.todoId}`}>{translate(language, "notes")}</label><textarea id={`edit-notes-${todo.todoId}`} bind:value={editNotes} maxlength="10000" rows="3"></textarea></div>
        <div class="detail-field detail-metadata-fields" class:active-edit-field={activeDetailField === "metadata"} data-detail-field="metadata">
          <div><label for={`edit-tags-${todo.todoId}`}>{translate(language, "tags")}</label><TagInput disabled={detailSaving} id={`edit-tags-${todo.todoId}`} tags={editTags} {language} suggestions={tagSuggestions} onTags={(tags) => { editTags = tags; scheduleDetailAutoSave(); }} /></div>
          <RecurrenceSelector disabled={detailSaving} {language} frequency={editRecurrence} interval={editRecurrenceInterval} options={editRecurrenceOptions} anchorAt={fromLocalInput(editDue)} change={(frequency,interval,options)=>{editRecurrence=frequency;editRecurrenceInterval=interval;editRecurrenceOptions=options;scheduleDetailAutoSave();}} />
          <label class="important-toggle"><input type="checkbox" bind:checked={editImportant} />{translate(language, "pinPriority")}</label>
        </div>
        <p class="field-help">{translate(language, "detailLocalMetadataHelp")}</p>
        </fieldset>
      </form>
      <div class="detail-actions"><button class="modal-delete" type="button" disabled={detailSaving || busyIds.has(todo.todoId)} onclick={() => requestTodoDelete(todo)}>{translate(language, "delete")}</button><button class="secondary-action" type="button" disabled={detailSaving || busyIds.has(todo.todoId)} onclick={cancelDetailEditing}>{translate(language, "cancel")}</button><button class="primary-action" type="submit" form="todo-detail-form" disabled={detailSaving || busyIds.has(todo.todoId) || !editTitle.trim()}>{controlText(language, detailSaving ? "saving" : "save")}</button></div>
    {:else if detailEditing && detailModal === "calendar" && selectedCalendar}
      {@const item = selectedCalendar}
      <form id="calendar-detail-form" class="inline-detail-form" autocomplete="off" onchange={scheduleDetailAutoSave} onsubmit={submitDetail} oninput={() => { if (remoteView) remoteView = null; scheduleDetailAutoSave(); }} aria-busy={detailSaving || remoteView?.busy || false}>
        {#if errorMessage}<p class="error" role="alert">{errorMessage}</p>{/if}
        <fieldset disabled={detailSaving || remoteView?.busy || !!remoteBlockedReason(calendarSettings.find((calendar) => calendar.calendarId === item.calendarId), item) || !projectionCandidate(item)}>
          <div class="detail-title detail-field detail-title-editor" class:active-edit-field={activeDetailField === "title"} data-detail-field="title"><span class="detail-color" style:background={item.calendarColor ?? undefined} aria-hidden="true"></span><label class="sr-only" for="calendar-edit-title">{translate(language, "title")}</label><input id="calendar-edit-title" bind:value={editTitle} maxlength="512" required /></div>
          <div class="detail-field" class:active-edit-field={activeDetailField === "calendar"} data-detail-field="calendar"><label for="calendar-edit-calendar">{translate(language, "calendar")}</label><select id="calendar-edit-calendar" disabled><option>{item.calendarName}</option></select></div>
          <div class="detail-field detail-time-fields" class:active-edit-field={activeDetailField === "time"} data-detail-field="time"><div><label for="calendar-edit-start">{translate(language, "start")}</label><DateTimeInput disabled={detailSaving} id="calendar-edit-start" value={editStart} step="1" onValue={(value) => { updateEditStart(value); scheduleDetailAutoSave(); }} {language} /></div><div><label for="calendar-edit-end">{translate(language, "end")}</label><DateTimeInput disabled={detailSaving} id="calendar-edit-end" value={editDue} step="1" onValue={(value) => { editDue = value; scheduleDetailAutoSave(); }} {language} /></div></div>
          <div class="detail-field" class:active-edit-field={activeDetailField === "location"} data-detail-field="location"><label for="calendar-edit-location">{translate(language, "location")}</label><input id="calendar-edit-location" bind:value={editLocation} maxlength="512" /></div>
          <div class="detail-field" class:active-edit-field={activeDetailField === "notes"} data-detail-field="notes"><label for="calendar-edit-notes">{translate(language, "notes")}</label><textarea id="calendar-edit-notes" bind:value={editNotes} maxlength="10000" rows="3"></textarea></div>
        </fieldset>
        <fieldset disabled={detailSaving}><div class="detail-field detail-metadata-fields" class:active-edit-field={activeDetailField === "metadata"} data-detail-field="metadata">
          <div><label for="calendar-edit-tags">{translate(language, "tags")}</label><TagInput disabled={detailSaving} id="calendar-edit-tags" tags={editTags} {language} suggestions={tagSuggestions} onTags={(tags) => { editTags = tags; scheduleDetailAutoSave(); }} /></div>
          <RecurrenceSelector disabled={detailSaving} {language} frequency={editRecurrence} interval={editRecurrenceInterval} options={editRecurrenceOptions} anchorAt={fromLocalInput(editDue)} change={(frequency,interval,options)=>{editRecurrence=frequency;editRecurrenceInterval=interval;editRecurrenceOptions=options;scheduleDetailAutoSave();}} />
          <label class="important-toggle"><input type="checkbox" bind:checked={editImportant} disabled={!item.workItemId} />{translate(language, "pinLocalOnly")}</label>
        </div>
        <p class="field-help">{translate(language, "detailLocalMetadataHelp")}</p>
        </fieldset>
        {#if remoteBlockedReason(calendarSettings.find((calendar) => calendar.calendarId === item.calendarId), item) || !projectionCandidate(item)}<p class="field-help" role="alert">{remoteBlockedReason(calendarSettings.find((calendar) => calendar.calendarId === item.calendarId), item) ?? preparationHint(item)}</p>{/if}
      </form>
      {#if remoteView}<p class="field-help" role="status" aria-live="polite">{remoteView.message}</p>{#each Object.values(remoteView.errors) as error}<p class="error" role="alert">{error}</p>{/each}{/if}
      <div class="detail-actions"><button class="modal-delete" type="button" aria-label={text("deleteFromIcloud", { title: item.title })} disabled={detailSaving || remoteView?.busy || !!item.pendingRemote || !!remoteBlockedReason(calendarSettings.find((calendar) => calendar.calendarId === item.calendarId), item) || !projectionCandidate(item)} onclick={() => removeRemoteEvent(item)}>{translate(language, "delete")}</button><button class="secondary-action" type="button" disabled={detailSaving || remoteView?.busy} onclick={cancelDetailEditing}>{translate(language, "cancel")}</button><button class="primary-action" type="submit" form="calendar-detail-form" disabled={detailSaving || remoteView?.busy || !editTitle.trim()}>{controlText(language, detailSaving ? "saving" : "save")}</button></div>
    {:else if activeDetailEntry}
      {@const detailItem = activeDetailEntry.item}
      <button class="detail-title" type="button" onclick={() => editActiveDetail("title")}><span class="detail-color" style:background={detailItem.calendarColor ?? undefined}></span><strong>{detailItem.title}</strong></button>
      <div class="detail-facts">
        <button type="button" onclick={() => editActiveDetail("calendar")}><b>{translate(language, "calendar")}</b><em>{detailItem.calendarName}</em></button>
        <button type="button" onclick={() => editActiveDetail("time")}><b>{translate(language, "time")}</b><em class="detail-time-range">{activeDetailEntry.kind === "calendar" ? formatCalendarRange(activeDetailEntry.item) : activeDetailEntry.item.startAtUtcMs || activeDetailEntry.item.dueAtUtcMs ? `${activeDetailEntry.item.startAtUtcMs ? formatTime(activeDetailEntry.item.startAtUtcMs) : ""}${activeDetailEntry.item.startAtUtcMs && activeDetailEntry.item.dueAtUtcMs ? " –\n" : ""}${activeDetailEntry.item.dueAtUtcMs ? formatTime(activeDetailEntry.item.dueAtUtcMs) : ""}` : "—"}</em></button>
        <button type="button" onclick={() => editActiveDetail("location")}><b>{translate(language, "location")}</b><em>{detailItem.location || "—"}</em></button>
        <button class="detail-notes" type="button" onclick={() => editActiveDetail("notes")}><b>{translate(language, "notes")}</b><em>{detailItem.notes || "—"}</em></button>
        {#if detailItem.important}<button type="button" onclick={() => editActiveDetail("metadata")}><b>{translate(language, "important")}</b><em>{translate(language, "pinned")}</em></button>{/if}
        {#if detailItem.tags.length > 0}<button type="button" onclick={() => editActiveDetail("metadata")}><b>{translate(language, "tags")}</b><em>{detailItem.tags.map((tag) => tag.name).join(" · ")}</em></button>{/if}
      </div>
      {#if activeDetailEntry.kind !== "calendar" || !activeDetailEntry.item.pendingRemote}<p class="detail-edit-hint">{translate(language, "clickToEdit")}</p>{/if}
      {#if activeDetailEntry.kind === "calendar" && activeDetailEntry.item.pendingRemote}
        <div class="pending-remote-detail"><span>{translate(language, activeDetailEntry.item.pendingRemote.status === "conflict" ? "syncConflict" : activeDetailEntry.item.pendingRemote.status === "retryNeeded" ? "remoteRetryNeeded" : "pendingRemoteSave")}</span>{#if activeDetailEntry.item.pendingRemote.kind === "updateEventV3" && activeDetailEntry.item.pendingRemote.status !== "conflict"}<button class="secondary-action" type="button" onclick={() => editActiveDetail("title")}>{translate(language, "editDetails")}</button>{/if}{#if activeDetailEntry.item.pendingRemote.status === "conflict" && activeDetailEntry.item.pendingRemote.kind === "delete"}<button class="secondary-action" type="button" disabled={busyIds.has(activeDetailEntry.item.pendingRemote.operationId)} onclick={() => keepPendingRemote(activeDetailEntry.item)}>{translate(language,"keepRemoteVersion")}</button>{/if}{#if activeDetailEntry.item.pendingRemote.status !== "conflict"}<button class="secondary-action" type="button" disabled={busyIds.has(activeDetailEntry.item.pendingRemote.operationId)} onclick={() => retryPendingRemote(activeDetailEntry.item)}>{controlText(language, busyIds.has(activeDetailEntry.item.pendingRemote.operationId) ? "processing" : activeDetailEntry.item.pendingRemote.kind === "delete" ? "retryDelete" : "retrySave")}</button>{/if}</div>
      {/if}
    {:else}<div class="detail-empty"><p>{translate(language, "nothingToday")}</p><span>{translate(language, "emptyAgendaHelp")}</span></div>{/if}
  </aside>
  {#if createModal}
    <div class="modal-scrim create-modal-scrim" role="presentation" onpointerdown={(event) => createBackdropClick.pointerDown(event.target, event.currentTarget)} onpointerup={(event) => createBackdropClick.pointerUp(event.target, event.currentTarget)} onpointercancel={createBackdropClick.pointerCancel} onclick={(event) => { if (createBackdropClick.click(event.target, event.currentTarget) && !submitting && !remoteView?.busy) createModal = false; }}>
      <div class="agenda-dialog" role="dialog" aria-modal="true" aria-labelledby="create-title" tabindex="-1" use:dialogFocus={{ close: () => createModal = false, disabled: submitting || !!remoteView?.busy }}>
  <form class="create-form" autocomplete="off" onsubmit={create} oninput={() => { if (remoteView) remoteView = null; }} aria-labelledby="create-title" aria-busy={submitting || remoteView?.busy || false}>
    <div class="create-dialog-body">
    {#if errorMessage}<p class="error" role="alert">{errorMessage}</p>{/if}
    <div class="modal-heading"><h2 id="create-title">{translate(language, "newEvent")}</h2><button type="button" aria-label={translate(language, "close")} disabled={submitting || remoteView?.busy} onclick={() => createModal = false}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18" /></svg></button></div><p class="field-help create-help">{translate(language, remoteCreateCalendar ? "saveToIcloudHelp" : "saveToLocalHelp")}</p>
    <fieldset class="create-fields" disabled={submitting || remoteView?.busy}>
      <div class="wide"><label for="todo-title">{translate(language, "title")}</label><input bind:this={createTitleInput} data-autofocus id="todo-title" type="text" bind:value={title} maxlength="512" autocomplete="off" disabled={submitting} required /></div>
      <div class="wide"><label for="todo-calendar">{translate(language, "calendar")}</label><select id="todo-calendar" bind:value={createCalendarId} onchange={resetRemoteEditor} disabled={submitting || remoteView?.busy}>{#each visibleLocalCalendars as calendar}<option value={calendar.calendarId}>{calendar.name}</option>{/each}{#each remoteDestinations as calendar}<option value={calendar.calendarId} disabled={!!remoteBlockedReason(calendar)}>{calendar.displayName} · iCloud{remoteBlockedReason(calendar) ? ` · ${remoteBlockedReason(calendar)}` : ""}</option>{/each}</select></div>
      <div><label for="todo-start">{translate(language, "start")}</label><DateTimeInput id="todo-start" value={createStart} onValue={updateCreateStart} disabled={submitting} {language} /></div>
      <div><label for="todo-due">{translate(language, "due")}</label><DateTimeInput id="todo-due" value={createDue} onValue={(value) => { createDue = value; createDueEdited = true; }} min={createStart || undefined} disabled={submitting} {language} /></div>
      <div class="wide"><label for="todo-notes">{translate(language, "notes")}</label><textarea id="todo-notes" bind:value={createNotes} maxlength="10000" rows="2" disabled={submitting}></textarea></div>
      <div><label for="todo-location">{translate(language, "location")}</label><input id="todo-location" bind:value={createLocation} maxlength="512" disabled={submitting} /></div>
      <div class="wide"><label for="todo-tags">{translate(language, "tags")}</label><TagInput id="todo-tags" tags={createTags} {language} suggestions={tagSuggestions} onTags={(tags) => createTags = tags} disabled={submitting} /></div>
      <div class="wide create-options-row">
        <div class="create-recurrence-fields"><RecurrenceSelector {language} frequency={createRecurrence} interval={createInterval} options={createRecurrenceOptions} anchorAt={fromLocalInput(createDue)} disabled={submitting} change={(frequency,interval,options)=>{createRecurrence=frequency;createInterval=interval;createRecurrenceOptions=options;}} /></div>
        <label class="important-toggle"><input type="checkbox" bind:checked={createImportant} disabled={submitting} /><span>{translate(language, "important")}{remoteCreateCalendar ? ` (${translate(language, "localOnly")})` : ""}</span></label>
      </div>
    </fieldset>
    {#if remoteCreateCalendar && remoteBlockedReason(remoteCreateCalendar)}<p class="field-help" role="alert">{remoteBlockedReason(remoteCreateCalendar)}</p>{/if}
    {#if remoteView}<p class="field-help" role="status" aria-live="polite">{remoteView.message}</p>{#each Object.values(remoteView.errors) as error}<p class="error" role="alert">{error}</p>{/each}{/if}
    </div>
    <div class="modal-actions"><button class="secondary-action" type="button" disabled={submitting || remoteView?.busy} onclick={() => createModal = false}>{translate(language, "cancel")}</button><button class="primary-action" type="submit" disabled={submitting || remoteView?.busy || !title.trim() || !!remoteCreateCalendar && !!remoteBlockedReason(remoteCreateCalendar)}>{controlText(language, submitting || remoteView?.busy ? "saving" : "save")}</button></div>
  </form>
      </div>
    </div>
  {/if}
  {/if}
  {#if accountModal}
    <DialogShell id="add-account-title" title={translate(language, "addCalendarAccount")} closeLabel={translate(language, "close")} close={() => { if (!savingAccount) accountModal = false; }} disabled={savingAccount}>
      <form id="add-account-form" class="account-form" onsubmit={addAccount} aria-busy={savingAccount}>
        {#if errorMessage}<div class="wide"><InlineStatus variant="error">{errorMessage}</InlineStatus></div>{/if}
        <div><label for="account-email">{translate(language, "email")}</label><input data-autofocus id="account-email" type="email" bind:value={accountEmail} maxlength="256" autocomplete="email" disabled={savingAccount} required placeholder="name@example.com" /></div>
        <div><label for="account-password">{translate(language, "appSpecificPassword")}</label><input id="account-password" type="password" bind:value={accountPassword} maxlength="2560" autocomplete="new-password" disabled={savingAccount} required /></div>
        <div><label for="account-label">{translate(language, "accountNameOptional")}</label><input id="account-label" bind:value={accountLabel} maxlength="256" disabled={savingAccount} placeholder={translate(language, "emailDefaultLabel")} /></div>
        <p class="field-help wide">{translate(language, "connectIcloudHelp")}</p>
      </form>
      {#snippet footer()}<Button variant="secondary" type="button" disabled={savingAccount} onclick={() => accountModal = false}>{translate(language, "cancel")}</Button><Button variant="primary" type="submit" form="add-account-form" disabled={savingAccount || !accountEmail.trim() || !accountPassword}>{controlText(language, savingAccount ? "connectingIcloud" : "connectIcloud")}</Button>{/snippet}
    </DialogShell>
  {/if}
  {#if licensesOpen}<LicenseViewer {language} close={() => licensesOpen = false} />{/if}
  {#if errorLogModal}
    <DialogShell id="error-log-title" title={translate(language, "errorLogTitle")} closeLabel={translate(language, "close")} close={() => errorLogModal = false}>
      {#if errorLogLoading}<p class="field-help" role="status">{translate(language, "errorLogLoading")}</p>
      {:else if errorLogReadError}<p class="error" role="alert">{errorLogReadError}</p>
      {:else if errorLogContent}<pre class="error-log-content">{errorLogContent}</pre>
      {:else}<p class="field-help">{translate(language, "errorLogEmpty")}</p>{/if}
      {#snippet footer()}<span class="error-log-clear"><Button variant="secondary" type="button" disabled={errorLogLoading || errorLogClearing} onclick={() => void clearDisplayedErrorLog()}>{translate(language, "clearErrorLog")}</Button></span><Button variant="primary" type="button" data-autofocus onclick={() => errorLogModal = false}>{translate(language, "close")}</Button>{/snippet}
    </DialogShell>
  {/if}
</main>

<style>
  .license-link{display:block;margin:28px auto 0;padding:10px 12px;max-width:100%;border:0;background:transparent;color:var(--color-text-muted);font:inherit;font-size:var(--font-size-caption);text-align:center;text-decoration:underline;text-underline-offset:3px;cursor:pointer}
  .license-link:hover{color:var(--color-text)}
  .detail-time-range{white-space:pre-line}
  .settings-tabs{display:flex;gap:6px;overflow-x:auto;padding-bottom:12px;border-bottom:1px solid var(--border)}.settings-tabs button{flex:0 0 auto;min-height:42px;padding:8px 13px;border:1px solid transparent;border-radius:9px;color:var(--muted);background:transparent;font:inherit;font-weight:650;white-space:nowrap}.settings-tabs button[aria-selected="true"]{border-color:var(--primary);color:var(--text);background:color-mix(in srgb,var(--primary) 12%,var(--surface))}.about-panel{display:grid;justify-items:center;gap:10px;padding:28px 12px;text-align:center}.about-panel h2,.about-panel p{margin:0}.about-actions{display:flex;flex-wrap:wrap;justify-content:center;gap:8px;margin-top:8px}.update-action{display:inline-flex;align-items:center;gap:7px}.update-dot{width:7px;height:7px;border-radius:50%;background:var(--color-danger);box-shadow:0 0 0 2px color-mix(in srgb,var(--color-danger) 16%,transparent)}.error-log-content{min-height:180px;max-height:min(56vh,480px);margin:12px 0 0;padding:12px;overflow:auto;border:1px solid var(--border);border-radius:10px;color:var(--text);background:var(--surface-soft);font:12px/1.55 var(--font-mono);text-align:left;white-space:pre-wrap;overflow-wrap:anywhere}
  fieldset.create-fields{border:0;margin:0;padding:0;min-width:0}
  :global(:root){color:var(--text);background:transparent}:global(body){min-width:320px;min-height:100vh;background:transparent}.skip-link:focus-visible{outline:2px solid var(--ring);outline-offset:2px}
  .skip-link{position:fixed;left:12px;top:-60px;z-index:30;padding:10px 14px;border-radius:8px;color:var(--color-on-primary);background:var(--primary)}.skip-link:focus{top:42px}.window-chrome{position:fixed;z-index:20;inset:0 0 auto 0;height:38px;padding-left:14px;display:flex;align-items:center;justify-content:space-between;color:var(--muted);background:var(--surface-soft);border-bottom:1px solid var(--color-border);font-size:var(--font-size-caption);font-weight:600;user-select:none}.window-controls{height:100%;display:flex}.window-controls button{width:46px;height:100%;display:grid;place-items:center;border:0;border-radius:0;color:var(--muted);background:transparent}.window-controls button:hover{color:var(--text);background:color-mix(in srgb,var(--color-text) 8%,transparent)}.window-controls button:focus-visible{position:relative;z-index:1;outline:2px solid var(--ring);outline-offset:-3px}.window-controls .window-close:hover{color:#fff;background:#c42b1c}.window-controls svg{width:14px;height:14px;display:block;fill:none;stroke:currentColor;stroke-width:1.5;stroke-linecap:round;stroke-linejoin:round}.shell{min-height:100vh;padding:22px;display:flex;flex-direction:column;gap:16px;background:linear-gradient(160deg,var(--surface-soft),var(--bg))}.titlebar,.todo-row{display:flex;align-items:center;justify-content:space-between;gap:12px}h1{margin:0;font-size:1.5rem;line-height:1.2}h2{margin:0 0 12px;font-size:1rem}h3{margin:0 0 8px;font-size:.95rem}
  .create-form{padding:14px;border:1px solid var(--border);border-radius:14px;background:color-mix(in srgb,var(--surface) 90%,transparent)}
  .settings-card{border:1px solid var(--border);border-radius:14px;background:var(--surface);overflow:hidden}.settings-form{padding:14px;display:grid;grid-template-columns:1fr 1fr;gap:12px;border-top:1px solid var(--border)}
  .account-settings{padding-top:12px;border-top:1px solid var(--border)}.account-list{margin:10px 0;padding:0;display:grid;gap:7px;list-style:none}.account-list li{padding:9px;display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap;border:1px solid var(--border);border-radius:10px}.account-summary{display:grid;gap:2px}.account-list span{color:var(--muted);font-size:.75rem}.account-primary-action{margin-left:auto}.account-actions{display:flex;gap:7px}.account-advanced{width:100%;border-top:1px solid var(--border);padding-top:8px}.account-advanced>summary{width:max-content;color:var(--muted);cursor:pointer;font-size:.8rem;font-weight:650}.account-form,.credential-form{margin-top:10px;display:grid;grid-template-columns:1fr 1fr;gap:10px}.credential-form{width:100%;padding-top:10px;border-top:1px solid var(--border)}
  label{display:block;margin-bottom:6px;font-size:.82rem;font-weight:650}input:not([type="checkbox"]),select,textarea{min-height:44px;width:100%;border:1px solid var(--border);border-radius:10px;padding:9px 11px;color:var(--text);background:var(--surface)}textarea{resize:vertical}.important-toggle{min-height:44px;margin:0;display:flex;align-items:center;gap:7px}.important-toggle input{width:20px;height:20px;accent-color:var(--primary)}
  .primary-action,.secondary-action{min-height:40px;border-radius:9px;padding:0 13px;font-weight:650}.primary-action{border:1px solid var(--primary);color:var(--color-on-primary);background:var(--primary)}.primary-action:hover:not(:disabled){background:var(--primary-hover)}.secondary-action{border:1px solid var(--border);color:var(--text);background:var(--surface)}.message{margin:0;padding:10px 12px;border-radius:10px;font-size:.86rem}.error{color:var(--color-danger-text);background:var(--color-danger-surface)}.sr-only{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
  .agenda{min-height:0}.calendar-copy{min-width:0;flex:1}.calendar-copy p{margin:0 0 4px;font-weight:650;overflow-wrap:anywhere}.calendar-copy span{display:block;color:var(--muted);font-size:.75rem;line-height:1.45}.unsupported-event{border-style:dashed!important}.todo-row{padding:10px}.complete-button{position:relative;width:44px;height:44px;flex:0 0 44px;display:grid;place-items:center;border:0;color:var(--completion-color,var(--primary));background:transparent}.complete-button::before{content:"";position:absolute;width:20px;height:20px;border:1.5px solid var(--completion-color,var(--color-border-strong));border-radius:5px;background:transparent}.complete-button[aria-pressed="true"]::before{background:transparent}.complete-button span{position:relative;font-size:13px;font-weight:750;line-height:1}.todo-copy{min-width:0;flex:1}.todo-copy p{margin:0 0 3px;overflow-wrap:anywhere;font-weight:650}.todo-copy span{display:block;color:var(--muted);font-size:.75rem}.field-help{margin:5px 0 0;color:var(--muted);font-size:.72rem;line-height:1.35}
  .wide{grid-column:1/-1}.actions{margin-top:12px;display:flex;gap:8px}.muted{color:var(--muted);font-size:.85rem}.empty-state,.loading-state{margin:28px auto;max-width:310px;text-align:center;color:var(--muted)}.empty-state p{margin:0;line-height:1.55}
  .agenda-heading{display:flex;align-items:center;gap:8px;justify-content:space-between;flex-wrap:wrap;margin-bottom:12px}.agenda-heading h2{margin:0}.agenda-summary{margin-left:auto;color:var(--muted);font-size:.82rem;font-weight:700;text-align:right}.settings-view .titlebar{justify-content:flex-start}.settings-card{padding:18px;overflow:visible}.settings-card>h2{margin-bottom:14px}.settings-form{padding:18px 0;margin-top:16px;gap:16px}.settings-form:first-of-type{border-top:0;margin-top:0;padding-top:0}.settings-form input[type="range"]{width:100%;min-height:32px;padding:0;border:0;background:transparent;accent-color:var(--primary)}.account-settings{margin-top:18px;padding:20px 0 4px}.account-list{gap:12px;margin:16px 0}.account-list li{padding:14px;gap:14px}.account-actions{display:flex;flex-wrap:wrap;gap:10px;width:100%;padding-top:6px}.account-form,.credential-form{gap:14px;margin-top:18px;padding-top:18px;border-top:1px solid var(--border)}.primary-action,.secondary-action{min-height:44px;padding:8px 16px;line-height:1.35}
  @media(max-width:600px){.agenda-heading{align-items:flex-start}.agenda-summary{max-width:100%}.primary-action{width:100%}.todo-row{align-items:flex-start;flex-wrap:wrap}.todo-copy{min-width:150px}.account-form,.credential-form{grid-template-columns:1fr}.wide{grid-column:auto}}
  .complete-button{width:36px;height:36px;flex:0 0 36px;align-self:center}
  @media(prefers-reduced-motion:reduce){*,*::before,*::after{scroll-behavior:auto!important;transition-duration:.01ms!important}}
  .danger-action{color:var(--danger);border-color:var(--danger)}
  .create-fields{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-top:14px}.create-fields .wide{grid-column:1/-1}
  .create-options-row{display:flex;align-items:end;gap:12px;flex-wrap:wrap}.create-recurrence-fields{min-width:min(100%,250px);flex:1 1 250px;display:grid;grid-template-columns:minmax(0,1fr) minmax(92px,.45fr);gap:12px}.create-options-row .important-toggle{flex:0 1 auto}.create-options-row .important-toggle span{line-height:1.35}
  .agenda-items{margin:0;padding:0;display:grid;gap:13px;list-style:none}.agenda-items>li{position:relative;border:1px solid var(--border);border-radius:12px;background:var(--surface);overflow:hidden}.agenda-items>li:has(.calendar-copy){padding:11px;display:flex;align-items:flex-start;gap:10px;flex-wrap:wrap}.agenda-items .todo-row{padding-right:44px}.agenda-reorder{position:absolute;right:8px;top:9px;width:28px;height:28px;display:grid;place-items:center;padding:3px;border:0;border-radius:6px;color:var(--muted);background:transparent;cursor:grab;touch-action:none}.agenda-reorder:active{cursor:grabbing;background:var(--surface-soft)}.agenda-reorder svg{width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round}
  @media(max-width:600px){.create-fields{grid-template-columns:1fr}.create-fields .wide{grid-column:auto}.create-recurrence-fields{grid-template-columns:minmax(0,1fr)}}
  .agenda-items>li{min-width:0}.agenda-items>li:has(.calendar-copy){flex-wrap:nowrap;align-items:center}.agenda-items .todo-row{display:flex;flex-wrap:nowrap;justify-content:flex-start;padding:10px;min-width:0}.agenda-items .todo-copy,.agenda-items .calendar-copy{min-width:0;flex:1 1 auto;overflow-wrap:anywhere}.agenda-items .todo-copy p,.agenda-items .calendar-copy p{font-size:.9rem;line-height:1.3}.agenda-items .todo-copy span,.agenda-items .calendar-copy span{font-size:.75rem}
  .agenda-items>li.dragging{opacity:1!important;color:var(--text)!important;background:var(--surface-soft)!important}.agenda-items>li.dragging .todo-copy,.agenda-items>li.dragging .todo-copy p,.agenda-items>li.dragging .entry-title,.agenda-items>li.dragging .entry-metadata{color:var(--text)!important}.agenda-items>li.drop-target{border-color:transparent!important;background:transparent!important}
  .modal-scrim{position:fixed;inset:38px 0 0;z-index:8;display:grid;place-items:center;padding:18px;background:var(--color-scrim)}.agenda-dialog{width:min(100%,520px);max-height:calc(100dvh - 74px);display:flex;flex-direction:column;overflow:hidden;border:1px solid var(--border);border-radius:16px;color:var(--text);background:var(--surface);box-shadow:var(--shadow-dialog)}.agenda-dialog .create-form{min-height:0;display:flex;flex-direction:column;border:0;padding:0;background:transparent}.create-dialog-body{min-height:0;padding:16px;overflow-y:auto}.modal-heading{display:flex;align-items:center;justify-content:space-between;gap:8px}.modal-heading h2{margin:0}.modal-heading button{width:30px;height:30px;border:0;border-radius:7px;background:transparent;color:var(--text);font-size:1.4rem}.modal-actions{position:relative;z-index:2;flex:0 0 auto;display:flex;justify-content:flex-end;gap:8px;margin:0;padding:10px 16px;border-top:1px solid var(--border);background:var(--surface-soft)}.modal-delete{margin-top:14px;padding:8px 12px;border:1px solid var(--danger);border-radius:8px;color:var(--danger);background:transparent}
  .account-heading,.calendar-settings-heading{display:flex;align-items:center;justify-content:space-between;gap:10px}.account-heading .account-summary{min-width:0}.compact-action{padding:6px 10px;white-space:nowrap}.calendar-settings{margin-top:12px;padding-top:12px;border-top:1px solid var(--border)}.calendar-settings h4{margin:0}.calendar-settings-heading{margin-bottom:8px}.calendar-setting-row{display:grid;grid-template-columns:auto minmax(0,1fr) auto;align-items:center;gap:10px;padding:9px 0}.calendar-setting-row>div{min-width:0}.calendar-setting-row strong{display:block}.calendar-color-dot{width:12px;height:12px;border-radius:50%;box-shadow:0 0 0 1px var(--border)}.calendar-palette{grid-column:2 / -1;display:flex;flex-wrap:wrap;gap:7px}.color-swatch{position:relative;width:36px;height:36px;padding:4px;border:0;border-radius:50%;background:transparent;cursor:pointer}.color-swatch::before{content:"";display:block;box-sizing:border-box;width:28px;height:28px;border:2px solid transparent;border-radius:50%;background:var(--swatch);box-shadow:0 0 0 1px color-mix(in srgb,var(--swatch) 60%,var(--border))}.color-swatch:hover::before{transform:scale(1.08)}.color-swatch.selected::before{border-color:var(--surface);outline:2px solid var(--text);outline-offset:1px}.color-swatch:focus-visible{outline:3px solid var(--primary);outline-offset:2px}
  .calendar-display-range{grid-column:2 / -1;display:flex;align-items:end;flex-wrap:wrap;gap:8px}.calendar-display-range label{display:flex;align-items:center;gap:6px;margin:0;color:var(--muted);font-size:.78rem}.calendar-display-range input{width:78px;min-height:40px;padding:7px 8px}
  .mutation-settings{width:100%;padding-top:12px;border-top:1px solid var(--border)}.mutation-conflict{margin-top:8px;padding:10px;border:1px solid var(--border);border-radius:9px;background:var(--surface-soft)}.mutation-conflict>span{display:block;margin-bottom:8px;color:var(--text);font-size:.82rem}.mutation-conflict>div{display:flex;flex-wrap:wrap;gap:7px}
  .custom-color{position:relative;width:36px;height:36px;display:grid;place-items:center;border:0;border-radius:50%;color:var(--muted);cursor:pointer;overflow:hidden}.custom-color::before{content:"";position:absolute;inset:4px;border:1px dashed currentColor;border-radius:50%}.custom-color:hover{color:var(--primary)}.custom-color input{position:absolute;inset:0;width:100%;height:100%;opacity:0;cursor:pointer}.local-account{padding:18px 0}.local-calendar-row{display:grid;gap:8px;padding:10px 0;border-top:1px solid var(--border)}.new-local-calendar{display:flex;gap:8px;margin-top:10px}.new-local-calendar input:first-child{min-width:0;flex:1}.new-local-calendar input[type="color"]{width:42px;padding:2px}.calendar-name{font-weight:650}
  .detail-actions{flex:0 0 auto;display:flex;align-items:center;gap:8px;padding:10px 16px;border-top:1px solid var(--border);background:var(--surface-soft)}
  .detail-actions button{flex:0 0 auto;min-width:72px;min-height:40px;padding:7px 12px;white-space:nowrap}
  .detail-actions .primary-action{width:auto}
  .detail-actions .modal-delete{margin:0 auto 0 0;background:var(--danger);color:var(--color-on-danger)}
  @media(max-width:380px){.detail-actions{padding:8px;gap:6px}.detail-actions button{min-width:0;flex:1 1 0;padding:6px 8px}.detail-actions .modal-delete{margin:0}}
  .complete-button,.modal-heading button{width:36px;height:36px;min-width:36px;min-height:36px}

  /* Quiet Daybook fidelity: one horizontal desktop shell, calm daily rhythm. */
  :global(html),:global(body){height:100%;overflow:hidden}
  :global(body){min-width:0}
  .shell{height:100vh;min-height:0;padding:58px 34px 22px 102px;display:block;overflow-y:auto;overflow-x:hidden;background:var(--bg)}
  .titlebar{position:fixed;inset:38px auto 0 0;width:68px;padding:20px 10px;display:flex;flex-direction:column;align-items:stretch;justify-content:flex-start;overflow:hidden;border-right:1px solid var(--color-border);background:var(--surface-soft);z-index:9;transition:width var(--duration-panel) var(--ease-out),box-shadow var(--duration-standard) var(--ease-standard)}
  .titlebar:hover,.titlebar:focus-within{width:184px;box-shadow:var(--shadow-menu)}
  .app-brand{width:164px;min-width:0;display:flex;align-items:center;gap:12px;padding:0 10px 24px;font-size:.88rem}.app-brand strong{opacity:0;transform:translateX(-5px);white-space:nowrap;transition:opacity var(--duration-standard) var(--ease-standard),transform var(--duration-panel) var(--ease-out)}.titlebar:hover .app-brand strong,.titlebar:focus-within .app-brand strong{opacity:1;transform:none}
  .primary-navigation{display:grid;gap:4px}.primary-navigation button,.desktop-toggle{width:48px;min-height:40px;padding:0 14px;display:flex;align-items:center;gap:12px;overflow:hidden;border:0;border-radius:var(--radius-control);color:var(--muted);background:transparent;font:inherit;font-weight:600;text-align:left;transition:width var(--duration-panel) var(--ease-out),color var(--duration-fast) var(--ease-standard),background-color var(--duration-fast) var(--ease-standard)}.titlebar:hover .primary-navigation button,.titlebar:focus-within .primary-navigation button,.titlebar:hover .desktop-toggle,.titlebar:focus-within .desktop-toggle{width:164px}.primary-navigation button:hover,.desktop-toggle:hover{color:var(--text);background:color-mix(in srgb,var(--color-primary) 8%,transparent)}.primary-navigation button[aria-current="page"]{color:var(--text);background:color-mix(in srgb,var(--color-primary) 12%,transparent)}.primary-navigation svg,.desktop-toggle svg{width:20px;height:20px;flex:0 0 20px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}.primary-navigation span,.desktop-toggle span{opacity:0;transform:translateX(-5px);white-space:nowrap;transition:opacity var(--duration-standard) var(--ease-standard),transform var(--duration-panel) var(--ease-out)}.titlebar:hover .primary-navigation span,.titlebar:focus-within .primary-navigation span,.titlebar:hover .desktop-toggle span,.titlebar:focus-within .desktop-toggle span{opacity:1;transform:none}.desktop-toggle{margin-top:auto;cursor:pointer}
  .page-heading{max-width:900px;min-height:58px;margin:0 auto 24px;display:flex;align-items:center;justify-content:space-between;gap:20px}.settings-view .page-heading{max-width:980px;align-items:flex-start}.page-heading h1{font-size:1.75rem;font-weight:640;letter-spacing:-.025em}.day-anchor{display:flex;align-items:baseline;gap:11px}.day-number{font-size:2.75rem;line-height:.95;font-weight:640;letter-spacing:-.035em;font-variant-numeric:tabular-nums}.day-copy{display:grid;gap:1px}.day-copy strong{font-size:1rem;font-weight:620}.day-copy span{color:var(--muted);font-size:var(--font-size-caption)}
  .agenda{max-width:900px;margin:0 auto}.agenda-heading{padding-bottom:8px;border-bottom:1px solid var(--color-border)}.agenda-heading h2{font-size:var(--font-size-caption);font-weight:650;letter-spacing:.08em}.agenda-summary{font-weight:500}
  .agenda-items{gap:0}.agenda-items>li{border:0;border-bottom:1px solid var(--color-border);border-radius:0;background:transparent;transition:background-color var(--duration-fast) var(--ease-standard)}.agenda-items>li:hover{background:color-mix(in srgb,var(--color-primary) 5%,transparent)}.agenda-items>li:has(.calendar-copy){padding:6px 4px}.agenda-items .todo-row{padding:6px 4px}.agenda-time{width:52px;flex:0 0 52px;padding-top:2px;color:var(--muted);font-size:var(--font-size-caption);font-variant-numeric:tabular-nums;text-align:right}.row-open{padding:5px 7px;border:0;color:inherit;background:transparent;font:inherit;text-align:left;cursor:pointer}.row-open:focus-visible{border-radius:var(--radius-control);outline:2px solid var(--ring);outline-offset:1px}.calendar-copy p,.todo-copy p{margin-bottom:3px;font-weight:600}.complete-button::before{width:18px;height:18px}

  /* Settings reads as a compact preference sheet with one owning scroll area. */
  .settings-card{max-width:980px;margin:0 auto;padding:0;display:grid;grid-template-columns:154px minmax(0,1fr);align-items:start;border:0;border-radius:0;background:transparent;gap:28px}
  .settings-tabs{position:sticky;top:0;min-width:0;padding:2px 16px 2px 0;display:flex;flex-direction:column;gap:3px;overflow:visible;border:0;border-right:1px solid var(--color-border)}
  .settings-tabs button{width:100%;min-height:38px;padding:0 10px;text-align:left;border:0;border-radius:var(--radius-control);font-weight:580}.settings-tabs button:hover{color:var(--text);background:color-mix(in srgb,var(--color-primary) 6%,transparent)}.settings-tabs button[aria-selected="true"]{border:0;color:var(--text);background:color-mix(in srgb,var(--color-primary) 11%,transparent)}
  .settings-card>[role="tabpanel"]{width:100%;max-width:720px;min-width:0;overflow:visible}.settings-form{width:100%;max-width:520px;margin:0;padding:2px 0 20px;grid-template-columns:minmax(0,1fr);gap:14px;border:0}.settings-form+.account-layout{border-top:1px solid var(--color-border)}
  #settings-panel-calendars{max-width:780px}.account-layout{min-width:0;padding-top:20px;display:grid;grid-template-columns:172px minmax(0,1fr);gap:22px}.account-master{min-width:0;display:flex;flex-direction:column;gap:3px}.account-master>button:not(.ui-button){min-height:48px;padding:7px 9px;display:grid;gap:2px;border:0;border-radius:var(--radius-control);color:var(--muted);background:transparent;font:inherit;text-align:left}.account-master>button:hover,.account-master>button.selected{color:var(--text);background:color-mix(in srgb,var(--color-primary) 9%,transparent)}.account-master strong,.account-master span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.account-master strong{font-weight:600}.account-master span{font-size:var(--font-size-caption);font-weight:400}.account-master>:last-child{margin-top:8px}.account-detail{min-width:0;overflow:visible}.account-settings{margin:0;padding:0;border:0}.account-list{margin:0}.account-list li{padding:0;border:0;border-radius:0;background:transparent}.local-account{padding:0}.local-calendar-row{padding:12px 0}.calendar-settings{margin-top:18px}.calendar-setting-row{padding:9px 0;border-bottom:1px solid var(--color-border)}.calendar-setting-select{min-width:0;padding:4px 0;border:0;color:var(--text);background:transparent;font:inherit;text-align:left}.calendar-setting-select strong{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-weight:600}.calendar-setting-detail{grid-column:1/-1;margin:4px 0 8px 22px;padding:12px 0 14px;display:grid;gap:11px;border:0;border-top:1px solid var(--color-border);background:transparent}.calendar-setting-detail .calendar-palette,.calendar-setting-detail .calendar-display-range{grid-column:auto}.maintenance-tools{width:min(100%,520px);margin-top:20px;padding-top:14px;border-top:1px solid var(--color-border);text-align:left}.maintenance-tools summary{margin-bottom:12px;color:var(--muted);cursor:pointer}.about-panel{align-content:start}.about-brand-mark{width:64px;height:64px;margin-bottom:4px;background:url('/favicon.png') center/contain no-repeat}.account-form{margin:0;padding:0;border:0;grid-template-columns:1fr}

  /* Quiet Daybook workspace completion. */
  .shell:not(.settings-view){padding-right:350px}.settings-view{padding-right:34px}
  .titlebar:not(:hover):not(:has(:focus-visible)){width:68px;box-shadow:none}.titlebar:not(:hover):not(:has(:focus-visible)) .app-brand strong,.titlebar:not(:hover):not(:has(:focus-visible)) .primary-navigation span,.titlebar:not(:hover):not(:has(:focus-visible)) .desktop-toggle span,.titlebar:not(:hover):not(:has(:focus-visible)) .settings-toggle span,.titlebar:not(:hover):not(:has(:focus-visible)) .sidebar-calendars h2>span:last-child,.titlebar:not(:hover):not(:has(:focus-visible)) .sidebar-calendars button>span:last-child{opacity:0;transform:translateX(-5px)}
  .titlebar:hover,.titlebar:has(:focus-visible){width:210px}.titlebar:hover .app-brand strong,.titlebar:has(:focus-visible) .app-brand strong{opacity:1;transform:none}
  .primary-navigation button,.desktop-toggle,.settings-toggle,.sidebar-calendars button{width:48px;min-height:40px;padding:0 14px;display:flex;align-items:center;gap:12px;overflow:hidden;border:0;border-radius:var(--radius-control);color:var(--muted);background:transparent;font:inherit;font-weight:600;text-align:left;transition:width var(--duration-panel) var(--ease-out),color var(--duration-fast) var(--ease-standard),background-color var(--duration-fast) var(--ease-standard)}
  .titlebar:hover .primary-navigation button,.titlebar:has(:focus-visible) .primary-navigation button,.titlebar:hover .desktop-toggle,.titlebar:has(:focus-visible) .desktop-toggle,.titlebar:hover .settings-toggle,.titlebar:has(:focus-visible) .settings-toggle,.titlebar:hover .sidebar-calendars button,.titlebar:has(:focus-visible) .sidebar-calendars button{width:190px}
  .primary-navigation span,.desktop-toggle span,.settings-toggle span,.sidebar-calendars button>span:last-child,.sidebar-calendars h2>span:last-child{opacity:0;transform:translateX(-5px);white-space:nowrap;transition:opacity var(--duration-standard) var(--ease-standard),transform var(--duration-panel) var(--ease-out)}.titlebar:hover .primary-navigation span,.titlebar:has(:focus-visible) .primary-navigation span,.titlebar:hover .desktop-toggle span,.titlebar:has(:focus-visible) .desktop-toggle span,.titlebar:hover .settings-toggle span,.titlebar:has(:focus-visible) .settings-toggle span,.titlebar:hover .sidebar-calendars button>span:last-child,.titlebar:has(:focus-visible) .sidebar-calendars button>span:last-child,.titlebar:hover .sidebar-calendars h2>span:last-child,.titlebar:has(:focus-visible) .sidebar-calendars h2>span:last-child{opacity:1;transform:none}
  .sidebar-calendars{margin-top:22px;padding-top:18px;border-top:1px solid var(--color-border)}.sidebar-calendars h2{height:30px;margin:0;padding:0 15px;display:flex;align-items:center;gap:13px;color:var(--muted);font-size:var(--font-size-caption);font-weight:650}.calendar-stack-icon{width:18px;height:18px;flex:0 0 18px;border:1.5px solid currentColor;border-radius:4px;box-shadow:4px 4px 0 -2px var(--surface-soft),4px 4px 0 -1px currentColor}.sidebar-calendars button{font-size:var(--font-size-caption)}.filter-dot{width:12px;height:12px;flex:0 0 12px;margin:0 4px;border:2px solid var(--calendar-color);border-radius:50%;background:var(--calendar-color)}.filter-dot.hollow{background:transparent}.rail-bottom{margin-top:auto;display:grid;gap:3px}.desktop-toggle{margin-top:0}.settings-toggle svg{width:20px;height:20px;flex:0 0 20px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}.settings-toggle:hover,.settings-toggle[aria-current="page"]{color:var(--text);background:color-mix(in srgb,var(--color-primary) 10%,transparent)}
  .today-workspace{min-height:calc(100vh - 142px)}.agenda{--agenda-action-column:36px;max-width:780px}.timeline-items{position:relative}.timeline-items::before{content:"";position:absolute;left:75px;top:37px;bottom:0;width:1px;background:var(--color-border)}.agenda-items>li:has(.calendar-copy),.agenda-items .todo-row{padding:4px 2px;min-height:48px}.calendar-copy p,.todo-copy p{margin:0;font-size:.88rem}.agenda-reorder{top:7px}.now-line{position:relative;height:40px;margin:3px 0 4px 75px;border-top:1px solid var(--color-accent)}.now-line::before{content:"";position:absolute;left:-4px;top:-4px;width:7px;height:7px;border-radius:50%;background:var(--color-accent)}.now-line time{position:absolute;left:-70px;top:-15px;width:62px;display:flex;flex-direction:column;align-items:flex-end;gap:1px;color:var(--color-accent);font-size:var(--font-size-caption);font-variant-numeric:tabular-nums;line-height:1.05}.timeline-entry{position:relative}.timeline-entry .agenda-time{width:62px;flex:0 0 62px;padding:0;display:flex;flex-direction:column;align-items:flex-end;gap:1px;line-height:1.05}.timeline-dot{width:9px;height:9px;flex:0 0 9px;margin:0 5.5px;border-radius:50%;background:var(--completion-color,var(--color-primary))}.timeline-entry .entry-title-line{gap:12px}.timeline-entry .entry-title-line .entry-title,.timeline-calendar{color:inherit!important;font-size:inherit!important}.timeline-calendar{flex:0 1 auto;min-width:0;font-weight:400;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.timeline-empty{margin:8px 0 0 78px;color:var(--muted);font-size:var(--font-size-caption)}.todo-heading{margin-top:18px;margin-bottom:0;padding-inline:2px;display:grid;grid-template-columns:minmax(0,1fr) var(--agenda-action-column);align-items:center;flex-wrap:nowrap}:global(.todo-heading .reset-order-button){justify-self:center}:global(.todo-heading .reset-order-button svg){width:16px;height:16px}.todo-items .todo-row{display:grid;grid-template-columns:36px minmax(0,1fr) var(--agenda-action-column);align-items:center}.todo-items .agenda-reorder{position:static;width:var(--agenda-action-column);height:var(--agenda-action-column);justify-self:center}.completed-entry{color:var(--muted)}.completed-entry .todo-copy p{text-decoration:line-through}.completed-entry .calendar-name{color:var(--muted)!important}.completed-entry .agenda-reorder{opacity:.55}
  .completion-finishing,.completion-finishing .entry-title,.completion-finishing .todo-copy p{color:var(--muted)!important}.completion-finishing .entry-title,.completion-finishing .todo-copy p{text-decoration:line-through}.completion-restoring,.completion-restoring .entry-title,.completion-restoring .todo-copy p{color:var(--text)!important}.completion-restoring .entry-title,.completion-restoring .todo-copy p{text-decoration:none}
  .event-detail-pane{position:fixed;z-index:2;right:0;top:38px;bottom:0;width:318px;padding:30px 22px;overflow-y:auto;border-left:1px solid var(--color-border);background:var(--surface-soft);font-size:var(--font-size-meta)}.detail-pane-heading{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:24px}.detail-pane-heading h2{margin:0;font-size:1rem}.detail-title{width:100%;padding:0 0 18px;display:grid;grid-template-columns:12px minmax(0,1fr);align-items:start;gap:10px;border:0;border-bottom:1px solid var(--color-border);color:var(--text);background:transparent;text-align:left}.detail-title strong{font-size:1.02rem;line-height:1.4}.detail-color{width:9px;height:9px;margin-top:5px;border-radius:50%;background:var(--primary)}.detail-edit-hint{margin-top:15px;color:var(--muted);font-size:var(--font-size-caption)}.detail-empty{padding-top:44px;color:var(--muted);text-align:center}.detail-empty p{color:var(--text);font-weight:650}.detail-empty span{font-size:var(--font-size-caption);line-height:1.5}
  .sync-preferences-row{max-width:680px!important;grid-template-columns:minmax(180px,1fr) auto!important;align-items:end;column-gap:36px!important}.sync-preferences-row :global(.settings-row){grid-template-columns:auto auto;justify-content:end;column-gap:10px}.calendar-visibility{grid-column:3;grid-row:1;align-self:start;justify-self:start;width:142px}.remote-calendar-row .calendar-visibility{justify-self:end;width:auto}.calendar-visibility.disabled-copy{color:var(--muted);opacity:.62}.calendar-setting-row{grid-template-columns:12px minmax(0,1fr) 142px}.local-calendar-row.calendar-setting-row{gap:10px}.local-calendar-row .calendar-setting-detail{grid-column:1/-1}.new-local-calendar{align-items:center}
  @media(max-width:1180px){.shell:not(.settings-view){padding-right:330px}.event-detail-pane{width:300px}}
  .detail-facts{width:100%;padding:0;display:block;color:inherit;background:transparent}.detail-facts>button{width:100%;padding:12px 0;display:grid;grid-template-columns:58px minmax(0,1fr);gap:10px;border:0;border-bottom:1px solid var(--color-border);color:inherit;background:transparent;font:inherit;text-align:left}.detail-facts>button:hover{background:color-mix(in srgb,var(--color-primary) 5%,transparent)}.detail-facts>button:focus-visible{border-radius:var(--radius-control);outline:2px solid var(--ring);outline-offset:1px}.detail-facts b{color:var(--muted);font-size:var(--font-size-caption);font-weight:400}.detail-facts em{font-style:normal;line-height:1.45;overflow-wrap:anywhere}.detail-facts .detail-notes{display:block}.detail-facts .detail-notes em{display:block;margin-top:7px;white-space:pre-wrap}.workspace-blank{width:100%;min-height:72px;border:0;background:transparent;cursor:default}
  .calendar-visibility.disabled-copy :global(.switch .label){color:var(--color-text-muted)}
  .event-detail-pane.detail-editing{padding:30px 14px 0;display:flex;flex-direction:column;overflow:hidden}.detail-editing .detail-pane-heading{padding:0 8px;margin-bottom:14px}.inline-detail-form{flex:1 1 auto;min-height:0;padding:0 8px 18px;overflow-y:auto}.inline-detail-form fieldset{margin:0;padding:0;border:0}.detail-field{margin:0 -6px 6px;padding:8px 6px;border-radius:var(--radius-control);transition:background-color var(--duration-fast) var(--ease-standard)}.detail-field.active-edit-field{background:color-mix(in srgb,var(--color-primary) 11%,var(--surface))}.detail-field label{margin-bottom:4px;color:var(--muted);font-size:var(--font-size-caption);font-weight:560}.detail-field input:not([type="checkbox"]),.detail-field select,.detail-field textarea{min-height:36px;padding:7px 8px;border:0;border-radius:6px;background:color-mix(in srgb,var(--color-primary) 5%,var(--surface-soft));box-shadow:none;font-size:var(--font-size-meta)}.detail-field input:not([type="checkbox"]):focus,.detail-field select:focus,.detail-field textarea:focus{outline:0;background:color-mix(in srgb,var(--color-primary) 9%,var(--surface));box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--color-primary) 34%,transparent)}.detail-field :global(.date-time-input>input){min-height:36px;border:0;background:color-mix(in srgb,var(--color-primary) 5%,var(--surface-soft));box-shadow:none;font-size:var(--font-size-meta)}.detail-field :global(.date-time-input>input:focus){background:color-mix(in srgb,var(--color-primary) 9%,var(--surface));box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--color-primary) 34%,transparent)}.detail-field :global(.date-time-input>.icon-button){border-left:0}.detail-field textarea{min-height:72px;resize:vertical}.detail-time-fields{display:grid;grid-template-columns:minmax(0,1fr);gap:8px}.detail-metadata-fields{display:grid;gap:9px}.detail-metadata-fields .important-toggle{min-height:32px;font-size:var(--font-size-caption)}.detail-editing>.detail-actions{margin:0 -14px;padding:10px 12px}.detail-editing>.detail-actions button{min-width:0;padding:7px 9px}.page-heading :global(.new-event-action.primary){border-color:color-mix(in srgb,var(--color-primary) 24%,var(--color-border));color:var(--color-primary-hover);background:color-mix(in srgb,var(--color-primary) 14%,var(--surface))}.page-heading :global(.new-event-action.primary:hover:not(:disabled)){border-color:color-mix(in srgb,var(--color-primary) 36%,var(--color-border));color:var(--color-primary-pressed);background:color-mix(in srgb,var(--color-primary) 21%,var(--surface))}#window-mode:disabled{opacity:1;color:var(--text);-webkit-text-fill-color:var(--text);background:var(--surface)}
  /* Manual rail geometry: pinned expansion reflows the workspace; hover preview overlays it. */
  .window-chrome{left:68px;padding-left:0;transition:left var(--duration-panel) var(--ease-out)}.window-chrome.rail-pinned{left:210px}.chrome-drag-space{flex:1;align-self:stretch}
  .shell{width:calc(100% - 68px);margin-left:68px;padding-left:34px;transition:width var(--duration-panel) var(--ease-out),margin-left var(--duration-panel) var(--ease-out)}.shell.rail-pinned{width:calc(100% - 210px);margin-left:210px}
  .titlebar{inset:0 auto 0 0!important;width:68px!important;padding:0 10px 20px;z-index:21}.titlebar.rail-open{width:210px!important}.titlebar.rail-preview{box-shadow:var(--shadow-menu)}
  .app-brand{width:100%;height:58px;padding:0 4px;display:flex;align-items:center;justify-content:center;gap:8px}.app-brand.expanded{justify-content:space-between;padding-left:10px}.app-brand strong{opacity:1!important;transform:none!important;font-size:.86rem}.rail-toggle{width:40px;height:40px;flex:0 0 40px;display:grid;place-items:center;border:0;border-radius:var(--radius-control);color:var(--muted);background:transparent}.rail-toggle:hover{color:var(--text);background:color-mix(in srgb,var(--color-primary) 9%,transparent)}.rail-toggle svg{width:20px;height:20px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}
  .titlebar:not(.rail-open) .primary-navigation button,.titlebar:not(.rail-open) .desktop-toggle,.titlebar:not(.rail-open) .settings-toggle,.titlebar:not(.rail-open) .sidebar-calendars button{width:48px}.titlebar.rail-open .primary-navigation button,.titlebar.rail-open .desktop-toggle,.titlebar.rail-open .settings-toggle,.titlebar.rail-open .sidebar-calendars button{width:190px}.titlebar:not(.rail-open) .primary-navigation span,.titlebar:not(.rail-open) .desktop-toggle span,.titlebar:not(.rail-open) .settings-toggle span,.titlebar:not(.rail-open) .sidebar-calendars button>span:last-child,.titlebar:not(.rail-open) .sidebar-calendars h2>span:last-child{opacity:0!important;transform:translateX(-5px)!important}.titlebar.rail-open .primary-navigation span,.titlebar.rail-open .desktop-toggle span,.titlebar.rail-open .settings-toggle span,.titlebar.rail-open .sidebar-calendars button>span:last-child,.titlebar.rail-open .sidebar-calendars h2>span:last-child{opacity:1!important;transform:none!important}
  #settings-panel-general,#settings-panel-desktop{display:grid;justify-items:center}#settings-panel-general .settings-form,#settings-panel-desktop .settings-form{margin-inline:auto}
  .create-modal-scrim{background:color-mix(in srgb,var(--color-text) 30%,transparent)}.create-help{margin:2px 0 18px}.create-form :global(.tag-input .controls){background:var(--surface)}.create-form :global(.tag-input .controls input){min-height:36px;padding-inline:10px}
  .event-detail-pane.detail-editing{padding:30px 22px;overflow-y:auto}.detail-editing .detail-pane-heading{padding:0;margin-bottom:24px}.inline-detail-form{flex:0 0 auto;padding:0;overflow:visible}.detail-field{margin:0;padding:12px 0;display:grid;grid-template-columns:58px minmax(0,1fr);align-items:center;gap:10px;border-radius:0;border-bottom:1px solid var(--color-border);background:transparent}.detail-field.active-edit-field{border-radius:var(--radius-control);background:color-mix(in srgb,var(--color-primary) 11%,var(--surface))}.detail-field>label{margin:0}.detail-field input:not([type="checkbox"]),.detail-field select,.detail-field textarea,.detail-field :global(.date-time-input>input){padding-inline:0;border:0;background:transparent;box-shadow:none}.detail-field input:not([type="checkbox"]):focus,.detail-field select:focus,.detail-field textarea:focus,.detail-field :global(.date-time-input>input:focus){outline:0;background:transparent;box-shadow:none}.detail-field :global(.date-time-input>.icon-button){border-left:0}.detail-title-editor{grid-template-columns:12px minmax(0,1fr);padding:0 0 18px}.detail-title-editor.active-edit-field{padding:0 0 18px}.detail-title-editor input{min-height:32px!important;padding:0!important;font-size:1.02rem!important;font-weight:620;background:transparent!important}.detail-time-fields{display:grid;grid-template-columns:1fr}.detail-time-fields>div{display:grid;grid-template-columns:58px minmax(0,1fr);align-items:center;gap:10px}.detail-time-fields>div+div{margin-top:6px}.detail-metadata-fields{display:grid;grid-template-columns:1fr;gap:8px}.detail-metadata-fields .important-toggle{min-height:32px}.detail-editing>.detail-actions{margin:12px 0 0;padding:10px 0 0;background:transparent}.detail-facts>button.detail-notes{color:inherit;background:transparent}.detail-facts>button.detail-notes:hover{background:color-mix(in srgb,var(--color-primary) 5%,transparent)}.agenda.hide-time .timeline-items::before{left:11px}.agenda.hide-time .now-line time{display:none}.agenda.hide-time .timeline-empty{margin-left:14px}
  .titlebar.rail-pinned,.titlebar.rail-pinned:hover,.titlebar.rail-pinned:focus-within{box-shadow:none!important}
  .primary-action,.secondary-action,.compact-action,.settings-tabs button,.account-actions button,.about-actions button{min-width:0;max-width:100%;height:auto;white-space:normal;overflow-wrap:anywhere;text-align:center}.account-actions,.about-actions,.modal-actions,.detail-actions{flex-wrap:wrap}.modal-heading button svg{width:18px;height:18px;display:block;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round}
  .entry-title-line{min-width:0;display:flex;align-items:baseline;gap:12px}.entry-title{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.entry-tags{flex:0 1 auto;min-width:0;color:var(--muted);font-size:var(--font-size-caption);font-weight:400;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.entry-metadata{display:flex!important;align-items:baseline;gap:12px}.entry-metadata>*{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  .detail-field,.detail-field.active-edit-field{background:transparent}.detail-field input:not([type="checkbox"]),.detail-field select,.detail-field textarea,.detail-field :global(.date-time-input>input),.detail-field :global(.tag-input .controls),.detail-field.active-edit-field input:not([type="checkbox"]),.detail-field.active-edit-field select,.detail-field.active-edit-field textarea,.detail-field.active-edit-field :global(.date-time-input>input),.detail-field.active-edit-field :global(.tag-input .controls){border:0!important;outline:0!important;background:var(--color-field-idle)!important;box-shadow:none!important}.detail-field input:not([type="checkbox"]):focus,.detail-field select:focus,.detail-field textarea:focus,.detail-field :global(.date-time-input>input:focus),.detail-field :global(.tag-input .controls:focus-within){border:0!important;outline:0!important;background:var(--color-field-idle)!important;box-shadow:var(--focus-ring)!important}.detail-field :global(.tag-input .controls input),.detail-field :global(.tag-input .controls input:focus-visible){border:0!important;outline:0!important;background:transparent!important;box-shadow:none!important}.detail-field select:disabled,.detail-field input:disabled{color:var(--muted)!important;background:var(--surface-soft)!important;opacity:.72}
  .event-detail-pane.detail-editing{overflow:hidden}.detail-editing>.inline-detail-form{flex:1 1 auto;min-height:0;padding-bottom:16px;overflow-y:auto}.detail-editing>.detail-actions{position:relative;z-index:2;flex:0 0 auto;margin:0 -22px -30px;padding:10px 22px;border-top:1px solid var(--color-border);background:var(--surface-soft)}
  .event-detail-pane{overflow-x:hidden}.detail-editing>.inline-detail-form{box-sizing:border-box;width:100%;max-width:100%;min-width:0;padding:4px 4px 16px;overflow-x:hidden}.inline-detail-form fieldset,.inline-detail-form .detail-field,.inline-detail-form input,.inline-detail-form select,.inline-detail-form textarea,.detail-field :global(.date-time-input){max-width:100%;min-width:0}
  .pending-remote-detail{margin-top:18px;padding-top:14px;display:flex;align-items:center;justify-content:space-between;gap:12px;border-top:1px solid var(--color-border);color:var(--muted);font-size:var(--font-size-caption)}
  .create-fields input:not([type="checkbox"]),.create-fields select,.create-fields textarea,.create-fields :global(.date-time-input>input),.create-fields :global(.tag-input .controls){border:0!important;outline:0!important;background:var(--color-field-idle)!important;box-shadow:none!important}
  .create-fields input:not([type="checkbox"]):focus,.create-fields select:focus,.create-fields textarea:focus,.create-fields :global(.date-time-input>input:focus),.create-fields :global(.tag-input .controls:focus-within){outline:0!important;box-shadow:var(--focus-ring)!important}.create-fields :global(.tag-input .controls input),.create-fields :global(.tag-input .controls input:focus-visible){border:0!important;outline:0!important;background:transparent!important;box-shadow:none!important}
  .detail-field :global(.date-time-input>.picker-trigger>.icon-button){border-left:0}
  .timeline-entry .entry-title-line .entry-title{color:inherit!important;font-size:.88rem!important}.timeline-calendar{color:var(--muted)!important;font-size:var(--font-size-caption)!important}
  .calendar-visibility,.remote-calendar-row .calendar-visibility{justify-self:stretch;width:142px;display:flex;justify-content:flex-end}.calendar-setting-row,.calendar-settings,.account-list,.account-list li{width:100%}
  .calendar-settings-heading h3,.calendar-settings-heading h4{margin:0}.calendar-settings-hint{margin-left:auto;color:var(--muted);font-size:.72rem;font-weight:400;white-space:nowrap}
  .window-controls button{color:var(--color-text-muted)}.window-controls svg{width:16px;height:16px;overflow:visible;opacity:1;visibility:visible;stroke:currentColor!important;stroke-width:1.6}.window-controls svg path,.window-controls svg rect{fill:none;stroke:inherit;stroke-width:inherit;vector-effect:non-scaling-stroke}
  .error-log-clear{margin-right:auto}.now-line::before{display:none}.now-line time{position:static;grid-column:1;width:auto;display:flex;flex-direction:column;align-items:flex-end;gap:1px;color:var(--color-accent);font-size:var(--font-size-caption);font-variant-numeric:tabular-nums;line-height:1.05}.now-marker{grid-column:2/4;display:flex;align-items:center;min-width:0}.now-marker::before{content:"";width:7px;height:7px;flex:0 0 7px;margin-left:6.5px;border-radius:50%;background:var(--color-accent)}.now-marker::after{content:"";height:1px;flex:1;margin-left:6.5px;background:var(--color-accent)}.agenda.hide-time .now-marker{grid-column:1/3}
  .agenda>.agenda-heading:first-child{margin-bottom:0;padding-bottom:0;border-bottom:0}
  .agenda-items>.timeline-entry:has(.calendar-copy){display:block;padding:0;border-top:1px solid var(--color-border);border-bottom:1px solid var(--color-border);overflow:visible}
  .timeline-items>.timeline-entry+.timeline-entry{border-top:0}
  .timeline-items::before{top:0}
  .timeline-content{display:flex;align-items:center;gap:0;min-height:48px;padding:4px 2px}
  .timeline-content>.calendar-copy{flex:1;min-width:0}
  .timeline-items>.now-slot{padding:0;border:0;overflow:visible;background:transparent}
  .timeline-items .now-line{position:relative;display:grid;grid-template-columns:62px 20px minmax(0,1fr);align-items:center;height:26px;min-height:26px;margin:0;padding:2px;border:0}
  .timeline-items .now-line time{position:static;width:auto;display:block;text-align:right}
  .agenda.hide-time .timeline-items .now-line{grid-template-columns:20px minmax(0,1fr)}
  .agenda.hide-time .timeline-items .now-line time{display:none}
  .agenda-dialog{--surface:var(--color-create-surface);--color-field-idle:var(--color-create-field)}
  .event-detail-pane{--surface-soft:var(--color-detail-surface);--color-field-idle:var(--color-detail-field)}
  .create-fields,.detail-field{--focus-ring:var(--field-focus-ring)}
  .now-line time{color:var(--color-current-time-text)}
  .create-fields input:disabled:not([type="checkbox"]),.create-fields select:disabled,.create-fields textarea:disabled,.detail-field input:disabled:not([type="checkbox"]),.detail-field select:disabled,.detail-field textarea:disabled{opacity:1!important;color:var(--color-text-muted)!important;background:var(--color-field-disabled)!important}
</style>
