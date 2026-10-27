//! Launch-at-login registration.
//!
//! The product registers exactly one value it owns in the per-user `Run` key
//! instead of depending on an autostart plugin or writing a shortcut into the
//! Startup folder: the registration follows the current executable path, needs
//! no arguments, and Windows lists it in Settings → Apps → Startup, so the user
//! keeps an independent way to disable it.

use std::path::Path;
use thiserror::Error;

/// Value name owned by the product inside `HKCU\...\Run`.
const RUN_VALUE_NAME: &str = "Calendar TODO";
/// A registration longer than this is not ours; nothing is ever allocated for a
/// value of an unknown size.
const MAX_VALUE_BYTES: u32 = 4_096;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// What reconciling the stored registration with the setting requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartChange {
    None,
    Write,
    Remove,
}

/// Compares the stored `Run` value with the desired command line.
///
/// `None` means "no registration should exist". A value that already matches is
/// left untouched so a settings change never rewrites the registry needlessly.
pub fn reconcile_change(existing: Option<&str>, desired: Option<&str>) -> AutostartChange {
    match (existing, desired) {
        (None, None) => AutostartChange::None,
        (Some(existing), Some(desired)) if existing == desired => AutostartChange::None,
        (_, Some(_)) => AutostartChange::Write,
        (Some(_), None) => AutostartChange::Remove,
    }
}

/// Quotes an executable path so a directory or file name with spaces stays one
/// argument for the login shell.
pub fn run_command_line(executable: &Path) -> String {
    format!("\"{}\"", executable.display())
}

/// Applies the setting to the per-user `Run` key.
///
/// This is called once when the setting changes and again at startup, so a
/// failed or stale registration converges without a repair path of its own.
#[cfg(windows)]
pub fn apply(launch_at_login: bool) -> Result<(), AutostartError> {
    let desired = if launch_at_login {
        let executable = std::env::current_exe().map_err(|_| AutostartError::Unavailable)?;
        Some(run_command_line(&executable))
    } else {
        None
    };
    let existing = read_run_value()?;
    match reconcile_change(existing.as_deref(), desired.as_deref()) {
        AutostartChange::None => Ok(()),
        AutostartChange::Write => write_run_value(desired.as_deref().unwrap_or_default()),
        AutostartChange::Remove => delete_run_value(),
    }
}

#[cfg(not(windows))]
pub fn apply(_launch_at_login: bool) -> Result<(), AutostartError> {
    // Launch at login is a Windows product behavior; other platforms keep the
    // setting on file without claiming a registration the platform cannot do.
    Ok(())
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
struct RegistryKey(windows_sys::Win32::System::Registry::HKEY);

#[cfg(windows)]
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Registry::RegCloseKey(self.0);
        }
    }
}

#[cfg(windows)]
fn open_run_key(
    access: windows_sys::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<RegistryKey, AutostartError> {
    let subkey = wide(RUN_KEY);
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        windows_sys::Win32::System::Registry::RegOpenKeyExW(
            windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            access,
            &mut key,
        )
    };
    if status == windows_sys::Win32::Foundation::ERROR_SUCCESS && !key.is_null() {
        Ok(RegistryKey(key))
    } else {
        Err(AutostartError::Unavailable)
    }
}

#[cfg(windows)]
fn read_run_value() -> Result<Option<String>, AutostartError> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{KEY_QUERY_VALUE, REG_SZ, RegQueryValueExW};
    let key = match open_run_key(KEY_QUERY_VALUE) {
        Ok(key) => key,
        // No Run key at all means nothing is registered.
        Err(_) => return Ok(None),
    };
    let name = wide(RUN_VALUE_NAME);
    let mut kind = 0u32;
    let mut size = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(AutostartError::Unavailable);
    }
    // A value of another type is not ours; treat it as absent so enabling the
    // setting replaces it with a well-formed REG_SZ command line.
    if kind != REG_SZ || !(2..=MAX_VALUE_BYTES).contains(&size) {
        return Ok(None);
    }
    let mut buffer = vec![0u16; size.div_ceil(2) as usize];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(AutostartError::Unavailable);
    }
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..end])))
}

#[cfg(windows)]
fn write_run_value(command: &str) -> Result<(), AutostartError> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{KEY_SET_VALUE, REG_SZ, RegSetValueExW};
    let command = wide(command);
    let byte_len = command.len() * size_of::<u16>();
    if byte_len > MAX_VALUE_BYTES as usize {
        return Err(AutostartError::Unavailable);
    }
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(RUN_VALUE_NAME);
    let status = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            command.as_ptr() as *const u8,
            byte_len as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(AutostartError::Unavailable)
    }
}

#[cfg(windows)]
fn delete_run_value() -> Result<(), AutostartError> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{KEY_SET_VALUE, RegDeleteValueW};
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(RUN_VALUE_NAME);
    let status = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(AutostartError::Unavailable)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AutostartError {
    #[error("launch-at-login registration is unavailable")]
    Unavailable,
}
