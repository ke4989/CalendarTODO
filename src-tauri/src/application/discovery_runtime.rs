use crate::{
    adapters::persistence::{Database, DiscoveredCalendarRecord, PersistenceError},
    application::{
        account_registry::{AccountRegistryError, AccountSourceRegistry},
        source_factory::{AttachmentCancellation, SourceAttachError},
    },
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use thiserror::Error;

/// One bounded set of collection metadata observed for exactly one account.
///
/// A stage is produced by the provider boundary and is only allowed to reach
/// SQLite while its request is still the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCalendarStage {
    pub account_id: String,
    pub calendars: Vec<DiscoveredCalendarRecord>,
    pub observed_at_ms: i64,
    pub binding: Option<crate::adapters::icloud_caldav::ICloudBinding>,
}

/// Provider boundary for explicit discovery.
///
/// `collect` reads the account credential and requests collection metadata
/// through the reviewed executor. It must observe `cancellation`, must return
/// either a complete bounded snapshot or a bounded failure, and must never write
/// durable state: persistence belongs to the runtime that owns the request
/// generation.
pub trait CalendarDiscoverySource: Send + Sync + 'static {
    fn collect(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<DiscoveredCalendarStage, SourceAttachError>;
}

/// Connection-time discovery boundary used by the attachment worker.
///
/// The caller supplies the attachment cancellation token so discovery and
/// source construction form one cancellable, generation-guarded operation.
pub trait AccountCalendarDiscovery: Send + Sync + 'static {
    fn discover_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), DiscoveryError>;
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    #[error("discovery request was superseded or cancelled")]
    Superseded,
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account is disabled")]
    Disabled,
    #[error("credential action is required")]
    CredentialActionRequired,
    #[error("calendar provider is unsupported")]
    UnsupportedProvider,
    #[error("discovery configuration is unsafe")]
    UnsafeConfiguration,
    #[error("discovery is unavailable")]
    Unavailable,
}

impl From<SourceAttachError> for DiscoveryError {
    fn from(error: SourceAttachError) -> Self {
        match error {
            SourceAttachError::Cancelled => Self::Superseded,
            SourceAttachError::InvalidIdentity => Self::InvalidIdentity,
            SourceAttachError::AccountUnavailable => Self::Unavailable,
            SourceAttachError::Disabled => Self::Disabled,
            SourceAttachError::CredentialActionRequired => Self::CredentialActionRequired,
            SourceAttachError::UnsupportedProvider => Self::UnsupportedProvider,
            SourceAttachError::UnsafeConfiguration => Self::UnsafeConfiguration,
            SourceAttachError::Unavailable => Self::Unavailable,
        }
    }
}

struct Request {
    generation: u64,
    cancellation: AttachmentCancellation,
}

struct Inner {
    registry: Arc<AccountSourceRegistry>,
    source: Arc<dyn CalendarDiscoverySource>,
    database: Database,
    requests: Mutex<HashMap<String, Request>>,
    next_generation: AtomicU64,
    stopped: AtomicBool,
}

/// Owns per-account discovery request generations.
///
/// Explicit discovery is user-initiated, so it runs to completion on the
/// calling worker and reports a bounded result. What the runtime adds is the
/// lifecycle guard: a later request for the same account replaces the earlier
/// cancellation token, and a disable, removal, replacement, or shutdown trips
/// it. A result whose request is no longer current can neither write the
/// collection snapshot nor report success.
#[derive(Clone)]
pub struct RemoteDiscoveryRuntime {
    inner: Arc<Inner>,
}

impl RemoteDiscoveryRuntime {
    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        source: Arc<dyn CalendarDiscoverySource>,
        database: Database,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry,
                source,
                database,
                requests: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(1),
                stopped: AtomicBool::new(false),
            }),
        }
    }

    /// Run one explicit discovery for `account_id`.
    ///
    /// The call blocks until the snapshot is collected and promoted, or until
    /// it is superseded. Only a current request can reach SQLite.
    pub fn discover(&self, account_id: &str) -> Result<(), DiscoveryError> {
        self.discover_cancellable(account_id, &AttachmentCancellation::new())
    }

    pub fn discover_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), DiscoveryError> {
        let generation = self.begin(account_id, cancellation.clone())?;
        let stage = match self.inner.source.collect(account_id, cancellation) {
            Ok(stage) => stage,
            Err(error) => {
                self.finish(account_id, generation);
                return if cancellation.is_cancelled() {
                    Err(DiscoveryError::Superseded)
                } else {
                    Err(error.into())
                };
            }
        };
        self.commit(account_id, generation, cancellation, stage)
    }

    /// Cancel an in-flight discovery for one account. Disable, removal, and
    /// explicit cancellation share this path.
    pub fn cancel(&self, account_id: &str) {
        let cancelled = self
            .inner
            .requests
            .lock()
            .ok()
            .and_then(|mut requests| requests.remove(account_id));
        if let Some(request) = cancelled {
            request.cancellation.cancel();
        }
    }

    /// Cancel every in-flight discovery and reject later submissions.
    pub fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let remaining = self
            .inner
            .requests
            .lock()
            .map(|mut requests| requests.drain().map(|(_, request)| request).collect())
            .unwrap_or_else(|_| Vec::new());
        for request in remaining {
            request.cancellation.cancel();
        }
    }

    fn begin(
        &self,
        account_id: &str,
        cancellation: AttachmentCancellation,
    ) -> Result<u64, DiscoveryError> {
        let mut requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| DiscoveryError::Unavailable)?;
        if self.inner.stopped.load(Ordering::SeqCst) {
            return Err(DiscoveryError::Unavailable);
        }
        if cancellation.is_cancelled() {
            return Err(DiscoveryError::Superseded);
        }
        // A discovery must never start for a disabled, removed, or unknown
        // account, so the profile is validated before any request is admitted.
        let profile = self
            .inner
            .registry
            .profile(account_id)
            .map_err(map_registry_error)?;
        if !profile.enabled {
            return Err(DiscoveryError::Disabled);
        }
        if let Some(previous) = requests.remove(account_id) {
            previous.cancellation.cancel();
        }
        let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
        requests.insert(
            account_id.to_owned(),
            Request {
                generation,
                cancellation: cancellation.clone(),
            },
        );
        Ok(generation)
    }

    fn commit(
        &self,
        account_id: &str,
        generation: u64,
        cancellation: &AttachmentCancellation,
        stage: DiscoveredCalendarStage,
    ) -> Result<(), DiscoveryError> {
        // The write happens while the request registry is locked, so a
        // disable, removal, replacement, or shutdown either cancels the
        // request before this snapshot is promoted or is ordered after a
        // promotion that had already become current.
        let mut requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| DiscoveryError::Unavailable)?;
        let current = !self.inner.stopped.load(Ordering::SeqCst)
            && !cancellation.is_cancelled()
            && requests
                .get(account_id)
                .is_some_and(|request| request.generation == generation);
        if !current {
            return Err(DiscoveryError::Superseded);
        }
        if stage.account_id != account_id || stage.observed_at_ms < 0 {
            requests.remove(account_id);
            return Err(DiscoveryError::UnsafeConfiguration);
        }
        let result = if let Some(binding) = stage.binding {
            self.inner.database.replace_icloud_discovery(
                account_id.to_owned(),
                stage.calendars,
                binding,
                stage.observed_at_ms,
            )
        } else {
            self.inner.database.replace_discovered_calendars(
                account_id.to_owned(),
                stage.calendars,
                stage.observed_at_ms,
            )
        };
        requests.remove(account_id);
        result.map_err(map_persistence_error)
    }

    fn finish(&self, account_id: &str, generation: u64) {
        if let Ok(mut requests) = self.inner.requests.lock()
            && requests
                .get(account_id)
                .is_some_and(|request| request.generation == generation)
        {
            requests.remove(account_id);
        }
    }
}

impl AccountCalendarDiscovery for RemoteDiscoveryRuntime {
    fn discover_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), DiscoveryError> {
        RemoteDiscoveryRuntime::discover_cancellable(self, account_id, cancellation)
    }
}

fn map_registry_error(error: AccountRegistryError) -> DiscoveryError {
    match error {
        AccountRegistryError::Cancelled => DiscoveryError::Superseded,
        AccountRegistryError::InvalidIdentity | AccountRegistryError::SourceUnavailable => {
            DiscoveryError::InvalidIdentity
        }
        AccountRegistryError::Disabled => DiscoveryError::Disabled,
        AccountRegistryError::IdentityMismatch => DiscoveryError::UnsafeConfiguration,
        AccountRegistryError::Unavailable => DiscoveryError::Unavailable,
    }
}

fn map_persistence_error(error: PersistenceError) -> DiscoveryError {
    match error {
        PersistenceError::NotFound | PersistenceError::Account(_) => {
            DiscoveryError::InvalidIdentity
        }
        PersistenceError::AccountDisabled => DiscoveryError::Disabled,
        PersistenceError::IdentityConflict => DiscoveryError::UnsafeConfiguration,
        _ => DiscoveryError::Unavailable,
    }
}
