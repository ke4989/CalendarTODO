use crate::domain::calendar_sync::{
    AccountIncrementalStage, AccountInventoryStage, IncrementalSyncRequest,
};
use crate::{domain::account::AccountProfile, ports::credentials::CredentialRecord};
use std::sync::Arc;
use thiserror::Error;

/// Provider-neutral read boundary. Concrete adapters own transport, regional
/// behavior, redirects, and credential access; callers receive only a staged
/// inventory that still requires domain validation before promotion.
pub trait ReadOnlyCalendarSource {
    fn account_id(&self) -> &str;
    fn collect_full_inventory(&self) -> Result<AccountInventoryStage, ProviderReadError>;
    fn collect_incremental(
        &self,
        requests: &[IncrementalSyncRequest],
    ) -> Result<AccountIncrementalStage, ProviderReadError>;

    /// Best-effort cancellation of an in-flight read. Returns `true` when a
    /// cancellation was newly requested. Sources that cannot interrupt a blocked
    /// exchange keep the default no-op behavior and stay bounded by their own
    /// request deadline.
    fn cancel_in_flight(&self) -> bool {
        false
    }
}

/// Construction boundary for a credential-backed CalDAV source. Network and
/// regional behavior belongs behind this port; application orchestration
/// supplies only validated local metadata and a short-lived borrowed secret.
pub trait CalDavSourceFactory: Send + Sync {
    fn build(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError>;

    fn build_cancellable(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
        _cancellation: &crate::application::source_factory::AttachmentCancellation,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError> {
        self.build(profile, credential)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SourceFactoryError {
    #[error("calendar source requires credential action")]
    Unauthorized,
    #[error("calendar source provider is unsupported")]
    Unsupported,
    #[error("calendar source is unavailable")]
    Unavailable,
    #[error("calendar source configuration is unsafe")]
    UnsafeConfiguration,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ProviderReadError {
    #[error("calendar provider is temporarily unavailable")]
    Transient,
    #[error("calendar provider requires credential action")]
    Unauthorized,
    #[error("calendar provider returned an incomplete response")]
    Incomplete,
    #[error("calendar provider returned unsupported or malformed data")]
    UnsafeData,
    #[error("calendar provider rejected the committed sync token")]
    InvalidSyncToken,
}

impl ProviderReadError {
    pub fn failure_class(self) -> crate::domain::calendar_sync::SyncFailureClass {
        use crate::domain::calendar_sync::SyncFailureClass;
        match self {
            Self::Transient => SyncFailureClass::Transient,
            Self::Unauthorized => SyncFailureClass::Unauthorized,
            Self::Incomplete => SyncFailureClass::Incomplete,
            Self::UnsafeData => SyncFailureClass::UnsafeData,
            Self::InvalidSyncToken => SyncFailureClass::Incomplete,
        }
    }
}
