use std::fmt;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

const REFERENCE_PREFIX: &str = "calendar-todo/account/";
const MAX_USERNAME_UTF16_UNITS: usize = 512;
const MAX_SECRET_BYTES: usize = 5 * 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub fn parse(value: &str) -> Result<Self, CredentialError> {
        let opaque = value
            .strip_prefix(REFERENCE_PREFIX)
            .ok_or(CredentialError::InvalidReference)?;
        Uuid::parse_str(opaque).map_err(|_| CredentialError::InvalidReference)?;
        Ok(Self(value.to_owned()))
    }

    pub fn new_random() -> Self {
        Self(format!("{REFERENCE_PREFIX}{}", Uuid::new_v4()))
    }

    pub fn target_name(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<credential-ref>")
    }
}

#[derive(Default)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBytes([REDACTED])")
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub struct CredentialRecord {
    username: String,
    password: SecretBytes,
}

impl CredentialRecord {
    pub fn new(username: String, password: SecretBytes) -> Result<Self, CredentialError> {
        if username.is_empty()
            || username.contains('\0')
            || username.encode_utf16().count() > MAX_USERNAME_UTF16_UNITS
        {
            return Err(CredentialError::InvalidUsername);
        }
        if password.expose().is_empty() || password.expose().len() > MAX_SECRET_BYTES {
            return Err(CredentialError::InvalidSecret);
        }
        Ok(Self { username, password })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &SecretBytes {
        &self.password
    }
}

impl fmt::Debug for CredentialRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialRecord([REDACTED])")
    }
}

impl Drop for CredentialRecord {
    fn drop(&mut self) {
        self.username.zeroize();
    }
}

pub trait CredentialStore: Send + Sync {
    fn read(&self, reference: &CredentialRef) -> Result<CredentialRecord, CredentialError>;
    fn write(
        &self,
        reference: &CredentialRef,
        credential: &CredentialRecord,
    ) -> Result<(), CredentialError>;
    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError>;
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CredentialError {
    #[error("credential reference must be an opaque account UUID")]
    InvalidReference,
    #[error("credential was not found")]
    NotFound,
    #[error("credential record has no username")]
    MissingUsername,
    #[error("credential username is invalid")]
    InvalidUsername,
    #[error("credential secret is invalid")]
    InvalidSecret,
    #[error("credential store operation failed with OS error {0}")]
    Platform(u32),
    #[error("credential storage is supported only on Windows")]
    UnsupportedPlatform,
}
