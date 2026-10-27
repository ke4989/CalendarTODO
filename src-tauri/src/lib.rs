pub mod adapters;
pub mod application;
// Compiled only for an explicit `--features dev-test` build. Nothing in the
// product registers a command, route or menu for it.
pub mod domain;
mod ipc;
pub mod ports;

#[cfg(not(windows))]
use application::reminders::NoopDelivery;
use application::{
    account_profiles::AccountProfileService,
    account_registry::AccountSourceRegistry,
    agenda::AgendaService,
    attachment_runtime::{
        ATTACHMENT_STATE_EVENT, AttachmentRuntime, AttachmentStateEmitter, AttachmentView,
    },
    credentials::CredentialService,
    discovery_runtime::RemoteDiscoveryRuntime,
    local_todos::LocalTodoService,
    recurrence_horizon::{HorizonTrigger, RecurrenceHorizonRefresher},
    recurrence_runtime::{
        AGENDA_REFRESH_EVENT, AgendaRefreshEmitter, AgendaRefreshNotice, HorizonRuntime,
    },
    reminders::ReminderRuntime,
    remote_mutation_facade::{
        MUTATION_APPLICATION_EVENT, MutationApplicationEmitter, MutationApplicationView,
        RemoteMutationApplication,
    },
    settings::SettingsService,
    sync_runtime::{SYNC_STATE_EVENT, SyncExecutionRuntime, SyncStateEmitter},
};
use ports::clock::{Clock, SystemClock};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tauri::{
    Emitter, Manager, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

struct TauriAgendaRefreshEmitter(tauri::AppHandle, Arc<ReminderRuntime>);
struct NativeTrayFallback(Menu<tauri::Wry>);
const TRAY_ICON_ID: &str = "calendar-todo";
/// Minimized-window observations join the observer's existing bounded episode.
#[cfg(windows)]
fn schedule_desktop_restore(_app: &tauri::AppHandle) {
    adapters::windows_desktop::request_restore();
}

/// Execute on the HWND owner thread, alongside explicit hide and mode changes.
#[cfg(windows)]
fn execute_desktop_restore(app: &tauri::AppHandle, request: adapters::windows_desktop::Request) {
    use adapters::windows_desktop as desktop;
    let current_app = app.clone();
    let dispatched = app.run_on_main_thread(move || {
        if request.cancels_reveal() {
            if let Some(window) = current_app.get_webview_window("desktop") {
                let _ = window.eval(request.reveal_script());
            }
            return;
        }
        if request.prepares_reveal() {
            let admitted = request.is_current()
                && current_app
                    .try_state::<ipc::AppState>()
                    .and_then(|state| state.settings.get().ok())
                    .is_some_and(|settings| {
                        desktop::restore_admitted(settings.desktop_list_mode, settings.window_mode)
                    });
            if !admitted {
                desktop::completed(request, None);
                return;
            }
            let hwnd = desktop::observed_window();
            let needs_repair = desktop::window_state(hwnd).is_some_and(|native| {
                !desktop::restore_succeeded(native)
                    || desktop::desktop_surface().is_none_or(|surface| {
                        desktop::desktop_order(hwnd, surface) != desktop::DesktopOrder::Above
                    })
            });
            if !needs_repair || !desktop::on_current_virtual_desktop(hwnd) {
                desktop::completed(request, Some(true));
                return;
            }
            if let Some(window) = current_app.get_webview_window("desktop") {
                // A successful callback confirms preparation. The observer's
                // one-shot fallback performs opaque native recovery if the
                // callback is lost; neither UI thread waits for JavaScript.
                if window
                    .eval_with_callback(request.reveal_script(), move |status| {
                        desktop::completed(request, Some(status == "\"prepared\""));
                    })
                    .is_ok()
                {
                    return;
                }
            }
            desktop::completed(request, Some(false));
            return;
        }
        if request.tombstones_reveal()
            && let Some(window) = current_app.get_webview_window("desktop")
        {
            // Preparation failed or timed out: reject any late fade script
            // before the opaque native recovery runs.
            let _ = window.eval(request.reveal_script());
        }
        let outcome = (|| {
            if !request.is_current() {
                return None;
            }
            let state = current_app.try_state::<ipc::AppState>()?;
            let settings = state.settings.get().ok()?;
            if !desktop::restore_admitted(settings.desktop_list_mode, settings.window_mode) {
                return None;
            }
            let hwnd = desktop::observed_window();
            let before = desktop::window_state(hwnd)?;
            if !desktop::on_current_virtual_desktop(hwnd) {
                return None;
            }
            if hwnd.is_null() || !request.is_current() {
                return None;
            }
            let applied = !request.needs_restore()
                || adapters::window::restore_after_show_desktop(hwnd, before.cloak_reason, || {
                    request.is_current()
                })
                .is_ok();
            let after = desktop::window_state(hwnd)?;
            let healthy = applied
                && desktop::restore_succeeded(after)
                && desktop::desktop_surface().is_some_and(|surface| {
                    desktop::desktop_order(hwnd, surface) == desktop::DesktopOrder::Above
                });
            if !request.needs_restore()
                && request.attempt == 2
                && !healthy
                && let Ok(directory) = current_app.path().app_log_dir()
            {
                let _ = adapters::error_log::append(
                    &directory,
                    "desktop_restore",
                    "window",
                    "Desktop recovery did not settle",
                );
            }
            Some(healthy)
        })();
        desktop::completed(request, outcome);
    });
    if dispatched.is_err() {
        desktop::completed(request, None);
    }
}

/// Cancels any pending Show Desktop recovery. Explicit hiding, a settings
/// change and shutdown all mean the same thing: nothing the Shell did may bring
/// the list back.
#[cfg(windows)]
pub(crate) fn cancel_desktop_restore(_app: &tauri::AppHandle) {
    adapters::windows_desktop::cancel();
}

/// Grace period after the tray popover opens. Windows can move focus back to the
/// shell right after a tray click, and that alone must not close the menu.
const TRAY_MENU_FOCUS_GRACE: Duration = Duration::from_millis(600);

impl AgendaRefreshEmitter for TauriAgendaRefreshEmitter {
    fn emit(&self, notice: AgendaRefreshNotice) {
        let _ = self.1.refresh();
        let _ = self.0.emit(AGENDA_REFRESH_EVENT, notice);
    }
}

impl SyncStateEmitter for TauriAgendaRefreshEmitter {
    fn emit(&self, view: application::sync_runtime::SyncExecutionView) {
        let _ = self.0.emit(SYNC_STATE_EVENT, view);
    }
}

impl AttachmentStateEmitter for TauriAgendaRefreshEmitter {
    fn emit(&self, view: AttachmentView) {
        let _ = self.0.emit(ATTACHMENT_STATE_EVENT, view);
    }
}

impl MutationApplicationEmitter for TauriAgendaRefreshEmitter {
    fn emit(&self, view: MutationApplicationView) {
        let _ = self.0.emit(MUTATION_APPLICATION_EVENT, view);
    }
}

/// One resume path for every platform: rebuild the recurrence horizon, let the
/// sync scheduler re-evaluate its deadlines, and reconcile reminder jobs.
fn reconcile_after_resume(state: &ipc::AppState) {
    let _ = state.trigger_horizon(HorizonTrigger::Resume);
    let _ = state.resume_sync();
    state.trigger_remote_recurrence_startup();
    let _ = state.refresh_reminders();
}

/// Notify the persistent tray popover when desktop visibility changes.
pub(crate) fn sync_desktop_toggle(app: &tauri::AppHandle) {
    let _ = app.emit("calendar-todo://tray-menu-state-changed", ());
}

/// Creates the windows declared in `tauri.conf.json`.
///
/// Tauri's own setup creates config windows *before* it runs this crate's setup
/// hook. A page that loaded quickly could therefore invoke a command while
/// `AppState` was still unmanaged and the Agenda reported a spurious "cannot read
/// the local agenda" until the user pressed retry. The config entries set
/// `create: false` and are built here instead, once every managed state exists.
fn create_configured_windows(app: &tauri::App) -> tauri::Result<()> {
    for config in app.config().app.windows.iter() {
        // The startup palette and resolved mode are painted before the page's
        // own first paint. The page then owns live system-mode changes.
        let theme_script = app
            .try_state::<ipc::AppState>()
            .and_then(|state| state.settings.get().ok())
            .map(|settings| {
                let palette = match settings.theme_palette {
                    domain::settings::ThemePalette::QuietDaybook => "quiet-daybook",
                    domain::settings::ThemePalette::DawnWheat => "dawn-wheat",
                };
                let mode_expression = match settings.color_mode {
                    domain::settings::ColorMode::System => {
                        r#"matchMedia("(prefers-color-scheme: dark)").matches?"dark":"light""#
                    }
                    domain::settings::ColorMode::Light => r#""light""#,
                    domain::settings::ColorMode::Dark => r#""dark""#,
                };
                format!(
                    r#"document.documentElement.dataset.themePalette="{palette}";document.documentElement.dataset.colorMode={mode_expression};"#
                )
            })
            .unwrap_or_default();
        let builder = tauri::WebviewWindowBuilder::from_config(app.handle(), config)?;
        let builder = if theme_script.is_empty() {
            builder
        } else {
            builder.initialization_script(theme_script)
        };
        builder.build()?;
    }
    Ok(())
}

/// True while the popover was just opened and must not be closed by the focus
/// change that a failed foreground request can produce.
fn tray_menu_within_focus_grace(app: &tauri::AppHandle) -> bool {
    app.try_state::<ipc::TrayMenuState>()
        .is_some_and(|state| state.was_shown_within(TRAY_MENU_FOCUS_GRACE))
}

fn set_desktop_list_visible(app: &tauri::AppHandle, visible: bool) {
    let Some(window) = app.get_webview_window("desktop") else {
        return;
    };
    #[cfg(windows)]
    if !visible {
        cancel_desktop_restore(app);
    }
    let applied = if visible {
        adapters::window::restore_from_tray(&window)
    } else {
        window.hide()
    };
    if applied.is_ok()
        && let Some(state) = app.try_state::<ipc::AppState>()
        && let Ok(current) = state.settings.get()
    {
        let _ = state.settings.update(domain::settings::UpdateAppSettings {
            language: current.language,
            theme_palette: current.theme_palette,
            color_mode: current.color_mode,
            window_mode: current.window_mode,
            sync_interval_minutes: current.sync_interval_minutes,
            sync_on_startup: current.sync_on_startup,
            desktop_list_mode: visible,
            desktop_opacity_percent: current.desktop_opacity_percent,
            desktop_show_calendar_name: current.desktop_show_calendar_name,
            desktop_show_time: current.desktop_show_time,
            desktop_show_tags: current.desktop_show_tags,
            main_show_calendar_name: current.main_show_calendar_name,
            main_show_time: current.main_show_time,
            main_show_tags: current.main_show_tags,
            start_hidden_in_tray: current.start_hidden_in_tray,
            launch_at_login: current.launch_at_login,
            local_todo_color: current.local_todo_color,
            local_display_past_days: current.local_display_past_days,
            local_display_future_days: current.local_display_future_days,
            default_calendar_id: current.default_calendar_id,
            reminder_notifications_enabled: current.reminder_notifications_enabled,
        });
        let _ = app.emit("calendar-todo://settings-changed", ());
    }
    sync_desktop_toggle(app);
}

/// Places the tray popover above the icon it was opened from.
///
/// The size is read back from the window instead of assumed: the menu is sized
/// from its own content, so every height change has to re-anchor it or the
/// popover would drift away from the tray after a submenu opens.
pub(crate) fn place_tray_menu(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("tray_menu") else {
        return;
    };
    let Some(anchor) = app
        .try_state::<ipc::TrayMenuState>()
        .and_then(|state| state.anchor())
    else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let screen = window
        .monitor_from_point(anchor.x, anchor.y)
        .ok()
        .flatten()
        .map(|monitor| adapters::window::MonitorRect::from(&monitor).work_area);
    let (x, y) = adapters::window::tray_popover_origin(
        anchor.x.round() as i32,
        anchor.y.round() as i32,
        size.width as i32,
        size.height as i32,
        screen,
    );
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

pub(crate) fn set_native_tray_fallback(app: &tauri::AppHandle, enabled: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ICON_ID) else {
        return;
    };
    let Some(fallback) = app.try_state::<NativeTrayFallback>() else {
        return;
    };
    if enabled {
        let _ = tray.set_menu(Some(fallback.0.clone()));
    } else {
        let _ = tray.set_menu(None::<Menu<tauri::Wry>>);
    }
}

fn show_tray_menu(app: &tauri::AppHandle, position: tauri::PhysicalPosition<f64>) -> bool {
    let Some(window) = app.get_webview_window("tray_menu") else {
        return false;
    };
    let Some(state) = app.try_state::<ipc::TrayMenuState>() else {
        return false;
    };
    if window.is_visible().unwrap_or(false) {
        state.cancel_open();
        let _ = window.hide();
        return true;
    }
    state.set_anchor(position);
    place_tray_menu(app);
    if window.show().is_err() {
        return false;
    }
    let _ = window.set_focus();
    state.mark_shown();
    let generation = state.begin_open();
    if window
        .emit("calendar-todo://tray-menu-open-check", generation)
        .is_err()
    {
        state.cancel_open();
        let _ = window.hide();
        return false;
    }
    let timeout_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        if timeout_app
            .try_state::<ipc::TrayMenuState>()
            .is_some_and(|state| state.expire_open_to_fallback(generation))
        {
            if let Some(window) = timeout_app.get_webview_window("tray_menu") {
                let _ = window.hide();
            }
            set_native_tray_fallback(&timeout_app, true);
        }
    });
    // The menu page re-measures and re-anchors itself on this event.
    sync_desktop_toggle(app);
    true
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let open_menu = MenuItem::with_id(app, "tray_open_menu", "打开完整菜单", true, None::<&str>)?;
    let show_main = MenuItem::with_id(app, "tray_show_main", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
    let native_menu = Menu::with_items(app, &[&open_menu, &show_main, &quit])?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ICON_ID)
        .tooltip("Calendar TODO")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray_open_menu" => {
                let anchor = app
                    .try_state::<ipc::TrayMenuState>()
                    .and_then(|state| state.anchor())
                    .or_else(|| app.cursor_position().ok());
                if let Some(anchor) = anchor {
                    let _ = show_tray_menu(app, anchor);
                }
            }
            "tray_show_main" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = adapters::window::restore_from_tray(&window);
                }
            }
            "tray_quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state: MouseButtonState::Up,
                position,
                ..
            } = event
            {
                match button {
                    MouseButton::Left => {
                        if let Some(menu) = tray.app_handle().get_webview_window("tray_menu") {
                            let _ = menu.hide();
                        }
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            let _ = adapters::window::restore_from_tray(&window);
                        }
                    }
                    MouseButton::Right => {
                        let Some(state) = tray.app_handle().try_state::<ipc::TrayMenuState>()
                        else {
                            return;
                        };
                        if state.take_native_fallback() {
                            set_native_tray_fallback(tray.app_handle(), false);
                        } else if !show_tray_menu(tray.app_handle(), position) {
                            state.arm_native_fallback();
                            set_native_tray_fallback(tray.app_handle(), true);
                        }
                    }
                    _ => {}
                }
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    app.manage(NativeTrayFallback(native_menu));
    Ok(())
}

pub fn run() {
    // Exactly one instance owns the tray icon, the reminder scheduler, and the
    // SQLite writer. A later launch asks the owner to restore its window; a
    // failed OS call fails open so a broken mutex cannot block startup.
    #[cfg(windows)]
    let single_instance = match adapters::single_instance::acquire() {
        Ok(adapters::single_instance::InstanceRole::Owner(owner)) => Some(owner),
        Ok(adapters::single_instance::InstanceRole::AlreadyRunning) => {
            let _ = adapters::single_instance::signal_owner();
            return;
        }
        Err(_) => None,
    };
    let app = tauri::Builder::default()
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                if window.label() == "tray_menu"
                    && let Some(state) = window.app_handle().try_state::<ipc::TrayMenuState>()
                {
                    state.cancel_open();
                }
                if window.label() == "desktop" {
                    set_desktop_list_visible(window.app_handle(), false);
                }
                return;
            }
            // The popover closes as soon as the user clicks anywhere else: that
            // click activates another window, so it takes focus away from ours.
            if let WindowEvent::Focused(false) = event
                && window.label() == "tray_menu"
                && !tray_menu_within_focus_grace(window.app_handle())
            {
                if let Some(state) = window.app_handle().try_state::<ipc::TrayMenuState>() {
                    state.cancel_open();
                }
                let _ = window.hide();
                return;
            }
            // Win+D minimizes the window, and a minimized window is not
            // cloaked, so this path carries the transitions the cloak observer
            // cannot see. Both paths feed the same bounded scheduler.
            #[cfg(windows)]
            if window.label() == "desktop"
                && matches!(event, WindowEvent::Resized(_))
                && window.is_minimized().unwrap_or(false)
            {
                schedule_desktop_restore(window.app_handle());
                return;
            }
            // Only real user moves, resizes, and DPI changes reach here; the
            // recorder coalesces them and never polls.
            if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
                let collapsed = window
                    .app_handle()
                    .try_state::<ipc::DesktopWindowState>()
                    .map(|state| state.is_collapsed())
                    .unwrap_or(false);
                if let Some(recorder) = window
                    .app_handle()
                    .try_state::<adapters::window_layout::WindowLayoutRecorder>(
                ) {
                    recorder.observe(window, collapsed);
                }
            }
        })
        .setup(move |app| {

            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let database = adapters::persistence::Database::open_with_clock(
                data_dir.join("calendar-todo.sqlite3"),
                clock.clone(),
            )?;
            #[cfg(windows)]
            let delivery: Arc<dyn application::reminders::ReminderDelivery> = Arc::new(
                adapters::notifications_windows::WindowsToastDelivery(app.handle().clone()),
            );
            #[cfg(not(windows))]
            let delivery: Arc<dyn application::reminders::ReminderDelivery> =
                Arc::new(NoopDelivery);
            // Read the stored switch before the runtime exists: construction
            // reconciles immediately, and a profile with reminders off must not
            // reconcile its way through a delivery window first.
            let settings_service = SettingsService::new(database.clone());
            let settings = settings_service.get()?;
            let reminder_runtime = Arc::new(ReminderRuntime::new(
                database.clone(),
                clock.clone(),
                delivery,
            )?);
            // Reconcile the login registration with the stored preference on every
            // launch, so a moved executable or an interrupted settings change
            // converges without a repair path of its own.
            let _ = adapters::autostart::apply(settings.launch_at_login);
            // Restore the previous session's rectangles before anything becomes
            // visible, so the user never sees the window jump after launch.
            let stored_placements = database.list_window_placements().unwrap_or_default();
            let stored_placement = |label: &str| {
                stored_placements
                    .iter()
                    .find(|item| item.window_label == label)
                    .map(|item| item.placement.clone())
            };
            #[cfg(windows)]
            if let Some(owner) = single_instance {
                owner.listen({
                    let app = app.handle().clone();
                    move || {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = adapters::window::restore_from_tray(&window);
                        }
                    }
                });
                app.manage(owner);
            }
            // The desktop build never receives Tauri's portable resume event,
            // so Windows broadcasts carry the sleep/resume and clock signals.
            #[cfg(windows)]
            adapters::windows_power::watch({
                let app = app.handle().clone();
                move |signal| match signal {
                    adapters::windows_power::PowerSignal::Resumed => {
                        if let Some(state) = app.try_state::<ipc::AppState>() {
                            reconcile_after_resume(&state);
                        }
                    }
                    adapters::windows_power::PowerSignal::ClockChanged => {
                        if let Some(state) = app.try_state::<ipc::AppState>() {
                            let _ = state.trigger_horizon(HorizonTrigger::Resume);
                            let _ = state.refresh_reminders();
                        }
                    }
                }
            });
            let refresher = RecurrenceHorizonRefresher::new(database.clone(), clock.clone())?;
            let (deadline_sender, deadline_receiver) = mpsc::channel();
            let scheduler = Arc::new(adapters::horizon_timer::SystemDeadlineScheduler::new(
                clock.clone(),
                deadline_sender,
            )?);
            let runtime_emitter = Arc::new(TauriAgendaRefreshEmitter(
                app.handle().clone(),
                reminder_runtime.clone(),
            ));
            let horizon_runtime = Arc::new(HorizonRuntime::new(
                refresher,
                scheduler,
                runtime_emitter.clone(),
                deadline_receiver,
            )?);
            let account_registry = Arc::new(AccountSourceRegistry::new(database.clone()));
            let credential_service =
                CredentialService::new(Arc::new(adapters::credentials::WindowsCredentialStore));
            let synthetic_attacher = Arc::new(
                adapters::synthetic_attachment::SyntheticSourceAttacher::new(
                    account_registry.clone(),
                ),
            );
            let production_attacher = Arc::new(
                adapters::production_attachment::ProductionSourceAttacher::new(
                    account_registry.clone(),
                    credential_service.clone(),
                    Arc::new(adapters::production_attachment::ReviewedHttpDavExecutorFactory),
                    clock.clone(),
                    database.clone(),
                ),
            );
            let calendar_discoverer = Arc::new(
                adapters::production_attachment::ProductionCalendarDiscoverer::new(
                    account_registry.clone(),
                    credential_service.clone(),
                    Arc::new(adapters::production_attachment::ReviewedHttpDavExecutorFactory),
                    clock.clone(),
                ),
            );
            let discovery_runtime = RemoteDiscoveryRuntime::new(
                account_registry.clone(),
                calendar_discoverer,
                database.clone(),
            );
            let account_profiles = AccountProfileService::new(
                account_registry.clone(),
                credential_service.clone(),
                synthetic_attacher,
                production_attacher,
            );
            let (sync_deadline_sender, sync_deadline_receiver) = mpsc::channel();
            let sync_scheduler = Arc::new(adapters::sync_timer::SystemSyncDeadlineScheduler::new(
                clock.clone(),
                sync_deadline_sender,
            )?);
            let sync_runtime = Arc::new(SyncExecutionRuntime::new(
                account_registry.clone(),
                runtime_emitter.clone(),
                runtime_emitter.clone(),
                clock.clone(),
                sync_scheduler,
                sync_deadline_receiver,
                settings.sync_interval_minutes,
            )?);
            let remote_mutations = RemoteMutationApplication::without_execution(
                database.clone(),
                account_registry.clone(),
                clock.clone(),
                runtime_emitter.clone(),
            );
            let mutation_sessions = Arc::new(adapters::production_mutation::ProductionMutationSessions::new(
                account_registry.clone(), database.clone(), credential_service.clone(), clock.clone(),
            ));
            let attachment_runtime = AttachmentRuntime::with_mutation_sessions(
                account_profiles.clone(),
                account_registry.clone(),
                sync_runtime.clone(),
                Arc::new(discovery_runtime.clone()),
                runtime_emitter,
                Some(mutation_sessions.clone()),
            );
            // Passive sessions do not install graphs or dispatch. Activation is
            // process-local, explicit, and additionally requires persisted opt-in.
            let remote_execution = Arc::new(application::remote_execution_activation::RemoteExecutionActivation::new(
                application::remote_execution_activation::ActivationOwners {
                    accounts: account_registry,
                    attachments: attachment_runtime.clone(),
                    sessions: mutation_sessions,
                    creation: application::remote_creation_facade::RemoteCreationApplication::without_execution(database.clone()),
                    conflicts: remote_mutations.clone(),
                    reminders: reminder_runtime.clone(),
                    clock: clock.clone(),
                },
            ));
            let (remote_recurrence_deadline_sender, remote_recurrence_deadline_receiver) =
                mpsc::channel();
            let remote_recurrence_scheduler = Arc::new(
                adapters::horizon_timer::SystemDeadlineScheduler::new(
                    clock.clone(),
                    remote_recurrence_deadline_sender,
                )?,
            );
            let remote_recurrence_runtime = Arc::new(
                application::remote_recurrence_runtime::RemoteRecurrenceRuntime::new(
                    database.clone(),
                    remote_execution.clone(),
                    sync_runtime.clone(),
                    clock.clone(),
                    remote_recurrence_scheduler,
                    remote_recurrence_deadline_receiver,
                )?,
            );
            let startup_attachment_runtime = attachment_runtime.clone();
            let layout_recorder = adapters::window_layout::WindowLayoutRecorder::start(
                database.clone(),
                stored_placements.clone(),
            )?;
            let app_state=ipc::AppState::with_remote_execution(ipc::AppStateParts {
                desktop_order_db: database.clone(),
                local_todos: LocalTodoService::new(database.clone()),
                settings: settings_service.clone(),
                agenda: AgendaService::new(database),
                horizon_runtime: horizon_runtime.clone(),
                remote_recurrence_runtime: Some(remote_recurrence_runtime),
                account_profiles,
                sync_runtime: sync_runtime.clone(),
                attachment_runtime,
                discovery_runtime,
                remote_mutations,
                reminder_runtime,
                clock: clock.clone(),
            }, remote_execution).map_err(|_| std::io::Error::other("remote activation owner mismatch"))?;
            app_state.install_confirmed_event_emitter(Arc::new(ipc::ConfirmedEventEmitter(app.handle().clone()))).map_err(|_|std::io::Error::other("scoped event owner mismatch"))?;
            app_state.trigger_remote_recurrence_startup();
            app.manage(app_state);
            let desktop_state = ipc::DesktopWindowState::default();
            desktop_state.set_pinned(settings.desktop_pinned);
            app.manage(desktop_state);
            app.manage(layout_recorder);
            app.manage(ipc::TrayMenuState::default());
            // The cache and local TODOs are already available. Requested startup
            // sync runs asynchronously through the same cancellable production
            // attachment path as the explicit Connect action.
            if settings.sync_on_startup {
                let _ = startup_attachment_runtime.submit_startup_accounts();
            }
            // Every managed state above exists before the first webview can load,
            // so no page can observe a half-initialized backend.
            create_configured_windows(app)?;
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("desktop") {
                // The observer only accepts signals for this window, and it is
                // installed only after that handle exists.
                let hwnd = window.hwnd()?.0 as windows_sys::Win32::Foundation::HWND;
                adapters::windows_desktop::observe_window(hwnd);
                adapters::windows_desktop::watch({
                    let app = app.handle().clone();
                    move |request| execute_desktop_restore(&app, request)
                });
            }
            if let Some(window) = app.get_webview_window("desktop") {
                adapters::window::apply_mode(&window, settings.window_mode)?;
                if let Some(placement) = stored_placement("desktop") {
                    let _ = adapters::window::restore_placement(&window, &placement, (320.0, 60.0));
                }
                // The remembered lock and fold are applied after the rectangle
                // is restored, so the folded size is never the restore target.
                if let Some(state) = app.try_state::<ipc::DesktopWindowState>()
                    && ipc::restore_desktop_window_state(&window, &state, &settings).is_err()
                {
                    // The card stays in its expanded rectangle in that case, so
                    // the stored choice is cleared to match what is visible.
                    let _ = settings_service.set_desktop_collapsed(false);
                }
            }
            if let Some(window) = app.get_webview_window("main") {
                if let Some(placement) = stored_placement("main") {
                    let _ =
                        adapters::window::restore_placement(&window, &placement, (1120.0, 700.0));
                } else {
                    let _ = adapters::window::fit_initial_bounds(
                        &window,
                        (1280.0, 760.0),
                        (1120.0, 700.0),
                    );
                }
                // `start_hidden_in_tray` keeps the frame out of the way entirely.
                if settings.start_hidden_in_tray {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                }
            }
            setup_tray(app)?;
            if settings.desktop_list_mode
                && let Some(window) = app.get_webview_window("desktop")
            {
                window.show()?;
            }
            sync_desktop_toggle(app.handle());
            horizon_runtime.trigger(HorizonTrigger::Startup)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_bootstrap_state,
            ipc::create_local_todo,
            ipc::save_local_todo,
            ipc::list_local_todos,
            ipc::list_local_calendars,
            ipc::save_local_calendar,
            ipc::set_local_calendar_hidden,
            ipc::delete_local_calendar,
            ipc::move_local_todo_to_calendar,
            ipc::get_desktop_item_order,
            ipc::set_desktop_item_order,
            ipc::reset_desktop_item_order,
            ipc::list_calendar_agenda_items,
            ipc::list_calendar_settings,
            ipc::set_calendar_color_override,
            ipc::set_calendar_visible,
            ipc::set_calendar_display_range,
            ipc::discover_remote_calendars,
            ipc::list_account_profiles,
            ipc::register_account_profile,
            ipc::set_account_profile_enabled,
            ipc::set_account_remote_editing_enabled,
            ipc::prepare_remote_crud,
            ipc::confirmed_remote_event,
            ipc::set_remote_execution_activation,
            ipc::get_remote_execution_activation,
            ipc::submit_prepared_remote_crud,
            ipc::shutdown_remote_execution,
            ipc::get_remote_mutation_view,
            ipc::queue_confirmed_remote_event_delete,
            ipc::resolve_remote_mutation_conflict,
            ipc::remove_account_profile,
            ipc::enroll_account_credential,
            ipc::attach_account_source,
            ipc::list_attachment_states,
            ipc::trigger_account_sync,
            ipc::list_account_sync_states,
            ipc::refresh_calendar_horizon,
            ipc::set_calendar_completed,
            ipc::set_calendar_important,
            ipc::update_calendar_local_metadata,
            ipc::create_calendar_subtask,
            ipc::update_calendar_subtask,
            ipc::set_calendar_subtask_completed,
            ipc::delete_calendar_subtask,
            ipc::update_local_todo,
            ipc::set_local_todo_completed,
            ipc::delete_local_todo,
            ipc::delete_local_todo_occurrence,
            ipc::restore_recently_deleted_todo,
            ipc::restore_recently_deleted_todo_occurrence,
            ipc::set_tray_source,
            ipc::show_main_from_tray,
            ipc::show_calendar_settings_from_tray,
            ipc::hide_tray_menu,
            ipc::tray_menu_ready,
            ipc::resize_tray_menu,
            ipc::quit_from_tray,
            ipc::create_subtask,
            ipc::update_subtask,
            ipc::move_local_subtask,
            ipc::set_subtask_completed,
            ipc::delete_subtask,
            ipc::get_app_settings,
            ipc::get_app_version,
            ipc::check_for_update,
            ipc::open_product_repository,
            ipc::open_latest_release,
            ipc::record_ui_error,
            ipc::open_error_log,
            ipc::read_error_log,
            ipc::clear_error_log,
            ipc::desktop_list_visible,
            ipc::update_app_settings,
            ipc::purge_expired_remote_cache,
            ipc::start_desktop_drag,
            ipc::desktop_pinned,
            ipc::desktop_collapsed,
            ipc::set_desktop_pinned,
            ipc::set_desktop_collapsed
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|app_handle, event| {
        if adapters::windows_resume::is_resume_event(&event)
            && let Some(state) = app_handle.try_state::<ipc::AppState>()
        {
            reconcile_after_resume(&state);
        }
        #[cfg(windows)]
        if matches!(event, tauri::RunEvent::Exit) {
            adapters::windows_desktop::shutdown();
        }
        if matches!(event, tauri::RunEvent::Exit)
            && let Some(state) = app_handle.try_state::<ipc::AppState>()
        {
            state.shutdown_horizon();
            state.shutdown_attachments();
            state.shutdown_discovery();
            state.shutdown_sync();
            state.shutdown_reminders();
        }
        if matches!(event, tauri::RunEvent::Exit)
            && let Some(recorder) =
                app_handle.try_state::<adapters::window_layout::WindowLayoutRecorder>()
        {
            // Flushes a move or resize that happened inside the settle window.
            recorder.shutdown();
        }
    });
}
