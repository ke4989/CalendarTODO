//! Checks the fixed public release feed and opens fixed product pages.
//! This adapter uses no application credentials and accepts no URL from the
//! webview, so it cannot become a general-purpose network or shell boundary.

use semver::Version;
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};

pub const REPOSITORY_URL: &str = "https://github.com/ke4989/CalendarTODO";
pub const LATEST_RELEASE_URL: &str = "https://github.com/ke4989/CalendarTODO/releases/latest";
const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/ke4989/CalendarTODO/releases/latest";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum UpdateCheckResult {
    UpToDate,
    Available { version: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCheckError {
    Network,
    Response,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

pub fn check_for_update() -> Result<UpdateCheckResult, UpdateCheckError> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|_| UpdateCheckError::Network)?;
    let response = client
        .get(LATEST_RELEASE_API_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "CalendarTODO-update-check")
        .send()
        .map_err(|_| UpdateCheckError::Network)?;
    read_release(
        env!("CARGO_PKG_VERSION"),
        response.status().is_success(),
        response.content_length(),
        response,
    )
}

fn read_release(
    current: &str,
    success: bool,
    length: Option<u64>,
    body: impl Read,
) -> Result<UpdateCheckResult, UpdateCheckError> {
    if !success || length.is_some_and(|length| length > MAX_RELEASE_RESPONSE_BYTES) {
        return Err(UpdateCheckError::Response);
    }
    let mut bytes = Vec::new();
    body.take(MAX_RELEASE_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UpdateCheckError::Network)?;
    if bytes.len() as u64 > MAX_RELEASE_RESPONSE_BYTES {
        return Err(UpdateCheckError::Response);
    }
    let latest: LatestRelease =
        serde_json::from_slice(&bytes).map_err(|_| UpdateCheckError::Response)?;
    available_update(current, &latest.tag_name)
}

fn available_update(current: &str, tag: &str) -> Result<UpdateCheckResult, UpdateCheckError> {
    let current = Version::parse(current).map_err(|_| UpdateCheckError::Response)?;
    let release = Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .map_err(|_| UpdateCheckError::Response)?;
    Ok(if release > current {
        UpdateCheckResult::Available {
            version: release.to_string(),
        }
    } else {
        UpdateCheckResult::UpToDate
    })
}

#[cfg(windows)]
pub fn open(url: &'static str) -> Result<(), ()> {
    use std::iter;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    if !matches!(url, REPOSITORY_URL | LATEST_RELEASE_URL) {
        return Err(());
    }
    let operation = "open"
        .encode_utf16()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let target = url.encode_utf16().chain(iter::once(0)).collect::<Vec<_>>();
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
    if result as isize > 32 {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(not(windows))]
pub fn open(_url: &'static str) -> Result<(), ()> {
    Err(())
}
