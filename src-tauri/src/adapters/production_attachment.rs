use crate::{
    adapters::{
        caldav::{
            AdapterAccountContext, DiscoveredCalendarCollection, ExecutingCalDavAdapter,
            discover_icloud,
        },
        caldav_http::{HttpDavConfig, HttpDavExecutor},
        caldav_transport::{DavAuthentication, DavExecutor},
        credentials::WindowsCredentialStore,
        persistence::{Database, DiscoveredCalendarRecord},
    },
    application::{
        account_registry::AccountSourceRegistry,
        credentials::CredentialService,
        discovery_runtime::{CalendarDiscoverySource, DiscoveredCalendarStage},
        source_factory::{
            AttachmentCancellation, SourceAttachError, SourceAttacher, SourceFactoryOrchestrator,
        },
    },
    domain::account::{AccountProfile, ProviderKind},
    ports::{
        caldav::{
            CalDavSourceFactory, ProviderReadError, ReadOnlyCalendarSource, SourceFactoryError,
        },
        clock::Clock,
        credentials::{CredentialError, CredentialRecord, CredentialRef, CredentialStore},
    },
};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;

const ENTRY_URL: &str = super::icloud_caldav::BOOTSTRAP;
const REGION_PROFILE: &str = "icloud-cn";

/// Bounded evidence from one explicit, read-only China-region diagnostic.
/// Collection names, hrefs, privilege strings, and credential data stay inside
/// this adapter and are never returned to its caller.
pub struct ICloudChinaReadDiagnostic {
    pub calendar_count: u32,
    pub approved_count: u32,
    pub writable_count: u32,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ICloudChinaReadDiagnosticError {
    #[error("invalid diagnostic selection")]
    InvalidSelection,
    #[error("credential action required")]
    CredentialActionRequired,
    #[error("credential service unavailable")]
    CredentialUnavailable,
    #[error("account identity mismatch")]
    AccountMismatch,
    #[error("reviewed read transport unavailable")]
    TransportUnavailable,
    #[error("calendar discovery unauthorized")]
    Unauthorized,
    #[error("calendar discovery transient failure")]
    Transient,
    #[error("calendar discovery unsafe or incomplete")]
    UnsafeDiscovery,
    #[error("approved calendar selection unavailable")]
    TargetUnavailable,
    #[error("approved calendar lacks content-write privilege")]
    TargetReadOnly,
}

/// Uses only the existing reviewed read executor and pinned China host policy.
/// It performs no SQLite write, credential provisioning, or DAV mutation.
pub fn diagnose_icloud_china_read_only(
    reference: &CredentialRef,
    expected_username: &str,
    approved_names: [&str; 2],
) -> Result<ICloudChinaReadDiagnostic, ICloudChinaReadDiagnosticError> {
    diagnose_icloud_china_with(
        reference,
        expected_username,
        approved_names,
        &WindowsCredentialStore,
        &ReviewedHttpDavExecutorFactory,
    )
}

/// Authorized diagnostic variant that uses the username stored inside the
/// uniquely selected credential record. The username never crosses argv,
/// stdout, SQLite, or the returned bounded report.
pub fn diagnose_icloud_china_read_only_stored_identity(
    reference: &CredentialRef,
    approved_names: [&str; 2],
) -> Result<ICloudChinaReadDiagnostic, ICloudChinaReadDiagnosticError> {
    diagnose_icloud_china_with_optional_identity(
        reference,
        None,
        approved_names,
        &WindowsCredentialStore,
        &ReviewedHttpDavExecutorFactory,
    )
}

fn diagnose_icloud_china_with(
    reference: &CredentialRef,
    expected_username: &str,
    approved_names: [&str; 2],
    store: &dyn CredentialStore,
    executors: &dyn DavExecutorFactory,
) -> Result<ICloudChinaReadDiagnostic, ICloudChinaReadDiagnosticError> {
    diagnose_icloud_china_with_optional_identity(
        reference,
        Some(expected_username),
        approved_names,
        store,
        executors,
    )
}

fn diagnose_icloud_china_with_optional_identity(
    reference: &CredentialRef,
    expected_username: Option<&str>,
    approved_names: [&str; 2],
    store: &dyn CredentialStore,
    executors: &dyn DavExecutorFactory,
) -> Result<ICloudChinaReadDiagnostic, ICloudChinaReadDiagnosticError> {
    if expected_username.is_some_and(|username| {
        username.is_empty() || username.len() > 512 || username.chars().any(char::is_control)
    }) || approved_names
        .iter()
        .any(|name| name.is_empty() || name.len() > 256 || name.chars().any(char::is_control))
        || approved_names[0] == approved_names[1]
    {
        return Err(ICloudChinaReadDiagnosticError::InvalidSelection);
    }
    let credential = store.read(reference).map_err(|error| match error {
        CredentialError::NotFound => ICloudChinaReadDiagnosticError::CredentialActionRequired,
        _ => ICloudChinaReadDiagnosticError::CredentialUnavailable,
    })?;
    if expected_username
        .is_some_and(|username| !credential.username().eq_ignore_ascii_case(username))
    {
        return Err(ICloudChinaReadDiagnosticError::AccountMismatch);
    }
    let executor = executors
        .create()
        .map_err(|_| ICloudChinaReadDiagnosticError::TransportUnavailable)?;
    let found = discover_icloud(
        ENTRY_URL,
        DavAuthentication::from_credential(&credential),
        executor,
    )
    .map_err(|error| match error {
        crate::ports::caldav::ProviderReadError::Unauthorized => {
            ICloudChinaReadDiagnosticError::Unauthorized
        }
        crate::ports::caldav::ProviderReadError::Transient => {
            ICloudChinaReadDiagnosticError::Transient
        }
        _ => ICloudChinaReadDiagnosticError::UnsafeDiscovery,
    })?;
    summarize_diagnostic_collections(&found.calendars, approved_names)
}

fn summarize_diagnostic_collections(
    found: &[DiscoveredCalendarCollection],
    approved_names: [&str; 2],
) -> Result<ICloudChinaReadDiagnostic, ICloudChinaReadDiagnosticError> {
    let mut writable_count = 0_u32;
    for name in approved_names {
        let mut matches = found
            .iter()
            .filter(|calendar| calendar.display_name == name);
        let Some(calendar) = matches.next() else {
            return Err(ICloudChinaReadDiagnosticError::TargetUnavailable);
        };
        if matches.next().is_some() {
            return Err(ICloudChinaReadDiagnosticError::TargetUnavailable);
        }
        if calendar
            .privileges
            .iter()
            .any(|privilege| privilege == "write" || privilege == "write-content")
        {
            writable_count += 1;
        }
    }
    if writable_count != 2 {
        return Err(ICloudChinaReadDiagnosticError::TargetReadOnly);
    }
    Ok(ICloudChinaReadDiagnostic {
        calendar_count: u32::try_from(found.len()).unwrap_or(u32::MAX),
        approved_count: 2,
        writable_count,
    })
}

pub trait DavExecutorFactory: Send + Sync + 'static {
    fn create(&self) -> Result<Arc<dyn DavExecutor>, SourceFactoryError>;
}

pub struct ReviewedHttpDavExecutorFactory;

impl DavExecutorFactory for ReviewedHttpDavExecutorFactory {
    fn create(&self) -> Result<Arc<dyn DavExecutor>, SourceFactoryError> {
        HttpDavExecutor::new(HttpDavConfig::default())
            .map(|executor| Arc::new(executor) as Arc<dyn DavExecutor>)
            .map_err(|_| SourceFactoryError::Unavailable)
    }
}

pub struct ProductionCalendarDiscoverer {
    registry: Arc<AccountSourceRegistry>,
    credentials: CredentialService,
    executors: Arc<dyn DavExecutorFactory>,
    clock: Arc<dyn Clock>,
}
impl ProductionCalendarDiscoverer {
    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        credentials: CredentialService,
        executors: Arc<dyn DavExecutorFactory>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            registry,
            credentials,
            executors,
            clock,
        }
    }
}

impl CalendarDiscoverySource for ProductionCalendarDiscoverer {
    /// Collect bounded collection metadata for one account.
    ///
    /// The credential is read only for this explicit action and is dropped
    /// before the snapshot is returned. Nothing is persisted here: the runtime
    /// that owns the request generation decides whether a snapshot is still
    /// current enough to reach SQLite.
    fn collect(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<DiscoveredCalendarStage, SourceAttachError> {
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let profile = self.registry.profile(account_id)?;
        if !profile.enabled {
            return Err(SourceAttachError::Disabled);
        }
        if profile.region_profile != REGION_PROFILE {
            return Err(SourceAttachError::UnsupportedProvider);
        }
        let reference = CredentialRef::parse(&profile.credential_ref)
            .map_err(|_| SourceAttachError::UnsafeConfiguration)?;
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let credential = self.credentials.read(&reference)?;
        let executor = self.executors.create().map_err(source_error)?;
        if let Some(handle) = executor.cancellation() {
            cancellation.register_interrupt(Arc::new(move || {
                handle.cancel();
            }));
        }
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let found = match discover_icloud(
            ENTRY_URL,
            DavAuthentication::from_credential(&credential),
            executor,
        ) {
            Ok(found) => found,
            Err(error) => {
                // A cancelled exchange classifies as transient at the
                // transport boundary; only the shared token knows that the
                // request was superseded rather than merely slowed down.
                return Err(if cancellation.is_cancelled() {
                    SourceAttachError::Cancelled
                } else {
                    source_error(map_provider_error(error))
                });
            }
        };
        drop(credential);
        if cancellation.is_cancelled() {
            return Err(SourceAttachError::Cancelled);
        }
        let namespace =
            Uuid::parse_str(account_id).map_err(|_| SourceAttachError::InvalidIdentity)?;
        let calendars = found
            .calendars
            .into_iter()
            .map(|calendar| DiscoveredCalendarRecord {
                calendar_id: deterministic_collection_id(namespace, &calendar.href).to_string(),
                collection_url: calendar.href,
                display_name: calendar.display_name,
                server_color: calendar.server_color,
                privileges: calendar.privileges.join(","),
            })
            .collect();
        let observed_at_ms = self
            .clock
            .now_utc_ms()
            .map_err(|_| SourceAttachError::Unavailable)?;
        Ok(DiscoveredCalendarStage {
            account_id: account_id.to_owned(),
            calendars,
            observed_at_ms,
            binding: Some(found.binding),
        })
    }
}

fn source_error(error: SourceFactoryError) -> SourceAttachError {
    match error {
        SourceFactoryError::Unauthorized => SourceAttachError::CredentialActionRequired,
        SourceFactoryError::Unsupported => SourceAttachError::UnsupportedProvider,
        SourceFactoryError::Unavailable => SourceAttachError::Unavailable,
        SourceFactoryError::UnsafeConfiguration => SourceAttachError::UnsafeConfiguration,
    }
}

pub struct ICloudChinaReadOnlyFactory {
    executors: Arc<dyn DavExecutorFactory>,
    clock: Arc<dyn Clock>,
    database: Database,
}

impl ICloudChinaReadOnlyFactory {
    pub fn new(
        executors: Arc<dyn DavExecutorFactory>,
        clock: Arc<dyn Clock>,
        database: Database,
    ) -> Self {
        Self {
            executors,
            clock,
            database,
        }
    }

    fn build_inner(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
        cancellation: Option<&AttachmentCancellation>,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError> {
        if profile.provider_kind != ProviderKind::CalDav || profile.region_profile != REGION_PROFILE
        {
            return Err(SourceFactoryError::Unsupported);
        }
        let executor = self.executors.create()?;
        if let Some(cancellation) = cancellation {
            if let Some(handle) = executor.cancellation() {
                cancellation.register_interrupt(Arc::new(move || {
                    handle.cancel();
                }));
            }
            if cancellation.is_cancelled() {
                return Err(SourceFactoryError::Unavailable);
            }
        }
        let binding = self
            .database
            .icloud_binding(profile.account_id.clone())
            .map_err(|_| SourceFactoryError::Unavailable)?
            .ok_or(SourceFactoryError::UnsafeConfiguration)?;
        let home = binding
            .validate()
            .map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
        let trusted_hosts = BTreeSet::from([home
            .host_str()
            .ok_or(SourceFactoryError::UnsafeConfiguration)?
            .to_owned()]);
        let calendar_ids_by_href = self
            .database
            .available_calendar_ids_by_url(profile.account_id.clone())
            .map_err(|_| SourceFactoryError::Unavailable)?;
        let scope_database = self.database.clone();
        let scope_account = profile.account_id.clone();
        let observed_at_ms = self
            .clock
            .now_utc_ms()
            .map_err(|_| SourceFactoryError::Unavailable)?;
        let scope_binding = binding.clone();
        let adapter = ExecutingCalDavAdapter::new(
            AdapterAccountContext {
                account_id: profile.account_id.clone(),
                provider_kind: profile.provider_kind.as_str().into(),
                user_label: profile.user_label.clone(),
                credential_ref: profile.credential_ref.clone(),
                region_profile: profile.region_profile.clone(),
                trusted_hosts,
                calendar_ids_by_href,
            },
            &binding.home_url,
            DavAuthentication::from_credential(credential),
            executor,
            observed_at_ms,
        )
        .map_err(map_provider_error)?
        .with_icloud_binding(binding)
        .map_err(map_provider_error)?
        .with_calendar_scope(Arc::new(move || {
            if scope_database
                .icloud_binding(scope_account.clone())
                .map_err(|_| ProviderReadError::Incomplete)?
                .as_ref()
                != Some(&scope_binding)
            {
                return Err(ProviderReadError::UnsafeData);
            }
            let selected = scope_database
                .list_calendar_settings()
                .map_err(|_| ProviderReadError::Incomplete)?
                .into_iter()
                .filter(|calendar| {
                    calendar.account_id == scope_account && calendar.available && calendar.visible
                })
                .map(|calendar| calendar.calendar_id)
                .collect::<HashSet<_>>();
            let mut routes = scope_database
                .available_calendar_ids_by_url(scope_account.clone())
                .map_err(|_| ProviderReadError::Incomplete)?;
            routes.retain(|_, calendar_id| selected.contains(calendar_id));
            Ok(routes)
        }));
        Ok(Arc::new(adapter))
    }
}

impl CalDavSourceFactory for ICloudChinaReadOnlyFactory {
    fn build(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError> {
        self.build_inner(profile, credential, None)
    }

    fn build_cancellable(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
        cancellation: &AttachmentCancellation,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError> {
        self.build_inner(profile, credential, Some(cancellation))
    }
}

pub struct ProductionSourceAttacher {
    registry: Arc<AccountSourceRegistry>,
    orchestrator: SourceFactoryOrchestrator,
}

impl ProductionSourceAttacher {
    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        credentials: CredentialService,
        executors: Arc<dyn DavExecutorFactory>,
        clock: Arc<dyn Clock>,
        database: Database,
    ) -> Self {
        Self {
            registry: registry.clone(),
            orchestrator: SourceFactoryOrchestrator::new(
                registry,
                credentials,
                Arc::new(ICloudChinaReadOnlyFactory::new(executors, clock, database)),
            ),
        }
    }
}

impl SourceAttacher for ProductionSourceAttacher {
    fn attach(&self, account_id: &str) -> Result<(), SourceAttachError> {
        let profile = self.registry.profile(account_id)?;
        if profile.provider_kind != ProviderKind::CalDav || profile.region_profile != REGION_PROFILE
        {
            return Err(SourceAttachError::UnsupportedProvider);
        }
        self.orchestrator.build_and_attach(account_id)
    }

    fn attach_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        let profile = self.registry.profile(account_id)?;
        if profile.provider_kind != ProviderKind::CalDav || profile.region_profile != REGION_PROFILE
        {
            return Err(SourceAttachError::UnsupportedProvider);
        }
        self.orchestrator
            .build_and_attach_cancellable(account_id, cancellation)
    }
}

fn deterministic_collection_id(namespace: Uuid, href: &str) -> Uuid {
    // A deterministic UUIDv8-shaped local identity. This is not a security
    // primitive; it prevents reattachment from changing the local calendar key.
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut value = OFFSET;
    for byte in namespace
        .as_bytes()
        .iter()
        .copied()
        .chain([0xff])
        .chain(href.bytes())
    {
        value ^= u128::from(byte);
        value = value.wrapping_mul(PRIME);
    }
    let mut bytes = value.to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn map_provider_error(error: crate::ports::caldav::ProviderReadError) -> SourceFactoryError {
    match error {
        crate::ports::caldav::ProviderReadError::Unauthorized => SourceFactoryError::Unauthorized,
        crate::ports::caldav::ProviderReadError::Transient => SourceFactoryError::Unavailable,
        crate::ports::caldav::ProviderReadError::Incomplete
        | crate::ports::caldav::ProviderReadError::UnsafeData
        | crate::ports::caldav::ProviderReadError::InvalidSyncToken => {
            SourceFactoryError::UnsafeConfiguration
        }
    }
}
