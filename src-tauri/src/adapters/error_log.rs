use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const FILE_NAME: &str = "calendar-todo-errors.log";

pub fn append(directory: &Path, action: &str, code: &str, message: &str) -> Result<(), ()> {
    if action.is_empty()
        || action.len() > 64
        || !action
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || code.is_empty()
        || code.len() > 64
        || message.chars().count() > 512
    {
        return Err(());
    }
    fs::create_dir_all(directory).map_err(|_| ())?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis();
    let message = message.replace(['\r', '\n'], " ");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(FILE_NAME))
        .map_err(|_| ())?;
    writeln!(
        file,
        "{timestamp}\taction={action}\tcode={code}\tmessage={message}"
    )
    .map_err(|_| ())
}

pub fn read(directory: &Path) -> Result<String, ()> {
    let path = directory.join(FILE_NAME);
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(_) => Err(()),
    }
}

pub fn clear(directory: &Path) -> Result<(), ()> {
    match OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(directory.join(FILE_NAME))
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(()),
    }
}

#[cfg(windows)]
pub fn open(directory: &Path) -> Result<(), ()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    fs::create_dir_all(directory).map_err(|_| ())?;
    let path = directory.join(FILE_NAME);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|_| ())?;
    let operation = "open"
        .encode_utf16()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let target = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    (result as isize > 32).then_some(()).ok_or(())
}

#[cfg(not(windows))]
pub fn open(_directory: &Path) -> Result<(), ()> {
    Err(())
}
