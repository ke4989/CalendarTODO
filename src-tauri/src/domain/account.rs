use std::fmt;
use thiserror::Error;
use uuid::Uuid;

const CREDENTIAL_PREFIX: &str = "calendar-todo/account/";
const MAX_LABEL_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    CalDav,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CalDav => "caldav",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, AccountError> {
        match value {
            "caldav" => Ok(Self::CalDav),
            _ => Err(AccountError::UnsupportedProvider),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccountProfile {
    pub account_id: String,
    pub provider_kind: ProviderKind,
    pub user_label: String,
    pub credential_ref: String,
    pub region_profile: String,
    pub enabled: bool,
    pub credential_configured: bool,
    pub remote_editing_enabled: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl fmt::Debug for AccountProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountProfile")
            .field("account_id", &self.account_id)
            .field("provider_kind", &self.provider_kind)
            .field("user_label", &self.user_label)
            .field("credential_ref", &"<credential-ref>")
            .field("region_profile", &self.region_profile)
            .field("enabled", &self.enabled)
            .field("credential_configured", &self.credential_configured)
            .field("remote_editing_enabled", &self.remote_editing_enabled)
            .field("created_at_ms", &self.created_at_ms)
            .field("updated_at_ms", &self.updated_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RegisterAccountProfile {
    pub account_id: String,
    pub provider_kind: ProviderKind,
    pub user_label: String,
    pub credential_ref: String,
    pub region_profile: String,
    pub enabled: bool,
}

impl fmt::Debug for RegisterAccountProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisterAccountProfile")
            .field("account_id", &self.account_id)
            .field("provider_kind", &self.provider_kind)
            .field("user_label", &self.user_label)
            .field("credential_ref", &"<credential-ref>")
            .field("region_profile", &self.region_profile)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl RegisterAccountProfile {
    pub fn validate(mut self) -> Result<Self, AccountError> {
        let account_id = Uuid::parse_str(&self.account_id)
            .map_err(|_| AccountError::InvalidIdentity)?
            .to_string();
        if self.credential_ref != format!("{CREDENTIAL_PREFIX}{account_id}") {
            return Err(AccountError::InvalidCredentialReference);
        }
        self.user_label = validate_label(self.user_label)?;
        self.region_profile = validate_label(self.region_profile)?;
        self.account_id = account_id;
        Ok(self)
    }
}

pub fn validate_account_id(value: &str) -> Result<(), AccountError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| AccountError::InvalidIdentity)
}

fn validate_label(value: String) -> Result<String, AccountError> {
    let value = value.trim().to_owned();
    if value.is_empty() || value.chars().count() > MAX_LABEL_CHARS {
        return Err(AccountError::InvalidLabel);
    }
    Ok(value)
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account label is invalid")]
    InvalidLabel,
    #[error("account credential reference is invalid")]
    InvalidCredentialReference,
    #[error("account provider is unsupported")]
    UnsupportedProvider,
}
