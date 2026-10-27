use crate::{
    application::{
        account_registry::{AccountRegistryError, AccountSourceRegistry},
        credentials::{CredentialService, CredentialServiceError},
    },
    ports::{
        caldav::{CalDavSourceFactory, SourceFactoryError},
        credentials::CredentialRef,
    },
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use thiserror::Error;

/// Resolves durable non-secret profile metadata and a short-lived credential,
/// delegates provider construction, then hands lifecycle ownership to the
/// account registry. Credential material is dropped before source attachment.
pub struct SourceFactoryOrchestrator {
    registry: Arc<AccountSourceRegistry>,
    credentials: CredentialService,
    factory: Arc<dyn CalDavSourceFactory>,
}

pub trait SourceAttacher: Send + Sync {
    fn attach(&self, account_id: &str) -> Result<(), SourceAttachError>;
    fn attach_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError>;
}

#[derive(Clone, Default)]
pub struct AttachmentCancellation {
    inner: Arc<AttachmentCancellationState>,
}

#[derive(Default)]
struct AttachmentCancellationState {
    cancelled: AtomicBool,
    interrupt: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl AttachmentCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut interrupt_slot) = self.inner.interrupt.lock()
            && let Some(interrupt) = interrupt_slot.take()
        {
            interrupt();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub fn register_interrupt(&self, interrupt: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut interrupt_slot) = self.inner.interrupt.lock() {
            if self.is_cancelled() {
                interrupt();
            } else {
                *interrupt_slot = Some(interrupt);
            }
        }
    }

    pub fn run_if_active<T>(
        &self,
        action: impl FnOnce() -> Result<T, SourceAttachError>,
    ) -> Result<T, SourceAttachError> {
        if self.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        action()
    }
}

impl SourceFactoryOrchestrator {
    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        credentials: CredentialService,
        factory: Arc<dyn CalDavSourceFactory>,
    ) -> Self {
        Self {
            registry,
            credentials,
            factory,
        }
    }

    pub fn build_and_attach(&self, account_id: &str) -> Result<(), SourceAttachError> {
        self.build_and_attach_cancellable(account_id, &AttachmentCancellation::new())
    }

    pub fn build_and_attach_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let profile = self
            .registry
            .profile(account_id)
            .map_err(map_profile_error)?;
        if !profile.enabled {
            return Err(SourceAttachError::Disabled);
        }
        if !profile.credential_configured {
            return Err(SourceAttachError::CredentialActionRequired);
        }
        let reference = CredentialRef::parse(&profile.credential_ref)
            .map_err(|_| SourceAttachError::UnsafeConfiguration)?;
        let credential = self
            .credentials
            .read(&reference)
            .map_err(SourceAttachError::from)?;
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let source = self
            .factory
            .build_cancellable(&profile, &credential, cancellation)
            .map_err(SourceAttachError::from)?;
        drop(credential);
        self.registry
            .attach_source_guarded(account_id, source, cancellation)
            .map_err(SourceAttachError::from)
    }
}

impl SourceAttacher for SourceFactoryOrchestrator {
    fn attach(&self, account_id: &str) -> Result<(), SourceAttachError> {
        self.build_and_attach(account_id)
    }

    fn attach_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        self.build_and_attach_cancellable(account_id, cancellation)
    }
}

fn map_profile_error(error: AccountRegistryError) -> SourceAttachError {
    match error {
        AccountRegistryError::InvalidIdentity => SourceAttachError::InvalidIdentity,
        AccountRegistryError::Disabled => SourceAttachError::Disabled,
        AccountRegistryError::Cancelled => SourceAttachError::Cancelled,
        AccountRegistryError::SourceUnavailable => SourceAttachError::AccountUnavailable,
        AccountRegistryError::IdentityMismatch => SourceAttachError::UnsafeConfiguration,
        AccountRegistryError::Unavailable => SourceAttachError::Unavailable,
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SourceAttachError {
    #[error("source attachment was cancelled")]
    Cancelled,
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account is unavailable")]
    AccountUnavailable,
    #[error("account is disabled")]
    Disabled,
    #[error("credential action is required")]
    CredentialActionRequired,
    #[error("calendar provider is unsupported")]
    UnsupportedProvider,
    #[error("source configuration is unsafe")]
    UnsafeConfiguration,
    #[error("source attachment is unavailable")]
    Unavailable,
}

impl From<CredentialServiceError> for SourceAttachError {
    fn from(error: CredentialServiceError) -> Self {
        match error {
            CredentialServiceError::NotFound => Self::CredentialActionRequired,
            CredentialServiceError::InvalidInput => Self::UnsafeConfiguration,
            CredentialServiceError::Unavailable => Self::Unavailable,
        }
    }
}

impl From<SourceFactoryError> for SourceAttachError {
    fn from(error: SourceFactoryError) -> Self {
        match error {
            SourceFactoryError::Unauthorized => Self::CredentialActionRequired,
            SourceFactoryError::Unsupported => Self::UnsupportedProvider,
            SourceFactoryError::UnsafeConfiguration => Self::UnsafeConfiguration,
            SourceFactoryError::Unavailable => Self::Unavailable,
        }
    }
}

impl From<AccountRegistryError> for SourceAttachError {
    fn from(error: AccountRegistryError) -> Self {
        match error {
            AccountRegistryError::InvalidIdentity => Self::InvalidIdentity,
            AccountRegistryError::Disabled => Self::Disabled,
            AccountRegistryError::Cancelled => Self::Cancelled,
            AccountRegistryError::IdentityMismatch => Self::UnsafeConfiguration,
            AccountRegistryError::SourceUnavailable | AccountRegistryError::Unavailable => {
                Self::Unavailable
            }
        }
    }
}
