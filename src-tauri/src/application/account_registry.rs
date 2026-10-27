use crate::{
    adapters::persistence::{Database, PersistenceError},
    application::{
        production_mutation_capability::{
            ConditionalTitleUpdateEvidence, ConditionalTitleUpdateTarget,
        },
        sync::ReadOnlySyncService,
        sync_coordinator::{AccountSyncCoordinator, CoordinatorError, CoordinatorReport},
    },
    domain::{
        account::{AccountProfile, RegisterAccountProfile, validate_account_id},
        calendar_sync::SyncTrigger,
    },
    ports::caldav::ReadOnlyCalendarSource,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::Receiver,
    },
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredAccount {
    pub profile: AccountProfile,
    pub source_attached: bool,
}

pub struct AccountSourceRegistry {
    database: Database,
    owner: Arc<()>,
    coordinators: Mutex<HashMap<String, CoordinatorRegistration>>,
    next_attachment_generation: AtomicU64,
}

struct CoordinatorRegistration {
    generation: u64,
    coordinator: AccountSyncCoordinator,
}

pub struct TriggeredSync {
    pub attachment_generation: u64,
    pub report: Receiver<CoordinatorReport>,
}

impl AccountSourceRegistry {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            owner: Arc::new(()),
            coordinators: Mutex::new(HashMap::new()),
            next_attachment_generation: AtomicU64::new(1),
        }
    }

    pub(crate) fn creation_owner_provenance(&self) -> (std::sync::Weak<()>, std::sync::Weak<()>) {
        (
            Arc::downgrade(&self.owner),
            self.database.actor_provenance(),
        )
    }
    pub(crate) fn creation_reservation_snapshot(
        &self,
        account: &str,
        operation: &str,
    ) -> Result<
        crate::application::production_creation_owner::CreationReservationSnapshot,
        AccountRegistryError,
    > {
        self.database
            .creation_reservation_snapshot(account.to_owned(), operation.to_owned())
            .map_err(Into::into)
    }
    pub fn register_profile(
        &self,
        input: RegisterAccountProfile,
    ) -> Result<AccountProfile, AccountRegistryError> {
        let mut coordinators = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?;
        let profile = self
            .database
            .upsert_account_profile(input)
            .map_err(AccountRegistryError::from)?;
        if !profile.enabled
            && let Some(coordinator) = coordinators.remove(&profile.account_id)
        {
            coordinator
                .coordinator
                .shutdown()
                .map_err(|_| AccountRegistryError::Unavailable)?;
        }
        Ok(profile)
    }

    pub fn list(&self) -> Result<Vec<RegisteredAccount>, AccountRegistryError> {
        let attached = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        Ok(self
            .database
            .list_account_profiles()
            .map_err(AccountRegistryError::from)?
            .into_iter()
            .map(|profile| RegisteredAccount {
                source_attached: attached.contains(&profile.account_id),
                profile,
            })
            .collect())
    }

    pub fn profile(&self, account_id: &str) -> Result<AccountProfile, AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        self.database
            .account_profile(account_id.to_owned())
            .map_err(AccountRegistryError::from)
    }

    pub(crate) fn conditional_title_update_evidence(
        &self,
        target: ConditionalTitleUpdateTarget,
    ) -> Result<ConditionalTitleUpdateEvidence, AccountRegistryError> {
        self.database
            .conditional_title_update_evidence(target)
            .map_err(AccountRegistryError::from)
    }

    pub fn attach_source(
        &self,
        account_id: &str,
        source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
    ) -> Result<(), AccountRegistryError> {
        self.attach_source_inner(account_id, source, None)
    }

    pub fn attach_source_guarded(
        &self,
        account_id: &str,
        source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
        cancellation: &crate::application::source_factory::AttachmentCancellation,
    ) -> Result<(), AccountRegistryError> {
        self.attach_source_inner(account_id, source, Some(cancellation))
    }

    fn attach_source_inner(
        &self,
        account_id: &str,
        source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
        cancellation: Option<&crate::application::source_factory::AttachmentCancellation>,
    ) -> Result<(), AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        if source.account_id() != account_id {
            return Err(AccountRegistryError::IdentityMismatch);
        }
        let mut coordinators = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?;
        let profile = self
            .database
            .account_profile(account_id.to_owned())
            .map_err(AccountRegistryError::from)?;
        if !profile.enabled {
            return Err(AccountRegistryError::Disabled);
        }
        if cancellation
            .is_some_and(crate::application::source_factory::AttachmentCancellation::is_cancelled)
        {
            return Err(AccountRegistryError::Cancelled);
        }
        if let Some(previous) = coordinators.remove(account_id) {
            previous
                .coordinator
                .shutdown()
                .map_err(|_| AccountRegistryError::Unavailable)?;
        }
        let coordinator =
            AccountSyncCoordinator::new(ReadOnlySyncService::new(self.database.clone()), source)
                .map_err(AccountRegistryError::from)?;
        let generation = self
            .next_attachment_generation
            .fetch_add(1, Ordering::SeqCst);
        coordinators.insert(
            account_id.to_owned(),
            CoordinatorRegistration {
                generation,
                coordinator,
            },
        );
        Ok(())
    }

    pub fn detach_source(&self, account_id: &str) -> Result<(), AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        let coordinator = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?
            .remove(account_id);
        if let Some(coordinator) = coordinator {
            coordinator
                .coordinator
                .shutdown()
                .map_err(|_| AccountRegistryError::Unavailable)?;
        }
        Ok(())
    }

    pub fn archive_profile(&self, account_id: &str) -> Result<(), AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        self.detach_source(account_id)?;
        self.database
            .archive_account_profile(account_id.to_owned())
            .map_err(Into::into)
    }

    pub fn set_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<AccountProfile, AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        let mut coordinators = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?;
        let profile = self
            .database
            .set_account_enabled(account_id.to_owned(), enabled)
            .map_err(AccountRegistryError::from)?;
        if !enabled && let Some(coordinator) = coordinators.remove(account_id) {
            coordinator
                .coordinator
                .shutdown()
                .map_err(|_| AccountRegistryError::Unavailable)?;
        }
        Ok(profile)
    }

    pub fn mark_credential_configured(
        &self,
        account_id: &str,
    ) -> Result<AccountProfile, AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        self.database
            .set_account_credential_configured(account_id.to_owned())
            .map_err(AccountRegistryError::from)
    }

    pub fn set_remote_editing_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<AccountProfile, AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        self.database
            .set_account_remote_editing_enabled(account_id.to_owned(), enabled)
            .map_err(AccountRegistryError::from)
    }

    pub fn trigger(
        &self,
        account_id: &str,
        trigger: SyncTrigger,
    ) -> Result<Receiver<CoordinatorReport>, AccountRegistryError> {
        Ok(self.trigger_versioned(account_id, trigger)?.report)
    }

    pub fn trigger_versioned(
        &self,
        account_id: &str,
        trigger: SyncTrigger,
    ) -> Result<TriggeredSync, AccountRegistryError> {
        self.trigger_versioned_expected(account_id, trigger, None)
    }

    /// Check captured ownership atomically with enqueueing the coordinator
    /// request. An old continuation must never trigger a successor source.
    pub(crate) fn trigger_versioned_expected(
        &self,
        account_id: &str,
        trigger: SyncTrigger,
        expected_generation: Option<u64>,
    ) -> Result<TriggeredSync, AccountRegistryError> {
        validate_account_id(account_id).map_err(|_| AccountRegistryError::InvalidIdentity)?;
        let profile = self
            .database
            .account_profile(account_id.to_owned())
            .map_err(AccountRegistryError::from)?;
        if !profile.enabled {
            return Err(AccountRegistryError::Disabled);
        }
        let coordinators = self
            .coordinators
            .lock()
            .map_err(|_| AccountRegistryError::Unavailable)?;
        let registration = coordinators
            .get(account_id)
            .ok_or(AccountRegistryError::SourceUnavailable)?;
        if expected_generation.is_some_and(|generation| generation != registration.generation) {
            return Err(AccountRegistryError::Cancelled);
        }
        Ok(TriggeredSync {
            attachment_generation: registration.generation,
            report: registration.coordinator.request(trigger)?,
        })
    }

    pub fn is_current_attachment(&self, account_id: &str, generation: u64) -> bool {
        self.coordinators.lock().is_ok_and(|coordinators| {
            coordinators
                .get(account_id)
                .is_some_and(|registration| registration.generation == generation)
        })
    }

    pub fn attachment_generation(&self, account_id: &str) -> Option<u64> {
        self.coordinators.lock().ok().and_then(|coordinators| {
            coordinators
                .get(account_id)
                .map(|registration| registration.generation)
        })
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AccountRegistryError {
    #[error("account attachment was cancelled")]
    Cancelled,
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account source identity does not match the registered account")]
    IdentityMismatch,
    #[error("account is disabled")]
    Disabled,
    #[error("account source is not attached")]
    SourceUnavailable,
    #[error("account registry is unavailable")]
    Unavailable,
}

impl From<PersistenceError> for AccountRegistryError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::NotFound => Self::SourceUnavailable,
            PersistenceError::AccountDisabled => Self::Disabled,
            PersistenceError::Account(_) => Self::InvalidIdentity,
            _ => Self::Unavailable,
        }
    }
}

impl From<CoordinatorError> for AccountRegistryError {
    fn from(_: CoordinatorError) -> Self {
        Self::Unavailable
    }
}
