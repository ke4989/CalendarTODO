import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ConfirmedEventRequest, ConfirmedEventView } from "./contracts";
import type { RemoteCrudRequest, RemoteCrudResponse } from "./contracts";
import type { AppSettings, AttachmentView, UpdateCheckResult, BootstrapState, CalendarAgendaItem, CalendarSettingsView, ConfirmedRemoteEventDelete, ConflictResolutionChoice, CreateCalendarSubtask, CreateLocalTodo, CreateSubtask, DeleteLocalCalendar, DeletedLocalCalendar, EnrollAccountCredential, LocalCalendar, LocalTodo, MutationApplicationView, RegisterRuntimeAccount, RuntimeAccountProfile, SaveLocalCalendar, SaveLocalTodo, SetLocalCalendarHidden, SourceAttachmentMode, SyncExecutionView, UpdateAppSettings, UpdateCalendarLocalMetadata, UpdateCalendarSubtask, UpdateLocalTodo, UpdateSubtask } from "./contracts";

export function getBootstrapState(): Promise<BootstrapState> {
  return invoke<BootstrapState>("get_bootstrap_state");
}

export function listLocalTodos(): Promise<LocalTodo[]> {
  return invoke<LocalTodo[]>("list_local_todos");
}

export function listLocalCalendars(): Promise<LocalCalendar[]> { return invoke<LocalCalendar[]>("list_local_calendars"); }
export function saveLocalCalendar(input: SaveLocalCalendar): Promise<LocalCalendar> { return invoke<LocalCalendar>("save_local_calendar", { input }); }
export function setLocalCalendarHidden(input: SetLocalCalendarHidden): Promise<LocalCalendar> { return invoke<LocalCalendar>("set_local_calendar_hidden", { input }); }
export function deleteLocalCalendar(input: DeleteLocalCalendar): Promise<DeletedLocalCalendar> { return invoke<DeletedLocalCalendar>("delete_local_calendar", { input }); }
export function moveLocalTodoToCalendar(todoId: string, calendarId: string): Promise<LocalTodo> { return invoke<LocalTodo>("move_local_todo_to_calendar", { todoId, calendarId }); }

export function getDesktopItemOrder(): Promise<string[]> {
  return invoke<string[]>("get_desktop_item_order");
}

export function setDesktopItemOrder(keys: string[]): Promise<void> {
  return invoke<void>("set_desktop_item_order", { keys });
}

export function resetDesktopItemOrder(): Promise<void> {
  return invoke<void>("reset_desktop_item_order");
}

export function listCalendarAgendaItems(): Promise<CalendarAgendaItem[]> {
  return invoke<CalendarAgendaItem[]>("list_calendar_agenda_items");
}

export function listCalendarSettings(): Promise<CalendarSettingsView[]> {
  return invoke<CalendarSettingsView[]>("list_calendar_settings");
}
export function discoverRemoteCalendars(accountId: string): Promise<CalendarSettingsView[]> { return invoke<CalendarSettingsView[]>("discover_remote_calendars", { accountId }); }

export function setCalendarColorOverride(calendarId: string, color: string | null): Promise<void> {
  return invoke<void>("set_calendar_color_override", { calendarId, color });
}
export function setCalendarVisible(accountId: string, calendarId: string, visible: boolean): Promise<void> { return invoke<void>("set_calendar_visible", { accountId, calendarId, visible }); }
export function setCalendarDisplayRange(accountId: string, calendarId: string, displayPastDays: number | null, displayFutureDays: number | null): Promise<void> { return invoke<void>("set_calendar_display_range", { accountId, calendarId, displayPastDays, displayFutureDays }); }

export function refreshCalendarHorizon(): Promise<void> {
  return invoke<void>("refresh_calendar_horizon");
}

export function listAccountProfiles(): Promise<RuntimeAccountProfile[]> {
  return invoke<RuntimeAccountProfile[]>("list_account_profiles");
}

export function registerAccountProfile(input: RegisterRuntimeAccount): Promise<RuntimeAccountProfile> {
  return invoke<RuntimeAccountProfile>("register_account_profile", { input });
}

export function setAccountProfileEnabled(accountId: string, enabled: boolean): Promise<RuntimeAccountProfile> {
  return invoke<RuntimeAccountProfile>("set_account_profile_enabled", { accountId, enabled });
}

export function setAccountRemoteEditingEnabled(accountId: string, enabled: boolean): Promise<RuntimeAccountProfile> {
  return invoke<RuntimeAccountProfile>("set_account_remote_editing_enabled", { accountId, enabled });
}

export function getRemoteMutationView(accountId: string): Promise<MutationApplicationView> {
  return invoke<MutationApplicationView>("get_remote_mutation_view", { accountId });
}

/** Raw inner JSON lets the backend reject duplicate/unknown semantic fields. */
export function prepareRemoteCrud(request: RemoteCrudRequest): Promise<RemoteCrudResponse> {
  return invoke<RemoteCrudResponse>("prepare_remote_crud", { input: JSON.stringify(request) });
}
export function confirmedRemoteEvent(request: ConfirmedEventRequest): Promise<ConfirmedEventView> {
  return invoke<ConfirmedEventView>("confirmed_remote_event", { input: JSON.stringify(request) });
}
export function listenConfirmedEvent(callback: (view: ConfirmedEventView) => void): Promise<() => void> {
  return listen<ConfirmedEventView>("calendar-todo://confirmed-event-state", event => callback(event.payload));
}

export function queueConfirmedRemoteEventDelete(input: ConfirmedRemoteEventDelete): Promise<MutationApplicationView> {
  return invoke<MutationApplicationView>("queue_confirmed_remote_event_delete", { input });
}

export function resolveRemoteMutationConflict(accountId: string, operationId: string, choice: ConflictResolutionChoice): Promise<MutationApplicationView> {
  return invoke<MutationApplicationView>("resolve_remote_mutation_conflict", { accountId, operationId, choice });
}

export function removeAccountProfile(accountId: string): Promise<void> {
  return invoke<void>("remove_account_profile", { accountId });
}

export function enrollAccountCredential(input: EnrollAccountCredential): Promise<RuntimeAccountProfile> {
  return invoke<RuntimeAccountProfile>("enroll_account_credential", { input });
}

export function attachAccountSource(accountId: string, mode: SourceAttachmentMode): Promise<AttachmentView> {
  return invoke<AttachmentView>("attach_account_source", { accountId, mode });
}

export function listAttachmentStates(): Promise<AttachmentView[]> {
  return invoke<AttachmentView[]>("list_attachment_states");
}

export function triggerAccountSync(accountId: string): Promise<SyncExecutionView> {
  return invoke<SyncExecutionView>("trigger_account_sync", { accountId });
}

export function listAccountSyncStates(): Promise<SyncExecutionView[]> {
  return invoke<SyncExecutionView[]>("list_account_sync_states");
}

export function setCalendarCompleted(seriesId: string, recurrenceKey: string, completed: boolean): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("set_calendar_completed", { seriesId, recurrenceKey, completed });
}

export function setCalendarImportant(seriesId: string, recurrenceKey: string, important: boolean): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("set_calendar_important", { seriesId, recurrenceKey, important });
}
export function updateCalendarLocalMetadata(input: UpdateCalendarLocalMetadata): Promise<void> {
  return invoke<void>("update_calendar_local_metadata", { input });
}

export function createCalendarSubtask(input: CreateCalendarSubtask): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("create_calendar_subtask", { input });
}

export function updateCalendarSubtask(input: UpdateCalendarSubtask): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("update_calendar_subtask", { input });
}

export function setCalendarSubtaskCompleted(seriesId: string, recurrenceKey: string, subtaskId: string, completed: boolean): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("set_calendar_subtask_completed", { seriesId, recurrenceKey, subtaskId, completed });
}

export function deleteCalendarSubtask(seriesId: string, recurrenceKey: string, subtaskId: string): Promise<CalendarAgendaItem> {
  return invoke<CalendarAgendaItem>("delete_calendar_subtask", { seriesId, recurrenceKey, subtaskId });
}

export function createLocalTodo(input: CreateLocalTodo): Promise<LocalTodo> {
  return invoke<LocalTodo>("create_local_todo", { input });
}

export function updateLocalTodo(input: UpdateLocalTodo): Promise<LocalTodo> {
  return invoke<LocalTodo>("update_local_todo", { input });
}

export function saveLocalTodo(input: SaveLocalTodo): Promise<LocalTodo> {
  return invoke<LocalTodo>("save_local_todo", { input });
}

export function setLocalTodoCompleted(todoId: string, occurrenceKey: string, completed: boolean): Promise<LocalTodo> {
  return invoke<LocalTodo>("set_local_todo_completed", { todoId, occurrenceKey, completed });
}

export function deleteLocalTodo(todoId: string): Promise<void> {
  return invoke<void>("delete_local_todo", { todoId });
}
export function deleteLocalTodoOccurrence(todoId: string, occurrenceKey: string): Promise<void> {
  return invoke<void>("delete_local_todo_occurrence", { todoId, occurrenceKey });
}

export function restoreRecentlyDeletedTodo(todoId: string): Promise<LocalTodo> {
  return invoke<LocalTodo>("restore_recently_deleted_todo", { todoId });
}
export function restoreRecentlyDeletedTodoOccurrence(todoId: string, occurrenceKey: string): Promise<LocalTodo> {
  return invoke<LocalTodo>("restore_recently_deleted_todo_occurrence", { todoId, occurrenceKey });
}

export function setTraySource(choice: "local" | "icloudChinaReadOnly"): Promise<void> {
  return invoke<void>("set_tray_source", { choice });
}

export function showMainFromTray(): Promise<void> {
  return invoke<void>("show_main_from_tray");
}

export function showCalendarSettingsFromTray(): Promise<void> {
  return invoke<void>("show_calendar_settings_from_tray");
}

export function hideTrayMenu(): Promise<void> {
  return invoke<void>("hide_tray_menu");
}

export function trayMenuReady(generation: number): Promise<void> {
  return invoke<void>("tray_menu_ready", { generation });
}

/** Reports the tray menu's measured content height so Rust can size the
 *  popover to its real content instead of a fixed card. */
export function resizeTrayMenu(height: number): Promise<void> {
  return invoke<void>("resize_tray_menu", { height });
}

export function quitFromTray(): Promise<void> {
  return invoke<void>("quit_from_tray");
}

export function createSubtask(input: CreateSubtask): Promise<LocalTodo> {
  return invoke<LocalTodo>("create_subtask", { input });
}

export function updateSubtask(input: UpdateSubtask): Promise<LocalTodo> {
  return invoke<LocalTodo>("update_subtask", { input });
}

export function moveLocalSubtask(input: import("./contracts").MoveLocalSubtask): Promise<LocalTodo> {
  return invoke<LocalTodo>("move_local_subtask", { input });
}

export function setSubtaskCompleted(todoId: string, subtaskId: string, completed: boolean): Promise<LocalTodo> {
  return invoke<LocalTodo>("set_subtask_completed", { todoId, subtaskId, completed });
}

export function deleteSubtask(todoId: string, subtaskId: string): Promise<LocalTodo> {
  return invoke<LocalTodo>("delete_subtask", { todoId, subtaskId });
}

export function getAppSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("get_app_settings");
}
export function getAppVersion(): Promise<string> { return invoke<string>("get_app_version"); }
export function checkForUpdate(): Promise<UpdateCheckResult> { return invoke<UpdateCheckResult>("check_for_update"); }
export function openProductRepository(): Promise<void> { return invoke<void>("open_product_repository"); }
export function openLatestRelease(): Promise<void> { return invoke<void>("open_latest_release"); }
export function recordUiError(action: string, code: string, message: string): Promise<void> { return invoke<void>("record_ui_error", { action, code, message }); }
export function openErrorLog(): Promise<void> { return invoke<void>("open_error_log"); }
export function readErrorLog(): Promise<string> { return invoke<string>("read_error_log"); }
export function clearErrorLog(): Promise<void> { return invoke<void>("clear_error_log"); }

export function desktopListVisible(): Promise<boolean> {
  return invoke<boolean>("desktop_list_visible");
}

export function updateAppSettings(input: UpdateAppSettings): Promise<AppSettings> {
  return invoke<AppSettings>("update_app_settings", { input });
}

export function purgeExpiredRemoteCache(): Promise<number> {
  return invoke<number>("purge_expired_remote_cache");
}

export function startDesktopDrag(): Promise<void> {
  return invoke<void>("start_desktop_drag");
}

export function desktopPinned(): Promise<boolean> {
  return invoke<boolean>("desktop_pinned");
}

export function desktopCollapsed(): Promise<boolean> {
  return invoke<boolean>("desktop_collapsed");
}

export function setDesktopPinned(pinned: boolean): Promise<boolean> {
  return invoke<boolean>("set_desktop_pinned", { pinned });
}

export function setDesktopCollapsed(collapsed: boolean): Promise<void> {
  return invoke<void>("set_desktop_collapsed", { collapsed });
}
