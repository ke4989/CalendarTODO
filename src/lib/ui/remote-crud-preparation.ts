import type { UpdateCalendarLocalMetadata, CalendarAgendaItem, RemoteCrudKind, RemoteCrudOperation, RemoteCrudReceipt, RemoteCrudRequest, RemoteCrudResponse, RemoteSelectedText } from "../ipc/contracts";
import type { ConfirmedEventRequest, ConfirmedEventView, ExactEventScope } from "../ipc/contracts";
import { remoteText } from "../i18n";

export type TextSelection = "retain" | "set" | "clear";
export interface RemoteDraft {
  title: string;
  selectTitle: boolean;
  description: string;
  descriptionAction: TextSelection;
  location: string;
  locationAction: TextSelection;
  selectTime: boolean;
  start: string;
  end: string;
  confirmed: boolean;
  base?: RemoteUpdateBase;
  metadata?: UpdateCalendarLocalMetadata;
}
type RemoteUpdateBase = { title?: string; description?: RemoteSelectedText; location?: RemoteSelectedText; time?: { startAtUtcMs: number; endAtUtcMs: number } };
export function selectedBase(item: CalendarAgendaItem, draft: RemoteDraft): RemoteUpdateBase {
  const text = (value: string | null): RemoteSelectedText => value ? { action: "set", value } : { action: "clear" };
  return { ...(draft.selectTitle ? { title: item.title } : {}), ...(draft.descriptionAction !== "retain" ? { description: text(item.notes) } : {}), ...(draft.locationAction !== "retain" ? { location: text(item.location) } : {}), ...(draft.selectTime ? { time: { startAtUtcMs: item.startAtUtcMs!, endAtUtcMs: item.endAtUtcMs! } } : {}) };
}
export type FieldErrors = Partial<Record<"title" | "description" | "location" | "start" | "end" | "confirmation" | "selection", string>>;
export interface PreparationContext {
  accountId: string;
  calendarId: string;
  resourceId?: string;
  candidate: boolean;
}
export function utcInput(at: number | null): string {
  return at !== null && Number.isSafeInteger(at) && at >= 0 && at < 4102444800000 && at % 1000 === 0
    ? new Date(at).toISOString().slice(0, 19) : "";
}
/** The input is explicitly UTC, never interpreted in the machine's local zone. */
export function parseUtcInput(value: string): number | null {
  // Native datetime-local omits trailing :00; that still denotes an exact second.
  if (/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value)) value += ":00";
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$/.test(value)) return null;
  const at = Date.parse(`${value}Z`);
  if (!Number.isSafeInteger(at) || at < 0 || at >= 4102444800000) return null;
  return new Date(at).toISOString().slice(0, 19) === value ? at : null;
}
export function initialDraft(item?: CalendarAgendaItem): RemoteDraft {
  return { title: item?.title ?? "", selectTitle: false, description: item?.notes ?? "", descriptionAction: "retain", location: item?.location ?? "", locationAction: "retain", selectTime: false, start: utcInput(item?.startAtUtcMs ?? null), end: utcInput(item?.endAtUtcMs ?? null), confirmed: false };
}
export function applyRemoteDraft(item: CalendarAgendaItem, draft: RemoteDraft): CalendarAgendaItem {
  const selectedText = (action: TextSelection, value: string, current: string | null) => action === "set" ? value : action === "clear" ? null : current;
  return {
    ...item,
    title: draft.selectTitle ? draft.title.trim() : item.title,
    notes: selectedText(draft.descriptionAction, draft.description, item.notes),
    location: selectedText(draft.locationAction, draft.location, item.location),
    startAtUtcMs: draft.selectTime ? parseUtcInput(draft.start) : item.startAtUtcMs,
    endAtUtcMs: draft.selectTime ? parseUtcInput(draft.end) : item.endAtUtcMs,
  };
}
export function preparationHint(item?: CalendarAgendaItem): string {
  if (!item) return remoteText("仅支持 UTC 定时事件。目的日历的写入资格由后端确认。", "Only timed UTC events are supported. The backend confirms whether the destination calendar is writable.");
  if (item.allDay) return remoteText("全天 DATE 事件仅可查看，不能准备此类修改或删除。", "All-day DATE events are read-only. Updates and deletions cannot be prepared for them.");
  if (item.status !== "ready") {
    if (item.unsupportedReason === "unsupportedRecurrence") return remoteText("重复事件或重复例外仅可查看。", "Recurring events and recurrence exceptions are read-only.");
    if (item.unsupportedReason === "unsupportedTime") return remoteText("TZID、非 UTC 或无法安全解析的时间仅可查看。", "Events with TZID, non-UTC, or times that cannot be parsed safely are read-only.");
    return remoteText("此资源为只读或包含无法安全解析的内容。", "This resource is read-only or contains content that cannot be parsed safely.");
  }
  return remoteText("可起草标题、描述、地点与完整 UTC 时间对。邀请、重复、只读权限及原始时间结构仍由后端检查；显示的字段不代表写入资格。", "You can draft a title, description, location, and complete UTC time pair. The backend still checks invitations, recurrence, read-only permissions, and the original time structure; displayed fields do not imply write eligibility.");
}
export function projectionCandidate(item?: CalendarAgendaItem): boolean {
  return !item || item.status === "ready" && !item.allDay && item.startAtUtcMs !== null && item.endAtUtcMs !== null && item.startAtUtcMs < item.endAtUtcMs;
}
export function draftRequest(kind: RemoteCrudKind, context: PreparationContext, draft: RemoteDraft, key: string): { request?: RemoteCrudRequest; errors: FieldErrors } {
  const errors: FieldErrors = {};
  if (!context.candidate || !context.accountId || !context.calendarId || kind !== "createUtcV2" && !context.resourceId) errors.selection = remoteText("此资源当前只能查看；请选择可准备的日历事件。", "This resource is currently read-only. Select a calendar event that can be prepared.");
  if (kind === "delete") {
    if (!draft.confirmed) errors.confirmation = remoteText("请明确确认删除服务器上的此事件。", "Explicitly confirm that you want to delete this event from the server.");
    return Object.keys(errors).length ? { errors } : { errors, request: { action: "delete", input: { idempotencyKey: key, resourceId: context.resourceId!, confirmation: "confirmed" } } };
  }
  const titleSelected = kind === "createUtcV2" || draft.selectTitle;
  if (titleSelected && (!draft.title.trim() || [...draft.title].length > 512)) errors.title = remoteText("请输入 1–512 个字符的标题。", "Enter a title between 1 and 512 characters.");
  if ((kind === "createUtcV2" || draft.descriptionAction === "set") && [...draft.description].length > 10000) errors.description = remoteText("描述最多 10000 个字符。", "The description can contain up to 10,000 characters.");
  if ((kind === "createUtcV2" || draft.locationAction === "set") && [...draft.location].length > 512) errors.location = remoteText("地点最多 512 个字符。", "The location can contain up to 512 characters.");
  if (kind === "updateEventV3" && draft.descriptionAction === "set" && !draft.description.trim()) errors.description = remoteText("请输入描述；移除内容请明确选择清空。", "Enter a description. To remove it, explicitly select Clear.");
  if (kind === "updateEventV3" && draft.locationAction === "set" && !draft.location.trim()) errors.location = remoteText("请输入地点；移除内容请明确选择清空。", "Enter a location. To remove it, explicitly select Clear.");
  const timeSelected = kind === "createUtcV2" || draft.selectTime;
  const start = parseUtcInput(draft.start), end = parseUtcInput(draft.end);
  if (timeSelected) {
    if (start === null) errors.start = remoteText("请输入 1970–2099 年的有效 UTC 时间，精确到秒。", "Enter a valid UTC time from 1970 through 2099, precise to the second.");
    if (end === null) errors.end = remoteText("请输入 1970–2099 年的有效 UTC 时间，精确到秒。", "Enter a valid UTC time from 1970 through 2099, precise to the second.");
    else if (start !== null && start >= end) errors.end = remoteText("结束必须晚于开始；两个时间一起保存。", "The end must be later than the start. Both times are saved together.");
  }
  if (kind === "updateEventV3" && !titleSelected && draft.descriptionAction === "retain" && draft.locationAction === "retain" && !timeSelected) errors.selection = remoteText("至少选择一个需要修改的字段。未选择的字段保持原样。", "Select at least one field to update. Unselected fields remain unchanged.");
  if (Object.keys(errors).length) return { errors };
  if (kind === "createUtcV2") return { errors, request: { action: "create", input: { idempotencyKey: key, calendarId: context.calendarId, title: draft.title.trim(), ...(draft.description ? { description: draft.description } : {}), ...(draft.location ? { location: draft.location } : {}), startAtUtcMs: start!, endAtUtcMs: end! } } };
  const text = (action: TextSelection, value: string): RemoteSelectedText | undefined => action === "set" ? { action: "set", value } : action === "clear" ? { action: "clear" } : undefined;
  const description = text(draft.descriptionAction, draft.description), location = text(draft.locationAction, draft.location);
  return { errors, request: { action: "update", input: { idempotencyKey: key, resourceId: context.resourceId!, fields: { ...(titleSelected ? { title: draft.title.trim() } : {}), ...(description ? { description } : {}), ...(location ? { location } : {}), ...(timeSelected ? { time: { startAtUtcMs: start!, endAtUtcMs: end! } } : {}) } } } };
}

export type PreparationPhase = "draft" | "pending" | "accepted" | "rejected";
export interface PreparationState {
  phase: PreparationPhase;
  message: string;
  errors: FieldErrors;
  receipt: RemoteCrudReceipt | null;
  operations: RemoteCrudOperation[];
  busy: boolean;
}
export type PreparePort = (request: RemoteCrudRequest) => Promise<RemoteCrudResponse>;
export type ConfirmedPort = (request: ConfirmedEventRequest) => Promise<ConfirmedEventView>;
export type ConfirmedListener = (callback: (view: ConfirmedEventView) => void) => Promise<() => void>;
/** Bounded cause text per typed backend code; every variant keeps the draft and request key. */
export function confirmedFailureMessage(code: unknown): string {
  return code === "remoteTargetChanged" ? remoteText("同步后事件或日历已改变。草稿仍保留，请刷新后重新确认。", "The event or calendar changed after syncing. Your draft is preserved; refresh and confirm it again.")
    : code === "remoteSyncUnavailable" ? remoteText("同步失败，本次修改尚未保存。草稿仍在；重试相同内容沿用同一请求。", "Sync failed, so this change was not saved. Your draft is preserved; retrying the same content reuses the same request.")
    : code === "remoteConnectionUnavailable" ? remoteText("连接未完成，未保存远程操作。草稿仍在；重试相同内容沿用同一请求。", "The connection did not complete, so the remote operation was not saved. Your draft is preserved; retrying the same content reuses the same request.")
    : code === "remoteEventIdentityChanged" ? remoteText("本机记录的事件身份或范围已改变，未保存此修改。草稿仍在；请刷新后重新确认。", "The locally recorded event identity or scope changed, so this change was not saved. Your draft is preserved; refresh and confirm it again.")
    : code === "accountUnavailable" ? remoteText("账户当前不可用于远程编辑。草稿仍在；重试相同内容沿用同一请求。", "The account is currently unavailable for remote editing. Your draft is preserved; retrying the same content reuses the same request.")
    : code === "remoteEventReadOnly" ? remoteText("服务器上的此事件当前不能安全编辑。草稿仍在；请同步或刷新后重试。", "This server event cannot currently be edited safely. Your draft is preserved; sync or refresh before retrying.")
    : code === "remoteMutationPending" ? remoteText("此事件已有尚未完成的远程操作。草稿仍在；请刷新操作状态。", "This event already has an unfinished remote operation. Your draft is preserved; refresh the operation status.")
    : code === "mutationUnavailable" ? remoteText("本机未能完成此远程编辑。草稿仍在；重试相同内容沿用同一请求。", "This device could not complete the remote edit. Your draft is preserved; retrying the same content reuses the same request.")
    : code === "validation" ? remoteText("此修改未通过本机校验。草稿仍在；请修正后重试相同内容。", "This change failed local validation. Your draft is preserved; correct it and retry the same content.")
    : remoteText("连接、同步或保存未完成。草稿仍在；重试相同内容沿用同一请求。", "The connection, sync, or save did not complete. Your draft is preserved; retrying the same content reuses the same request.");
}
function confirmedFailureCode(error: unknown): unknown {
  if (typeof error === "object" && error !== null && "code" in error && typeof error.code === "string") return error.code;
  const serialized = typeof error === "string" ? error
    : error instanceof Error ? error.message
    : undefined;
  if (!serialized) return undefined;
  try {
    const parsed: unknown = JSON.parse(serialized);
    return typeof parsed === "object" && parsed !== null && "code" in parsed && typeof parsed.code === "string" ? parsed.code : undefined;
  } catch {
    return undefined;
  }
}

export interface BackgroundRemoteWriteCallbacks {
  applied: () => void;
  failed: (diagnostic: string) => void;
}
export type BackgroundRemoteWriteResult =
  | { started: false; errors: FieldErrors; message: string }
  | { started: true; stop: () => void; receipt: RemoteCrudReceipt & { calendarId: string; resourceId: string } };

export async function retryBackgroundRemoteWrite(
  scope: ExactEventScope,
  confirmedPort: ConfirmedPort,
  listen: ConfirmedListener,
  callbacks: BackgroundRemoteWriteCallbacks,
): Promise<{ started: boolean; stop: () => void }> {
  let stopped = false;
  let unlisten: (() => void) | undefined;
  const exact = (view: ConfirmedEventView) => view.receipt.operationId === scope.operationId
    && view.receipt.kind === scope.kind
    && view.receipt.accountId === scope.accountId
    && view.receipt.calendarId === scope.calendarId
    && view.receipt.resourceId === scope.resourceId;
  const finish = (failure?: string) => {
    if (stopped) return;
    stopped = true;
    unlisten?.();
    failure ? callbacks.failed(failure) : callbacks.applied();
  };
  const accept = (view: ConfirmedEventView) => {
    if (stopped || !exact(view)) return;
    if (view.operation?.state === "applied") finish();
    else if (view.failure || view.operation?.state === "conflict" || view.operation?.state === "cancelled"
      || ["deferred", "conflict", "failed", "cancelled"].includes(view.runtimeState ?? "")) {
      finish(`failure=${view.failure ?? "none"};operation=${view.operation?.state ?? "none"};runtime=${view.runtimeState}`);
    }
  };
  try {
    unlisten = await listen(accept);
    const view = await confirmedPort({ action: "retry", input: scope });
    if (!exact(view)) throw Error("scope mismatch");
    accept(view);
    return { started: true, stop: () => { if (!stopped) { stopped = true; unlisten?.(); } } };
  } catch {
    stopped = true;
    unlisten?.();
    return { started: false, stop: () => {} };
  }
}

/** Starts one confirmed operation whose result lifetime is independent of an editor modal. */
export async function startBackgroundRemoteWrite(
  kind: RemoteCrudKind,
  context: PreparationContext,
  draft: RemoteDraft,
  confirmedPort: ConfirmedPort,
  listen: ConfirmedListener,
  callbacks: BackgroundRemoteWriteCallbacks,
  mint = () => crypto.randomUUID(),
): Promise<BackgroundRemoteWriteResult> {
  const built = draftRequest(kind, context, draft, "");
  if (!built.request || built.request.action === "status" || built.request.action === "resolveConflict") {
    return { started: false, errors: built.errors, message: remoteText("请修正标出的字段后再确认。", "Correct the highlighted fields before confirming.") };
  }
  const baseRequest = built.request;
  let key: string;
  try { key = mint(); }
  catch { return { started: false, errors: {}, message: remoteText("无法生成安全请求，请重试。", "A secure request could not be generated. Try again.") }; }

  let stopped = false;
  let unlisten: (() => void) | undefined;
  let operationId: string | undefined;
  const buffered: ConfirmedEventView[] = [];
  const exact = (view: ConfirmedEventView, expectedOperation?: string) => {
    const receipt = view?.receipt, operation = view?.operation;
    return !!receipt && receipt.kind === kind && receipt.accountId === context.accountId
      && receipt.calendarId === context.calendarId && !!receipt.resourceId
      && (!context.resourceId || receipt.resourceId === context.resourceId)
      && !!receipt.operationId && (!expectedOperation || receipt.operationId === expectedOperation)
      && (!operation || operation.operationId === receipt.operationId && operation.calendarId === receipt.calendarId
        && operation.resourceId === receipt.resourceId && ["pending", "applied", "conflict", "cancelled"].includes(operation.state));
  };
  const finish = (failure?: string) => {
    if (stopped) return;
    stopped = true;
    unlisten?.();
    failure ? callbacks.failed(failure) : callbacks.applied();
  };
  const accept = (view: ConfirmedEventView) => {
    if (stopped) return;
    if (!operationId) { buffered.push(view); return; }
    if (!exact(view, operationId)) return;
    if (view.operation?.state === "applied") finish();
    else if (view.failure || view.operation?.state === "conflict" || view.operation?.state === "cancelled"
      || view.runtimeState === "deferred" || view.runtimeState === "conflict" || view.runtimeState === "failed" || view.runtimeState === "cancelled") {
      finish(`failure=${view.failure ?? "none"};operation=${view.operation?.state ?? "none"};runtime=${view.runtimeState}`);
    }
  };
  try {
    unlisten = await listen(accept);
    if (stopped) return { started: false, errors: {}, message: confirmedFailureMessage(undefined) };
    const request = { action: "confirm", kind, input: { ...baseRequest.input, idempotencyKey: key, confirmation: "confirmed", ...(kind === "updateEventV3" ? { base: draft.base, metadata: draft.metadata } : {}) } } as ConfirmedEventRequest;
    const view = await confirmedPort(request);
    if (stopped || !exact(view)) throw Object.assign(Error("scope mismatch"), { code: "remoteEventIdentityChanged" });
    const receipt = view.receipt;
    if (!receipt.calendarId || !receipt.resourceId) throw Object.assign(Error("scope mismatch"), { code: "remoteEventIdentityChanged" });
    operationId = receipt.operationId;
    accept(view);
    for (const pending of buffered.splice(0)) accept(pending);
    if (!stopped && view.operation?.state === "pending" && !view.failure) {
      void confirmedPort({ action: "status", input: { kind, operationId, accountId: receipt.accountId, calendarId: receipt.calendarId, resourceId: receipt.resourceId } })
        .then(accept)
        .catch(() => { /* The live exact event remains authoritative; do not poll. */ });
    }
    return { started: true, receipt: { ...receipt, calendarId: receipt.calendarId, resourceId: receipt.resourceId }, stop: () => { if (!stopped) { stopped = true; unlisten?.(); } } };
  } catch (error) {
    stopped = true;
    unlisten?.();
    return { started: false, errors: {}, message: confirmedFailureMessage(confirmedFailureCode(error)) };
  }
}
export class PreparationSession {
  private alive = true;
  private fingerprint = "";
  private key = "";
  private revision = 0;
  private receiptRevision = -1;
  private subscription: Promise<void> = Promise.resolve();
  private unlisten: (() => void) | undefined;
  state: PreparationState = { phase: "draft", message: remoteText("草稿只留在当前窗口；尚未保存意图。", "The draft exists only in this window; no intent has been saved yet."), errors: {}, receipt: null, operations: [], busy: false };
  constructor(readonly kind: RemoteCrudKind, private context: PreparationContext, private port: PreparePort, private publish: (state: PreparationState) => void, private mint = () => crypto.randomUUID(), private confirmedPort?: ConfirmedPort, listen?: ConfirmedListener) {
    if (confirmedPort && listen) this.subscription = listen(view => this.receive(view)).then(unlisten => { if (this.alive) this.unlisten = unlisten; else unlisten(); }).catch(() => {});
  }
  private change(values: Partial<PreparationState>) { if (this.alive) { this.state = { ...this.state, ...values }; this.publish(this.state); } }
  dispose() { this.alive = false; this.revision++; this.unlisten?.(); this.unlisten = undefined; }
  eligibility(candidate: boolean) { this.context = { ...this.context, candidate }; }
  edited() { this.revision++; this.receiptRevision = -1; this.change({ phase: "draft", errors: {}, receipt: null, operations: [], busy: false, message: remoteText("草稿已修改；修改内容尚未保存为意图。", "The draft was edited; the changes have not been saved as an intent.") }); }
  private exactScope(): ExactEventScope | undefined {
    const receipt = this.state.receipt;
    if (!this.alive || this.receiptRevision !== this.revision || receipt?.kind !== this.kind || receipt.accountId !== this.context.accountId || receipt.calendarId !== this.context.calendarId || !receipt.resourceId || this.context.resourceId && receipt.resourceId !== this.context.resourceId) return;
    return { kind: this.kind, operationId: receipt.operationId, accountId: receipt.accountId, calendarId: receipt.calendarId, resourceId: receipt.resourceId };
  }
  private acceptConfirmed(view: ConfirmedEventView, operationId?: string): boolean {
    const receipt = view?.receipt, operation = view?.operation;
    if (!receipt || receipt.kind !== this.kind || receipt.accountId !== this.context.accountId || receipt.calendarId !== this.context.calendarId || !receipt.resourceId || this.context.resourceId && receipt.resourceId !== this.context.resourceId || !receipt.operationId || operationId && receipt.operationId !== operationId) return false;
    if (operation && (operation.operationId !== receipt.operationId || operation.calendarId !== receipt.calendarId || operation.resourceId !== receipt.resourceId || !["pending", "applied", "conflict", "cancelled"].includes(operation.state))) return false;
    if (view.failure !== null && !["ownerUnavailable", "submitUnavailable", "statusUnavailable", "retryLocalBlocked"].includes(view.failure)) return false;
    if (view.runtimeState !== null && !["queued", "running", "idle", "deferred", "conflict", "failed", "cancelled"].includes(view.runtimeState)) return false;
    const previous = this.state.operations.find(previous => previous.operationId === receipt.operationId);
    if (previous && previous.state !== "pending" && operation && (operation.state === "pending" || previous.state === "applied" && operation.state !== "applied" || previous.state === "cancelled" && operation.state !== "cancelled")) return true;
    const message = view.failure === "retryLocalBlocked" ? remoteText("本机重试已阻止：需刷新远端数据取得新的 ETag；此处不会启动普通同步。", "Local retry was blocked: refresh the remote data to obtain a new ETag. This action will not start a regular sync.")
      : operation?.state === "applied" ? this.kind === "delete" ? remoteText("事件已删除，已核对远端结果。", "The event was deleted and the remote result was verified.") : remoteText("日程已同步，已核对远端内容。", "The event was synced and the remote content was verified.")
      : operation?.state === "cancelled" ? remoteText("已保留远端版本并取消原意图。", "The remote version was kept and the original intent was cancelled.")
      : operation?.state === "conflict" ? remoteText("远端内容已改变，存在冲突。可保留远端版本。", "The remote content changed, causing a conflict. You can keep the remote version.")
      : view.failure ? remoteText("意图已保存，但当前绑定或执行所有权不可用；可刷新此操作状态后重试相同内容。", "The intent was saved, but its current binding or execution ownership is unavailable. Refresh this operation's status, then retry the same content.")
      : view.runtimeState === "deferred" || view.runtimeState === "failed" || view.runtimeState === "cancelled" ? remoteText("意图仍待处理，未确认同步成功。请刷新此操作状态后重试；恢复会先核对远端。", "The intent is still pending and successful sync has not been confirmed. Refresh this operation's status before retrying; recovery checks the remote state first.")
      : remoteText("意图已保存，正在同步此操作；可显式刷新状态。", "The intent was saved and this operation is syncing. You can refresh its status explicitly.");
    this.receiptRevision = this.revision;
    this.change({ phase: "accepted", receipt: { kind: receipt.kind, operationId: receipt.operationId, accountId: receipt.accountId, calendarId: receipt.calendarId, resourceId: receipt.resourceId, createdAtMs: receipt.createdAtMs, productionExecutionAvailable: false }, operations: operation ? [{ operationId: operation.operationId, calendarId: operation.calendarId, resourceId: operation.resourceId, state: operation.state, createdAtMs: operation.createdAtMs, updatedAtMs: operation.updatedAtMs, resolutionState: operation.resolutionState, retryOperationId: operation.retryOperationId, revision: operation.revision, predecessorOperationId: operation.predecessorOperationId, stage: operation.stage }] : [], message });
    return true;
  }
  private receive(view: ConfirmedEventView) {
    const scope = this.exactScope();
    if (scope && this.context.candidate) this.acceptConfirmed(view, scope.operationId);
  }
  async remoteWrite(draft?: RemoteDraft): Promise<boolean> {
    if (!this.alive || !this.confirmedPort || !draft || this.state.busy) return false;
    const built = draftRequest(this.kind, this.context, draft, "");
    if (!built.request || built.request.action === "status" || built.request.action === "resolveConflict") { this.change({ errors: built.errors, phase: "rejected", message: remoteText("请修正标出的字段后再确认。", "Correct the highlighted fields before confirming.") }); return false; }
    const fingerprint = JSON.stringify(built.request);
    const changed = fingerprint !== this.fingerprint;
    if (changed) { try { this.key = this.mint(); this.fingerprint = fingerprint; } catch { this.change({ phase: "rejected", message: remoteText("无法生成安全请求。草稿保留，请重试。", "A secure request could not be generated. Your draft is preserved; try again.") }); return false; } }
    const revision = this.revision;
    this.change({ busy: true, phase: "pending", errors: {}, ...(changed ? {receipt: null, operations: []} : {}), message: remoteText("正在连接并同步…", "Connecting and syncing…") });
    try {
      await this.subscription;
      if (!this.alive || revision !== this.revision) return false;
      const request = { action: "confirm", kind: this.kind, input: { ...built.request.input, idempotencyKey: this.key, confirmation: "confirmed" } } as ConfirmedEventRequest;
      const view = await this.confirmedPort(request);
      if (!this.alive || revision !== this.revision) return false;
      if (!this.acceptConfirmed(view)) throw Object.assign(Error("scope mismatch"), { code: "remoteEventIdentityChanged" });
      // One exact read closes the event-before-command-receipt race; never poll.
      const scope = this.exactScope();
      if (scope && view.operation?.state === "pending" && !view.failure) {
        try { const latest = await this.confirmedPort({ action: "status", input: scope }); if (this.alive && revision === this.revision) this.acceptConfirmed(latest, scope.operationId); } catch { /* Keep the durable receipt; explicit refresh remains available. */ }
      }
      return true;
    } catch (error) {
      const code = confirmedFailureCode(error);
      if (this.alive && revision === this.revision) this.change({ phase: "rejected", message: confirmedFailureMessage(code) });
      return false;
    }
    finally { if (this.alive && revision === this.revision) this.change({ busy: false }); }
  }
  async prepare(draft: RemoteDraft) {
    if (!this.alive || this.state.busy) return;
    const revision = this.revision;
    const built = draftRequest(this.kind, this.context, draft, "");
    if (!built.request) { this.change({ errors: built.errors, phase: "rejected", message: remoteText("请修正标出的字段后再准备。", "Correct the highlighted fields before preparing this operation.") }); return; }
    const fingerprint = JSON.stringify(built.request);
    if (fingerprint !== this.fingerprint) {
      try { this.key = this.mint(); this.fingerprint = fingerprint; }
      catch { this.change({ phase: "rejected", message: remoteText("无法生成安全请求。草稿保留，请重试或重新打开窗口。", "A secure request could not be generated. Your draft is preserved; try again or reopen the window.") }); return; }
    }
    built.request.input = { ...built.request.input, idempotencyKey: this.key } as typeof built.request.input;
    this.change({ busy: true, phase: "pending", errors: {}, message: remoteText("正在向本机保存意图；没有远端写入。", "Saving the intent on this device; no remote write is being made."), receipt: null });
    try {
      const result = await this.port(built.request);
      if (!this.alive || revision !== this.revision) return;
      if (result.type !== "receipt" || result.data.kind !== this.kind || result.data.accountId !== this.context.accountId || this.context.resourceId && result.data.resourceId !== this.context.resourceId || this.kind === "createUtcV2" && result.data.calendarId !== this.context.calendarId) throw new Error("receipt mismatch");
      this.change({ phase: "accepted", receipt: result.data, message: remoteText("本机已接受此意图。远端执行尚未启用，服务器内容未由此次操作修改。", "This device accepted the intent. Remote execution is not enabled, so this operation did not modify server content."), operations: [] });
    } catch { if (this.alive && revision === this.revision) this.change({ phase: "rejected", message: remoteText("未能确认意图保存。草稿仍在，可检查资格后重试；相同内容沿用同一请求。", "Intent saving could not be confirmed. Your draft is preserved; check eligibility and retry. The same content reuses the same request.") }); }
    finally { if (this.alive && revision === this.revision) this.change({ busy: false }); }
  }
  async refresh() {
    if (!this.alive || this.state.busy || !this.context.accountId) return;
    if (this.confirmedPort) {
      const scope = this.exactScope(); if (!scope) return;
      const revision = this.revision; this.change({ busy: true, message: remoteText("正在读取此操作状态…", "Reading this operation's status…") });
      try { const view = await this.confirmedPort({ action: "status", input: scope }); if (this.alive && revision === this.revision && !this.acceptConfirmed(view, scope.operationId)) throw Error("scope mismatch"); }
      catch { if (this.alive && revision === this.revision) this.change({ message: remoteText("此操作状态暂不可读取；草稿与回执保留，可再次刷新。", "This operation's status is temporarily unavailable. The draft and receipt are preserved; you can refresh again.") }); }
      finally { if (this.alive && revision === this.revision) this.change({ busy: false }); }
      return;
    }
    this.change({ busy: true, message: remoteText("正在读取本机记录…", "Reading local records…") });
    try {
      const result = await this.port({ action: "status", input: { accountId: this.context.accountId, kind: this.kind } });
      if (result.type !== "status" || result.data.kind !== this.kind || result.data.accountId !== this.context.accountId) throw new Error("status mismatch");
      const operations = result.data.operations.filter((op) => this.context.resourceId ? op.resourceId === this.context.resourceId && op.calendarId === this.context.calendarId : op.operationId === this.state.receipt?.operationId);
      this.change({ operations, message: remoteText("已读取本机状态。待处理不表示已同步；当前不会执行远端写入。", "Local status was read. Pending does not mean synced; no remote write will run now.") });
    } catch { this.change({ message: remoteText("本机状态暂不可读取。可再次刷新，草稿不会丢失。", "Local status is temporarily unavailable. You can refresh again; your draft will not be lost.") }); }
    finally { this.change({ busy: false }); }
  }
  async resolve(operationId: string, choice: "keepRemote" | "retryLocal") {
    if (!this.alive || this.state.busy || choice === "retryLocal" && !this.context.candidate || this.kind === "createUtcV2" || !this.state.operations.some((op) => op.operationId === operationId && op.state === "conflict" && op.resolutionState === "unresolved")) return;
    if (this.confirmedPort) {
      const scope = this.exactScope(); if (!scope || scope.operationId !== operationId) return;
      const revision = this.revision; this.change({ busy: true, message: remoteText("正在保存此操作的冲突选择…", "Saving the conflict choice for this operation…") });
      try { const view = await this.confirmedPort({ action: "resolveConflict", input: { scope, choice } }); if (this.alive && revision === this.revision && !this.acceptConfirmed(view, operationId)) throw Error("scope mismatch"); }
      catch { if (this.alive && revision === this.revision) this.change({ message: remoteText("冲突选择未确认；请刷新此操作状态后重试。", "The conflict choice was not confirmed. Refresh this operation's status before retrying.") }); }
      finally { if (this.alive && revision === this.revision) this.change({ busy: false }); }
      return;
    }
    this.change({ busy: true, message: remoteText("正在保存冲突选择…", "Saving the conflict choice…") });
    try {
      const result = await this.port({ action: "resolveConflict", input: { operationId, kind: this.kind, choice } });
      if (result.type !== "conflict" || result.data.kind !== this.kind || result.data.accountId !== this.context.accountId || result.data.operationId !== operationId || result.data.choice !== choice) throw new Error("conflict mismatch");
      this.change({ operations: this.state.operations.map((op) => op.operationId === operationId ? { ...op, state: choice === "keepRemote" ? "cancelled" : "conflict", resolutionState: choice, retryOperationId: result.data.retryOperationId } : op), message: choice === "keepRemote" ? remoteText("已保留远端版本并取消原意图；没有远端写入。", "The remote version was kept and the original intent was cancelled; no remote write was made.") : remoteText("已准备新的本机重试意图；远端执行仍需启用。", "A new local retry intent was prepared; remote execution still needs to be enabled.") });
    } catch { this.change({ message: remoteText("冲突选择未获确认。请刷新本机状态后重试；未发送远端写入。", "The conflict choice was not confirmed. Refresh local status before retrying; no remote write was sent.") }); }
    finally { this.change({ busy: false }); }
  }
}
