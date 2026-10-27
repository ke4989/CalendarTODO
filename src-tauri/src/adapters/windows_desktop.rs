//! Event-driven, bounded Show Desktop recovery. The observer owns one episode
//! and one one-shot timer; all native mutation is dispatched to the HWND owner.
#[path = "desktop_recovery.rs"]
mod recovery;
#[path = "windows_desktop_trace.rs"]
mod trace;

use crate::domain::settings::WindowMode;
pub(crate) use recovery::Request;
use recovery::{Action, Effect, Recovery, Signal};
use std::{
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use trace::DesktopTrace;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
    System::Threading::GetCurrentThreadId,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED,
            EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART, EnumWindows, FindWindowExW,
            GWL_EXSTYLE, GetClassNameW, GetMessageW, GetShellWindow, GetWindowLongPtrW,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible, KillTimer, MSG, PM_NOREMOVE,
            PeekMessageW, PostThreadMessageW, SetTimer, WINEVENT_OUTOFCONTEXT, WM_APP, WM_QUIT,
            WM_TIMER, WS_EX_TOPMOST,
        },
    },
};

const RESTORE_DELAY_MS: u32 = 220;
pub(crate) const DWM_CLOAKED_SHELL: i32 = 0x2;
const SIGNAL_MESSAGE: u32 = WM_APP + 101;
const RESULT_MESSAGE: u32 = WM_APP + 102;
const CANCEL_MESSAGE: u32 = WM_APP + 103;
static DESKTOP_WINDOW: AtomicU64 = AtomicU64::new(0);
static RESTORE_GENERATION: AtomicU64 = AtomicU64::new(0);
static ACTIVE_EPISODE: AtomicU64 = AtomicU64::new(0);
static OBSERVER_THREAD: AtomicU32 = AtomicU32::new(0);
static HANDLER: OnceLock<Box<dyn Fn(Request) + Send + Sync>> = OnceLock::new();
static RESULTS: OnceLock<mpsc::Sender<(Request, Option<bool>)>> = OnceLock::new();
static TRACE: OnceLock<DesktopTrace> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopWindowState {
    pub visible: bool,
    pub minimized: bool,
    pub cloak_reason: i32,
    pub topmost: bool,
}
fn admitted_desktop_surface(shell: bool, owns_view: bool, same_process: bool) -> bool {
    owns_view && (shell || same_process)
}
unsafe fn owns_desktop_view(window: HWND) -> bool {
    let class = "SHELLDLL_DefView\0".encode_utf16().collect::<Vec<_>>();
    !unsafe { FindWindowExW(window, ptr::null_mut(), class.as_ptr(), ptr::null()) }.is_null()
}
unsafe fn process_id(window: HWND) -> Option<u32> {
    let mut id = 0;
    unsafe { GetWindowThreadProcessId(window, &mut id) };
    (id != 0).then_some(id)
}
unsafe fn is_shell_process_window(window: HWND) -> bool {
    if window.is_null() {
        return false;
    }
    let shell = unsafe { GetShellWindow() };
    !shell.is_null()
        && unsafe { process_id(window) }
            .zip(unsafe { process_id(shell) })
            .is_some_and(|(left, right)| left == right)
}
unsafe fn is_explorer_desktop_surface(foreground: HWND) -> bool {
    if foreground.is_null() {
        return false;
    }
    let shell = unsafe { GetShellWindow() };
    if shell.is_null() {
        return false;
    }
    let same = unsafe { is_shell_process_window(foreground) };
    admitted_desktop_surface(
        foreground == shell,
        unsafe { owns_desktop_view(foreground) },
        same,
    )
}
/// The validated Explorer desktop surface, resolved without depending on which
/// window currently owns the foreground. Show Desktop can publish the taskbar or
/// transient Shell windows while the desktop surface and its ordering remain
/// authoritative.
pub(crate) fn desktop_surface() -> Option<HWND> {
    struct Surface {
        shell_pid: u32,
        found: HWND,
        ambiguous: bool,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> i32 {
        let surface = unsafe { &mut *(data as *mut Surface) };
        if unsafe { process_id(window) } != Some(surface.shell_pid)
            || unsafe { IsWindowVisible(window) } == 0
            || unsafe { IsIconic(window) } != 0
        {
            return 1;
        }
        let mut reason = 0i32;
        if unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                ptr::from_mut(&mut reason).cast(),
                std::mem::size_of_val(&reason) as u32,
            )
        } < 0
            || reason != 0
            || !unsafe { owns_desktop_view(window) }
        {
            return 1;
        }
        if !surface.found.is_null() {
            surface.ambiguous = true;
            return 0;
        }
        surface.found = window;
        1
    }
    let shell = unsafe { GetShellWindow() };
    if shell.is_null() {
        return None;
    }
    let mut surface = Surface {
        shell_pid: unsafe { process_id(shell) }.unwrap_or(0),
        found: ptr::null_mut(),
        ambiguous: false,
    };
    if surface.shell_pid == 0 {
        return None;
    }
    unsafe { EnumWindows(Some(visit), ptr::from_mut(&mut surface) as LPARAM) };
    (!surface.ambiguous && !surface.found.is_null()).then_some(surface.found)
}

fn window_class(window: HWND) -> String {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

/// A Shell surface that legitimately represents Show Desktop without owning
/// `SHELLDLL_DefView`: the taskbars. These are admitted only with additional
/// native evidence, never as general foreground authority.
pub(crate) fn transition_surface(window: HWND) -> bool {
    !window.is_null()
        && unsafe { is_shell_process_window(window) }
        && matches!(
            window_class(window).as_str(),
            "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        )
}

/// Explorer's own foreground-staging helper is not user activity. Every other
/// foreground window, including Explorer-owned application frames, cancels.
fn staging_window(window: HWND) -> bool {
    !window.is_null()
        && unsafe { is_shell_process_window(window) }
        && window_class(window) == "ForegroundStaging"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesktopOrder {
    Above,
    Below,
    Unknown,
}

/// EnumWindows gives a stable enumeration, without reading titles or content.
/// Missing handles are not proof that the window is below the desktop.
pub(crate) fn desktop_order(hwnd: HWND, desktop: HWND) -> DesktopOrder {
    struct Order {
        hwnd: HWND,
        desktop: HWND,
        result: DesktopOrder,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> i32 {
        let order = unsafe { &mut *(data as *mut Order) };
        if window == order.hwnd {
            order.result = DesktopOrder::Above;
            return 0;
        }
        if window == order.desktop {
            order.result = DesktopOrder::Below;
            return 0;
        }
        1
    }
    let mut order = Order {
        hwnd,
        desktop,
        result: DesktopOrder::Unknown,
    };
    unsafe { EnumWindows(Some(visit), ptr::from_mut(&mut order) as LPARAM) };
    order.result
}

pub(crate) fn window_state(hwnd: HWND) -> Option<DesktopWindowState> {
    let mut cloak_reason = 0i32;
    let status = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            ptr::from_mut(&mut cloak_reason).cast(),
            std::mem::size_of_val(&cloak_reason) as u32,
        )
    };
    if status < 0 {
        return None;
    }
    Some(DesktopWindowState {
        visible: unsafe { IsWindowVisible(hwnd) } != 0,
        minimized: unsafe { IsIconic(hwnd) } != 0,
        cloak_reason,
        topmost: unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_TOPMOST as isize != 0,
    })
}
/// Called on the window owner thread. It initializes COM only when the thread
/// does not already have an apartment, then queries the current virtual desktop.
pub(crate) fn on_current_virtual_desktop(hwnd: HWND) -> bool {
    use windows::Win32::{
        Foundation::HWND as ComHwnd,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager},
    };
    let initialized_here = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    let manager: windows::core::Result<IVirtualDesktopManager> =
        unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER) };
    let result = manager
        .and_then(|manager| unsafe { manager.IsWindowOnCurrentVirtualDesktop(ComHwnd(hwnd)) })
        .is_ok_and(|value| value.as_bool());
    if initialized_here {
        unsafe { CoUninitialize() };
    }
    result
}
pub fn observe_window(hwnd: HWND) {
    cancel();
    DESKTOP_WINDOW.store(hwnd as usize as u64, Ordering::SeqCst);
}
pub(crate) fn observed_window() -> HWND {
    DESKTOP_WINDOW.load(Ordering::SeqCst) as usize as HWND
}
pub fn restore_admitted(enabled: bool, mode: WindowMode) -> bool {
    enabled && mode == WindowMode::DesktopCompanion
}
pub(crate) fn restore_succeeded(state: DesktopWindowState) -> bool {
    state.visible && !state.minimized && state.cloak_reason == 0 && !state.topmost
}
impl Request {
    pub(crate) fn is_current(self) -> bool {
        RESTORE_GENERATION.load(Ordering::SeqCst) == self.generation
            && ACTIVE_EPISODE.load(Ordering::SeqCst) == self.episode
    }
    pub(crate) fn needs_restore(self) -> bool {
        matches!(self.action, Action::Restore | Action::Fallback)
    }
    pub(crate) fn prepares_reveal(self) -> bool {
        self.action == Action::Prepare
    }
    pub(crate) fn cancels_reveal(self) -> bool {
        self.action == Action::Cancel
    }
    pub(crate) fn tombstones_reveal(self) -> bool {
        matches!(self.action, Action::Cancel | Action::Fallback)
    }
    pub(crate) fn reveal_script(self) -> String {
        include_str!("desktop_reveal.js")
            .replace("__GENERATION__", &self.generation.to_string())
            .replace("__EPISODE__", &self.episode.to_string())
            .replace(
                "__CANCEL__",
                if self.tombstones_reveal() {
                    "true"
                } else {
                    "false"
                },
            )
    }
}
fn post(message: u32, value: usize) {
    let thread = OBSERVER_THREAD.load(Ordering::SeqCst);
    if thread != 0 {
        unsafe { PostThreadMessageW(thread, message, value, 0) };
    }
}
pub fn cancel() {
    let generation = RESTORE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    ACTIVE_EPISODE.store(0, Ordering::SeqCst);
    post(CANCEL_MESSAGE, generation as usize);
    if let Some(handler) = HANDLER.get() {
        handler(Request {
            generation,
            episode: 0,
            attempt: 0,
            action: Action::Cancel,
        });
    }
}
pub fn shutdown() {
    cancel();
    post(WM_QUIT, 0);
}
fn signal(signal: Signal) {
    let code = match signal {
        Signal::DesktopForeground => 0,
        Signal::Minimized => 1,
        Signal::ShellCloaked => 2,
        Signal::Uncloaked => 3,
        Signal::OtherForeground => 4,
    };
    let thread = OBSERVER_THREAD.load(Ordering::SeqCst);
    if thread != 0 {
        unsafe {
            PostThreadMessageW(
                thread,
                SIGNAL_MESSAGE,
                RESTORE_GENERATION.load(Ordering::SeqCst) as usize,
                code,
            )
        };
    }
}
pub(crate) fn request_restore() {
    signal(Signal::Minimized);
}
pub(crate) fn completed(request: Request, healthy: Option<bool>) {
    if let Some(trace) = TRACE.get() {
        trace.result(
            request.generation,
            request.episode,
            request.attempt,
            healthy,
            observed_window(),
            window_state(observed_window()),
        );
    }
    if let Some(results) = RESULTS.get()
        && results.send((request, healthy)).is_ok()
    {
        post(RESULT_MESSAGE, 0);
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    let observed = observed_window();
    if observed.is_null() {
        return;
    }
    if let Some(trace) = TRACE.get()
        && (event == EVENT_SYSTEM_FOREGROUND || window == observed)
    {
        if let Some(state) = window_state(observed) {
            trace.event(event, observed, state);
        }
    }
    if event == EVENT_SYSTEM_FOREGROUND {
        if unsafe { is_explorer_desktop_surface(window) } {
            signal(Signal::DesktopForeground);
        } else if transition_surface(window) {
            // The Shell may move the list below its desktop after this callback.
            // Admit one delayed observation; the owner only repairs an unhealthy
            // window or one below the independently validated desktop surface.
            signal(Signal::DesktopForeground);
        } else if !staging_window(window) {
            cancel();
        }
    } else if window == observed {
        if event == EVENT_SYSTEM_MINIMIZESTART {
            signal(Signal::Minimized);
        } else if object == 0 && child == 0 {
            if event == EVENT_OBJECT_UNCLOAKED {
                signal(Signal::Uncloaked);
            } else if event == EVENT_OBJECT_CLOAKED
                && window_state(observed)
                    .is_some_and(|state| state.cloak_reason & DWM_CLOAKED_SHELL != 0)
            {
                signal(Signal::ShellCloaked);
            }
        }
    }
}
pub(crate) fn watch<F>(handler: F) -> Option<JoinHandle<()>>
where
    F: Fn(Request) + Send + Sync + 'static,
{
    if HANDLER.set(Box::new(handler)).is_err() {
        return None;
    }
    let (sender, receiver) = mpsc::channel();
    RESULTS.set(sender).ok()?;
    if let Some(trace) = DesktopTrace::from_environment() {
        let _ = TRACE.set(trace);
    }
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let observer = thread::Builder::new()
        .name("calendar-todo-desktop-observer".into())
        .spawn(move || run_message_loop(receiver, ready_sender))
        .ok()?;
    if ready.recv().ok() != Some(true) {
        let _ = observer.join();
        return None;
    }
    Some(observer)
}
fn apply(effect: Effect, timer: &mut usize, state: &mut Recovery) {
    match effect {
        Effect::None => {}
        Effect::Arm => {
            if *timer != 0 {
                unsafe { KillTimer(ptr::null_mut(), *timer) };
            }
            *timer = unsafe { SetTimer(ptr::null_mut(), 0, RESTORE_DELAY_MS, None) };
            if *timer == 0 {
                state.cancel(RESTORE_GENERATION.load(Ordering::SeqCst));
                ACTIVE_EPISODE.store(0, Ordering::SeqCst);
            }
        }
        Effect::Execute(request) => {
            if *timer != 0 {
                unsafe { KillTimer(ptr::null_mut(), *timer) };
                *timer = 0;
            }
            ACTIVE_EPISODE.store(request.episode, Ordering::SeqCst);
            if request.prepares_reveal() {
                *timer = unsafe { SetTimer(ptr::null_mut(), 0, 100, None) };
                if *timer == 0 {
                    let next = state.completed(request, Some(false));
                    apply(next, timer, state);
                    return;
                }
            }
            if let Some(handler) = HANDLER.get() {
                handler(request);
            }
        }
        Effect::Stop => {
            ACTIVE_EPISODE.store(0, Ordering::SeqCst);
            if *timer != 0 {
                unsafe { KillTimer(ptr::null_mut(), *timer) };
                *timer = 0;
            }
        }
    }
}
fn run_message_loop(
    results: mpsc::Receiver<(Request, Option<bool>)>,
    ready: mpsc::SyncSender<bool>,
) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    // Create the queue before publishing the thread id to external producers.
    unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
    OBSERVER_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let hooks = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZESTART),
    ]
    .map(|(first, last)| unsafe {
        SetWinEventHook(
            first,
            last,
            ptr::null_mut(),
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    });
    let mut recovery = Recovery::new();
    let mut timer = 0;
    let installed = hooks.iter().all(|hook| !hook.is_null());
    let _ = ready.send(installed);
    if installed {
        loop {
            if unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } <= 0 {
                break;
            }
            let generation = RESTORE_GENERATION.load(Ordering::SeqCst);
            let effect = match message.message {
                SIGNAL_MESSAGE if message.wParam as u64 == generation => {
                    let kind = match message.lParam {
                        0 => Signal::DesktopForeground,
                        1 => Signal::Minimized,
                        2 => Signal::ShellCloaked,
                        3 => Signal::Uncloaked,
                        _ => Signal::OtherForeground,
                    };
                    recovery.signal(kind, generation)
                }
                CANCEL_MESSAGE if message.wParam as u64 == generation => {
                    recovery.cancel(generation)
                }
                WM_TIMER if message.wParam == timer && timer != 0 => {
                    unsafe { KillTimer(ptr::null_mut(), timer) };
                    timer = 0;
                    recovery.deadline()
                }
                _ => Effect::None,
            };
            apply(effect, &mut timer, &mut recovery);
            for (request, healthy) in results.try_iter() {
                if request.generation == RESTORE_GENERATION.load(Ordering::SeqCst) {
                    let effect = recovery.completed(request, healthy);
                    apply(effect, &mut timer, &mut recovery);
                }
            }
            unsafe { DispatchMessageW(&message) };
        }
    }
    OBSERVER_THREAD.store(0, Ordering::SeqCst);
    ACTIVE_EPISODE.store(0, Ordering::SeqCst);
    if timer != 0 {
        unsafe { KillTimer(ptr::null_mut(), timer) };
    }
    for hook in hooks {
        if !hook.is_null() {
            unsafe { UnhookWinEvent(hook) };
        }
    }
}

