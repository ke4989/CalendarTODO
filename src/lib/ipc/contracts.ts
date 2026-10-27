export interface BootstrapState {
  schemaVersion: number;
  backendReady: boolean;
  currentPhase: "productionReadOnlyAttachment";
  defaultWindowMode: "desktopCompanion";
  persistenceReady: boolean;
}

export type Priority = "low" | "normal" | "high";
export type RecurrenceFrequency = "daily" | "weekly" | "monthly" | "yearly" | "customWeekly" | "customMonthly" | "customYearly";
export type RecurrenceEndKind = "count" | "until";
export interface RecurrenceOptions { timezone: string; weekdays: number[]; monthDays: number[]; yearMonths: number[]; endKind: RecurrenceEndKind; endCount: number | null; endUntilAtUtcMs: number | null; }
export type WindowMode = "desktopCompanion" | "alwaysOnTop";
export type AppLanguage = "system" | "zh-CN" | "en";
export type ThemePalette = "quietDaybook" | "dawnWheat";
export type ColorMode = "system" | "light" | "dark";
export interface AppSettings {
  language: AppLanguage;
  /** Presentation-only palette and light/dark preference. */
  themePalette: ThemePalette;
  colorMode: ColorMode;
  windowMode: WindowMode;
  syncIntervalMinutes: number;
  syncOnStartup: boolean;
  desktopListMode: boolean;
  desktopOpacityPercent: number;
  desktopShowCalendarName: boolean;
  desktopShowTime: boolean;
  desktopShowTags: boolean;
  mainShowCalendarName: boolean;
  mainShowTime: boolean;
  mainShowTags: boolean;
  /** When set, a launch keeps the main window hidden in the tray. */
  startHiddenInTray: boolean;
  /** Registers the application in the per-user Windows startup list. */
  launchAtLogin: boolean;
  localTodoColor: string;
  localDisplayPastDays: number | null;
  localDisplayFutureDays: number | null;
  defaultCalendarId: string | null;
  /** Turns Windows reminder delivery on or off for every local and iCloud item. */
  reminderNotificationsEnabled: boolean;
  desktopPinned: boolean;
  desktopCollapsed: boolean;
  updatedAtMs: number;
}
export interface UpdateAppSettings {
  language: AppLanguage;
  themePalette: ThemePalette;
  colorMode: ColorMode;
  windowMode: WindowMode;
  syncIntervalMinutes: number;
  syncOnStartup: boolean;
  desktopListMode: boolean;
  desktopOpacityPercent: number;
  desktopShowCalendarName: boolean;
  desktopShowTime: boolean;
  desktopShowTags: boolean;
  mainShowCalendarName: boolean;
  mainShowTime: boolean;
  mainShowTags: boolean;
  startHiddenInTray: boolean;
  launchAtLogin: boolean;
  localTodoColor: string;
  localDisplayPastDays: number | null;
  localDisplayFutureDays: number | null;
  defaultCalendarId: string | null;
  reminderNotificationsEnabled: boolean;
}
export type UpdateCheckResult = { status: "upToDate" } | { status: "available"; version: string };
export type RuntimeAccountStatus = "disabled" | "actionRequired" | "ready";
export type AccountActionRequired = "credentialSetup" | "sourceAttachment";
export type SourceAttachmentMode = "synthetic" | "icloudChinaReadOnly";
export type AttachmentState = "detached" | "attaching" | "ready" | "actionRequired" | "failed";
export type AttachmentFailure = "credential" | "disabled" | "unsafeConfiguration" | "unavailable";
export interface AttachmentView { accountId: string; sourceMode: SourceAttachmentMode | null; state: AttachmentState; failure: AttachmentFailure | null; }
export interface RuntimeAccountProfile {
  accountId: string;
  providerKind: "caldav";
  userLabel: string;
  regionProfile: string;
  enabled: boolean;
  sourceAttached: boolean;
  credentialConfigured: boolean;
  remoteEditingEnabled: boolean;
  status: RuntimeAccountStatus;
  actionRequired: AccountActionRequired | null;
}
export type RemoteMutationState = "pending" | "conflict" | "applied" | "cancelled";
export type ConflictResolutionState = "unresolved" | "keepRemote" | "retryLocal";
export type ConflictResolutionChoice = "keepRemote" | "retryLocal";
export type MutationRuntimeState = "queued" | "running" | "idle" | "deferred" | "conflict" | "failed" | "cancelled";
export interface MutationAuthorizationView { canQueueIntent: boolean; canKeepRemote: boolean; canRetryLocal: boolean; productionExecutionAvailable: boolean; }
export interface MutationOperationView { operationId: string; resourceId: string; state: RemoteMutationState; updatedAtMs: number; }
export interface MutationConflictView { operationId: string; resourceId: string; resolutionState: ConflictResolutionState; retryOperationId: string | null; detectedAtMs: number; resolvedAtMs: number | null; }
export interface MutationRuntimeView { accountId: string; state: MutationRuntimeState; appliedCount: number; conflictCount: number; }
export interface MutationApplicationView {
  accountId: string;
  authorization: MutationAuthorizationView;
  runtime: MutationRuntimeView;
  operationCount: number;
  conflictCount: number;
  operations: MutationOperationView[];
  conflicts: MutationConflictView[];
}
export interface ConfirmedRemoteEventDelete {
  idempotencyKey: string;
  resourceId: string;
  confirmation: "confirmed";
}
/** Admission/status preparation only. No remote worker is activated. */
export type RemoteCrudKind = "createUtcV2" | "updateEventV3" | "delete";
export interface RemoteCreateUtcV2Input {
  idempotencyKey: string;
  calendarId: string;
  title: string;
  description?: string;
  location?: string;
  startAtUtcMs: number;
  endAtUtcMs: number;
}
export type RemoteSelectedText = { action: "set"; value: string } | { action: "clear" };
export interface RemoteUpdateEventV3Input {
  idempotencyKey: string;
  resourceId: string;
  fields: {
    title?: string;
    description?: RemoteSelectedText;
    location?: RemoteSelectedText;
    time?: { startAtUtcMs: number; endAtUtcMs: number };
  };
}
export type RemoteCrudRequest =
  | { action: "create"; input: RemoteCreateUtcV2Input }
  | { action: "update"; input: RemoteUpdateEventV3Input }
  | { action: "delete"; input: ConfirmedRemoteEventDelete }
  | { action: "status"; input: { accountId: string; kind: RemoteCrudKind } }
  | { action: "resolveConflict"; input: { operationId: string; kind: RemoteCrudKind; choice: ConflictResolutionChoice } };
export interface RemoteCrudReceipt {
  kind: RemoteCrudKind;
  operationId: string;
  accountId: string;
  calendarId: string | null;
  resourceId: string | null;
  createdAtMs: number;
  productionExecutionAvailable: false;
}
export interface RemoteCrudOperation {
  operationId: string;
  calendarId: string;
  resourceId: string;
  state: RemoteMutationState;
  createdAtMs: number;
  updatedAtMs: number;
  resolutionState: ConflictResolutionState | null;
  retryOperationId: string | null;
  revision: number;
  predecessorOperationId: string | null;
  stage: "admittedPending" | "running" | "recoveryNeeded" | "applied" | "conflict" | "cancelled";
}
export interface RemoteCrudStatus {
  accountId: string;
  kind: RemoteCrudKind;
  operationCount: number;
  operations: RemoteCrudOperation[];
  productionExecutionAvailable: false;
}
export interface RemoteCrudConflictReceipt {
  kind: RemoteCrudKind;
  operationId: string;
  accountId: string;
  choice: ConflictResolutionChoice;
  retryOperationId: string | null;
  productionExecutionAvailable: false;
}
export type RemoteCrudResponse =
  | { type: "receipt"; data: RemoteCrudReceipt }
  | { type: "status"; data: RemoteCrudStatus }
  | { type: "conflict"; data: RemoteCrudConflictReceipt };
export interface ExactEventScope { kind: RemoteCrudKind; operationId: string; accountId: string; calendarId: string; resourceId: string }
export type ConfirmedEventRequest =
  | { action: "confirm"; kind: "updateEventV3"; input: RemoteUpdateEventV3Input & { confirmation: "confirmed"; base?: RemoteUpdateEventV3Input["fields"]; metadata?: UpdateCalendarLocalMetadata } }
  | { action: "confirm"; kind: "createUtcV2"; input: RemoteCreateUtcV2Input & { confirmation: "confirmed" } }
  | { action: "confirm"; kind: "delete"; input: ConfirmedRemoteEventDelete }
  | { action: "status"; input: ExactEventScope }
  | { action: "retry"; input: ExactEventScope }
  | { action: "resolveConflict"; input: { scope: ExactEventScope; choice: ConflictResolutionChoice } };
export interface ConfirmedEventView {
  receipt: RemoteCrudReceipt;
  operation: RemoteCrudOperation | null;
  runtimeState: MutationRuntimeState | null;
  failure: "ownerUnavailable" | "submitUnavailable" | "statusUnavailable" | "retryLocalBlocked" | null;
}
export interface RegisterRuntimeAccount {
  userLabel: string;
  regionProfile: string;
  enabled: boolean;
}
export interface EnrollAccountCredential {
  accountId: string;
  username: string;
  password: string;
}
export type SyncExecutionState = "detached" | "idle" | "running" | "succeeded" | "actionRequired" | "failed";
export type SyncExecutionTrigger = "startup" | "manual" | "periodic" | "retry";
export type SyncExecutionPath = "incremental" | "fullInventoryFallback";
export type SyncExecutionFailure = "transient" | "unauthorized" | "incomplete" | "unsafeData" | "validation" | "persistence";
export interface SyncExecutionView {
  accountId: string;
  state: SyncExecutionState;
  trigger: SyncExecutionTrigger | null;
  path: SyncExecutionPath | null;
  failure: SyncExecutionFailure | null;
  coalescedRequests: number;
  nextRetryDelayMs: number | null;
}
export interface FixedRecurrence { seriesId: string; frequency: RecurrenceFrequency; interval: number; options?: RecurrenceOptions; }
export interface FixedRecurrenceInput { frequency: RecurrenceFrequency; interval: number; options?: RecurrenceOptions; }

export interface LocalTodo {
  todoId: string;
  calendarId: string;
  calendarName: string;
  calendarColor: string;
  workItemId: string | null;
  occurrenceKey: string;
  title: string;
  notes: string | null;
  location: string | null;
  startAtUtcMs: number | null;
  dueAtUtcMs: number | null;
  priority: Priority;
  important: boolean;
  completed: boolean;
  completedAtMs?: number | null;
  recurrence: FixedRecurrence | null;
  backlogCount: number;
  tags: Tag[];
  subtasks: Subtask[];
  createdAtMs: number;
  updatedAtMs: number;
}
export interface LocalTodoCompletionChanged { todoId: string; previousOccurrenceKey: string; completed: boolean; item: LocalTodo; }

export interface LocalCalendar { calendarId: string; name: string; color: string; isDefault: boolean; hidden: boolean; displayPastDays: number | null; displayFutureDays: number | null; }
export interface SaveLocalCalendar { calendarId: string | null; name: string; color: string; displayPastDays: number | null; displayFutureDays: number | null; }
export interface SetLocalCalendarHidden { calendarId: string; hidden: boolean; }
export interface DeleteLocalCalendar { calendarId: string; }
export interface DeletedLocalCalendar { movedTodos: number; }

export interface Tag { tagId: string; name: string; }

export interface Subtask {
  subtaskId: string;
  workItemId: string;
  text: string;
  completed: boolean;
  sortOrder: number;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface CreateLocalTodo {
  title: string;
  priority: Priority;
  important: boolean;
}

export interface UpdateLocalTodo {
  todoId: string;
  selectedOccurrenceKey?: string | null;
  title: string;
  notes: string | null;
  location: string | null;
  startAtUtcMs: number | null;
  dueAtUtcMs: number | null;
  priority: Priority;
  important: boolean;
  tagNames: string[];
  recurrence: FixedRecurrenceInput | null;
}

export interface SaveLocalTodo extends Omit<UpdateLocalTodo, "todoId"> {
  todoId: string | null;
  calendarId: string;
}

export interface CreateSubtask { todoId: string; occurrenceKey: string; text: string; }
export interface UpdateSubtask { todoId: string; subtaskId: string; text: string; }
export interface MoveLocalSubtask {
  todoId: string;
  subtaskId: string;
  direction: "up" | "down";
  expectedNeighborId: string | null;
}

export interface IpcError {
  code: "validation" | "persistence" | "window" | "autostart" | "accountDisabled" | "credentialSetupRequired" | "credentialActionRequired" | "sourceSelectionRequired" | "sourceDetached" | "syncUnavailable" | "subtaskMoveUnavailable" | "subtaskOrderChanged" | "remoteEventReadOnly" | "remoteMutationPending";
  message: string;
}

export type CalendarProjectionStatus = "ready" | "unsupported";
export type CalendarUnsupportedReason = "invalidStructure" | "unsupportedRecurrence" | "unsupportedTime" | "invalidData";
export interface AgendaRefreshNotice { reason: "recurrenceHorizon" | "calendarSync"; }

export interface CalendarAgendaItem {
  remoteEditEligible?: boolean;
  remoteEditBlockedReason?: string | null;
  remoteTimeEditEligible?: boolean;
  seriesId: string;
  resourceId: string;
  recurrenceKey: string;
  calendarId: string;
  calendarName: string;
  calendarColor: string | null;
  title: string;
  notes: string | null;
  location: string | null;
  startAtUtcMs: number | null;
  endAtUtcMs: number | null;
  allDay: boolean;
  status: CalendarProjectionStatus;
  unsupportedReason: CalendarUnsupportedReason | null;
  workItemId: string | null;
  completed: boolean;
  completedAtMs?: number | null;
  important: boolean;
  tags: Tag[];
  recurrence: CalendarFixedRecurrence | null;
  subtasks: Subtask[];
  pendingRemote: {
    operationId: string;
    accountId: string;
    kind: "createUtcV2" | "updateEventV3" | "delete";
    status: "pending" | "retryNeeded" | "conflict";
  } | null;
}
export interface CalendarCompletionChanged { seriesId: string; previousRecurrenceKey: string; completed: boolean; item: CalendarAgendaItem; }
export interface CalendarFixedRecurrence { frequency: RecurrenceFrequency; interval: number; options?: RecurrenceOptions; }
export interface UpdateCalendarLocalMetadata {
  resourceId: string;
  tagNames: string[];
  recurrence: CalendarFixedRecurrence | null;
  important?: boolean;
}

export interface CalendarSettingsView {
  remoteSessionReady?: boolean;
  remoteCreateEligible?: boolean;
  remoteCreateBlockedReason?: string | null;
  calendarId: string;
  accountId: string;
  displayName: string;
  serverColor: string | null;
  colorOverride: string | null;
  effectiveColor: string | null;
  available: boolean;
  visible: boolean;
  displayPastDays: number | null;
  displayFutureDays: number | null;
}

export interface CreateCalendarSubtask { seriesId: string; recurrenceKey: string; text: string; }
export interface UpdateCalendarSubtask { seriesId: string; recurrenceKey: string; subtaskId: string; text: string; }
