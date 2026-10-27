//! Execute-only metadata selection. Never credential validity or account authority.
use crate::ports::credentials::CredentialRef;

const PREFIX: &str = "calendar-todo/account/";
const MAX_RECORDS: usize = 32;
const MAX_TARGET_UNITS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::adapters) enum SelectionFailure {
    Unavailable,
    NoMatch,
    Ambiguous,
    InvalidMetadata,
    Bounds,
}

// Deliberately no Debug, Display, Clone or serialization. Selection is not proof.
pub(in crate::adapters) struct SelectedReference(CredentialRef);
impl SelectedReference {
    pub(in crate::adapters) fn consume(self) -> CredentialRef {
        self.0
    }
}

trait MetadataBatch {
    fn count(&self) -> usize;
    fn target(&self, index: usize) -> Result<Vec<u16>, SelectionFailure>;
}

fn validate_target(units: &[u16]) -> Result<SelectedReference, SelectionFailure> {
    if units.len() > MAX_TARGET_UNITS {
        return Err(SelectionFailure::Bounds);
    }
    let text = String::from_utf16(units).map_err(|_| SelectionFailure::InvalidMetadata)?;
    let id = text
        .strip_prefix(PREFIX)
        .ok_or(SelectionFailure::InvalidMetadata)?;
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| SelectionFailure::InvalidMetadata)?;
    if uuid.is_nil() || uuid.to_string() != id {
        return Err(SelectionFailure::InvalidMetadata);
    }
    let reference = CredentialRef::parse(&text).map_err(|_| SelectionFailure::InvalidMetadata)?;
    Ok(SelectedReference(reference))
}


fn select_batch(batch: impl MetadataBatch) -> Result<SelectedReference, SelectionFailure> {
    let count = batch.count();
    if count > MAX_RECORDS {
        return Err(SelectionFailure::Bounds);
    }
    if count == 0 {
        return Err(SelectionFailure::NoMatch);
    }
    let mut selected = None;
    for index in 0..count {
        let reference = validate_target(&batch.target(index)?)?;
        if selected.is_some() {
            return Err(SelectionFailure::Ambiguous);
        }
        selected = Some(reference);
    }
    selected.ok_or(SelectionFailure::NoMatch)
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{ERROR_NOT_FOUND, GetLastError},
        Security::Credentials::{CREDENTIALW, CredEnumerateW, CredFree},
    };

    trait NativeApi {
        fn enumerate(
            &self,
            count: &mut u32,
            records: &mut *mut *mut CREDENTIALW,
        ) -> Result<(), SelectionFailure>;
        fn free(&self, records: *mut *mut CREDENTIALW);
    }
    struct WindowsApi;
    impl NativeApi for WindowsApi {
        fn enumerate(
            &self,
            count: &mut u32,
            records: &mut *mut *mut CREDENTIALW,
        ) -> Result<(), SelectionFailure> {
            let filter = format!("{PREFIX}*")
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<_>>();
            // No caller-controlled filter; explicitly forbid ENUMERATE_ALL.
            if unsafe { CredEnumerateW(filter.as_ptr(), 0, count, records) } == 0 {
                return Err(if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                    SelectionFailure::NoMatch
                } else {
                    SelectionFailure::Unavailable
                });
            }
            Ok(())
        }
        fn free(&self, records: *mut *mut CREDENTIALW) {
            unsafe { CredFree(records.cast()) };
        }
    }
    struct NativeBatch<'a> {
        api: &'a dyn NativeApi,
        records: *mut *mut CREDENTIALW,
        count: u32,
    }
    impl Drop for NativeBatch<'_> {
        fn drop(&mut self) {
            if !self.records.is_null() {
                self.api.free(self.records);
            }
        }
    }
    impl MetadataBatch for NativeBatch<'_> {
        fn count(&self) -> usize {
            self.count as usize
        }
        fn target(&self, index: usize) -> Result<Vec<u16>, SelectionFailure> {
            if index >= self.count as usize || self.records.is_null() {
                return Err(SelectionFailure::InvalidMetadata);
            }
            let record = unsafe { *self.records.add(index) };
            if record.is_null() {
                return Err(SelectionFailure::InvalidMetadata);
            }
            // Read this field only, without constructing a reference to the full
            // credential or touching UserName/blob/attributes/comment/alias.
            let target = unsafe { ptr::addr_of!((*record).TargetName).read() };
            bounded_target(target)
        }
    }
    fn bounded_target(target: *const u16) -> Result<Vec<u16>, SelectionFailure> {
        if target.is_null() {
            return Err(SelectionFailure::InvalidMetadata);
        }
        let mut units = Vec::with_capacity(MAX_TARGET_UNITS);
        for index in 0..=MAX_TARGET_UNITS {
            let unit = unsafe { *target.add(index) };
            if unit == 0 {
                return Ok(units);
            }
            if index == MAX_TARGET_UNITS {
                return Err(SelectionFailure::Bounds);
            }
            units.push(unit);
        }
        Err(SelectionFailure::Bounds)
    }
    fn select_with(api: &dyn NativeApi) -> Result<SelectedReference, SelectionFailure> {
        // Create the guard before native enumeration so every returned non-null
        // allocation is freed, even on native failure or malformed metadata.
        let mut batch = NativeBatch {
            api,
            records: ptr::null_mut(),
            count: 0,
        };
        api.enumerate(&mut batch.count, &mut batch.records)?;
        select_batch(batch)
    }
    pub(super) fn select() -> Result<SelectedReference, SelectionFailure> {
        select_with(&WindowsApi)
    }
    }

pub(in crate::adapters) fn select() -> Result<SelectedReference, SelectionFailure> {
    #[cfg(windows)]
    {
        native::select()
    }
    #[cfg(not(windows))]
    {
        Err(SelectionFailure::Unavailable)
    }
}

