use crate::{
    adapters::persistence::{Database, PersistenceError, SyncCapture},
    domain::calendar_sync::{
        AccountDeletionReconciliation, CachedCalendarResource, CalendarSyncError, SyncFailureClass,
        SyncOutcome, SyncRunSummary, SyncTrigger,
    },
    ports::caldav::{ProviderReadError, ReadOnlyCalendarSource},
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone)]
pub struct ReadOnlySyncService {
    database: Database,
}

impl ReadOnlySyncService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn list_cached_resources(&self) -> Result<Vec<CachedCalendarResource>, ReadOnlySyncError> {
        Ok(self.database.list_cached_calendar_resources()?)
    }

    pub fn reconcile_deletions(
        &self,
        capture: SyncCapture,
        reconciliation: AccountDeletionReconciliation,
    ) -> Result<Vec<CachedCalendarResource>, ReadOnlySyncError> {
        self.database
            .reconcile_deletion_evidence(capture, reconciliation.validate()?)?;
        Ok(self.database.list_cached_calendar_resources()?)
    }

    pub fn run_full_inventory(
        &self,
        source: &dyn ReadOnlyCalendarSource,
        trigger: SyncTrigger,
    ) -> Result<Vec<CachedCalendarResource>, ReadOnlySyncError> {
        self.run_full_inventory_with_stale_retry(source, trigger, true)
    }

    fn run_full_inventory_with_stale_retry(
        &self,
        source: &dyn ReadOnlyCalendarSource,
        trigger: SyncTrigger,
        retry_stale_capture: bool,
    ) -> Result<Vec<CachedCalendarResource>, ReadOnlySyncError> {
        let account_id = source.account_id().to_owned();
        Uuid::parse_str(&account_id).map_err(|_| CalendarSyncError::InvalidIdentity)?;
        if !self.should_read_calendar_scope(&account_id)? {
            self.database.record_sync_run(
                account_id,
                trigger,
                SyncOutcome::Success,
                SyncFailureClass::None,
                0,
                0,
            )?;
            return Ok(self.database.list_cached_calendar_resources()?);
        }
        let capture = self.database.begin_sync_capture(account_id.clone())?;
        let stage = match source.collect_full_inventory() {
            Ok(stage) => stage,
            Err(error) => {
                self.record_failure(&account_id, trigger, error.failure_class())?;
                return Err(ReadOnlySyncError::Provider(error));
            }
        };
        let stage = match stage.validate() {
            Ok(stage) if stage.account_id == account_id => stage,
            Ok(_) | Err(_) => {
                self.record_failure(&account_id, trigger, SyncFailureClass::Validation)?;
                return Err(ReadOnlySyncError::Validation(
                    CalendarSyncError::InvalidIdentity,
                ));
            }
        };
        let calendar_count =
            u32::try_from(stage.calendars.len()).map_err(|_| CalendarSyncError::InvalidIdentity)?;
        let resource_count = u32::try_from(
            stage
                .calendars
                .iter()
                .map(|calendar| calendar.resources.len())
                .sum::<usize>(),
        )
        .map_err(|_| CalendarSyncError::InvalidIdentity)?;
        if let Err(error) = self
            .database
            .promote_captured_account_inventory(capture, stage)
        {
            if retry_stale_capture && matches!(error, PersistenceError::StaleSyncCapture) {
                return self.run_full_inventory_with_stale_retry(source, trigger, false);
            }
            self.record_failure(&account_id, trigger, persistence_failure_class(&error))?;
            return Err(ReadOnlySyncError::Persistence(error));
        }
        self.database.record_sync_run(
            account_id,
            trigger,
            SyncOutcome::Success,
            SyncFailureClass::None,
            calendar_count,
            resource_count,
        )?;
        Ok(self.database.list_cached_calendar_resources()?)
    }

    pub fn run_incremental_or_full(
        &self,
        source: &dyn ReadOnlyCalendarSource,
        trigger: SyncTrigger,
    ) -> Result<IncrementalSyncResult, ReadOnlySyncError> {
        self.run_incremental_or_full_with_stale_retry(source, trigger, true)
    }

    fn run_incremental_or_full_with_stale_retry(
        &self,
        source: &dyn ReadOnlyCalendarSource,
        trigger: SyncTrigger,
        retry_stale_capture: bool,
    ) -> Result<IncrementalSyncResult, ReadOnlySyncError> {
        let account_id = source.account_id().to_owned();
        Uuid::parse_str(&account_id).map_err(|_| CalendarSyncError::InvalidIdentity)?;
        if !self.should_read_calendar_scope(&account_id)? {
            self.database.record_sync_run(
                account_id,
                trigger,
                SyncOutcome::Success,
                SyncFailureClass::None,
                0,
                0,
            )?;
            return Ok(IncrementalSyncResult {
                path: IncrementalSyncPath::Incremental,
                resources: self.database.list_cached_calendar_resources()?,
            });
        }
        let (capture, requests) = self
            .database
            .begin_incremental_sync_capture(account_id.clone())?;
        if requests.is_empty() {
            return Ok(IncrementalSyncResult {
                path: IncrementalSyncPath::FullInventoryFallback,
                resources: self.run_full_inventory_with_stale_retry(
                    source,
                    trigger,
                    retry_stale_capture,
                )?,
            });
        }
        for request in &requests {
            request.validate()?;
        }
        let stage = match source.collect_incremental(&requests) {
            Ok(stage) => stage,
            Err(ProviderReadError::InvalidSyncToken) => {
                return Ok(IncrementalSyncResult {
                    path: IncrementalSyncPath::FullInventoryFallback,
                    resources: self.run_full_inventory_with_stale_retry(
                        source,
                        trigger,
                        retry_stale_capture,
                    )?,
                });
            }
            Err(error) => {
                self.record_failure(&account_id, trigger, error.failure_class())?;
                return Err(ReadOnlySyncError::Provider(error));
            }
        };
        let stage = match stage.validate() {
            Ok(stage) if stage.account_id == account_id => stage,
            Ok(_) | Err(_) => {
                self.record_failure(&account_id, trigger, SyncFailureClass::Validation)?;
                return Err(ReadOnlySyncError::Validation(
                    CalendarSyncError::InvalidIdentity,
                ));
            }
        };
        let calendar_count =
            u32::try_from(stage.calendars.len()).map_err(|_| CalendarSyncError::InvalidIdentity)?;
        let resource_count = u32::try_from(
            stage
                .calendars
                .iter()
                .map(|calendar| calendar.changed_resources.len() + calendar.removed_hrefs.len())
                .sum::<usize>(),
        )
        .map_err(|_| CalendarSyncError::InvalidIdentity)?;
        if let Err(error) = self
            .database
            .promote_captured_account_incremental(capture, stage)
        {
            if retry_stale_capture && matches!(error, PersistenceError::StaleSyncCapture) {
                return self.run_incremental_or_full_with_stale_retry(source, trigger, false);
            }
            self.record_failure(&account_id, trigger, persistence_failure_class(&error))?;
            return Err(ReadOnlySyncError::Persistence(error));
        }
        self.database.record_sync_run(
            account_id,
            trigger,
            SyncOutcome::Success,
            SyncFailureClass::None,
            calendar_count,
            resource_count,
        )?;
        Ok(IncrementalSyncResult {
            path: IncrementalSyncPath::Incremental,
            resources: self.database.list_cached_calendar_resources()?,
        })
    }

    pub fn list_sync_runs(&self) -> Result<Vec<SyncRunSummary>, ReadOnlySyncError> {
        Ok(self.database.list_sync_runs()?)
    }

    fn record_failure(
        &self,
        account_id: &str,
        trigger: SyncTrigger,
        failure_class: SyncFailureClass,
    ) -> Result<(), PersistenceError> {
        self.database.record_sync_run(
            account_id.to_owned(),
            trigger,
            SyncOutcome::Failure,
            failure_class,
            0,
            0,
        )?;
        Ok(())
    }

    fn should_read_calendar_scope(&self, account_id: &str) -> Result<bool, PersistenceError> {
        let calendars = self
            .database
            .list_calendar_settings()?
            .into_iter()
            .filter(|calendar| calendar.account_id == account_id)
            .collect::<Vec<_>>();
        Ok(calendars.is_empty()
            || calendars
                .iter()
                .any(|calendar| calendar.available && calendar.visible))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrementalSyncPath {
    Incremental,
    FullInventoryFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalSyncResult {
    pub path: IncrementalSyncPath,
    pub resources: Vec<CachedCalendarResource>,
}

#[derive(Debug, Error)]
pub enum ReadOnlySyncError {
    #[error(transparent)]
    Provider(#[from] ProviderReadError),
    #[error(transparent)]
    Validation(#[from] CalendarSyncError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

impl ReadOnlySyncError {
    pub fn failure_class(&self) -> SyncFailureClass {
        match self {
            Self::Provider(error) => error.failure_class(),
            Self::Validation(_) => SyncFailureClass::Validation,
            Self::Persistence(error) => persistence_failure_class(error),
        }
    }
}

fn persistence_failure_class(error: &PersistenceError) -> SyncFailureClass {
    match error {
        PersistenceError::UnsafeSyncStage => SyncFailureClass::UnsafeData,
        PersistenceError::StaleSyncCapture
        | PersistenceError::SyncTokenConflict
        | PersistenceError::IdentityConflict
        | PersistenceError::AccountDisabled => SyncFailureClass::Validation,
        _ => SyncFailureClass::Persistence,
    }
}
