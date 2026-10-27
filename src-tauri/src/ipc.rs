use crate::{
    adapters::window,
    application::account_profiles::{
        AccountProfileService, AccountProfileServiceError, EnrollAccountCredential,
        RegisterRuntimeAccount, RuntimeAccountView, SourceAttachmentMode,
    },
    application::agenda::{AgendaService, AgendaServiceError},
    application::attachment_runtime::{AttachmentRuntime, AttachmentView},
    application::discovery_runtime::{DiscoveryError, RemoteDiscoveryRuntime},
    application::local_todos::{LocalTodoService, LocalTodoServiceError},
    application::remote_mutation_facade::{
        ConfirmedRemoteEventDelete, MutationApplicationError, MutationApplicationView,
        RemoteEventDeleteConfirmation, RemoteMutationApplication,
    },
    application::settings::{SettingsService, SettingsServiceError},
    application::{
        recurrence_horizon::HorizonTrigger,
        recurrence_runtime::{HorizonRuntime, HorizonRuntimeError},
        reminders::ReminderRuntime,
        sync_runtime::{SyncExecutionRuntime, SyncExecutionView, SyncRuntimeError},
    },
    domain::calendar_event::{
        CalendarAgendaItem, CalendarSettingsView, CreateCalendarSubtask, UpdateCalendarSubtask,
    },
    domain::local_todo::{
        CreateLocalTodo, CreateSubtask, DeleteLocalCalendar, DeletedLocalCalendar, LocalCalendar,
        LocalTodo, LocalTodoError, SaveLocalCalendar, SaveLocalTodo, SetLocalCalendarHidden,
        UpdateLocalTodo, UpdateSubtask,
    },
    domain::remote_mutation::ConflictResolutionChoice,
    domain::settings::{AppSettings, UpdateAppSettings},
    ports::clock::Clock,
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, State};

pub const LOCAL_TODO_CHANGED_EVENT: &str = "calendar-todo://local-todo-changed";

#[tauri::command]
pub async fn discover_remote_calendars(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<CalendarSettingsView>, IpcError> {
    let discovery = state.discovery_runtime.clone();
    let requested = account_id.clone();
    tauri::async_runtime::spawn_blocking(move || discovery.discover(&requested))
        .await
        .map_err(|_| IpcError {
            code: "syncUnavailable",
            message: "无法发现远程日历。",
        })?
        .map_err(IpcError::from)?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    let app_state = app.state::<AppState>();
    Ok(app_state
        .calendar_settings_with_connection()?
        .into_iter()
        .filter(|calendar| calendar.account_id == account_id)
        .collect())
}

#[derive(Default)]
pub struct DesktopWindowState {
    expanded: Mutex<Option<tauri::PhysicalSize<u32>>>,
    collapsed: AtomicBool,
    pinned: AtomicBool,
}

impl DesktopWindowState {
    /// True while the desktop list is deliberately collapsed to its minimum
    /// height. That rectangle is a presentation state, so it must never become
    /// the size a later launch restores.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed.load(Ordering::SeqCst)
    }

    pub(crate) fn is_pinned(&self) -> bool {
        self.pinned.load(Ordering::SeqCst)
    }

    pub(crate) fn set_pinned(&self, pinned: bool) {
        self.pinned.store(pinned, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct TrayOpenState {
    generation: u64,
    ready: bool,
    native_fallback_armed: bool,
}

/// Anchor of the last tray right-click. The popover is content-sized, so every
/// height change has to be re-anchored to the icon that opened it.
#[derive(Default)]
pub struct TrayMenuState {
    anchor: Mutex<Option<tauri::PhysicalPosition<f64>>>,
    shown_at: Mutex<Option<Instant>>,
    open: Mutex<TrayOpenState>,
}

impl TrayMenuState {
    pub(crate) fn begin_open(&self) -> u64 {
        let mut open = self.open.lock().expect("tray open state");
        open.generation += 1;
        open.ready = false;
        open.generation
    }

    pub(crate) fn acknowledge_open(&self, generation: u64) -> bool {
        let mut open = self.open.lock().expect("tray open state");
        if open.generation != generation {
            return false;
        }
        open.ready = true;
        true
    }

    pub(crate) fn expire_open_to_fallback(&self, generation: u64) -> bool {
        let mut open = self.open.lock().expect("tray open state");
        if open.generation != generation || open.ready {
            return false;
        }
        open.generation += 1;
        open.native_fallback_armed = true;
        true
    }

    pub(crate) fn cancel_open(&self) {
        let mut open = self.open.lock().expect("tray open state");
        open.generation += 1;
        open.ready = false;
    }

    pub(crate) fn arm_native_fallback(&self) {
        self.open
            .lock()
            .expect("tray open state")
            .native_fallback_armed = true;
    }

    pub(crate) fn take_native_fallback(&self) -> bool {
        let mut open = self.open.lock().expect("tray open state");
        std::mem::take(&mut open.native_fallback_armed)
    }
    pub(crate) fn anchor(&self) -> Option<tauri::PhysicalPosition<f64>> {
        self.anchor.lock().ok().and_then(|anchor| *anchor)
    }

    pub(crate) fn set_anchor(&self, position: tauri::PhysicalPosition<f64>) {
        if let Ok(mut anchor) = self.anchor.lock() {
            *anchor = Some(position);
        }
    }

    pub(crate) fn mark_shown(&self) {
        if let Ok(mut shown_at) = self.shown_at.lock() {
            *shown_at = Some(Instant::now());
        }
    }

    /// True while the popover was opened less than `window` ago. The shell can
    /// take focus back immediately after a tray click, and that alone must not
    /// count as "the user clicked somewhere else".
    pub(crate) fn was_shown_within(&self, window: Duration) -> bool {
        self.shown_at
            .lock()
            .ok()
            .and_then(|shown_at| *shown_at)
            .is_some_and(|shown_at| shown_at.elapsed() < window)
    }
}

#[tauri::command]
pub fn resize_tray_menu(app: AppHandle, height: f64) -> Result<(), IpcError> {
    let Some(window) = app.get_webview_window("tray_menu") else {
        return Ok(());
    };
    let current = window.inner_size().map_err(|_| IpcError {
        code: "window",
        message: "无法调整菜单。",
    })?;
    let scale = match window.scale_factor() {
        Ok(scale) if scale.is_finite() && scale > 0.0 => scale,
        _ => 1.0,
    };
    let width_dip = if current.width == 0 {
        window::TRAY_MENU_WIDTH_DIP
    } else {
        f64::from(current.width) / scale
    };
    let requested = window::tray_menu_height_dip(height);
    let height_dip = app
        .try_state::<TrayMenuState>()
        .and_then(|state| state.anchor())
        .and_then(|anchor| window::tray_menu_screen_height_dip(&window, (anchor.x, anchor.y)))
        .map(|available| window::tray_menu_height_for_screen(requested, available))
        .unwrap_or(requested);
    let target_height = (height_dip * scale).round() as u32;
    if current.height == target_height {
        return Ok(());
    }
    window
        .set_size(LogicalSize::new(width_dip, height_dip))
        .map_err(|_| IpcError {
            code: "window",
            message: "无法调整菜单。",
        })?;
    if window.is_visible().unwrap_or(false) {
        crate::place_tray_menu(&app);
    }
    Ok(())
}

#[tauri::command]
pub fn desktop_pinned(state: State<'_, DesktopWindowState>) -> bool {
    state.is_pinned()
}

/// True while the desktop list is folded, so a freshly mounted page renders the
/// same presentation the window was restored with.
#[tauri::command]
pub fn desktop_collapsed(state: State<'_, DesktopWindowState>) -> bool {
    state.is_collapsed()
}

#[tauri::command]
pub fn set_desktop_pinned(
    app: AppHandle,
    state: State<'_, AppState>,
    window_state: State<'_, DesktopWindowState>,
    pinned: bool,
) -> Result<bool, IpcError> {
    // Stored first: memory follows a durable choice, so a restart reproduces it.
    state
        .settings
        .set_desktop_pinned(pinned)
        .map_err(IpcError::from)?;
    window_state.set_pinned(pinned);
    let _ = app.emit("calendar-todo://settings-changed", ());
    Ok(pinned)
}

#[tauri::command]
pub fn start_desktop_drag(
    app: AppHandle,
    state: State<'_, DesktopWindowState>,
) -> Result<(), IpcError> {
    if state.is_pinned() {
        return Ok(());
    }
    let window = app.get_webview_window("desktop").ok_or(IpcError {
        code: "window",
        message: "桌面清单不可用。",
    })?;
    window.start_dragging().map_err(|_| IpcError {
        code: "window",
        message: "无法移动桌面清单。",
    })
}

#[tauri::command]
pub fn set_desktop_collapsed(
    app: AppHandle,
    state: State<'_, AppState>,
    window_state: State<'_, DesktopWindowState>,
    collapsed: bool,
) -> Result<(), IpcError> {
    let window = desktop_window(&app)?;
    if collapsed {
        fold_desktop(&window, &window_state)?;
    } else {
        unfold_desktop(&window, &window_state)?;
    }
    // Remember the choice only once the window actually changed, so a stored
    // fold can never disagree with the visible card.
    state
        .settings
        .set_desktop_collapsed(collapsed)
        .map_err(IpcError::from)
}

fn desktop_window(app: &AppHandle) -> Result<tauri::WebviewWindow, IpcError> {
    app.get_webview_window("desktop").ok_or(IpcError {
        code: "window",
        message: "桌面清单不可用。",
    })
}

/// Fold the desktop list to its minimum height, remembering the rectangle it
/// must return to. The folded rectangle is presentation state and never
/// reaches `window_placements`.
fn fold_desktop(window: &tauri::WebviewWindow, state: &DesktopWindowState) -> Result<(), IpcError> {
    let mut expanded = state.expanded.lock().map_err(|_| IpcError {
        code: "window",
        message: "无法调整桌面清单。",
    })?;
    if state.is_collapsed() {
        return Ok(());
    }
    let size = window.inner_size().map_err(|_| IpcError {
        code: "window",
        message: "无法调整桌面清单。",
    })?;
    let factor = window.scale_factor().map_err(|_| IpcError {
        code: "window",
        message: "无法调整桌面清单。",
    })?;
    let logical: LogicalSize<f64> = size.to_logical(factor);
    state.collapsed.store(true, Ordering::SeqCst);
    if window
        .set_size(LogicalSize::new(logical.width, 60.0))
        .is_err()
    {
        state.collapsed.store(false, Ordering::SeqCst);
        return Err(IpcError {
            code: "window",
            message: "无法调整桌面清单。",
        });
    }
    *expanded = Some(size);
    Ok(())
}

fn unfold_desktop(
    window: &tauri::WebviewWindow,
    state: &DesktopWindowState,
) -> Result<(), IpcError> {
    let mut expanded = state.expanded.lock().map_err(|_| IpcError {
        code: "window",
        message: "无法调整桌面清单。",
    })?;
    if !state.is_collapsed() {
        return Ok(());
    }
    let size = expanded.unwrap_or_else(|| {
        let factor = window.scale_factor().unwrap_or(1.0);
        LogicalSize::new(400.0, 560.0).to_physical(factor)
    });
    state.collapsed.store(false, Ordering::SeqCst);
    if window.set_size(size).is_err() {
        state.collapsed.store(true, Ordering::SeqCst);
        return Err(IpcError {
            code: "window",
            message: "无法调整桌面清单。",
        });
    }
    *expanded = None;
    Ok(())
}

/// Apply the desktop list's remembered presentation before it is first shown.
///
/// The stored rectangle is restored by the caller first, so folding here never
/// corrupts the size a later launch restores. A failure leaves the list in its
/// expanded rectangle; the caller then clears the stored fold so what the user
/// sees still matches what is stored.
pub(crate) fn restore_desktop_window_state(
    window: &tauri::WebviewWindow,
    state: &DesktopWindowState,
    settings: &crate::domain::settings::AppSettings,
) -> Result<(), IpcError> {
    if settings.desktop_collapsed {
        fold_desktop(window, state)?;
    }
    Ok(())
}

pub struct AppState {
    remote_execution:
        Option<Arc<crate::application::remote_execution_activation::RemoteExecutionActivation>>,
    remote_crud: crate::application::remote_crud::RemoteCrudApplication,
    desktop_order_db: crate::adapters::persistence::Database,
    local_todos: LocalTodoService,
    pub(crate) settings: SettingsService,
    agenda: AgendaService,
    horizon_runtime: Arc<HorizonRuntime>,
    remote_recurrence_runtime:
        Option<Arc<crate::application::remote_recurrence_runtime::RemoteRecurrenceRuntime>>,
    pub(crate) account_profiles: AccountProfileService,
    sync_runtime: Arc<SyncExecutionRuntime>,
    pub(crate) attachment_runtime: AttachmentRuntime,
    pub(crate) discovery_runtime: RemoteDiscoveryRuntime,
    remote_mutations: RemoteMutationApplication,
    reminder_runtime: Arc<ReminderRuntime>,
    clock: Arc<dyn Clock>,
}

/// Everything the typed IPC boundary owns, named at the construction site
/// instead of travelling as a ten-position argument list.
pub struct AppStateParts {
    pub desktop_order_db: crate::adapters::persistence::Database,
    pub local_todos: LocalTodoService,
    pub settings: SettingsService,
    pub agenda: AgendaService,
    pub horizon_runtime: Arc<HorizonRuntime>,
    pub remote_recurrence_runtime:
        Option<Arc<crate::application::remote_recurrence_runtime::RemoteRecurrenceRuntime>>,
    pub account_profiles: AccountProfileService,
    pub sync_runtime: Arc<SyncExecutionRuntime>,
    pub attachment_runtime: AttachmentRuntime,
    pub discovery_runtime: RemoteDiscoveryRuntime,
    pub remote_mutations: RemoteMutationApplication,
    pub reminder_runtime: Arc<ReminderRuntime>,
    pub clock: Arc<dyn Clock>,
}

impl AppState {
    fn calendar_settings_with_connection(&self) -> Result<Vec<CalendarSettingsView>, IpcError> {
        let mut calendars = self
            .agenda
            .list_calendar_settings()
            .map_err(IpcError::from)?;
        for calendar in &mut calendars {
            calendar.remote_session_ready = self
                .remote_execution
                .as_ref()
                .is_some_and(|execution| execution.remote_session_ready(&calendar.account_id));
        }
        Ok(calendars)
    }
    pub(crate) fn install_confirmed_event_emitter(
        &self,
        emitter: Arc<dyn crate::application::remote_mutation_runtime::MutationStateEmitter>,
    ) -> Result<(), ()> {
        self.remote_execution
            .as_ref()
            .ok_or(())?
            .install_scoped_emitter(emitter)
    }
}
struct ConfirmedEventContinuation {
    remote_crud: crate::application::remote_crud::RemoteCrudApplication,
    remote_execution:
        Option<Arc<crate::application::remote_execution_activation::RemoteExecutionActivation>>,
    sync_runtime: Arc<SyncExecutionRuntime>,
}
impl ConfirmedEventContinuation {
    fn capture(state: &AppState) -> Self {
        Self {
            remote_crud: state.remote_crud.clone(),
            remote_execution: state.remote_execution.clone(),
            sync_runtime: state.sync_runtime.clone(),
        }
    }
    fn run(
        &self,
        input: &str,
    ) -> Result<crate::application::remote_crud::ConfirmedEventView, IpcError> {
        confirmed_flow(
            || {
                self.remote_crud
                    .confirmed_target(input)
                    .map_err(|e| match e {
                        crate::application::remote_crud::RemoteCrudError::InvalidInput => {
                            IpcError {
                                code: "validation",
                                message: "确认的事件请求无效，草稿仍保留。",
                            }
                        }
                        crate::application::remote_crud::RemoteCrudError::Persistence(e) => {
                            MutationApplicationError::Persistence(e).into()
                        }
                    })
            },
            |target| {
                let execution = self
                    .remote_execution
                    .as_ref()
                    .ok_or_else(remote_account_connection_required)?;
                let (account, route) = target.account_and_route();
                execution
                    .ensure_confirmed_connection(account, route)
                    .map_err(|e| match e {
                        AccountProfileServiceError::Unavailable => IpcError {
                            code: "remoteConnectionUnavailable",
                            message: "连接未完成，未保存远程操作；草稿仍保留。",
                        },
                        other => other.into(),
                    })
            },
            |target, connection| {
                if target.is_successor() {
                    return Ok(None);
                }
                self.sync_runtime
                    .confirmed_read_sync(target.account_and_route().0, connection.generation())
                    .map(Some)
                    .map_err(|_| IpcError {
                        code: "remoteSyncUnavailable",
                        message: "同步未完成，未保存远程操作；草稿仍保留。",
                    })
            },
            |ready| {
                let preflight = ready.map(|(target, connection, read)| {
                    let current: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                        connection.current() && read.as_ref().is_none_or(|read| read.current())
                    });
                    (target, current)
                });
                self.remote_crud
            .confirmed_json_guarded(input, self.remote_execution.as_deref(), preflight)
            .map_err(|error| match error {
                crate::application::remote_crud::RemoteCrudError::InvalidInput => IpcError {
                    code: "validation",
                    message: "确认的事件请求或当前操作范围无效。",
                },
                crate::application::remote_crud::RemoteCrudError::Persistence(error) => {
                    if matches!(error, crate::adapters::persistence::PersistenceError::StaleSyncCapture) {
                        return IpcError { code: "remoteTargetChanged", message: "同步后事件或日历已改变；未保存操作，草稿仍保留，请刷新后重新确认。" };
                    }
                    MutationApplicationError::Persistence(error).into()
                }
            })
            },
        )
    }
}
fn confirmed_flow<T, C, R, V>(
    select: impl FnOnce() -> Result<Option<T>, IpcError>,
    connect: impl FnOnce(&T) -> Result<C, IpcError>,
    sync: impl FnOnce(&T, &C) -> Result<R, IpcError>,
    admit: impl FnOnce(Option<(T, C, R)>) -> Result<V, IpcError>,
) -> Result<V, IpcError> {
    let ready = match select()? {
        Some(target) => {
            let connection = connect(&target)?;
            let read = sync(&target, &connection)?;
            Some((target, connection, read))
        }
        None => None,
    };
    admit(ready)
}
impl AppState {
    pub fn new(parts: AppStateParts) -> Self {
        Self {
            remote_execution: None,
            remote_crud: crate::application::remote_crud::RemoteCrudApplication::without_execution(
                parts.desktop_order_db.clone(),
            ),
            desktop_order_db: parts.desktop_order_db,
            local_todos: parts.local_todos,
            settings: parts.settings,
            agenda: parts.agenda,
            horizon_runtime: parts.horizon_runtime,
            remote_recurrence_runtime: parts.remote_recurrence_runtime,
            account_profiles: parts.account_profiles,
            sync_runtime: parts.sync_runtime,
            attachment_runtime: parts.attachment_runtime,
            discovery_runtime: parts.discovery_runtime,
            remote_mutations: parts.remote_mutations,
            reminder_runtime: parts.reminder_runtime,
            clock: parts.clock,
        }
    }

    pub(crate) fn with_remote_execution(
        parts: AppStateParts,
        execution: Arc<crate::application::remote_execution_activation::RemoteExecutionActivation>,
    ) -> Result<Self, ()> {
        if !execution.belongs_to_application(
            &parts.desktop_order_db,
            &parts.attachment_runtime,
            &parts.account_profiles,
            &parts.remote_mutations,
            &parts.reminder_runtime,
            &parts.clock,
        ) || !execution.matches_confirmed_sync(&parts.sync_runtime)
        {
            return Err(());
        }
        let mut state = Self::new(parts);
        state.remote_execution = Some(execution);
        Ok(state)
    }
}

// AppState owns the installed reminder lifecycle even when emitters still hold
// Arc handles. Final application-owner loss must stop the single timer.
impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(runtime) = &self.remote_recurrence_runtime {
            runtime.shutdown();
        }
        self.shutdown_attachments();
        self.reminder_runtime.shutdown();
    }
}

#[tauri::command]
pub fn purge_expired_remote_cache(state: State<'_, AppState>) -> Result<u32, IpcError> {
    let now_ms = state.clock.now_utc_ms().map_err(|_| IpcError {
        code: "persistence",
        message: "无法确定当前时间，未清理远程缓存。",
    })?;
    state
        .desktop_order_db
        .purge_deleted_raw_cache(now_ms)
        .map_err(|_| IpcError {
            code: "persistence",
            message: "无法清理已过期的远程缓存。",
        })
}

#[tauri::command]
pub fn get_desktop_item_order(state: State<'_, AppState>) -> Result<Vec<String>, IpcError> {
    state
        .desktop_order_db
        .desktop_item_order()
        .map_err(|_| IpcError {
            code: "persistence",
            message: "无法读取清单顺序。",
        })
}

#[tauri::command]
pub fn set_desktop_item_order(
    app: AppHandle,
    state: State<'_, AppState>,
    keys: Vec<String>,
) -> Result<(), IpcError> {
    state
        .desktop_order_db
        .set_desktop_item_order(keys)
        .map_err(|_| IpcError {
            code: "persistence",
            message: "无法保存清单顺序。",
        })?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

#[tauri::command]
pub fn reset_desktop_item_order(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), IpcError> {
    state
        .desktop_order_db
        .reset_desktop_item_order()
        .map_err(|_| IpcError {
            code: "persistence",
            message: "无法重置清单顺序。",
        })?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

impl AppState {
    pub fn trigger_horizon(&self, trigger: HorizonTrigger) -> Result<(), HorizonRuntimeError> {
        self.horizon_runtime.trigger(trigger)
    }

    pub fn shutdown_horizon(&self) {
        self.horizon_runtime.shutdown();
    }

    pub(crate) fn trigger_remote_recurrence_startup(&self) {
        if let Some(runtime) = &self.remote_recurrence_runtime {
            let _ = runtime.startup_or_resume();
        }
    }

    pub(crate) fn trigger_remote_recurrence_change(&self) {
        if let Some(runtime) = &self.remote_recurrence_runtime {
            let _ = runtime.recurrence_changed();
        }
    }

    pub(crate) fn trigger_remote_recurrence_terminal(&self) {
        if let Some(runtime) = &self.remote_recurrence_runtime {
            let _ = runtime.remote_terminal();
        }
    }

    pub fn shutdown_sync(&self) {
        self.sync_runtime.shutdown();
    }

    pub fn shutdown_attachments(&self) {
        if let Some(execution) = &self.remote_execution {
            let _ = execution.shutdown();
        } else {
            self.attachment_runtime.shutdown();
        }
    }

    pub fn shutdown_discovery(&self) {
        self.discovery_runtime.shutdown();
    }

    pub fn resume_sync(&self) -> Result<(), SyncRuntimeError> {
        self.sync_runtime.resume()
    }

    pub fn refresh_reminders(&self) -> Result<(), crate::application::reminders::ReminderError> {
        self.reminder_runtime.refresh()
    }

    pub fn shutdown_reminders(&self) {
        self.reminder_runtime.shutdown();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapState {
    schema_version: u32,
    backend_ready: bool,
    current_phase: &'static str,
    default_window_mode: &'static str,
    persistence_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcError {
    code: &'static str,
    message: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalTodoCompletionChanged {
    todo_id: String,
    previous_occurrence_key: String,
    completed: bool,
    item: LocalTodo,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CalendarCompletionChanged {
    series_id: String,
    previous_recurrence_key: String,
    completed: bool,
    item: CalendarAgendaItem,
}

impl From<LocalTodoServiceError> for IpcError {
    fn from(error: LocalTodoServiceError) -> Self {
        match error {
            LocalTodoServiceError::Persistence(
                crate::adapters::persistence::PersistenceError::SubtaskMoveUnavailable,
            ) => Self {
                code: "subtaskMoveUnavailable",
                message: "此子任务当前不能排序，请重新读取任务详情。",
            },
            LocalTodoServiceError::Persistence(
                crate::adapters::persistence::PersistenceError::StaleSubtaskOrder,
            ) => Self {
                code: "subtaskOrderChanged",
                message: "子任务顺序已改变，请刷新详情后再选择移动。",
            },
            LocalTodoServiceError::Validation(LocalTodoError::LastVisibleCalendar) => Self {
                code: "lastVisibleCalendar",
                message: "请至少保留一个显示的本地日历。",
            },
            LocalTodoServiceError::Validation(LocalTodoError::LastCalendar) => Self {
                code: "lastCalendar",
                message: "至少保留一个本地日历。",
            },
            LocalTodoServiceError::Validation(_) => Self {
                code: "validation",
                message: "输入内容无效，请检查标题、时间、标签或子任务。",
            },
            LocalTodoServiceError::Persistence(_) => Self {
                code: "persistence",
                message: "无法保存本地 TODO，请重试。",
            },
        }
    }
}

impl From<SettingsServiceError> for IpcError {
    fn from(error: SettingsServiceError) -> Self {
        match error {
            SettingsServiceError::Validation(_) => Self {
                code: "validation",
                message: "设置无效，请检查同步间隔、不透明度或颜色。",
            },
            SettingsServiceError::Persistence(_) => Self {
                code: "persistence",
                message: "无法保存设置，请重试。",
            },
        }
    }
}

impl From<AgendaServiceError> for IpcError {
    fn from(error: AgendaServiceError) -> Self {
        match error {
            AgendaServiceError::Validation(_) => Self {
                code: "validation",
                message: "日历本地状态输入无效，请检查显示范围、标签或周期设置。",
            },
            AgendaServiceError::Persistence(_) => Self {
                code: "persistence",
                message: "无法更新日历本地状态，请重试。",
            },
        }
    }
}

impl From<AccountProfileServiceError> for IpcError {
    fn from(error: AccountProfileServiceError) -> Self {
        match error {
            AccountProfileServiceError::Validation => Self {
                code: "validation",
                message: "账户配置无效，请检查名称和区域配置。",
            },
            AccountProfileServiceError::Disabled => Self {
                code: "accountDisabled",
                message: "账户已停用，请先启用后再连接来源。",
            },
            AccountProfileServiceError::CredentialSetupRequired => Self {
                code: "credentialSetupRequired",
                message: "请先完成凭据配置。",
            },
            AccountProfileServiceError::CredentialActionRequired => Self {
                code: "credentialActionRequired",
                message: "凭据不可用或未获授权，请重新配置凭据。",
            },
            AccountProfileServiceError::Unavailable => Self {
                code: "persistence",
                message: "无法更新账户配置，请重试。",
            },
        }
    }
}

impl From<DiscoveryError> for IpcError {
    fn from(error: DiscoveryError) -> Self {
        match error {
            DiscoveryError::Superseded => Self {
                code: "discoverySuperseded",
                message: "发现请求已被取消或取代，请重试。",
            },
            DiscoveryError::InvalidIdentity => Self {
                code: "validation",
                message: "账户标识无效。",
            },
            DiscoveryError::Disabled => Self {
                code: "accountDisabled",
                message: "账户已停用，请先启用后再发现日历。",
            },
            DiscoveryError::CredentialActionRequired => Self {
                code: "credentialActionRequired",
                message: "凭据不可用或未获授权，请重新配置凭据。",
            },
            DiscoveryError::UnsupportedProvider => Self {
                code: "unsupportedProvider",
                message: "该账户区域暂不支持日历发现。",
            },
            DiscoveryError::UnsafeConfiguration => Self {
                code: "validation",
                message: "账户配置不安全，已阻止日历发现。",
            },
            DiscoveryError::Unavailable => Self {
                code: "syncUnavailable",
                message: "无法发现远程日历。",
            },
        }
    }
}

impl From<SyncRuntimeError> for IpcError {
    fn from(error: SyncRuntimeError) -> Self {
        match error {
            SyncRuntimeError::InvalidIdentity => Self {
                code: "validation",
                message: "账户标识无效。",
            },
            SyncRuntimeError::Disabled => Self {
                code: "accountDisabled",
                message: "账户已停用，无法同步。",
            },
            SyncRuntimeError::Detached => Self {
                code: "sourceDetached",
                message: "日历来源尚未连接。",
            },
            SyncRuntimeError::InvalidSchedule => Self {
                code: "validation",
                message: "同步计划无效，请检查同步间隔。",
            },
            SyncRuntimeError::Unavailable => Self {
                code: "syncUnavailable",
                message: "同步执行器暂时不可用，请重试。",
            },
        }
    }
}

impl From<MutationApplicationError> for IpcError {
    fn from(error: MutationApplicationError) -> Self {
        match error {
            MutationApplicationError::Persistence(
                crate::adapters::persistence::PersistenceError::RemoteAccountConnectionRequired,
            ) => remote_account_connection_required(),
            MutationApplicationError::Persistence(
                crate::adapters::persistence::PersistenceError::RemoteMutationReadOnly,
            ) => Self {
                code: "remoteEventReadOnly",
                message: "此事件的当前服务器内容不支持安全编辑。",
            },
            MutationApplicationError::Persistence(
                crate::adapters::persistence::PersistenceError::PendingMutationExists,
            ) => Self {
                code: "remoteMutationPending",
                message: "此事件已有尚未完成的远程操作。",
            },
            MutationApplicationError::InvalidIdentity => Self {
                code: "validation",
                message: "远程编辑操作标识无效。",
            },
            MutationApplicationError::Account(_) => Self {
                code: "accountUnavailable",
                message: "账户当前不可用于远程编辑。",
            },
            MutationApplicationError::Persistence(_)
            | MutationApplicationError::Runtime(_)
            | MutationApplicationError::Clock(_)
            | MutationApplicationError::Unavailable => Self {
                code: "mutationUnavailable",
                message: "无法读取或更新远程编辑状态，请重试。",
            },
        }
    }
}

#[tauri::command]
pub fn get_bootstrap_state() -> BootstrapState {
    BootstrapState {
        schema_version: 11,
        backend_ready: true,
        current_phase: "productionReadOnlyAttachment",
        default_window_mode: "desktopCompanion",
        persistence_ready: true,
    }
}

#[tauri::command]
pub fn create_local_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    input: CreateLocalTodo,
) -> Result<LocalTodo, IpcError> {
    let result = state.local_todos.create(input).map_err(IpcError::from)?;
    let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    Ok(result)
}

#[tauri::command]
pub fn list_local_todos(state: State<'_, AppState>) -> Result<Vec<LocalTodo>, IpcError> {
    state.local_todos.list().map_err(Into::into)
}

#[tauri::command]
pub fn list_local_calendars(state: State<'_, AppState>) -> Result<Vec<LocalCalendar>, IpcError> {
    state.local_todos.list_calendars().map_err(Into::into)
}

#[tauri::command]
pub fn save_local_calendar(
    app: AppHandle,
    state: State<'_, AppState>,
    input: SaveLocalCalendar,
) -> Result<LocalCalendar, IpcError> {
    let result = state.local_todos.save_calendar(input).map_err(Into::into);
    if result.is_ok() {
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn set_local_calendar_hidden(
    app: AppHandle,
    state: State<'_, AppState>,
    input: SetLocalCalendarHidden,
) -> Result<LocalCalendar, IpcError> {
    let result = state
        .local_todos
        .set_calendar_hidden(input)
        .map_err(IpcError::from);
    if result.is_ok() {
        // Hiding cancels that calendar's pending reminders; revealing replans
        // them. The tasks themselves are never touched.
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn delete_local_calendar(
    app: AppHandle,
    state: State<'_, AppState>,
    input: DeleteLocalCalendar,
) -> Result<DeletedLocalCalendar, IpcError> {
    let result = state
        .local_todos
        .delete_calendar(input)
        .map_err(IpcError::from);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn move_local_todo_to_calendar(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
    calendar_id: String,
) -> Result<LocalTodo, IpcError> {
    let result = state
        .local_todos
        .move_to_calendar(todo_id, calendar_id)
        .map_err(Into::into);
    if result.is_ok() {
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn list_calendar_agenda_items(
    state: State<'_, AppState>,
) -> Result<Vec<CalendarAgendaItem>, IpcError> {
    let now_utc_ms = state.clock.now_utc_ms().map_err(|_| IpcError {
        code: "clock",
        message: "无法读取当前时间，暂时不能筛选日历显示范围。",
    })?;
    state
        .agenda
        .list_calendar_items(now_utc_ms)
        .map_err(Into::into)
}

#[tauri::command]
pub fn list_calendar_settings(
    state: State<'_, AppState>,
) -> Result<Vec<CalendarSettingsView>, IpcError> {
    state.calendar_settings_with_connection()
}

#[tauri::command]
pub fn set_calendar_color_override(
    app: AppHandle,
    state: State<'_, AppState>,
    calendar_id: String,
    color: Option<String>,
) -> Result<(), IpcError> {
    state
        .agenda
        .set_calendar_color_override(calendar_id, color)
        .map_err(Into::<IpcError>::into)?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

#[tauri::command]
pub fn set_calendar_visible(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    calendar_id: String,
    visible: bool,
) -> Result<(), IpcError> {
    state
        .agenda
        .set_calendar_visible(account_id, calendar_id, visible)
        .map_err(Into::<IpcError>::into)?;
    let _ = state.refresh_reminders();
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

#[tauri::command]
pub fn set_calendar_display_range(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    calendar_id: String,
    display_past_days: Option<i64>,
    display_future_days: Option<i64>,
) -> Result<(), IpcError> {
    state
        .agenda
        .set_calendar_display_range(
            account_id,
            calendar_id,
            display_past_days,
            display_future_days,
        )
        .map_err(Into::<IpcError>::into)?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

#[tauri::command]
pub fn list_account_profiles(
    state: State<'_, AppState>,
) -> Result<Vec<RuntimeAccountView>, IpcError> {
    state.account_profiles.list().map_err(Into::into)
}

#[tauri::command]
pub fn register_account_profile(
    state: State<'_, AppState>,
    input: RegisterRuntimeAccount,
) -> Result<RuntimeAccountView, IpcError> {
    state.account_profiles.register(input).map_err(Into::into)
}

#[tauri::command]
pub fn set_account_profile_enabled(
    state: State<'_, AppState>,
    account_id: String,
    enabled: bool,
) -> Result<RuntimeAccountView, IpcError> {
    if !enabled {
        state.discovery_runtime.cancel(&account_id);
    }
    let account = state
        .attachment_runtime
        .set_enabled(&account_id, enabled)
        .map_err(IpcError::from)?;
    if !enabled {
        state.sync_runtime.account_detached(&account_id);
    }
    let _ = state.refresh_reminders();
    Ok(account)
}

#[tauri::command]
pub fn set_account_remote_editing_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    enabled: bool,
) -> Result<RuntimeAccountView, IpcError> {
    if !enabled && let Some(execution) = &state.remote_execution {
        execution.change(crate::application::remote_execution_activation::ActivationInput {
            account_id: account_id.clone(),
            choice: crate::application::remote_execution_activation::ActivationChoice::DisableExecution,
        }).map_err(|_| execution_unavailable())?;
    }
    let account = state
        .account_profiles
        .set_remote_editing_enabled(&account_id, enabled)
        .map_err(IpcError::from)?;
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(account)
}

/// Strict raw semantic envelope; only actor admission/status/conflict actions.
#[tauri::command]
pub fn prepare_remote_crud(
    state: State<'_, AppState>,
    input: String,
) -> Result<crate::application::remote_crud::RemoteCrudResponse, IpcError> {
    state
        .remote_crud
        .prepare_connected_json(&input, state.remote_execution.as_deref())
        .map_err(|e| match e {
            crate::application::remote_crud::RemoteCrudError::InvalidInput => IpcError {
                code: "validation",
                message: "远程日历请求无效。",
            },
            crate::application::remote_crud::RemoteCrudError::Persistence(e) => {
                MutationApplicationError::Persistence(e).into()
            }
        })
}

fn execution_unavailable() -> IpcError {
    IpcError {
        code: "remoteExecutionUnavailable",
        message: "账号范围远程执行不可用。请通过事件的确认并同步修改动作执行当前操作。",
    }
}
fn remote_account_connection_required() -> IpcError {
    IpcError {
        code: "remoteAccountConnectionRequired",
        message: "请先连接 iCloud 账户，再修改远程日历。",
    }
}

pub(crate) const CONFIRMED_EVENT_STATE_EVENT: &str = "calendar-todo://confirmed-event-state";
pub(crate) struct ConfirmedEventEmitter(pub AppHandle);
impl crate::application::remote_mutation_runtime::MutationStateEmitter for ConfirmedEventEmitter {
    fn emit(&self, _: crate::application::remote_mutation_runtime::MutationRuntimeView) {}
    fn emit_scoped(
        &self,
        view: crate::application::remote_mutation_runtime::ScopedEventRuntimeView,
    ) {
        if let Some(state) = self.0.try_state::<AppState>() {
            state.trigger_remote_recurrence_terminal();
        }
        let _ = self.0.emit(
            CONFIRMED_EVENT_STATE_EVENT,
            crate::application::remote_crud::ConfirmedEventView::from_runtime(view),
        );
        let _ = self.0.emit("calendar-todo://agenda-data-changed", ());
    }
}
#[tauri::command]
pub(crate) async fn confirmed_remote_event(
    app: AppHandle,
    state: State<'_, AppState>,
    input: String,
) -> Result<crate::application::remote_crud::ConfirmedEventView, IpcError> {
    let continuation = ConfirmedEventContinuation::capture(&state);
    let result = tauri::async_runtime::spawn_blocking(move || continuation.run(&input))
        .await
        .map_err(|_| IpcError {
            code: "remoteConnectionUnavailable",
            message: "连接流程未完成，草稿仍保留。",
        })?;
    if result.is_ok() {
        let _ = app.emit("calendar-todo://agenda-data-changed", ());
    }
    result
}

#[tauri::command]
pub(crate) fn set_remote_execution_activation(
    state: State<'_, AppState>,
    input: String,
) -> Result<crate::application::remote_execution_activation::ActivationView, IpcError> {
    let input: crate::application::remote_execution_activation::ActivationInput =
        crate::application::remote_execution_activation::parse_input(&input)
            .map_err(|_| execution_unavailable())?;
    if matches!(
        input.choice,
        crate::application::remote_execution_activation::ActivationChoice::EnableReviewedExecution
    ) {
        return Err(execution_unavailable());
    }
    state
        .remote_execution
        .as_ref()
        .ok_or_else(execution_unavailable)?
        .change(input)
        .map_err(|_| execution_unavailable())
}

#[tauri::command]
pub(crate) fn get_remote_execution_activation(
    state: State<'_, AppState>,
    input: String,
) -> Result<crate::application::remote_execution_activation::ActivationView, IpcError> {
    let input: crate::application::remote_execution_activation::ActivationAccountInput =
        crate::application::remote_execution_activation::parse_input(&input)
            .map_err(|_| execution_unavailable())?;
    state
        .remote_execution
        .as_ref()
        .ok_or_else(execution_unavailable)?
        .view(&input.account_id)
        .map_err(|_| execution_unavailable())
}

#[tauri::command]
pub(crate) fn submit_prepared_remote_crud(
    _state: State<'_, AppState>,
    input: String,
) -> Result<crate::application::remote_mutation_runtime::MutationRuntimeView, IpcError> {
    let _: crate::application::remote_execution_activation::ActivationAccountInput =
        crate::application::remote_execution_activation::parse_input(&input)
            .map_err(|_| execution_unavailable())?;
    Err(execution_unavailable())
}

#[tauri::command]
pub(crate) fn shutdown_remote_execution(
    state: State<'_, AppState>,
    input: String,
) -> Result<(), IpcError> {
    let _: crate::application::remote_execution_activation::ActivationShutdownInput =
        crate::application::remote_execution_activation::parse_input(&input)
            .map_err(|_| execution_unavailable())?;
    state
        .remote_execution
        .as_ref()
        .ok_or_else(execution_unavailable)?
        .shutdown()
        .map_err(|_| IpcError {
            code: "remoteExecutionShutdownRetryRequired",
            message: "远程执行正在关闭。当前所有权已保留，请重试关闭；不要启用新的执行。",
        })
}

#[tauri::command]
pub fn get_remote_mutation_view(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<MutationApplicationView, IpcError> {
    state.remote_mutations.view(&account_id).map_err(Into::into)
}

#[tauri::command]
pub async fn queue_confirmed_remote_event_delete(
    state: State<'_, AppState>,
    input: ConfirmedRemoteEventDelete,
) -> Result<MutationApplicationView, IpcError> {
    let RemoteEventDeleteConfirmation::Confirmed = input.confirmation;
    let raw = serde_json::json!({"action":"confirm","kind":"delete","input":{"idempotencyKey":input.idempotency_key,"resourceId":input.resource_id,"confirmation":"confirmed"}}).to_string();
    let continuation = ConfirmedEventContinuation::capture(&state);
    let mutations = state.remote_mutations.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let view = continuation.run(&raw)?;
        mutations.view(&view.receipt.account_id).map_err(Into::into)
    })
    .await
    .map_err(|_| IpcError {
        code: "remoteConnectionUnavailable",
        message: "连接流程未完成，删除操作尚未确认。",
    })?
}

#[tauri::command]
pub fn resolve_remote_mutation_conflict(
    state: State<'_, AppState>,
    account_id: String,
    operation_id: String,
    choice: ConflictResolutionChoice,
) -> Result<MutationApplicationView, IpcError> {
    if choice == ConflictResolutionChoice::RetryLocal {
        return Err(execution_unavailable());
    }
    let result = state
        .remote_mutations
        .resolve_account_conflict(&account_id, operation_id, choice)
        .map_err(IpcError::from)?;
    debug_assert_eq!(result.account_id, account_id);
    state.remote_mutations.view(&account_id).map_err(Into::into)
}

#[tauri::command]
pub fn remove_account_profile(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<(), IpcError> {
    state.discovery_runtime.cancel(&account_id);
    state
        .attachment_runtime
        .remove_account(&account_id)
        .map_err(IpcError::from)?;
    let _ = state.refresh_reminders();
    let _ = app.emit("calendar-todo://agenda-data-changed", ());
    Ok(())
}

#[tauri::command]
pub fn enroll_account_credential(
    state: State<'_, AppState>,
    input: EnrollAccountCredential,
) -> Result<RuntimeAccountView, IpcError> {
    state
        .account_profiles
        .enroll_credential(input)
        .map_err(Into::into)
}

#[tauri::command]
pub fn attach_account_source(
    state: State<'_, AppState>,
    account_id: String,
    mode: SourceAttachmentMode,
) -> Result<AttachmentView, IpcError> {
    let startup_sync = state
        .settings
        .get()
        .map_err(IpcError::from)?
        .sync_on_startup;
    state
        .attachment_runtime
        .submit(&account_id, mode, startup_sync)
        .map_err(Into::into)
}

#[tauri::command]
pub fn list_attachment_states(state: State<'_, AppState>) -> Result<Vec<AttachmentView>, IpcError> {
    state.attachment_runtime.list().map_err(Into::into)
}

#[tauri::command]
pub fn trigger_account_sync(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<SyncExecutionView, IpcError> {
    state
        .sync_runtime
        .trigger_manual(&account_id)
        .map_err(Into::into)
}

#[tauri::command]
pub fn list_account_sync_states(
    state: State<'_, AppState>,
) -> Result<Vec<SyncExecutionView>, IpcError> {
    state.sync_runtime.list().map_err(Into::into)
}

#[tauri::command]
pub fn refresh_calendar_horizon(state: State<'_, AppState>) -> Result<(), IpcError> {
    state
        .trigger_horizon(HorizonTrigger::Manual)
        .map_err(|_| IpcError {
            code: "persistence",
            message: "无法刷新日历时间范围，请重试。",
        })
}

#[tauri::command]
pub fn set_calendar_completed(
    app: AppHandle,
    state: State<'_, AppState>,
    series_id: String,
    recurrence_key: String,
    completed: bool,
) -> Result<CalendarAgendaItem, IpcError> {
    let previous_series_id = series_id.clone();
    let previous_recurrence_key = recurrence_key.clone();
    let result = state
        .agenda
        .set_completed(series_id, recurrence_key, completed)
        .map_err(Into::into);
    if let Ok(item) = &result {
        let _ = state.refresh_reminders();
        if completed {
            state.trigger_remote_recurrence_change();
        }
        let _ = app.emit(
            "calendar-todo://agenda-data-changed",
            CalendarCompletionChanged {
                series_id: previous_series_id,
                previous_recurrence_key,
                completed,
                item: item.clone(),
            },
        );
    }
    result
}

#[tauri::command]
pub fn set_calendar_important(
    app: AppHandle,
    state: State<'_, AppState>,
    series_id: String,
    recurrence_key: String,
    important: bool,
) -> Result<CalendarAgendaItem, IpcError> {
    let result = state
        .agenda
        .set_important(series_id, recurrence_key, important)
        .map_err(Into::into);
    if result.is_ok() {
        let _ = app.emit("calendar-todo://agenda-data-changed", ());
    }
    result
}

#[tauri::command]
pub fn update_calendar_local_metadata(
    app: AppHandle,
    state: State<'_, AppState>,
    input: crate::domain::calendar_event::UpdateCalendarLocalMetadata,
) -> Result<(), IpcError> {
    let result = state
        .agenda
        .update_local_metadata(input)
        .map_err(Into::into);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        state.trigger_remote_recurrence_change();
        let _ = app.emit("calendar-todo://agenda-data-changed", ());
    }
    result
}

#[tauri::command]
pub fn create_calendar_subtask(
    state: State<'_, AppState>,
    input: CreateCalendarSubtask,
) -> Result<CalendarAgendaItem, IpcError> {
    state.agenda.create_subtask(input).map_err(Into::into)
}

#[tauri::command]
pub fn update_calendar_subtask(
    state: State<'_, AppState>,
    input: UpdateCalendarSubtask,
) -> Result<CalendarAgendaItem, IpcError> {
    state.agenda.update_subtask(input).map_err(Into::into)
}

#[tauri::command]
pub fn set_calendar_subtask_completed(
    state: State<'_, AppState>,
    series_id: String,
    recurrence_key: String,
    subtask_id: String,
    completed: bool,
) -> Result<CalendarAgendaItem, IpcError> {
    state
        .agenda
        .set_subtask_completed(series_id, recurrence_key, subtask_id, completed)
        .map_err(Into::into)
}

#[tauri::command]
pub fn delete_calendar_subtask(
    state: State<'_, AppState>,
    series_id: String,
    recurrence_key: String,
    subtask_id: String,
) -> Result<CalendarAgendaItem, IpcError> {
    state
        .agenda
        .delete_subtask(series_id, recurrence_key, subtask_id)
        .map_err(Into::into)
}

#[tauri::command]
pub fn update_local_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    input: UpdateLocalTodo,
) -> Result<LocalTodo, IpcError> {
    let result = state.local_todos.update(input).map_err(Into::into);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn save_local_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    input: SaveLocalTodo,
) -> Result<LocalTodo, IpcError> {
    let result = state.local_todos.save(input).map_err(Into::into);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn set_local_todo_completed(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
    occurrence_key: String,
    completed: bool,
) -> Result<LocalTodo, IpcError> {
    let previous_todo_id = todo_id.clone();
    let previous_occurrence_key = occurrence_key.clone();
    let result = state
        .local_todos
        .set_completed(todo_id, occurrence_key, completed)
        .map_err(Into::into);
    if let Ok(item) = &result {
        let _ = state.refresh_reminders();
        let _ = app.emit(
            LOCAL_TODO_CHANGED_EVENT,
            LocalTodoCompletionChanged {
                todo_id: previous_todo_id,
                previous_occurrence_key,
                completed,
                item: item.clone(),
            },
        );
    }
    result
}

#[tauri::command]
pub fn delete_local_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
) -> Result<(), IpcError> {
    let result = state.local_todos.delete(todo_id).map_err(Into::into);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn delete_local_todo_occurrence(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
    occurrence_key: String,
) -> Result<(), IpcError> {
    let result = state
        .local_todos
        .delete_occurrence(todo_id, occurrence_key)
        .map_err(Into::into);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn restore_recently_deleted_todo_occurrence(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
    occurrence_key: String,
) -> Result<LocalTodo, IpcError> {
    let result = state
        .local_todos
        .restore_recently_deleted_occurrence(todo_id, occurrence_key)
        .map_err(IpcError::from);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[tauri::command]
pub fn restore_recently_deleted_todo(
    app: AppHandle,
    state: State<'_, AppState>,
    todo_id: String,
) -> Result<LocalTodo, IpcError> {
    let result = state
        .local_todos
        .restore_recently_deleted(todo_id)
        .map_err(IpcError::from);
    if result.is_ok() {
        let _ = state.refresh_reminders();
        let _ = app.emit(LOCAL_TODO_CHANGED_EVENT, ());
    }
    result
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TraySourceChoice {
    Local,
    IcloudChinaReadOnly,
}

#[tauri::command]
pub fn set_tray_source(
    app: AppHandle,
    state: State<'_, AppState>,
    choice: TraySourceChoice,
) -> Result<(), IpcError> {
    match choice {
        TraySourceChoice::Local => {
            state
                .attachment_runtime
                .detach_all()
                .map_err(IpcError::from)?;
            let _ = app.emit("calendar-todo://source-mode-changed", ());
            Ok(())
        }
        TraySourceChoice::IcloudChinaReadOnly => {
            let accounts = state.account_profiles.list().map_err(IpcError::from)?;
            let eligible: Vec<_> = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && account.credential_configured
                        && account.region_profile == "icloud-cn"
                })
                .collect();
            if eligible.len() != 1 {
                let _ = app.emit("calendar-todo://source-selection-required", ());
                return Err(IpcError {
                    code: "sourceSelectionRequired",
                    message: "请先在主窗口选择或配置一个 iCloud 账户。",
                });
            }
            state
                .attachment_runtime
                .submit(
                    &eligible[0].account_id,
                    SourceAttachmentMode::IcloudChinaReadOnly,
                    state
                        .settings
                        .get()
                        .map_err(IpcError::from)?
                        .sync_on_startup,
                )
                .map_err(IpcError::from)?;
            Ok(())
        }
    }
}

#[tauri::command]
pub fn show_main_from_tray(app: AppHandle) -> Result<(), IpcError> {
    if let Some(menu) = app.get_webview_window("tray_menu") {
        let _ = menu.hide();
    }
    let window = app.get_webview_window("main").ok_or(IpcError {
        code: "window",
        message: "主窗口不可用。",
    })?;
    window::restore_from_tray(&window).map_err(|_| IpcError {
        code: "window",
        message: "无法显示主窗口。",
    })
}

#[tauri::command]
pub fn show_calendar_settings_from_tray(app: AppHandle) -> Result<(), IpcError> {
    let _ = app.emit("calendar-todo://source-selection-required", ());
    show_main_from_tray(app)
}

#[tauri::command]
pub fn hide_tray_menu(app: AppHandle, state: State<'_, TrayMenuState>) {
    state.cancel_open();
    if let Some(menu) = app.get_webview_window("tray_menu") {
        let _ = menu.hide();
    }
}

#[tauri::command]
pub fn tray_menu_ready(state: State<'_, TrayMenuState>, generation: u64) {
    state.acknowledge_open(generation);
}

#[tauri::command]
pub fn quit_from_tray(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn create_subtask(
    state: State<'_, AppState>,
    input: CreateSubtask,
) -> Result<LocalTodo, IpcError> {
    state.local_todos.create_subtask(input).map_err(Into::into)
}

#[tauri::command]
pub fn update_subtask(
    state: State<'_, AppState>,
    input: UpdateSubtask,
) -> Result<LocalTodo, IpcError> {
    state.local_todos.update_subtask(input).map_err(Into::into)
}

#[tauri::command]
pub fn move_local_subtask(
    state: State<'_, AppState>,
    input: crate::domain::local_todo::MoveLocalSubtask,
) -> Result<LocalTodo, IpcError> {
    state.local_todos.move_subtask(input).map_err(Into::into)
}

#[tauri::command]
pub fn set_subtask_completed(
    state: State<'_, AppState>,
    todo_id: String,
    subtask_id: String,
    completed: bool,
) -> Result<LocalTodo, IpcError> {
    state
        .local_todos
        .set_subtask_completed(todo_id, subtask_id, completed)
        .map_err(Into::into)
}

#[tauri::command]
pub fn delete_subtask(
    state: State<'_, AppState>,
    todo_id: String,
    subtask_id: String,
) -> Result<LocalTodo, IpcError> {
    state
        .local_todos
        .delete_subtask(todo_id, subtask_id)
        .map_err(Into::into)
}

#[tauri::command]
pub fn get_app_settings(state: State<'_, AppState>) -> Result<AppSettings, IpcError> {
    state.settings.get().map_err(Into::into)
}

#[tauri::command]
pub fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
pub async fn check_for_update()
-> Result<crate::adapters::product_links::UpdateCheckResult, IpcError> {
    tauri::async_runtime::spawn_blocking(crate::adapters::product_links::check_for_update)
        .await
        .map_err(|_| IpcError {
            code: "updateCheckTaskFailed",
            message: "Update check task failed",
        })?
        .map_err(|error| match error {
            crate::adapters::product_links::UpdateCheckError::Network => IpcError {
                code: "updateCheckNetworkFailed",
                message: "Update check request failed",
            },
            crate::adapters::product_links::UpdateCheckError::Response => IpcError {
                code: "updateCheckResponseInvalid",
                message: "Update check response invalid",
            },
        })
}

#[tauri::command]
pub fn open_product_repository() -> Result<(), IpcError> {
    crate::adapters::product_links::open(crate::adapters::product_links::REPOSITORY_URL).map_err(
        |_| IpcError {
            code: "externalNavigation",
            message: "无法打开 GitHub 项目页面。",
        },
    )
}

#[tauri::command]
pub fn open_latest_release() -> Result<(), IpcError> {
    crate::adapters::product_links::open(crate::adapters::product_links::LATEST_RELEASE_URL)
        .map_err(|_| IpcError {
            code: "externalNavigation",
            message: "无法打开版本发布页面。",
        })
}

#[tauri::command]
pub fn record_ui_error(
    app: AppHandle,
    action: String,
    code: String,
    message: String,
) -> Result<(), IpcError> {
    let directory = app.path().app_log_dir().map_err(|_| IpcError {
        code: "persistence",
        message: "无法写入错误日志。",
    })?;
    crate::adapters::error_log::append(&directory, &action, &code, &message).map_err(|_| IpcError {
        code: "persistence",
        message: "无法写入错误日志。",
    })
}

#[tauri::command]
pub fn open_error_log(app: AppHandle) -> Result<(), IpcError> {
    let directory = app.path().app_log_dir().map_err(|_| IpcError {
        code: "window",
        message: "无法打开错误日志。",
    })?;
    crate::adapters::error_log::open(&directory).map_err(|_| IpcError {
        code: "window",
        message: "无法打开错误日志。",
    })
}

#[tauri::command]
pub fn read_error_log(app: AppHandle) -> Result<String, IpcError> {
    let directory = app.path().app_log_dir().map_err(|_| IpcError {
        code: "persistence",
        message: "无法读取错误日志。",
    })?;
    crate::adapters::error_log::read(&directory).map_err(|_| IpcError {
        code: "persistence",
        message: "无法读取错误日志。",
    })
}

#[tauri::command]
pub fn clear_error_log(app: AppHandle) -> Result<(), IpcError> {
    let directory = app.path().app_log_dir().map_err(|_| IpcError {
        code: "persistence",
        message: "无法清空错误日志。",
    })?;
    crate::adapters::error_log::clear(&directory).map_err(|_| IpcError {
        code: "persistence",
        message: "无法清空错误日志。",
    })
}

#[tauri::command]
pub fn desktop_list_visible(app: AppHandle) -> bool {
    app.get_webview_window("desktop")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

#[tauri::command]
pub fn update_app_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    input: UpdateAppSettings,
) -> Result<AppSettings, IpcError> {
    #[cfg(windows)]
    crate::cancel_desktop_restore(&app);
    // The login registration is applied before the row is written: a platform
    // failure then leaves the stored preference untouched instead of claiming a
    // startup behavior that is not registered.
    let input = input
        .validate()
        .map_err(|error| IpcError::from(SettingsServiceError::Validation(error)))?;
    crate::adapters::autostart::apply(input.launch_at_login).map_err(|_| IpcError {
        code: "autostart",
        message: "无法更新开机自启项，请重试。",
    })?;
    let settings = state.settings.update(input).map_err(IpcError::from)?;
    let window = app.get_webview_window("desktop").ok_or(IpcError {
        code: "window",
        message: "无法应用窗口模式，请重试。",
    })?;
    window::apply_mode(&window, settings.window_mode).map_err(|_| IpcError {
        code: "window",
        message: "无法应用窗口模式，请重试。",
    })?;
    if settings.desktop_list_mode {
        window.show().map_err(|_| IpcError {
            code: "window",
            message: "无法显示桌面清单，请重试。",
        })?;
    } else {
        window.hide().map_err(|_| IpcError {
            code: "window",
            message: "无法隐藏桌面清单，请重试。",
        })?;
    }
    // The main-window control and the tray button are two views of one switch.
    crate::sync_desktop_toggle(&app);
    state
        .sync_runtime
        .settings_changed(settings.sync_interval_minutes)
        .map_err(IpcError::from)?;
    // The reminder switch is already durable in the actor; this only moves the
    // idle deadline to match it.
    let _ = state.reminder_runtime.settings_changed();
    let _ = app.emit("calendar-todo://settings-changed", ());
    Ok(settings)
}
