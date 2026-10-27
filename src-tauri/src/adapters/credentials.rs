use crate::ports::credentials::{
    CredentialError, CredentialRecord, CredentialRef, CredentialStore, SecretBytes,
};
use zeroize::Zeroize;

pub struct WindowsCredentialStore;

impl WindowsCredentialStore {
    /// Copies a Phase 0 probe credential into the product account namespace.
    /// This is intentionally narrow, keeps the original record, and never
    /// returns credential material to the caller.
    #[cfg(windows)]
    pub fn provision_from_probe(
        &self,
        probe_target: &str,
        account_reference: &CredentialRef,
    ) -> Result<(), CredentialError> {
        use uuid::Uuid;

        let probe_id = probe_target
            .strip_prefix("calendar-todo/probe/")
            .ok_or(CredentialError::InvalidReference)?;
        let account_id = account_reference
            .target_name()
            .strip_prefix("calendar-todo/account/")
            .ok_or(CredentialError::InvalidReference)?;
        if Uuid::parse_str(probe_id).is_err() || probe_id != account_id {
            return Err(CredentialError::InvalidReference);
        }
        let credential = read_windows_target(probe_target)?;
        self.write(account_reference, &credential)
    }
}

#[cfg(windows)]
fn read_windows_target(target_name: &str) -> Result<CredentialRecord, CredentialError> {
    use std::{ptr, slice};
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };

    let mut target: Vec<u16> = target_name.encode_utf16().chain(Some(0)).collect();
    let mut credential: *mut CREDENTIALW = ptr::null_mut();
    if unsafe { CredReadW(target.as_mut_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
        let code = unsafe { GetLastError() };
        return if code == ERROR_NOT_FOUND {
            Err(CredentialError::NotFound)
        } else {
            Err(CredentialError::Platform(code))
        };
    }

    let value = unsafe { &*credential };
    let username = if value.UserName.is_null() {
        String::new()
    } else {
        let mut length = 0usize;
        while length < 1024 && unsafe { *value.UserName.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { slice::from_raw_parts(value.UserName, length) })
    };
    let password = if value.CredentialBlobSize == 0 {
        Vec::new()
    } else if value.CredentialBlob.is_null() {
        unsafe { CredFree(credential.cast()) };
        return Err(CredentialError::Platform(13));
    } else {
        unsafe {
            slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize).to_vec()
        }
    };
    unsafe { CredFree(credential.cast()) };
    if username.is_empty() {
        return Err(CredentialError::MissingUsername);
    }
    CredentialRecord::new(username, SecretBytes::new(normalize_secret_blob(password)?))
}

fn normalize_secret_blob(mut bytes: Vec<u8>) -> Result<Vec<u8>, CredentialError> {
    if bytes.len() >= 2
        && bytes.len().is_multiple_of(2)
        && bytes.as_chunks::<2>().0.iter().any(|pair| pair[1] == 0)
    {
        let mut units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        let decoded = String::from_utf16(&units).map_err(|_| CredentialError::InvalidSecret)?;
        units.zeroize();
        bytes.zeroize();
        return Ok(decoded.into_bytes());
    }
    std::str::from_utf8(&bytes).map_err(|_| CredentialError::InvalidSecret)?;
    Ok(bytes)
}

#[cfg(windows)]
impl CredentialStore for WindowsCredentialStore {
    fn read(&self, reference: &CredentialRef) -> Result<CredentialRecord, CredentialError> {
        read_windows_target(reference.target_name())
    }

    fn write(
        &self,
        reference: &CredentialRef,
        credential: &CredentialRecord,
    ) -> Result<(), CredentialError> {
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
        };
        let mut target = reference
            .target_name()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut username = credential
            .username()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let blob = credential.password().expose();
        let native = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: ptr::null_mut(),
            LastWritten: Default::default(),
            CredentialBlobSize: u32::try_from(blob.len())
                .map_err(|_| CredentialError::InvalidSecret)?,
            CredentialBlob: blob.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: ptr::null_mut(),
            TargetAlias: ptr::null_mut(),
            UserName: username.as_mut_ptr(),
        };
        let result = if unsafe { CredWriteW(&native, 0) } == 0 {
            Err(CredentialError::Platform(unsafe { GetLastError() }))
        } else {
            Ok(())
        };
        username.zeroize();
        result
    }

    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
        use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};

        let target = reference
            .target_name()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_NOT_FOUND {
                Err(CredentialError::NotFound)
            } else {
                Err(CredentialError::Platform(code))
            }
        } else {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
impl CredentialStore for WindowsCredentialStore {
    fn read(&self, _: &CredentialRef) -> Result<CredentialRecord, CredentialError> {
        Err(CredentialError::UnsupportedPlatform)
    }

    fn write(&self, _: &CredentialRef, _: &CredentialRecord) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedPlatform)
    }

    fn delete(&self, _: &CredentialRef) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedPlatform)
    }
}
