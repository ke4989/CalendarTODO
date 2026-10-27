//! Windows desktop lifecycle signals for sleep/resume and clock changes.
//!
//! Tauri's portable `RunEvent::Resumed` is produced by the Android and iOS
//! backends only, so a desktop build that relies on it never reconciles after a
//! machine wakes up. Windows instead sends `WM_POWERBROADCAST` to every
//! top-level window and `WM_TIMECHANGE` when the wall clock moves. A hidden
//! top-level window on its own thread receives those broadcasts: message-only
//! windows are not eligible for broadcast traffic, so they cannot be used here.

use std::{
    ptr,
    sync::OnceLock,
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG,
        PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, RegisterClassW, WM_POWERBROADCAST,
        WM_TIMECHANGE, WNDCLASSW,
    },
};

const WINDOW_CLASS: &str = "CalendarTodoLifecycleWindow";

/// Bounded lifecycle signals the product reacts to. Suspension itself needs no
/// work: the process is frozen and the durable job store already holds the
/// state that resume reconciliation reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSignal {
    Resumed,
    ClockChanged,
}

static HANDLER: OnceLock<Box<dyn Fn(PowerSignal) + Send + Sync>> = OnceLock::new();

fn classify(message: u32, wparam: WPARAM) -> Option<PowerSignal> {
    match message {
        WM_POWERBROADCAST
            if wparam as u32 == PBT_APMRESUMEAUTOMATIC || wparam as u32 == PBT_APMRESUMESUSPEND =>
        {
            Some(PowerSignal::Resumed)
        }
        WM_TIMECHANGE => Some(PowerSignal::ClockChanged),
        _ => None,
    }
}

fn dispatch(signal: PowerSignal) {
    if let Some(handler) = HANDLER.get() {
        handler(signal);
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(signal) = classify(message, wparam) {
        dispatch(signal);
        // WM_POWERBROADCAST expects TRUE when the application handled it.
        return if message == WM_POWERBROADCAST { 1 } else { 0 };
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Register the lifecycle callback and start the message thread. The thread and
/// its hidden window live for the process lifetime; a failure to create them is
/// reported by returning `None` so the caller can start without the signal.
pub fn watch<F>(handler: F) -> Option<JoinHandle<()>>
where
    F: Fn(PowerSignal) + Send + Sync + 'static,
{
    if HANDLER.set(Box::new(handler)).is_err() {
        return None;
    }
    thread::Builder::new()
        .name("calendar-todo-lifecycle".into())
        .spawn(run_message_loop)
        .ok()
}

fn run_message_loop() {
    let class_name = wide(WINDOW_CLASS);
    let instance: HINSTANCE = unsafe { GetModuleHandleW(ptr::null()) };
    let window_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    // A duplicate registration only means the class already exists.
    unsafe { RegisterClassW(&window_class) };
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            ptr::null(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        return;
    }
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe { DispatchMessageW(&message) };
    }
}
