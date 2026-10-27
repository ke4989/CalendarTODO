//! Explicit independent diagnostic entry with read-only product configuration.
use super::persistence::{
    ConfiguredProfileSelection, SelectionFailure, select_configured_probe_profile,
};
pub use crate::adapters::production_mutation::probe_preflight::acquisition_report::{
    Failure, Outcome, Report, Stage,
};
use crate::{
    adapters::{
        caldav_transport::{DavCancellation, DavTransportPolicy},
        credentials::WindowsCredentialStore,
        persistence::Database,
        production_mutation::ProductionMutationSessions,
    },
    application::{
        account_profiles::AccountProfileService,
        account_registry::AccountSourceRegistry,
        attachment_runtime::{AttachmentRuntime, AttachmentStateEmitter, AttachmentView},
        credentials::CredentialService,
        discovery_runtime::{AccountCalendarDiscovery, DiscoveryError},
        recurrence_runtime::{AgendaRefreshEmitter, AgendaRefreshNotice},
        source_factory::{AttachmentCancellation, SourceAttachError, SourceAttacher},
        sync_runtime::{
            ScheduledSyncDeadline, SyncDeadlineScheduler, SyncExecutionRuntime, SyncExecutionView,
            SyncSchedulerError, SyncStateEmitter,
        },
    },
    domain::{
        account::{ProviderKind, RegisterAccountProfile},
        calendar_sync::{AccountIncrementalStage, AccountInventoryStage, IncrementalSyncRequest},
    },
    ports::{
        caldav::{ProviderReadError, ReadOnlyCalendarSource},
        clock::{Clock, SystemClock},
        credentials::CredentialRef,
    },
};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Disabled,
    Readiness,
    Execute,
    ConfigurationCheck,
}
pub fn rejected_input() -> Report {
    input_failure(Failure::InvalidInput)
}
pub fn input_failure(failure: Failure) -> Report {
    let mut report = Report::new(Outcome::Failed, Stage::Input);
    report.fail(failure);
    report
}
pub fn unavailable_signal() -> Report {
    let mut report = Report::new(Outcome::Failed, Stage::Preparation);
    report.fail(Failure::PreparationUnavailable);
    report
}
pub fn parse_command(arguments: &[String]) -> Result<Command, Failure> {
    if arguments.len() > 3 || arguments.iter().any(|arg| arg.len() > 64) {
        return Err(Failure::InvalidInput);
    }
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Ok(Command::Disabled),
        ["readiness"] => Ok(Command::Readiness),
        ["configuration-check"] => Ok(Command::ConfigurationCheck),
        [
            "execute",
            "--exact-probe-calendars",
            "--single-read-only-run",
        ] => Ok(Command::Execute),
        _ => Err(Failure::InvalidInput),
    }
}
fn fixed_configuration() -> Result<(), Failure> {
    // Diagnostic-only historical read gate. Production routing uses the
    // account's persisted discovery binding instead of this observed shard.
    const ENTRY_URL: &str = "https://p235-caldav.icloud.com.cn/";
    const CHINA_SHARD: &str = "p235-caldav.icloud.com.cn";
    let entry = url::Url::parse(ENTRY_URL).map_err(|_| Failure::PreparationUnavailable)?;
    let policy = DavTransportPolicy::new(std::collections::BTreeSet::from([
        "caldav.icloud.com".into(),
        CHINA_SHARD.into(),
    ]))
    .map_err(|_| Failure::PreparationUnavailable)?;
    policy
        .validate_destination(&entry)
        .map_err(|_| Failure::PreparationUnavailable)
}
pub fn run_command(command: Command, cancellation: DavCancellation) -> Report {
    if command == Command::ConfigurationCheck {
        return run_configuration_check(
            cancellation,
            super::persistence::check_configured_probe_profile,
        );
    }
    run_with_selection(command, cancellation, select_configured_probe_profile)
}
fn run_configuration_check(
    cancellation: DavCancellation,
    check: impl FnOnce(&DavCancellation) -> super::persistence::ConfigurationCheck,
) -> Report {
    use super::persistence::ConfigurationAvailability;
    let check = if cancellation.is_cancelled() {
        ConfigurationAvailability::Cancelled.into()
    } else if !cfg!(windows) {
        ConfigurationAvailability::OtherUnavailable.into()
    } else {
        let result = check(&cancellation);
        if cancellation.is_cancelled() {
            ConfigurationAvailability::Cancelled.into()
        } else {
            result
        }
    };
    let availability = check.availability;
    let mut report = Report::new(
        if availability == ConfigurationAvailability::Available {
            Outcome::ConfigurationAvailable
        } else {
            Outcome::Failed
        },
        Stage::Configuration,
    );
    report.configuration_availability = Some(availability);
    if availability == ConfigurationAvailability::OwnerUnavailable {
        report.owner_detail = check.owner_detail;
        if matches!(
            check.owner_detail,
            Some(
                super::persistence::OwnerDetail::AccessDenied
                    | super::persistence::OwnerDetail::OtherUnavailable
                    | super::persistence::OwnerDetail::PathUnavailable
            )
        ) {
            report.owner_step = check.owner_step;
        }
        if check.owner_detail == Some(super::persistence::OwnerDetail::AccessDenied)
            && check.owner_step == Some(super::persistence::OwnerStep::LockFileAcquire)
        {
            report.fallback_detail = check.fallback_detail;
            if check.fallback_detail
                == Some(super::persistence::LockFallbackObservation::ExistingLockAbsent)
            {
                report.parent_access_requests = check.parent_access_requests;
            }
        }
    }
    report
}
fn run_with_selection(
    command: Command,
    cancellation: DavCancellation,
    selector: impl FnOnce() -> Result<ConfiguredProfileSelection, SelectionFailure>,
) -> Report {
    run_with_selection_store(
        command,
        cancellation,
        selector,
        CredentialService::new(Arc::new(WindowsCredentialStore)),
    )
}
fn run_with_selection_store(
    command: Command,
    cancellation: DavCancellation,
    selector: impl FnOnce() -> Result<ConfiguredProfileSelection, SelectionFailure>,
    exact_store: CredentialService,
) -> Report {
    match command {
        // This private execution path cannot start a configuration check.
        Command::ConfigurationCheck => rejected_input(),
        Command::Disabled => Report::new(Outcome::Disabled, Stage::Input),
        Command::Readiness => {
            let mut report = Report::new(Outcome::ReadinessChecksOnly, Stage::Readiness);
            if let Err(failure) = fixed_configuration() {
                report.fail(failure)
            }
            if !cfg!(windows) {
                report.fail(Failure::UnsupportedPlatform)
            }
            report
        }
        Command::Execute => {
            let mut report = Report::new(Outcome::Failed, Stage::Input);
            if cancellation.is_cancelled() {
                report.fail(Failure::OwnerLostOrCancelled);
                return report;
            }
            if let Err(failure) = fixed_configuration() {
                report.fail(failure);
                return report;
            }
            if !cfg!(windows) {
                report.fail(Failure::UnsupportedPlatform);
                return report;
            }
            report.stage = Stage::Selection;
            let selection = selector();
            if cancellation.is_cancelled() {
                report.fail(Failure::OwnerLostOrCancelled);
                return report;
            }
            let selection = match selection {
                Ok(selection) => selection,
                Err(failure) => {
                    report.fail(selection_failure(failure));
                    return report;
                }
            };
            if cancellation.is_cancelled() {
                report.fail(Failure::OwnerLostOrCancelled);
                return report;
            }
            let (reference, record) = match selection.consume_read(&exact_store, &cancellation) {
                Ok(parts) => parts,
                Err((failure, attempted)) => {
                    if attempted {
                        report.stage = Stage::Credential;
                        report.credential_check = "attempted_once_at_execute";
                    }
                    report.fail(failure);
                    return report;
                }
            };
            report.credential_check = "attempted_once_at_execute";
            let credentials = CredentialService::new(Arc::new(LoadedCredential {
                reference: reference.clone(),
                record: std::sync::Mutex::new(Some(record)),
            }));
            match HarnessRuntime::prepare(reference, credentials, cancellation) {
                Ok(runtime) => runtime.run_after_exact_credential_read(),
                Err(failure) => {
                    report.stage = Stage::Preparation;
                    report.fail(failure);
                    report
                }
            }
        }
    }
}

// The native store is read exactly once before any memory actor/session is
// constructed. Existing pinned-session authentication consumes this record,
// without a second native read or any retry/fallback.
struct LoadedCredential {
    reference: CredentialRef,
    record: std::sync::Mutex<Option<crate::ports::credentials::CredentialRecord>>,
}
impl crate::ports::credentials::CredentialStore for LoadedCredential {
    fn read(
        &self,
        reference: &CredentialRef,
    ) -> Result<
        crate::ports::credentials::CredentialRecord,
        crate::ports::credentials::CredentialError,
    > {
        if reference != &self.reference {
            return Err(crate::ports::credentials::CredentialError::InvalidReference);
        }
        self.record
            .lock()
            .map_err(|_| crate::ports::credentials::CredentialError::NotFound)?
            .take()
            .ok_or(crate::ports::credentials::CredentialError::NotFound)
    }
    fn write(
        &self,
        _: &CredentialRef,
        _: &crate::ports::credentials::CredentialRecord,
    ) -> Result<(), crate::ports::credentials::CredentialError> {
        Err(crate::ports::credentials::CredentialError::InvalidReference)
    }
    fn delete(&self, _: &CredentialRef) -> Result<(), crate::ports::credentials::CredentialError> {
        Err(crate::ports::credentials::CredentialError::InvalidReference)
    }
}

fn selection_failure(failure: SelectionFailure) -> Failure {
    match failure {
        SelectionFailure::Unavailable => Failure::SavedReferenceUnavailable,
        SelectionFailure::NoMatch => Failure::SavedReferenceNotFound,
        SelectionFailure::Ambiguous => Failure::SavedReferenceAmbiguous,
        SelectionFailure::InvalidMetadata => Failure::SavedReferenceInvalidMetadata,
        SelectionFailure::Bounds => Failure::SavedReferenceBoundsExceeded,
    }
}

struct HarnessRuntime {
    accounts: Arc<AccountSourceRegistry>,
    sessions: Arc<ProductionMutationSessions>,
    attachments: AttachmentRuntime,
    sync: Arc<SyncExecutionRuntime>,
    account: String,
    cancellation: DavCancellation,
}
impl HarnessRuntime {
    fn run_after_exact_credential_read(self) -> Report {
        let mut report = self.run();
        // The configured-profile owner already completed the one native read.
        // Later preparation/cancellation failures cannot make that read deferred.
        report.credential_check = "attempted_once_at_execute";
        report
    }
    fn prepare(
        reference: CredentialRef,
        credentials: CredentialService,
        cancellation: DavCancellation,
    ) -> Result<Self, Failure> {
        if cancellation.is_cancelled() {
            return Err(Failure::OwnerLostOrCancelled);
        }
        fixed_configuration()?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let database = Database::open_with_clock(":memory:", clock.clone())
            .map_err(|_| Failure::PreparationUnavailable)?;
        let account = reference
            .target_name()
            .strip_prefix("calendar-todo/account/")
            .ok_or(Failure::InvalidInput)?
            .to_owned();
        let accounts = Arc::new(AccountSourceRegistry::new(database.clone()));
        accounts
            .register_profile(RegisterAccountProfile {
                account_id: account.clone(),
                provider_kind: ProviderKind::CalDav,
                user_label: "Private Probe diagnostic".into(),
                credential_ref: reference.target_name().into(),
                region_profile: "icloud-cn".into(),
                enabled: true,
            })
            .map_err(|_| Failure::PreparationUnavailable)?;
        // This is configured-reference evidence only. Existence/secret is checked
        // by the same concrete session at explicit execute, never by readiness.
        accounts
            .mark_credential_configured(&account)
            .map_err(|_| Failure::PreparationUnavailable)?;
        accounts
            .set_remote_editing_enabled(&account, true)
            .map_err(|_| Failure::PreparationUnavailable)?;
        accounts
            .attach_source(&account, Arc::new(MetadataOnlySource(account.clone())))
            .map_err(|_| Failure::PreparationUnavailable)?;
        let sessions = Arc::new(ProductionMutationSessions::new(
            accounts.clone(),
            database,
            credentials.clone(),
            clock.clone(),
        ));
        sessions
            .register_probe_harness(&account, cancellation.clone())
            .map_err(|_| Failure::PreparationUnavailable)?;
        let inert = Arc::new(ClosedPorts);
        let profiles =
            AccountProfileService::new(accounts.clone(), credentials, inert.clone(), inert.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        let sync = Arc::new(
            SyncExecutionRuntime::new(
                accounts.clone(),
                inert.clone(),
                inert.clone(),
                clock,
                inert.clone(),
                receiver,
                15,
            )
            .map_err(|_| Failure::PreparationUnavailable)?,
        );
        let attachments = AttachmentRuntime::with_mutation_sessions(
            profiles,
            accounts.clone(),
            sync.clone(),
            inert.clone(),
            inert,
            Some(sessions.clone()),
        );
        Ok(Self {
            accounts,
            sessions,
            attachments,
            sync,
            account,
            cancellation,
        })
    }
    fn run(self) -> Report {
        let (owner, prepared) = match self
            .attachments
            .prepare_probe_harness(&self.sessions, &self.account)
        {
            Ok(prepared) => prepared,
            Err(()) => {
                let mut report = Report::new(Outcome::Failed, Stage::Preparation);
                report.fail(Failure::OwnerLostOrCancelled);
                return report;
            }
        };
        let mut report = self.sessions.probe_harness_report(prepared);
        if self.cancellation.is_cancelled() {
            report.fail(Failure::OwnerLostOrCancelled)
        }
        owner.cancel();
        drop(owner);
        // No acquired routes/candidates leave this diagnostic; no actor import.
        self.attachments.shutdown();
        self.sessions.shutdown();
        self.sync.shutdown();
        if self.accounts.detach_source(&self.account).is_err() {
            report.stage = Stage::Shutdown;
            report.fail(Failure::ShutdownUnavailable)
        }
        report
    }
}
impl Drop for HarnessRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.attachments.shutdown();
        self.sessions.shutdown();
        self.sync.shutdown();
        let _ = self.accounts.detach_source(&self.account);
    }
}

struct MetadataOnlySource(String);
impl ReadOnlyCalendarSource for MetadataOnlySource {
    fn account_id(&self) -> &str {
        &self.0
    }
    fn collect_full_inventory(&self) -> Result<AccountInventoryStage, ProviderReadError> {
        Err(ProviderReadError::UnsafeData)
    }
    fn collect_incremental(
        &self,
        _: &[IncrementalSyncRequest],
    ) -> Result<AccountIncrementalStage, ProviderReadError> {
        Err(ProviderReadError::UnsafeData)
    }
}
struct ClosedPorts;
impl SourceAttacher for ClosedPorts {
    fn attach(&self, _: &str) -> Result<(), SourceAttachError> {
        Err(SourceAttachError::UnsupportedProvider)
    }
    fn attach_cancellable(
        &self,
        _: &str,
        _: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        Err(SourceAttachError::UnsupportedProvider)
    }
}
impl AccountCalendarDiscovery for ClosedPorts {
    fn discover_cancellable(
        &self,
        _: &str,
        _: &AttachmentCancellation,
    ) -> Result<(), DiscoveryError> {
        Err(DiscoveryError::UnsupportedProvider)
    }
}
impl AttachmentStateEmitter for ClosedPorts {
    fn emit(&self, _: AttachmentView) {}
}
impl SyncStateEmitter for ClosedPorts {
    fn emit(&self, _: SyncExecutionView) {}
}
impl AgendaRefreshEmitter for ClosedPorts {
    fn emit(&self, _: AgendaRefreshNotice) {}
}
impl SyncDeadlineScheduler for ClosedPorts {
    fn arm(&self, _: ScheduledSyncDeadline) -> Result<(), SyncSchedulerError> {
        Err(SyncSchedulerError::Unavailable)
    }
    fn cancel(&self, _: &str) -> Result<(), SyncSchedulerError> {
        Ok(())
    }
    fn shutdown(&self) {}
}
