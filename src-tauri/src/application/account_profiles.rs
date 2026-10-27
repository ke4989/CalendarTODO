use crate::{
    application::account_registry::{
        AccountRegistryError, AccountSourceRegistry, RegisteredAccount,
    },
    application::credentials::{CredentialService, CredentialServiceError},
    application::source_factory::{AttachmentCancellation, SourceAttachError, SourceAttacher},
    domain::account::{ProviderKind, RegisterAccountProfile},
    ports::credentials::{CredentialRecord, CredentialRef, SecretBytes},
};
use serde::{Deserialize, Serialize};
use std::{fmt, mem, sync::Arc};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRuntimeAccount {
    pub user_label: String,
    pub region_profile: String,
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollAccountCredential {
    pub account_id: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceAttachmentMode {
    Synthetic,
    IcloudChinaReadOnly,
}

impl fmt::Debug for EnrollAccountCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EnrollAccountCredential([REDACTED])")
    }
}

impl Drop for EnrollAccountCredential {
    fn drop(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeAccountStatus {
    Disabled,
    ActionRequired,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountActionRequired {
    CredentialSetup,
    SourceAttachment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAccountView {
    pub account_id: String,
    pub provider_kind: &'static str,
    pub user_label: String,
    pub region_profile: String,
    pub enabled: bool,
    pub source_attached: bool,
    pub credential_configured: bool,
    pub remote_editing_enabled: bool,
    pub status: RuntimeAccountStatus,
    pub action_required: Option<AccountActionRequired>,
}

#[derive(Clone)]
pub struct AccountProfileService {
    registry: Arc<AccountSourceRegistry>,
    credentials: CredentialService,
    synthetic_attacher: Arc<dyn SourceAttacher>,
    production_attacher: Arc<dyn SourceAttacher>,
}

impl AccountProfileService {
    pub(crate) fn belongs_to_registry(&self, registry: &Arc<AccountSourceRegistry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }
    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        credentials: CredentialService,
        synthetic_attacher: Arc<dyn SourceAttacher>,
        production_attacher: Arc<dyn SourceAttacher>,
    ) -> Self {
        Self {
            registry,
            credentials,
            synthetic_attacher,
            production_attacher,
        }
    }

    pub fn list(&self) -> Result<Vec<RuntimeAccountView>, AccountProfileServiceError> {
        self.registry
            .list()?
            .into_iter()
            .map(runtime_view)
            .collect()
    }

    pub fn register(
        &self,
        input: RegisterRuntimeAccount,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        let account_id = Uuid::new_v4().to_string();
        let profile = self.registry.register_profile(RegisterAccountProfile {
            credential_ref: format!("calendar-todo/account/{account_id}"),
            account_id,
            provider_kind: ProviderKind::CalDav,
            user_label: input.user_label,
            region_profile: input.region_profile,
            enabled: input.enabled,
        })?;
        runtime_view(RegisteredAccount {
            profile,
            source_attached: false,
        })
    }

    pub fn set_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        self.registry.set_enabled(account_id, enabled)?;
        self.registry
            .list()?
            .into_iter()
            .find(|account| account.profile.account_id == account_id)
            .ok_or(AccountProfileServiceError::Unavailable)
            .and_then(runtime_view)
    }

    pub fn set_remote_editing_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        self.registry
            .set_remote_editing_enabled(account_id, enabled)?;
        self.registry
            .list()?
            .into_iter()
            .find(|account| account.profile.account_id == account_id)
            .ok_or(AccountProfileServiceError::Unavailable)
            .and_then(runtime_view)
    }

    pub fn enroll_credential(
        &self,
        mut input: EnrollAccountCredential,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        let profile = self.registry.profile(&input.account_id)?;
        let reference = CredentialRef::parse(&profile.credential_ref)
            .map_err(|_| AccountProfileServiceError::Validation)?;
        let username = mem::take(&mut input.username);
        let password = mem::take(&mut input.password).into_bytes();
        let credential = CredentialRecord::new(username, SecretBytes::new(password))
            .map_err(|_| AccountProfileServiceError::Validation)?;
        self.credentials.write(&reference, &credential)?;
        self.registry
            .mark_credential_configured(&input.account_id)?;
        self.registry
            .list()?
            .into_iter()
            .find(|account| account.profile.account_id == input.account_id)
            .ok_or(AccountProfileServiceError::Unavailable)
            .and_then(runtime_view)
    }

    pub fn remove(&self, account_id: &str) -> Result<(), AccountProfileServiceError> {
        let profile = self.registry.profile(account_id)?;
        let reference = CredentialRef::parse(&profile.credential_ref)
            .map_err(|_| AccountProfileServiceError::Validation)?;
        match self.credentials.delete(&reference) {
            Ok(()) | Err(CredentialServiceError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
        self.registry.archive_profile(account_id)?;
        Ok(())
    }

    pub fn attach_source(
        &self,
        account_id: &str,
        mode: SourceAttachmentMode,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        self.attach_source_cancellable(account_id, mode, &AttachmentCancellation::new())
    }

    pub fn validate_attachment(&self, account_id: &str) -> Result<(), AccountProfileServiceError> {
        let profile = self.registry.profile(account_id)?;
        if !profile.enabled {
            return Err(AccountProfileServiceError::Disabled);
        }
        if !profile.credential_configured {
            return Err(AccountProfileServiceError::CredentialSetupRequired);
        }
        Ok(())
    }

    pub fn attach_source_cancellable(
        &self,
        account_id: &str,
        mode: SourceAttachmentMode,
        cancellation: &AttachmentCancellation,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        self.validate_attachment(account_id)?;
        match mode {
            SourceAttachmentMode::Synthetic => self
                .synthetic_attacher
                .attach_cancellable(account_id, cancellation)?,
            SourceAttachmentMode::IcloudChinaReadOnly => self
                .production_attacher
                .attach_cancellable(account_id, cancellation)?,
        }
        self.registry
            .list()?
            .into_iter()
            .find(|account| account.profile.account_id == account_id)
            .ok_or(AccountProfileServiceError::Unavailable)
            .and_then(runtime_view)
    }
}

fn runtime_view(
    account: RegisteredAccount,
) -> Result<RuntimeAccountView, AccountProfileServiceError> {
    let (status, action_required) = if !account.profile.enabled {
        (RuntimeAccountStatus::Disabled, None)
    } else if account.source_attached {
        (RuntimeAccountStatus::Ready, None)
    } else if account.profile.credential_configured {
        (
            RuntimeAccountStatus::ActionRequired,
            Some(AccountActionRequired::SourceAttachment),
        )
    } else {
        (
            RuntimeAccountStatus::ActionRequired,
            Some(AccountActionRequired::CredentialSetup),
        )
    };
    Ok(RuntimeAccountView {
        account_id: account.profile.account_id,
        provider_kind: account.profile.provider_kind.as_str(),
        user_label: account.profile.user_label,
        region_profile: account.profile.region_profile,
        enabled: account.profile.enabled,
        source_attached: account.source_attached,
        credential_configured: account.profile.credential_configured,
        remote_editing_enabled: account.profile.remote_editing_enabled,
        status,
        action_required,
    })
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AccountProfileServiceError {
    #[error("account profile input is invalid")]
    Validation,
    #[error("account is disabled")]
    Disabled,
    #[error("credential setup is required")]
    CredentialSetupRequired,
    #[error("credential action is required")]
    CredentialActionRequired,
    #[error("account profile service is unavailable")]
    Unavailable,
}

impl From<SourceAttachError> for AccountProfileServiceError {
    fn from(error: SourceAttachError) -> Self {
        match error {
            SourceAttachError::Cancelled => Self::Unavailable,
            SourceAttachError::InvalidIdentity | SourceAttachError::UnsafeConfiguration => {
                Self::Validation
            }
            SourceAttachError::Disabled => Self::Disabled,
            SourceAttachError::CredentialActionRequired => Self::CredentialActionRequired,
            SourceAttachError::AccountUnavailable
            | SourceAttachError::UnsupportedProvider
            | SourceAttachError::Unavailable => Self::Unavailable,
        }
    }
}

impl From<AccountRegistryError> for AccountProfileServiceError {
    fn from(error: AccountRegistryError) -> Self {
        match error {
            AccountRegistryError::InvalidIdentity | AccountRegistryError::IdentityMismatch => {
                Self::Validation
            }
            AccountRegistryError::Cancelled
            | AccountRegistryError::Disabled
            | AccountRegistryError::SourceUnavailable
            | AccountRegistryError::Unavailable => Self::Unavailable,
        }
    }
}

impl From<CredentialServiceError> for AccountProfileServiceError {
    fn from(error: CredentialServiceError) -> Self {
        match error {
            CredentialServiceError::InvalidInput => Self::Validation,
            CredentialServiceError::NotFound | CredentialServiceError::Unavailable => {
                Self::Unavailable
            }
        }
    }
}
