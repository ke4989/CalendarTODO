//! Optional bounded diagnostics for the Show Desktop transition.
//!
//! The trace exists to make one question answerable from real evidence: which
//! signal Windows actually publishes when the user presses Win+D or performs the
//! touchpad Show Desktop gesture, and what our window's state is at that moment.
//! It records only window handles, Shell state bits, the foreground window's
//! class name and process id. It never records calendar content, account data,
//! credentials, window titles or user input.
//!
//! Development trace stays inert unless `CALENDAR_TODO_DESKTOP_TRACE` is set.
//! Default optimized product builds do not enable it.

use std::{fs::OpenOptions, io::Write, path::PathBuf, sync::Mutex, time::Instant};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetLayeredWindowAttributes,
        GetWindowLongPtrW, GetWindowThreadProcessId, WS_EX_LAYERED,
    },
};

use super::DesktopWindowState;

/// Environment variable naming the trace file. Unset means no diagnostics.
const TRACE_PATH_VARIABLE: &str = "CALENDAR_TODO_DESKTOP_TRACE";
/// Bound on a single trace file so a long session cannot fill the disk.
const MAX_TRACE_BYTES: u64 = 512 * 1024;

pub struct DesktopTrace {
    path: PathBuf,
    write_lock: Mutex<()>,
    started: Instant,
}

impl DesktopTrace {
    pub fn from_environment() -> Option<Self> {
        if !cfg!(any(debug_assertions, feature = "dev-test")) {
            return None;
        }
        let path = std::env::var_os(TRACE_PATH_VARIABLE)?;
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path: PathBuf::from(path),
            write_lock: Mutex::new(()),
            started: Instant::now(),
        })
    }

    pub fn event(&self, event: u32, window: HWND, state: DesktopWindowState) {
        self.write(&format!(
            "event={} window={:?} visible={} minimized={} cloak_reason={} topmost={} {} foreground={}",
            event_name(event),
            window,
            state.visible,
            state.minimized,
            state.cloak_reason,
            state.topmost,
            native_surface(window),
            foreground_description(),
        ));
    }

    pub fn result(
        &self,
        generation: u64,
        episode: u64,
        attempt: u8,
        healthy: Option<bool>,
        window: HWND,
        state: Option<DesktopWindowState>,
    ) {
        self.write(&format!(
            "result generation={generation} episode={episode} attempt={attempt} healthy={healthy:?} state={state:?} {} foreground={}",
            native_surface(window),
            foreground_description()
        ));
    }

    fn write(&self, entry: &str) {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if std::fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() >= MAX_TRACE_BYTES) {
            return;
        }
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = writeln!(file, "{} {entry}", self.started.elapsed().as_millis());
    }
}



fn event_name(event: u32) -> &'static str {
    match event {
        windows_sys::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_CLOAKED => "cloaked",
        windows_sys::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_UNCLOAKED => "uncloaked",
        windows_sys::Win32::UI::WindowsAndMessaging::EVENT_SYSTEM_FOREGROUND => "foreground",
        windows_sys::Win32::UI::WindowsAndMessaging::EVENT_SYSTEM_MINIMIZESTART => "minimized",
        _ => "state",
    }
}

fn native_surface(window: HWND) -> String {
    let layered = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } & WS_EX_LAYERED as isize != 0;
    let mut alpha = 255u8;
    let mut color = 0u32;
    let mut flags = 0u32;
    let known = layered
        && unsafe { GetLayeredWindowAttributes(window, &mut color, &mut alpha, &mut flags) } != 0;
    let owner = unsafe { GetWindowThreadProcessId(window, std::ptr::null_mut()) };
    format!(
        "layered={layered} alpha={:?} owner_thread={owner}",
        known.then_some(alpha)
    )
}

/// Identify the foreground window by class name and process id only. Titles are
/// user content and are deliberately never read.
fn foreground_description() -> String {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return "none".into();
    }
    let mut buffer = [0u16; 128];
    let length = unsafe { GetClassNameW(foreground, buffer.as_mut_ptr(), buffer.len() as i32) };
    let class = if length > 0 {
        String::from_utf16_lossy(&buffer[..length as usize])
    } else {
        "?".into()
    };
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(foreground, &mut process_id) };
    format!("{class}/{process_id}")
}
