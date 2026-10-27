//! Windows single-instance ownership for the desktop companion.
//!
//! The product must own exactly one tray icon and one reminder scheduler. A
//! second process would add a second tray icon and could deliver the same
//! reminder twice, which the notification contract forbids. A named mutex
//! decides ownership; a named event lets a later launch ask the running
//! instance to restore its window instead of starting a second copy.

use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};
use thiserror::Error;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0},
    System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
        WaitForSingleObject,
    },
};

const MUTEX_NAME: &str = "Local\\com.calendartodo.desktop.single-instance";
const SHOW_EVENT_NAME: &str = "Local\\com.calendartodo.desktop.show-window";

/// Owns the process-lifetime handles that make this process the only instance.
pub struct InstanceOwner {
    mutex: HANDLE,
    show_event: HANDLE,
}

// Kernel handles are process-wide values usable from any thread; the owner
// hands one clone to the signalling thread and one to Tauri's managed state.
unsafe impl Send for InstanceOwner {}
unsafe impl Sync for InstanceOwner {}

impl Drop for InstanceOwner {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.show_event);
            CloseHandle(self.mutex);
        }
    }
}

pub enum InstanceRole {
    Owner(Arc<InstanceOwner>),
    AlreadyRunning,
}

/// Bounded single-instance failure. The caller only needs to know that Windows
/// refused the mutex or the wake-up event, never why.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum InstanceError {
    #[error("Windows refused the single-instance mutex or event")]
    Unavailable,
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn acquire() -> Result<InstanceRole, InstanceError> {
    acquire_named(MUTEX_NAME, SHOW_EVENT_NAME)
}

/// Separate diagnostic ownership; it never signals or launches the product.
pub(crate) fn acquire_mutation_diagnostic() -> Result<InstanceRole, InstanceError> {
    acquire_named(
        "Local\\com.calendartodo.mutation-diagnostic",
        "Local\\com.calendartodo.mutation-diagnostic.unused-event",
    )
}

/// Ask the running instance to restore its window. A missing event means the
/// owner is shutting down, which the caller treats as a normal no-op.
pub fn signal_owner() -> Result<(), InstanceError> {
    signal_named(SHOW_EVENT_NAME)
}

fn acquire_named(mutex_name: &str, event_name: &str) -> Result<InstanceRole, InstanceError> {
    let mutex_name = wide(mutex_name);
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        return Err(InstanceError::Unavailable);
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Ok(InstanceRole::AlreadyRunning);
    }
    let event_name = wide(event_name);
    let show_event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if show_event.is_null() {
        unsafe { CloseHandle(mutex) };
        return Err(InstanceError::Unavailable);
    }
    Ok(InstanceRole::Owner(Arc::new(InstanceOwner {
        mutex,
        show_event,
    })))
}

fn signal_named(event_name: &str) -> Result<(), InstanceError> {
    let event_name = wide(event_name);
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
    if event.is_null() {
        return Err(InstanceError::Unavailable);
    }
    let signalled = unsafe { SetEvent(event) } != 0;
    unsafe { CloseHandle(event) };
    if signalled {
        Ok(())
    } else {
        Err(InstanceError::Unavailable)
    }
}

impl InstanceOwner {
    /// Keep both handles open for the process lifetime and run the supplied
    /// action whenever another launch signals this owner.
    pub fn listen<F>(self: &Arc<Self>, on_signal: F) -> Option<JoinHandle<()>>
    where
        F: Fn() + Send + 'static,
    {
        let owner = Arc::clone(self);
        thread::Builder::new()
            .name("calendar-todo-single-instance".into())
            .spawn(move || {
                let _owner = owner;
                loop {
                    // The event is auto-reset, so one wait serves one launch.
                    if unsafe { WaitForSingleObject(_owner.show_event, INFINITE) } != WAIT_OBJECT_0
                    {
                        break;
                    }
                    on_signal();
                }
            })
            .ok()
    }
}
