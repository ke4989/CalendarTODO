use crate::{
    adapters::caldav_transport::{
        DavAuthentication, DavDepth, DavExecutionError, DavExecutor, DavMethod, DavRequest,
        DavTransportPolicy,
    },
    domain::{
        account::{AccountProfile, ProviderKind, validate_account_id},
        calendar_sync::{
            AccountIncrementalStage, AccountInventoryStage, CalendarIncrementalStage,
            CalendarInventoryStage, IncrementalSyncRequest, ParseStatus, StagedResource,
            extract_vevent_series_uid,
        },
        remote_mutation::resource_belongs_to_collection,
    },
    ports::{
        caldav::{
            CalDavSourceFactory, ProviderReadError, ReadOnlyCalendarSource, SourceFactoryError,
        },
        credentials::CredentialRecord,
    },
};
use quick_xml::{Reader, XmlVersion, events::Event};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use url::Url;
mod icloud;
#[cfg(test)]
#[path = "icloud_caldav_tests.rs"]
mod icloud_tests;
pub(crate) use icloud::discover_icloud;

const REQUEST_TIMEOUT_MS: u64 = 30_000;
const DISCOVERY_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:current-user-principal/><d:principal-URL/><c:calendar-home-set/></d:prop></d:propfind>"#;
const CALENDARS_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/"><d:prop><d:displayname/><d:resourcetype/><a:calendar-color/><d:current-user-privilege-set/><c:supported-calendar-component-set/></d:prop></d:propfind>"#;
const INVENTORY_BODY: &[u8] =
    br#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:sync-token/></d:prop></d:propfind>"#;

#[derive(Debug, Clone)]
pub struct AdapterAccountContext {
    pub account_id: String,
    pub provider_kind: String,
    pub user_label: String,
    pub credential_ref: String,
    pub region_profile: String,
    pub trusted_hosts: BTreeSet<String>,
    pub calendar_ids_by_href: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct DavFixture {
    pub entry_url: String,
    pub redirect_chain: Vec<String>,
    pub discovery_xml: Vec<u8>,
    pub calendars_xml: Vec<u8>,
    pub calendars: Vec<CalendarFixture>,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct CalendarFixture {
    pub collection_href: String,
    pub inventory_xml: Vec<u8>,
    pub fetched_resources: Vec<FetchedResource>,
    pub sync_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchedResource {
    pub href: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// Revision label observed by the exact-target GET for this body. A
    /// provider may answer a later read with a different label for the same
    /// resource; that label belongs with this body and takes precedence over
    /// the inventory/REPORT label.
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IncrementalFixture {
    pub complete: bool,
    pub authoritative: bool,
    pub observed_at_ms: i64,
    pub calendars: Vec<CalendarIncrementalFixture>,
}

#[derive(Debug, Clone)]
pub struct CalendarIncrementalFixture {
    pub collection_href: String,
    pub request_token: String,
    pub response_status: u16,
    pub response_xml: Vec<u8>,
    pub fetched_resources: Vec<FetchedResource>,
}

pub struct OfflineCalDavAdapter {
    context: AdapterAccountContext,
    fixture: DavFixture,
    incremental_fixture: Option<IncrementalFixture>,
}

/// Executes the existing conservative CalDAV parser through an injected
/// transport. The explicit production attachment and deterministic tests both
/// supply implementations of the same bounded executor contract.
pub struct ExecutingCalDavAdapter {
    context: AdapterAccountContext,
    entry_url: Url,
    authentication: DavAuthentication,
    executor: Arc<dyn DavExecutor>,
    policy: DavTransportPolicy,
    observed_at_ms: i64,
    calendar_scope: Option<CalendarScopeReader>,
    icloud_binding: Option<super::icloud_caldav::ICloudBinding>,
}

pub type CalendarScopeReader =
    Arc<dyn Fn() -> Result<BTreeMap<String, String>, ProviderReadError> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCalendarCollection {
    pub href: String,
    pub display_name: String,
    pub server_color: Option<String>,
    pub privileges: Vec<String>,
}

/// Performs only the principal and calendar-home discovery needed to bootstrap
/// an explicitly approved read-only gate. Callers must fail closed when their
/// selection does not resolve to the expected collection count.
pub fn discover_vevent_collections(
    entry_url: &str,
    trusted_hosts: BTreeSet<String>,
    authentication: DavAuthentication,
    executor: Arc<dyn DavExecutor>,
) -> Result<Vec<DiscoveredCalendarCollection>, ProviderReadError> {
    let adapter = ExecutingCalDavAdapter::new(
        AdapterAccountContext {
            account_id: String::new(),
            provider_kind: "caldav".into(),
            user_label: String::new(),
            credential_ref: String::new(),
            region_profile: String::new(),
            trusted_hosts,
            calendar_ids_by_href: BTreeMap::new(),
        },
        entry_url,
        authentication,
        executor,
        0,
    )?;
    let mut discovery_response = adapter.execute(
        DavMethod::Propfind,
        adapter.entry_url.clone(),
        DISCOVERY_BODY.to_vec(),
        DavDepth::Zero,
    )?;
    eprintln!(
        "read_gate stage=entry status={} body_bytes={}",
        discovery_response.status(),
        discovery_response.body().len()
    );
    require_read_status(discovery_response.status())?;
    let mut discovery = match parse_discovery_relaxed(discovery_response.body()) {
        Ok(discovery) => discovery,
        Err(error) => {
            let shape = discovery_shape(discovery_response.body());
            eprintln!(
                "read_gate stage=entry_parse responses={} propstats={} statuses={} principal_properties={} home_properties={} malformed={}",
                shape.responses,
                shape.propstats,
                shape.statuses,
                shape.principal_properties,
                shape.home_properties,
                shape.malformed
            );
            return Err(error);
        }
    };
    if discovery.calendar_home_href.is_empty() {
        let principal_url = discovery_response
            .final_url()
            .join(&discovery.principal_href)
            .map_err(|_| ProviderReadError::UnsafeData)?;
        discovery_response = adapter.execute(
            DavMethod::Propfind,
            principal_url,
            DISCOVERY_BODY.to_vec(),
            DavDepth::Zero,
        )?;
        eprintln!(
            "read_gate stage=principal status={} body_bytes={}",
            discovery_response.status(),
            discovery_response.body().len()
        );
        require_read_status(discovery_response.status())?;
        let principal_discovery = parse_discovery_document(discovery_response.body())?;
        if principal_discovery.calendar_home_href.is_empty() {
            return Err(ProviderReadError::Incomplete);
        }
        discovery.calendar_home_href = principal_discovery.calendar_home_href;
    }
    let calendar_home_url = discovery_response
        .final_url()
        .join(&discovery.calendar_home_href)
        .map_err(|_| ProviderReadError::UnsafeData)?;
    let calendars_response = adapter.execute(
        DavMethod::Propfind,
        calendar_home_url,
        CALENDARS_BODY.to_vec(),
        DavDepth::One,
    )?;
    eprintln!(
        "read_gate stage=calendar_home status={} body_bytes={}",
        calendars_response.status(),
        calendars_response.body().len()
    );
    require_read_status(calendars_response.status())?;
    let parsed = match parse_calendars_detailed(calendars_response.body()) {
        Ok(parsed) => parsed,
        Err(failure) => {
            let shape = calendar_shape(calendars_response.body());
            eprintln!(
                "read_gate stage=calendar_parse_shape checkpoint={} responses={} propstats={} statuses={} hrefs={} calendars={} supported_components={} privileges={} display_names={} colors={} successful_propstats={} failed_propstats={} failed_resourcetypes={} failed_supported_components={} failed_privilege_sets={} failed_display_names={} failed_colors={} malformed={}",
                failure.checkpoint,
                shape.responses,
                shape.propstats,
                shape.statuses,
                shape.hrefs,
                shape.calendars,
                shape.supported_components,
                shape.privileges,
                shape.display_names,
                shape.colors,
                shape.successful_propstats,
                shape.failed_propstats,
                shape.failed_resourcetypes,
                shape.failed_supported_components,
                shape.failed_privilege_sets,
                shape.failed_display_names,
                shape.failed_colors,
                shape.malformed
            );
            return Err(failure.class);
        }
    };
    eprintln!(
        "read_gate stage=calendar_parse vevent_count={}",
        parsed.len()
    );
    Ok(parsed
        .into_iter()
        .map(|calendar| DiscoveredCalendarCollection {
            href: calendar.href,
            display_name: calendar.display_name,
            server_color: calendar.color,
            privileges: calendar.privileges,
        })
        .collect())
}

/// Backend-only synthetic factory used to exercise the complete application
/// orchestration path without selecting or constructing a network client.
pub struct ScriptedCalDavSourceFactory {
    account_id: String,
    region_profile: String,
    entry_url: String,
    trusted_hosts: BTreeSet<String>,
    calendar_ids_by_href: BTreeMap<String, String>,
    executor: Arc<dyn DavExecutor>,
    observed_at_ms: i64,
    calendar_scope: Option<CalendarScopeReader>,
}

impl ScriptedCalDavSourceFactory {
    pub fn new(
        account_id: String,
        region_profile: String,
        entry_url: String,
        trusted_hosts: BTreeSet<String>,
        calendar_ids_by_href: BTreeMap<String, String>,
        executor: Arc<dyn DavExecutor>,
        observed_at_ms: i64,
    ) -> Result<Self, SourceFactoryError> {
        if validate_account_id(&account_id).is_err()
            || region_profile.trim().is_empty()
            || calendar_ids_by_href
                .values()
                .any(|calendar_id| validate_account_id(calendar_id).is_err())
            || observed_at_ms < 0
        {
            return Err(SourceFactoryError::UnsafeConfiguration);
        }
        DavTransportPolicy::new(trusted_hosts.clone())
            .map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
        let entry = Url::parse(&entry_url).map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
        DavRequest::new(
            DavMethod::Get,
            entry.clone(),
            Vec::new(),
            DavDepth::Zero,
            REQUEST_TIMEOUT_MS,
        )
        .map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
        if !entry
            .host_str()
            .is_some_and(|host| trusted_hosts.contains(host))
        {
            return Err(SourceFactoryError::UnsafeConfiguration);
        }
        let mut calendar_ids = BTreeSet::new();
        for (href, calendar_id) in &calendar_ids_by_href {
            let collection = entry
                .join(href)
                .map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
            DavRequest::new(
                DavMethod::Get,
                collection.clone(),
                Vec::new(),
                DavDepth::Zero,
                REQUEST_TIMEOUT_MS,
            )
            .map_err(|_| SourceFactoryError::UnsafeConfiguration)?;
            if !collection
                .host_str()
                .is_some_and(|host| trusted_hosts.contains(host))
                || !calendar_ids.insert(calendar_id)
            {
                return Err(SourceFactoryError::UnsafeConfiguration);
            }
        }
        Ok(Self {
            account_id,
            region_profile,
            entry_url,
            trusted_hosts,
            calendar_ids_by_href,
            executor,
            observed_at_ms,
            calendar_scope: None,
        })
    }

    pub fn with_calendar_scope(mut self, calendar_scope: CalendarScopeReader) -> Self {
        self.calendar_scope = Some(calendar_scope);
        self
    }
}

impl CalDavSourceFactory for ScriptedCalDavSourceFactory {
    fn build(
        &self,
        profile: &AccountProfile,
        credential: &CredentialRecord,
    ) -> Result<Arc<dyn ReadOnlyCalendarSource + Send + Sync>, SourceFactoryError> {
        if profile.provider_kind != ProviderKind::CalDav {
            return Err(SourceFactoryError::Unsupported);
        }
        if !profile.enabled
            || profile.account_id != self.account_id
            || profile.region_profile != self.region_profile
        {
            return Err(SourceFactoryError::UnsafeConfiguration);
        }
        let context = AdapterAccountContext {
            account_id: profile.account_id.clone(),
            provider_kind: profile.provider_kind.as_str().to_owned(),
            user_label: profile.user_label.clone(),
            credential_ref: profile.credential_ref.clone(),
            region_profile: profile.region_profile.clone(),
            trusted_hosts: self.trusted_hosts.clone(),
            calendar_ids_by_href: self.calendar_ids_by_href.clone(),
        };
        let mut adapter = ExecutingCalDavAdapter::new(
            context,
            &self.entry_url,
            DavAuthentication::from_credential(credential),
            self.executor.clone(),
            self.observed_at_ms,
        )
        .map_err(|error| match error {
            ProviderReadError::Unauthorized => SourceFactoryError::Unauthorized,
            ProviderReadError::Transient => SourceFactoryError::Unavailable,
            ProviderReadError::Incomplete
            | ProviderReadError::UnsafeData
            | ProviderReadError::InvalidSyncToken => SourceFactoryError::UnsafeConfiguration,
        })?;
        adapter.calendar_scope = self.calendar_scope.clone();
        Ok(Arc::new(adapter))
    }
}

impl ExecutingCalDavAdapter {
    pub fn new(
        context: AdapterAccountContext,
        entry_url: &str,
        authentication: DavAuthentication,
        executor: Arc<dyn DavExecutor>,
        observed_at_ms: i64,
    ) -> Result<Self, ProviderReadError> {
        let entry_url = Url::parse(entry_url).map_err(|_| ProviderReadError::UnsafeData)?;
        let policy =
            DavTransportPolicy::new(context.trusted_hosts.clone()).map_err(map_execution_error)?;
        Ok(Self {
            context,
            entry_url,
            authentication,
            executor,
            policy,
            observed_at_ms,
            calendar_scope: None,
            icloud_binding: None,
        })
    }

    /// Development Probe source: use already committed exact collection routes.
    /// No principal, home or account collection discovery is performed.

    fn scoped_context(&self) -> Result<AdapterAccountContext, ProviderReadError> {
        let mut context = self.context.clone();
        if let Some(reader) = &self.calendar_scope {
            context.calendar_ids_by_href = reader()?;
        }
        if context.calendar_ids_by_href.is_empty() {
            return Err(ProviderReadError::Incomplete);
        }
        Ok(context)
    }

    fn execute(
        &self,
        method: DavMethod,
        url: Url,
        body: Vec<u8>,
        depth: DavDepth,
    ) -> Result<crate::adapters::caldav_transport::DavResponse, ProviderReadError> {
        let request = DavRequest::new(method, url, body, depth, REQUEST_TIMEOUT_MS)
            .map_err(map_execution_error)?;
        if let Some(binding) = &self.icloud_binding {
            let current = self.scoped_context()?;
            return DavTransportPolicy::icloud_bound(
                binding,
                current.calendar_ids_by_href.into_keys(),
            )
            .map_err(map_execution_error)?
            .execute(self.executor.as_ref(), request, &self.authentication)
            .map_err(map_execution_error);
        }
        self.policy
            .execute(self.executor.as_ref(), request, &self.authentication)
            .map_err(map_execution_error)
    }

    fn collect_full_from_transport(&self) -> Result<AccountInventoryStage, ProviderReadError> {
        if self.icloud_binding.is_some() {
            return self.collect_icloud_full();
        }
        let context = self.scoped_context()?;
        let mut discovery_response = self.execute(
            DavMethod::Propfind,
            self.entry_url.clone(),
            DISCOVERY_BODY.to_vec(),
            DavDepth::Zero,
        )?;
        require_read_status(discovery_response.status())?;
        let mut discovery = parse_discovery_relaxed(discovery_response.body())?;
        if discovery.calendar_home_href.is_empty() {
            let principal_url = discovery_response
                .final_url()
                .join(&discovery.principal_href)
                .map_err(|_| ProviderReadError::UnsafeData)?;
            discovery_response = self.execute(
                DavMethod::Propfind,
                principal_url,
                DISCOVERY_BODY.to_vec(),
                DavDepth::Zero,
            )?;
            require_read_status(discovery_response.status())?;
            let principal_discovery = parse_discovery_document(discovery_response.body())?;
            if principal_discovery.calendar_home_href.is_empty() {
                return Err(ProviderReadError::Incomplete);
            }
            discovery.calendar_home_href = principal_discovery.calendar_home_href;
        }
        discovery_response
            .final_url()
            .join(&discovery.principal_href)
            .map_err(|_| ProviderReadError::UnsafeData)?;
        let calendar_home_url = discovery_response
            .final_url()
            .join(&discovery.calendar_home_href)
            .map_err(|_| ProviderReadError::UnsafeData)?;

        let calendars_response = self.execute(
            DavMethod::Propfind,
            calendar_home_url,
            CALENDARS_BODY.to_vec(),
            DavDepth::One,
        )?;
        require_read_status(calendars_response.status())?;
        let discovered = parse_calendars(calendars_response.body())?;
        let mut calendars = Vec::new();
        for calendar in discovered.into_iter().filter(|item| {
            item.supports_vevent && context.calendar_ids_by_href.contains_key(&item.href)
        }) {
            let collection_url = calendars_response
                .final_url()
                .join(&calendar.href)
                .map_err(|_| ProviderReadError::UnsafeData)?;
            let inventory_response = self.execute(
                DavMethod::Propfind,
                collection_url,
                INVENTORY_BODY.to_vec(),
                DavDepth::One,
            )?;
            require_read_status(inventory_response.status())?;
            let inventory = parse_inventory(inventory_response.body(), &calendar.href)?;
            let sync_token = parse_optional_sync_token(inventory_response.body(), &calendar.href)?;
            let mut fetched_resources = Vec::with_capacity(inventory.len());
            for item in inventory {
                let resource_url = inventory_response
                    .final_url()
                    .join(&item.href)
                    .map_err(|_| ProviderReadError::UnsafeData)?;
                let response =
                    self.execute(DavMethod::Get, resource_url, Vec::new(), DavDepth::Zero)?;
                require_resource_status(response.status())?;
                // The exact-target GET returns its own revision label together
                // with its own body. A provider may re-serialize or relabel a
                // resource after the inventory read; that difference is not a
                // safety contradiction, and rejecting the whole stage here
                // would block the account's synchronization permanently. The
                // body is never paired with the older inventory label: the
                // fetched label wins and stays bounded/validated while staging.
                fetched_resources.push(FetchedResource {
                    href: item.href,
                    status: response.status(),
                    content_type: response.content_type().unwrap_or_default().to_owned(),
                    body: response.body().to_vec(),
                    etag: response.etag().map(str::to_owned),
                });
            }
            calendars.push(CalendarFixture {
                collection_href: calendar.href,
                inventory_xml: inventory_response.body().to_vec(),
                fetched_resources,
                sync_token,
            });
        }
        let fixture = DavFixture {
            entry_url: discovery_response.final_url().to_string(),
            redirect_chain: Vec::new(),
            discovery_xml: normalized_discovery_xml(&discovery),
            calendars_xml: calendars_response.body().to_vec(),
            calendars,
            observed_at_ms: self.observed_at_ms,
        };
        OfflineCalDavAdapter::new(context, fixture).collect_full_inventory()
    }

    fn collect_incremental_from_transport(
        &self,
        requests: &[IncrementalSyncRequest],
    ) -> Result<AccountIncrementalStage, ProviderReadError> {
        let context = self.scoped_context()?;
        let mut calendars = Vec::with_capacity(requests.len());
        for request in requests {
            let collection_href = context
                .calendar_ids_by_href
                .iter()
                .find_map(|(href, id)| (id == &request.calendar_id).then_some(href.clone()))
                .ok_or(ProviderReadError::UnsafeData)?;
            let collection_url = self
                .entry_url
                .join(&collection_href)
                .map_err(|_| ProviderReadError::UnsafeData)?;
            let report_body = sync_collection_body(&request.sync_token)?;
            let report = self.execute(
                DavMethod::Report,
                collection_url,
                report_body,
                DavDepth::One,
            )?;
            if report.status() == 401 {
                return Err(ProviderReadError::Unauthorized);
            }
            if report.status() >= 500 {
                return Err(ProviderReadError::Transient);
            }
            let mut fetched_resources = Vec::new();
            if report.status() == 207 {
                let report_collection = if self.icloud_binding.is_some() {
                    report.final_url().as_str()
                } else {
                    &collection_href
                };
                let parsed = match parse_incremental(report.body(), report_collection) {
                    Ok(parsed) => parsed,
                    // Some iCloud endpoints return a usable authoritative
                    // inventory while their sync-collection member shape omits
                    // getetag or includes provider-specific response entries.
                    // Discard that REPORT wholesale and let the application
                    // perform its existing complete-inventory fallback. No
                    // href, token, revision, or body from the rejected delta is
                    // promoted through this path.
                    Err(ProviderReadError::Incomplete | ProviderReadError::UnsafeData) => {
                        return Err(ProviderReadError::InvalidSyncToken);
                    }
                    Err(error) => return Err(error),
                };
                fetched_resources.reserve(parsed.changed.len());
                for item in parsed.changed {
                    let resource_url = if self.icloud_binding.is_some() {
                        super::icloud_caldav::resolve(report.final_url(), &item.href)
                            .map_err(map_execution_error)?
                    } else {
                        report
                            .final_url()
                            .join(&item.href)
                            .map_err(|_| ProviderReadError::UnsafeData)?
                    };
                    let response =
                        self.execute(DavMethod::Get, resource_url, Vec::new(), DavDepth::Zero)?;
                    require_resource_status(response.status())?;
                    // Same rule as the full inventory: the fetched revision
                    // label belongs with the fetched body instead of failing
                    // the whole account stage on a provider relabel.
                    fetched_resources.push(FetchedResource {
                        href: item.href,
                        status: response.status(),
                        content_type: response.content_type().unwrap_or_default().to_owned(),
                        body: response.body().to_vec(),
                        etag: response.etag().map(str::to_owned),
                    });
                }
            }
            calendars.push(CalendarIncrementalFixture {
                collection_href,
                request_token: request.sync_token.clone(),
                response_status: report.status(),
                response_xml: report.body().to_vec(),
                fetched_resources,
            });
        }
        let base_fixture = DavFixture {
            entry_url: self.entry_url.to_string(),
            redirect_chain: Vec::new(),
            discovery_xml: Vec::new(),
            calendars_xml: Vec::new(),
            calendars: Vec::new(),
            observed_at_ms: self.observed_at_ms,
        };
        OfflineCalDavAdapter::new(context, base_fixture)
            .with_incremental_fixture(IncrementalFixture {
                complete: true,
                authoritative: true,
                observed_at_ms: self.observed_at_ms,
                calendars,
            })
            .collect_incremental(requests)
    }
}

impl ReadOnlyCalendarSource for ExecutingCalDavAdapter {
    fn account_id(&self) -> &str {
        &self.context.account_id
    }

    fn collect_full_inventory(&self) -> Result<AccountInventoryStage, ProviderReadError> {
        self.collect_full_from_transport()
    }

    fn collect_incremental(
        &self,
        requests: &[IncrementalSyncRequest],
    ) -> Result<AccountIncrementalStage, ProviderReadError> {
        self.collect_incremental_from_transport(requests)
    }

    /// Trips the executor's shared cancellation flag so a blocked read returns
    /// at its next check instead of waiting for the request deadline.
    fn cancel_in_flight(&self) -> bool {
        self.executor
            .cancellation()
            .is_some_and(|cancellation| cancellation.cancel())
    }
}

fn map_execution_error(error: DavExecutionError) -> ProviderReadError {
    match error {
        DavExecutionError::Timeout
        | DavExecutionError::Offline
        | DavExecutionError::Dns
        | DavExecutionError::Tls
        | DavExecutionError::Cancelled
        | DavExecutionError::Unavailable => ProviderReadError::Transient,
        DavExecutionError::InvalidRequest
        | DavExecutionError::InvalidResponse
        | DavExecutionError::UnsafeRedirect
        | DavExecutionError::TooManyRedirects
        | DavExecutionError::ResponseTooLarge
        | DavExecutionError::ScriptMismatch => ProviderReadError::UnsafeData,
    }
}

fn require_read_status(status: u16) -> Result<(), ProviderReadError> {
    match status {
        207 => Ok(()),
        401 | 403 => Err(ProviderReadError::Unauthorized),
        500..=599 => Err(ProviderReadError::Transient),
        _ => Err(ProviderReadError::Incomplete),
    }
}

fn require_resource_status(status: u16) -> Result<(), ProviderReadError> {
    match status {
        200 => Ok(()),
        401 | 403 => Err(ProviderReadError::Unauthorized),
        500..=599 => Err(ProviderReadError::Transient),
        _ => Err(ProviderReadError::Incomplete),
    }
}

fn sync_collection_body(token: &str) -> Result<Vec<u8>, ProviderReadError> {
    if token.is_empty()
        || token.len() > 4 * 1024
        || token.chars().any(|value| {
            value == '\0' || (value.is_control() && !matches!(value, '\t' | '\n' | '\r'))
        })
    {
        return Err(ProviderReadError::UnsafeData);
    }
    let escaped = token
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(format!(
        "<d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{escaped}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"
    )
    .into_bytes())
}

fn normalized_discovery_xml(discovery: &Discovery) -> Vec<u8> {
    fn escaped(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    format!(
        "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:propstat><d:prop><d:current-user-principal><d:href>{}</d:href></d:current-user-principal><c:calendar-home-set><d:href>{}</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
        escaped(&discovery.principal_href),
        escaped(&discovery.calendar_home_href)
    )
    .into_bytes()
}

impl OfflineCalDavAdapter {
    pub fn new(context: AdapterAccountContext, fixture: DavFixture) -> Self {
        Self {
            context,
            fixture,
            incremental_fixture: None,
        }
    }

    pub fn with_incremental_fixture(mut self, fixture: IncrementalFixture) -> Self {
        self.incremental_fixture = Some(fixture);
        self
    }
}

impl ReadOnlyCalendarSource for OfflineCalDavAdapter {
    fn account_id(&self) -> &str {
        &self.context.account_id
    }

    fn collect_full_inventory(&self) -> Result<AccountInventoryStage, ProviderReadError> {
        let base_url = validate_redirect_chain(
            &self.fixture.entry_url,
            &self.fixture.redirect_chain,
            &self.context.trusted_hosts,
        )?;
        let discovery = parse_discovery(&self.fixture.discovery_xml)?;
        base_url
            .join(&discovery.principal_href)
            .map_err(|_| ProviderReadError::UnsafeData)?;
        base_url
            .join(&discovery.calendar_home_href)
            .map_err(|_| ProviderReadError::UnsafeData)?;

        let discovered = parse_calendars(&self.fixture.calendars_xml)?;
        let expected_calendar_count = discovered
            .iter()
            .filter(|item| {
                item.supports_vevent && self.context.calendar_ids_by_href.contains_key(&item.href)
            })
            .count();
        if self.fixture.calendars.len() != expected_calendar_count {
            return Err(ProviderReadError::Incomplete);
        }
        let mut calendars = Vec::new();
        for calendar in discovered.into_iter().filter(|item| {
            item.supports_vevent && self.context.calendar_ids_by_href.contains_key(&item.href)
        }) {
            let calendar_id = self
                .context
                .calendar_ids_by_href
                .get(&calendar.href)
                .ok_or(ProviderReadError::UnsafeData)?;
            let fixture = self
                .fixture
                .calendars
                .iter()
                .find(|item| item.collection_href == calendar.href)
                .ok_or(ProviderReadError::Incomplete)?;
            let inventory = parse_inventory(&fixture.inventory_xml, &calendar.href)?;
            let fetched: BTreeMap<&str, &FetchedResource> = fixture
                .fetched_resources
                .iter()
                .map(|resource| (resource.href.as_str(), resource))
                .collect();
            if fetched.len() != fixture.fetched_resources.len() {
                return Err(ProviderReadError::UnsafeData);
            }
            if fetched.len() != inventory.len() {
                return Err(ProviderReadError::Incomplete);
            }
            let mut resources = Vec::with_capacity(inventory.len());
            for item in inventory {
                let resource = fetched
                    .get(item.href.as_str())
                    .ok_or(ProviderReadError::Incomplete)?;
                if resource.status != 200
                    || !resource
                        .content_type
                        .to_ascii_lowercase()
                        .starts_with("text/calendar")
                    || resource.body.is_empty()
                {
                    return Err(ProviderReadError::Incomplete);
                }
                let uid = extract_single_uid(&resource.body)?;
                resources.push(StagedResource {
                    href: item.href,
                    etag: staged_revision(resource.etag.as_deref(), &item.etag)?,
                    content_type: resource.content_type.clone(),
                    raw_ics: resource.body.clone(),
                    parse_status: mutation_parse_status(&resource.body, &uid),
                    uid,
                });
            }
            calendars.push(CalendarInventoryStage {
                calendar_id: calendar_id.clone(),
                collection_url: base_url
                    .join(&calendar.href)
                    .map_err(|_| ProviderReadError::UnsafeData)?
                    .to_string(),
                display_name: calendar.display_name,
                server_color: calendar.color,
                privileges: calendar.privileges.join(","),
                sync_token: fixture.sync_token.clone(),
                resources,
            });
        }
        if calendars.is_empty() {
            return Err(ProviderReadError::Incomplete);
        }
        Ok(AccountInventoryStage {
            account_id: self.context.account_id.clone(),
            provider_kind: self.context.provider_kind.clone(),
            user_label: self.context.user_label.clone(),
            credential_ref: self.context.credential_ref.clone(),
            region_profile: self.context.region_profile.clone(),
            complete: true,
            authoritative: true,
            observed_at_ms: self.fixture.observed_at_ms,
            calendars,
        })
    }

    fn collect_incremental(
        &self,
        requests: &[IncrementalSyncRequest],
    ) -> Result<AccountIncrementalStage, ProviderReadError> {
        validate_redirect_chain(
            &self.fixture.entry_url,
            &self.fixture.redirect_chain,
            &self.context.trusted_hosts,
        )?;
        let fixture = self
            .incremental_fixture
            .as_ref()
            .ok_or(ProviderReadError::Incomplete)?;
        if !fixture.complete || !fixture.authoritative || fixture.calendars.len() != requests.len()
        {
            return Err(ProviderReadError::Incomplete);
        }
        let mut calendars = Vec::with_capacity(fixture.calendars.len());
        for calendar_fixture in &fixture.calendars {
            let calendar_id = self
                .context
                .calendar_ids_by_href
                .get(&calendar_fixture.collection_href)
                .ok_or(ProviderReadError::UnsafeData)?;
            let request = requests
                .iter()
                .find(|request| request.calendar_id == *calendar_id)
                .ok_or(ProviderReadError::Incomplete)?;
            if request.sync_token != calendar_fixture.request_token {
                return Err(ProviderReadError::InvalidSyncToken);
            }
            if matches!(calendar_fixture.response_status, 403 | 409) {
                return Err(ProviderReadError::InvalidSyncToken);
            }
            if calendar_fixture.response_status != 207 {
                return Err(ProviderReadError::Incomplete);
            }
            let absolute_collection = Url::parse(&self.fixture.entry_url)
                .ok()
                .filter(|base| super::icloud_caldav::validate_url(base).is_ok())
                .map(|base| super::icloud_caldav::resolve(&base, &calendar_fixture.collection_href))
                .transpose()
                .map_err(map_execution_error)?;
            let parsed = parse_incremental(
                &calendar_fixture.response_xml,
                absolute_collection
                    .as_ref()
                    .map(Url::as_str)
                    .unwrap_or(&calendar_fixture.collection_href),
            )?;
            let fetched: BTreeMap<&str, &FetchedResource> = calendar_fixture
                .fetched_resources
                .iter()
                .map(|resource| (resource.href.as_str(), resource))
                .collect();
            if fetched.len() != calendar_fixture.fetched_resources.len()
                || fetched.len() != parsed.changed.len()
            {
                return Err(ProviderReadError::Incomplete);
            }
            let mut changed_resources = Vec::with_capacity(parsed.changed.len());
            for item in parsed.changed {
                let resource = fetched
                    .get(item.href.as_str())
                    .ok_or(ProviderReadError::Incomplete)?;
                if resource.status != 200
                    || !resource
                        .content_type
                        .to_ascii_lowercase()
                        .starts_with("text/calendar")
                    || resource.body.is_empty()
                {
                    return Err(ProviderReadError::Incomplete);
                }
                let uid = extract_single_uid(&resource.body)?;
                changed_resources.push(StagedResource {
                    href: item.href,
                    etag: staged_revision(resource.etag.as_deref(), &item.etag)?,
                    content_type: resource.content_type.clone(),
                    raw_ics: resource.body.clone(),
                    parse_status: mutation_parse_status(&resource.body, &uid),
                    uid,
                });
            }
            calendars.push(CalendarIncrementalStage {
                calendar_id: calendar_id.clone(),
                base_sync_token: request.sync_token.clone(),
                new_sync_token: parsed.sync_token,
                changed_resources,
                removed_hrefs: parsed.removed,
            });
        }
        Ok(AccountIncrementalStage {
            account_id: self.context.account_id.clone(),
            complete: true,
            authoritative: true,
            observed_at_ms: fixture.observed_at_ms,
            calendars,
        })
    }
}

fn validate_redirect_chain(
    entry: &str,
    redirects: &[String],
    trusted_hosts: &BTreeSet<String>,
) -> Result<Url, ProviderReadError> {
    if redirects.len() > 5 {
        return Err(ProviderReadError::UnsafeData);
    }
    let mut current = Url::parse(entry).map_err(|_| ProviderReadError::UnsafeData)?;
    validate_trusted_url(&current, trusted_hosts)?;
    for value in redirects {
        let candidate = Url::parse(value).map_err(|_| ProviderReadError::UnsafeData)?;
        validate_trusted_url(&candidate, trusted_hosts)?;
        current = candidate;
    }
    Ok(current)
}

fn validate_trusted_url(
    candidate: &Url,
    trusted_hosts: &BTreeSet<String>,
) -> Result<(), ProviderReadError> {
    let host = candidate.host_str().ok_or(ProviderReadError::UnsafeData)?;
    if candidate.scheme() != "https" || !trusted_hosts.contains(host) {
        return Err(ProviderReadError::UnsafeData);
    }
    Ok(())
}

#[derive(Default)]
struct Discovery {
    principal_href: String,
    calendar_home_href: String,
}

fn parse_discovery(bytes: &[u8]) -> Result<Discovery, ProviderReadError> {
    let result = parse_discovery_relaxed(bytes)?;
    if result.calendar_home_href.is_empty() {
        return Err(ProviderReadError::Incomplete);
    }
    Ok(result)
}

fn parse_discovery_relaxed(bytes: &[u8]) -> Result<Discovery, ProviderReadError> {
    let result = parse_discovery_document(bytes)?;
    if result.principal_href.is_empty() {
        return Err(ProviderReadError::Incomplete);
    }
    Ok(result)
}

#[derive(Default)]
struct DiscoveryShape {
    responses: u32,
    propstats: u32,
    statuses: u32,
    principal_properties: u32,
    home_properties: u32,
    malformed: bool,
}

fn discovery_shape(bytes: &[u8]) -> DiscoveryShape {
    let mut reader = Reader::from_reader(bytes);
    let mut shape = DiscoveryShape::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                let Ok(name) = local_name(event.local_name().as_ref()) else {
                    shape.malformed = true;
                    break;
                };
                match name.as_str() {
                    "response" => shape.responses = shape.responses.saturating_add(1),
                    "propstat" => shape.propstats = shape.propstats.saturating_add(1),
                    "status" => shape.statuses = shape.statuses.saturating_add(1),
                    "current-user-principal" | "principal-URL" => {
                        shape.principal_properties = shape.principal_properties.saturating_add(1);
                    }
                    "calendar-home-set" => {
                        shape.home_properties = shape.home_properties.saturating_add(1);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                shape.malformed = true;
                break;
            }
            _ => {}
        }
    }
    shape
}

#[derive(Default)]
struct CalendarShape {
    responses: u32,
    propstats: u32,
    statuses: u32,
    hrefs: u32,
    calendars: u32,
    supported_components: u32,
    privileges: u32,
    display_names: u32,
    colors: u32,
    successful_propstats: u32,
    failed_propstats: u32,
    failed_resourcetypes: u32,
    failed_supported_components: u32,
    failed_privilege_sets: u32,
    failed_display_names: u32,
    failed_colors: u32,
    malformed: bool,
}

fn calendar_shape(bytes: &[u8]) -> CalendarShape {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut shape = CalendarShape::default();
    let mut stack = Vec::<String>::new();
    let mut propstat_status = None;
    let mut propstat_properties = [false; 5];
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let Ok(name) = local_name(event.local_name().as_ref()) else {
                    shape.malformed = true;
                    break;
                };
                let counter = match name.as_str() {
                    "response" => Some(&mut shape.responses),
                    "propstat" => Some(&mut shape.propstats),
                    "status" => Some(&mut shape.statuses),
                    "href" => Some(&mut shape.hrefs),
                    "calendar" => Some(&mut shape.calendars),
                    "supported-calendar-component-set" => Some(&mut shape.supported_components),
                    "privilege" => Some(&mut shape.privileges),
                    "displayname" => Some(&mut shape.display_names),
                    "calendar-color" => Some(&mut shape.colors),
                    _ => None,
                };
                if let Some(counter) = counter {
                    *counter = counter.saturating_add(1);
                }
                if name == "propstat" {
                    propstat_status = None;
                    propstat_properties = [false; 5];
                } else if stack.iter().any(|part| part == "propstat") {
                    mark_calendar_shape_property(&name, &mut propstat_properties);
                }
                stack.push(name);
            }
            Ok(Event::Empty(event)) => {
                let Ok(name) = local_name(event.local_name().as_ref()) else {
                    shape.malformed = true;
                    break;
                };
                let counter = match name.as_str() {
                    "response" => Some(&mut shape.responses),
                    "propstat" => Some(&mut shape.propstats),
                    "status" => Some(&mut shape.statuses),
                    "href" => Some(&mut shape.hrefs),
                    "calendar" => Some(&mut shape.calendars),
                    "supported-calendar-component-set" => Some(&mut shape.supported_components),
                    "privilege" => Some(&mut shape.privileges),
                    "displayname" => Some(&mut shape.display_names),
                    "calendar-color" => Some(&mut shape.colors),
                    _ => None,
                };
                if let Some(counter) = counter {
                    *counter = counter.saturating_add(1);
                }
                if stack.iter().any(|part| part == "propstat") {
                    mark_calendar_shape_property(&name, &mut propstat_properties);
                }
            }
            Ok(Event::Text(text)) if stack.last().is_some_and(|name| name == "status") => {
                if stack
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|name| name == "propstat")
                {
                    let value = text.xml_content(XmlVersion::Implicit1_0).into_owned();
                    propstat_status = Some(dav_status_code(&value));
                }
            }
            Ok(Event::End(event)) => {
                let Ok(name) = local_name(event.local_name().as_ref()) else {
                    shape.malformed = true;
                    break;
                };
                if name == "propstat" {
                    if propstat_status == Some(200) {
                        shape.successful_propstats = shape.successful_propstats.saturating_add(1);
                    } else {
                        shape.failed_propstats = shape.failed_propstats.saturating_add(1);
                        let counters = [
                            &mut shape.failed_resourcetypes,
                            &mut shape.failed_supported_components,
                            &mut shape.failed_privilege_sets,
                            &mut shape.failed_display_names,
                            &mut shape.failed_colors,
                        ];
                        for (present, counter) in propstat_properties.into_iter().zip(counters) {
                            if present {
                                *counter = counter.saturating_add(1);
                            }
                        }
                    }
                }
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                shape.malformed = true;
                break;
            }
            _ => {}
        }
    }
    shape
}

fn mark_calendar_shape_property(name: &str, properties: &mut [bool; 5]) {
    let index = match name {
        "resourcetype" => Some(0),
        "supported-calendar-component-set" => Some(1),
        "current-user-privilege-set" => Some(2),
        "displayname" => Some(3),
        "calendar-color" => Some(4),
        _ => None,
    };
    if let Some(index) = index {
        properties[index] = true;
    }
}

fn parse_discovery_document(bytes: &[u8]) -> Result<Discovery, ProviderReadError> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<String>::new();
    let mut result = Discovery::default();
    let mut propstat_status = None;
    let mut propstat = Discovery::default();
    let mut response = Discovery::default();
    let mut response_status = None;
    let mut identity_response_seen = false;
    let mut saw_multistatus = false;
    let mut closed_multistatus = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                saw_multistatus |= name == "multistatus";
                if name == "response" {
                    response = Discovery::default();
                    response_status = None;
                } else if name == "propstat" {
                    propstat_status = None;
                    propstat = Discovery::default();
                }
                stack.push(name);
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                closed_multistatus |= name == "multistatus";
                if name == "propstat"
                    && (!propstat.principal_href.is_empty()
                        || !propstat.calendar_home_href.is_empty())
                {
                    if propstat_status != Some(200) {
                        return Err(ProviderReadError::Incomplete);
                    }
                    if (!response.principal_href.is_empty() && !propstat.principal_href.is_empty())
                        || (!response.calendar_home_href.is_empty()
                            && !propstat.calendar_home_href.is_empty())
                    {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    if !propstat.principal_href.is_empty() {
                        response.principal_href = std::mem::take(&mut propstat.principal_href);
                    }
                    if !propstat.calendar_home_href.is_empty() {
                        response.calendar_home_href =
                            std::mem::take(&mut propstat.calendar_home_href);
                    }
                }
                if name == "response"
                    && (!response.principal_href.is_empty()
                        || !response.calendar_home_href.is_empty())
                {
                    if response_status.is_some_and(|status| status != 200) {
                        return Err(ProviderReadError::Incomplete);
                    }
                    if identity_response_seen {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    identity_response_seen = true;
                    result = std::mem::take(&mut response);
                }
                stack.pop();
            }
            Ok(Event::Text(text)) if stack.last().is_some_and(|name| name == "status") => {
                let value = text.xml_content(XmlVersion::Implicit1_0).into_owned();
                match stack.iter().rev().nth(1).map(String::as_str) {
                    Some("response") => {
                        if response_status.replace(dav_status_code(&value)).is_some() {
                            return Err(ProviderReadError::UnsafeData);
                        }
                    }
                    Some("propstat")
                        if propstat_status.replace(dav_status_code(&value)).is_some() =>
                    {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) if stack.last().is_some_and(|name| name == "href") => {
                let value = text.xml_content(XmlVersion::Implicit1_0).into_owned();
                match stack.iter().rev().nth(1).map(String::as_str) {
                    Some("current-user-principal" | "principal-URL") => {
                        if !propstat.principal_href.is_empty() && propstat.principal_href != value {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        if propstat.principal_href.is_empty() {
                            propstat.principal_href = value;
                        }
                    }
                    Some("calendar-home-set") => {
                        if !propstat.calendar_home_href.is_empty() {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        propstat.calendar_home_href = value;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ProviderReadError::UnsafeData),
            _ => {}
        }
    }
    if !saw_multistatus || !closed_multistatus || !stack.is_empty() {
        return Err(ProviderReadError::Incomplete);
    }
    Ok(result)
}

#[derive(Default)]
struct DiscoveredCalendar {
    href: String,
    display_name: String,
    color: Option<String>,
    privileges: Vec<String>,
    supports_vevent: bool,
    is_calendar: bool,
    is_scheduling_collection: bool,
}

fn parse_calendars(bytes: &[u8]) -> Result<Vec<DiscoveredCalendar>, ProviderReadError> {
    parse_calendars_detailed(bytes).map_err(|failure| failure.class)
}

struct CalendarParseFailure {
    class: ProviderReadError,
    checkpoint: &'static str,
}

fn parse_calendars_detailed(bytes: &[u8]) -> Result<Vec<DiscoveredCalendar>, CalendarParseFailure> {
    macro_rules! fail {
        ($class:expr, $checkpoint:literal) => {
            return Err(CalendarParseFailure {
                class: $class,
                checkpoint: $checkpoint,
            })
        };
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<String>::new();
    let mut current: Option<DiscoveredCalendar> = None;
    let mut propstat = DiscoveredCalendar::default();
    let mut href_seen = false;
    let mut propstat_status = None;
    let mut propstat_has_used_property = false;
    let mut failed_used_propstat = false;
    let mut successful_propstat = false;
    let mut calendars = Vec::new();
    let mut seen_hrefs = BTreeSet::new();
    let mut saw_multistatus = false;
    let mut closed_multistatus = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref()).map_err(|class| {
                    CalendarParseFailure {
                        class,
                        checkpoint: "start_name",
                    }
                })?;
                saw_multistatus |= name == "multistatus";
                if name == "response" {
                    current = Some(DiscoveredCalendar::default());
                    href_seen = false;
                    failed_used_propstat = false;
                    successful_propstat = false;
                } else if name == "propstat" {
                    propstat_status = None;
                    propstat_has_used_property = false;
                    propstat = DiscoveredCalendar::default();
                }
                if matches!(
                    name.as_str(),
                    "resourcetype"
                        | "supported-calendar-component-set"
                        | "current-user-privilege-set"
                        | "displayname"
                        | "calendar-color"
                ) && stack.iter().any(|part| part == "propstat")
                {
                    propstat_has_used_property = true;
                }
                if name == "comp"
                    && stack
                        .iter()
                        .any(|part| part == "supported-calendar-component-set")
                {
                    for attribute in event.attributes() {
                        let attribute = attribute.map_err(|_| CalendarParseFailure {
                            class: ProviderReadError::UnsafeData,
                            checkpoint: "component_attribute",
                        })?;
                        if attribute.key.local_name().as_ref() == "name"
                            && attribute.value.as_ref().eq_ignore_ascii_case("VEVENT")
                        {
                            propstat.supports_vevent = true;
                        }
                    }
                }
                if stack.iter().any(|part| part == "resourcetype") {
                    match name.as_str() {
                        "calendar" => propstat.is_calendar = true,
                        "schedule-inbox" | "schedule-outbox" => {
                            propstat.is_scheduling_collection = true
                        }
                        _ => {}
                    }
                }
                if stack
                    .iter()
                    .any(|part| part == "current-user-privilege-set")
                    && matches!(name.as_str(), "read" | "write" | "write-content")
                {
                    propstat.privileges.push(name.clone());
                }
                stack.push(name);
            }
            Ok(Event::Empty(event)) => {
                let name = local_name(event.local_name().as_ref()).map_err(|class| {
                    CalendarParseFailure {
                        class,
                        checkpoint: "empty_name",
                    }
                })?;
                if matches!(
                    name.as_str(),
                    "resourcetype"
                        | "supported-calendar-component-set"
                        | "current-user-privilege-set"
                        | "displayname"
                        | "calendar-color"
                ) && stack.iter().any(|part| part == "propstat")
                {
                    propstat_has_used_property = true;
                }
                if name == "comp"
                    && stack
                        .iter()
                        .any(|part| part == "supported-calendar-component-set")
                {
                    for attribute in event.attributes() {
                        let attribute = attribute.map_err(|_| CalendarParseFailure {
                            class: ProviderReadError::UnsafeData,
                            checkpoint: "empty_component_attribute",
                        })?;
                        if attribute.key.local_name().as_ref() == "name"
                            && attribute.value.as_ref().eq_ignore_ascii_case("VEVENT")
                        {
                            propstat.supports_vevent = true;
                        }
                    }
                }
                if stack.iter().any(|part| part == "resourcetype") {
                    match name.as_str() {
                        "calendar" => propstat.is_calendar = true,
                        "schedule-inbox" | "schedule-outbox" => {
                            propstat.is_scheduling_collection = true
                        }
                        _ => {}
                    }
                }
                if stack
                    .iter()
                    .any(|part| part == "current-user-privilege-set")
                    && matches!(name.as_str(), "read" | "write" | "write-content")
                {
                    propstat.privileges.push(name);
                }
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref()).map_err(|class| {
                    CalendarParseFailure {
                        class,
                        checkpoint: "end_name",
                    }
                })?;
                closed_multistatus |= name == "multistatus";
                if name == "propstat" {
                    if propstat_status == Some(200) {
                        successful_propstat = true;
                        if let Some(calendar) = current.as_mut() {
                            merge_calendar_propstat(calendar, &mut propstat).map_err(|class| {
                                CalendarParseFailure {
                                    class,
                                    checkpoint: "successful_propstat_merge",
                                }
                            })?;
                        }
                    } else if propstat_has_used_property {
                        failed_used_propstat = true;
                    }
                }
                stack.pop();
                if name == "response"
                    && let Some(mut calendar) = current.take()
                {
                    if calendar.href.is_empty() {
                        fail!(ProviderReadError::Incomplete, "missing_response_href");
                    }
                    if !seen_hrefs.insert(calendar.href.clone()) {
                        fail!(ProviderReadError::UnsafeData, "duplicate_response_href");
                    }
                    if !successful_propstat {
                        fail!(ProviderReadError::Incomplete, "missing_successful_propstat");
                    }
                    calendar.privileges.sort();
                    calendar.privileges.dedup();
                    if calendar.is_calendar
                        && calendar.supports_vevent
                        && !calendar.is_scheduling_collection
                    {
                        if failed_used_propstat {
                            fail!(ProviderReadError::Incomplete, "selected_failed_propstat");
                        }
                        calendars.push(calendar);
                    }
                }
            }
            Ok(Event::Text(text)) => {
                let value = text.xml_content(XmlVersion::Implicit1_0).into_owned();
                if let Some(calendar) = current.as_mut() {
                    match stack.last().map(String::as_str) {
                        Some("href")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|name| name == "response") =>
                        {
                            if href_seen {
                                fail!(ProviderReadError::UnsafeData, "repeated_response_href");
                            }
                            href_seen = true;
                            calendar.href = value;
                        }
                        Some("displayname") => propstat.display_name = value,
                        Some("status")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|part| part == "response") =>
                        {
                            if dav_status_code(&value) != 200 {
                                fail!(ProviderReadError::Incomplete, "failed_response_status");
                            }
                        }
                        Some("status")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|part| part == "propstat") =>
                        {
                            if propstat_status.replace(dav_status_code(&value)).is_some() {
                                fail!(ProviderReadError::UnsafeData, "repeated_propstat_status");
                            }
                        }
                        Some("calendar-color") if valid_color(&value) => {
                            propstat.color = Some(value)
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => fail!(ProviderReadError::UnsafeData, "xml_reader"),
            _ => {}
        }
    }
    if !saw_multistatus || !closed_multistatus || !stack.is_empty() {
        fail!(ProviderReadError::UnsafeData, "document_boundary");
    }
    Ok(calendars)
}

fn merge_calendar_propstat(
    calendar: &mut DiscoveredCalendar,
    propstat: &mut DiscoveredCalendar,
) -> Result<(), ProviderReadError> {
    if !propstat.display_name.is_empty() {
        if !calendar.display_name.is_empty() && calendar.display_name != propstat.display_name {
            return Err(ProviderReadError::UnsafeData);
        }
        calendar.display_name = std::mem::take(&mut propstat.display_name);
    }
    if let Some(color) = propstat.color.take() {
        if calendar
            .color
            .as_ref()
            .is_some_and(|current| current != &color)
        {
            return Err(ProviderReadError::UnsafeData);
        }
        calendar.color = Some(color);
    }
    calendar.privileges.append(&mut propstat.privileges);
    calendar.supports_vevent |= propstat.supports_vevent;
    calendar.is_calendar |= propstat.is_calendar;
    calendar.is_scheduling_collection |= propstat.is_scheduling_collection;
    Ok(())
}

struct InventoryItem {
    href: String,
    etag: String,
}

#[derive(Default)]
struct InventoryResponse {
    href: String,
    href_seen: bool,
    etag: String,
    successful: bool,
    successful_propstat: bool,
    response_status: Option<u16>,
    propstat: Option<ParsedPropstat>,
}

#[derive(Default)]
struct ParsedPropstat {
    etag: String,
    etag_seen: bool,
    status: u16,
}

/// Emit one semantic text value per element, immediately before its end event.
/// quick-xml emits entity references separately from surrounding text; those
/// fragments must not become separate DAV identity/revision observations.
struct DavElementReader<'a> {
    reader: Reader<&'a [u8]>,
    text: Vec<(String, String)>,
    pending_end: Option<Event<'a>>,
}

impl<'a> DavElementReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let mut reader = Reader::from_reader(bytes);
        reader.config_mut().expand_empty_elements = true;
        Self {
            reader,
            text: Vec::new(),
            pending_end: None,
        }
    }

    fn read_event(&mut self) -> Result<Event<'a>, ProviderReadError> {
        if let Some(event) = self.pending_end.take() {
            return Ok(event);
        }
        loop {
            let event = self
                .reader
                .read_event()
                .map_err(|_| ProviderReadError::UnsafeData)?;
            match event {
                Event::Start(ref element) => {
                    if self.text.last().is_some_and(|(name, _)| {
                        matches!(name.as_str(), "href" | "getetag" | "status" | "sync-token")
                    }) {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    self.text
                        .push((local_name(element.local_name().as_ref())?, String::new()));
                    return Ok(event);
                }
                Event::Text(value) => self.append(&value.xml_content(XmlVersion::Implicit1_0))?,
                Event::CData(value) => self.append(&value.xml_content(XmlVersion::Implicit1_0))?,
                Event::GeneralRef(reference) => {
                    let value = reference
                        .resolve_char_ref()
                        .map_err(|_| ProviderReadError::UnsafeData)?
                        .or_else(|| match reference.as_ref() {
                            "quot" => Some('"'),
                            "apos" => Some('\''),
                            "amp" => Some('&'),
                            "lt" => Some('<'),
                            "gt" => Some('>'),
                            _ => None,
                        })
                        .ok_or(ProviderReadError::UnsafeData)?;
                    self.append(&value.to_string())?;
                }
                Event::End(_) => {
                    let (_, value) = self.text.pop().ok_or(ProviderReadError::UnsafeData)?;
                    self.pending_end = Some(event);
                    return Ok(Event::Text(quick_xml::events::BytesText::from_escaped(
                        value,
                    )));
                }
                Event::DocType(_) => return Err(ProviderReadError::UnsafeData),
                _ => return Ok(event),
            }
        }
    }

    fn append(&mut self, value: &str) -> Result<(), ProviderReadError> {
        if value.chars().any(|character| {
            !matches!(character as u32,
            0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
        }) {
            return Err(ProviderReadError::UnsafeData);
        }
        if let Some((_, text)) = self.text.last_mut() {
            text.push_str(value);
        } else if !value.trim().is_empty() {
            return Err(ProviderReadError::UnsafeData);
        }
        Ok(())
    }
}

fn normalize_icloud_response_href(
    collection: &str,
    href: &str,
) -> Result<String, ProviderReadError> {
    let Ok(base) = Url::parse(collection) else {
        return Ok(href.to_owned());
    };
    if super::icloud_caldav::validate_url(&base).is_err() {
        return Ok(href.to_owned());
    }
    let target = super::icloud_caldav::resolve(&base, href).map_err(map_execution_error)?;
    if target.origin() != base.origin() {
        return Err(ProviderReadError::UnsafeData);
    }
    Ok(target.path().to_owned())
}

fn parse_inventory(
    bytes: &[u8],
    collection_href: &str,
) -> Result<Vec<InventoryItem>, ProviderReadError> {
    let mut reader = DavElementReader::new(bytes);
    let mut stack = Vec::<String>::new();
    let mut current: Option<InventoryResponse> = None;
    let mut items = Vec::new();
    let mut saw_collection_self = false;
    let mut saw_multistatus = false;
    let mut closed_multistatus = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                if name == "multistatus" {
                    saw_multistatus = true;
                } else if name == "response" {
                    current = Some(InventoryResponse::default());
                } else if name == "propstat"
                    && let Some(response) = current.as_mut()
                {
                    response.propstat = Some(ParsedPropstat::default());
                }
                stack.push(name);
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                closed_multistatus |= name == "multistatus";
                if name == "propstat"
                    && let Some(response) = current.as_mut()
                    && let Some(propstat) = response.propstat.take()
                    && propstat.status == 200
                {
                    response.successful_propstat = true;
                    if !propstat.etag.is_empty() {
                        if response.successful && response.etag != propstat.etag {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        response.etag = propstat.etag;
                        response.successful = true;
                    }
                }
                stack.pop();
                if name == "response"
                    && let Some(mut response) = current.take()
                {
                    response.href =
                        normalize_icloud_response_href(collection_href, &response.href)?;
                    if crate::domain::remote_mutation::collection_routes_match(
                        &response.href,
                        collection_href,
                    ) {
                        if saw_collection_self {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        saw_collection_self = true;
                        if !matches!(response.response_status, Some(200))
                            && !(response.response_status.is_none() && response.successful_propstat)
                        {
                            return Err(ProviderReadError::Incomplete);
                        }
                        continue;
                    }
                    if !response.successful
                        || response.href.is_empty()
                        || response.etag.is_empty()
                        || !matches!(response.response_status, None | Some(200))
                    {
                        return Err(ProviderReadError::Incomplete);
                    }
                    if !resource_belongs_to_collection(collection_href, &response.href) {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    items.push(InventoryItem {
                        href: response.href,
                        etag: response.etag,
                    });
                }
            }
            Ok(Event::Text(text)) => {
                let value = text.as_ref().to_owned();
                if let Some(response) = current.as_mut() {
                    match stack.last().map(String::as_str) {
                        Some("href")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|name| name == "response") =>
                        {
                            if response.href_seen {
                                return Err(ProviderReadError::UnsafeData);
                            }
                            response.href_seen = true;
                            response.href = value;
                        }
                        Some("getetag") if stack.iter().any(|name| name == "propstat") => {
                            if let Some(propstat) = response.propstat.as_mut() {
                                if propstat.etag_seen {
                                    return Err(ProviderReadError::UnsafeData);
                                }
                                propstat.etag_seen = true;
                                propstat.etag = value;
                            }
                        }
                        Some("status")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|name| name == "propstat") =>
                        {
                            if let Some(propstat) = response.propstat.as_mut() {
                                propstat.status = dav_status_code(&value);
                            }
                        }
                        Some("status")
                            if stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|name| name == "response") =>
                        {
                            if response.response_status.is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                            response.response_status = Some(dav_status_code(&value));
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ProviderReadError::UnsafeData),
            _ => {}
        }
    }
    if !saw_multistatus || !closed_multistatus || !stack.is_empty() {
        return Err(ProviderReadError::UnsafeData);
    }
    if !saw_collection_self {
        return Err(ProviderReadError::Incomplete);
    }
    Ok(items)
}

#[derive(Default)]
struct IncrementalResponse {
    href: String,
    href_seen: bool,
    etag: String,
    response_status: Option<u16>,
    propstat: Option<ParsedPropstat>,
    saw_propstat: bool,
}

struct ParsedIncremental {
    sync_token: String,
    changed: Vec<InventoryItem>,
    removed: Vec<String>,
}

fn parse_incremental(
    bytes: &[u8],
    collection_href: &str,
) -> Result<ParsedIncremental, ProviderReadError> {
    let mut reader = DavElementReader::new(bytes);
    let mut stack = Vec::<String>::new();
    let mut current: Option<IncrementalResponse> = None;
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut sync_token: Option<String> = None;
    let mut saw_multistatus = false;
    let mut closed_multistatus = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                saw_multistatus |= name == "multistatus";
                if name == "response" {
                    current = Some(IncrementalResponse::default());
                } else if name == "propstat"
                    && let Some(response) = current.as_mut()
                {
                    response.propstat = Some(ParsedPropstat::default());
                    response.saw_propstat = true;
                }
                stack.push(name);
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                closed_multistatus |= name == "multistatus";
                if name == "propstat"
                    && let Some(response) = current.as_mut()
                    && let Some(propstat) = response.propstat.take()
                    && propstat.status == 200
                    && !propstat.etag.is_empty()
                {
                    if !response.etag.is_empty() && response.etag != propstat.etag {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    response.etag = propstat.etag;
                }
                stack.pop();
                if name == "response"
                    && let Some(mut response) = current.take()
                {
                    response.href =
                        normalize_icloud_response_href(collection_href, &response.href)?;
                    if response.href.is_empty() {
                        return Err(ProviderReadError::Incomplete);
                    }
                    if !resource_belongs_to_collection(collection_href, &response.href) {
                        return Err(ProviderReadError::UnsafeData);
                    }
                    match response.response_status {
                        None | Some(200) if !response.etag.is_empty() => {
                            changed.push(InventoryItem {
                                href: response.href,
                                etag: response.etag,
                            })
                        }
                        Some(404 | 410) if !response.saw_propstat => removed.push(response.href),
                        _ => return Err(ProviderReadError::Incomplete),
                    }
                }
            }
            Ok(Event::Text(text)) => {
                let value = text.as_ref().to_owned();
                match stack.last().map(String::as_str) {
                    Some("sync-token") => {
                        if !stack
                            .iter()
                            .rev()
                            .nth(1)
                            .is_some_and(|name| name == "multistatus")
                            || sync_token.replace(value).is_some()
                        {
                            return Err(ProviderReadError::UnsafeData);
                        }
                    }
                    Some("href")
                        if current.is_some()
                            && stack
                                .iter()
                                .rev()
                                .nth(1)
                                .is_some_and(|name| name == "response") =>
                    {
                        let response = current.as_mut().unwrap();
                        if response.href_seen {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        response.href_seen = true;
                        response.href = value;
                    }
                    Some("getetag") if stack.iter().any(|name| name == "propstat") => {
                        if let Some(propstat) = current
                            .as_mut()
                            .and_then(|response| response.propstat.as_mut())
                        {
                            if propstat.etag_seen {
                                return Err(ProviderReadError::UnsafeData);
                            }
                            propstat.etag_seen = true;
                            propstat.etag = value;
                        }
                    }
                    Some("status") if current.is_some() => {
                        match stack.iter().rev().nth(1).map(String::as_str) {
                            Some("response") => {
                                let response = current.as_mut().unwrap();
                                if response.response_status.is_some() {
                                    return Err(ProviderReadError::UnsafeData);
                                }
                                response.response_status = Some(dav_status_code(&value));
                            }
                            Some("propstat") => {
                                if let Some(propstat) = current
                                    .as_mut()
                                    .and_then(|response| response.propstat.as_mut())
                                {
                                    propstat.status = dav_status_code(&value);
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ProviderReadError::UnsafeData),
            _ => {}
        }
    }
    if !saw_multistatus || !closed_multistatus || !stack.is_empty() {
        return Err(ProviderReadError::Incomplete);
    }
    let sync_token = sync_token
        .filter(|token| !token.trim().is_empty())
        .ok_or(ProviderReadError::Incomplete)?;
    Ok(ParsedIncremental {
        sync_token,
        changed,
        removed,
    })
}

fn dav_status_code(value: &str) -> u16 {
    let mut parts = value.split_whitespace();
    if !parts
        .next()
        .is_some_and(|version| version.starts_with("HTTP/"))
    {
        return 0;
    }
    parts.next().and_then(|code| code.parse().ok()).unwrap_or(0)
}

fn parse_optional_sync_token(
    bytes: &[u8],
    collection_href: &str,
) -> Result<Option<String>, ProviderReadError> {
    let mut reader = DavElementReader::new(bytes);
    let mut stack = Vec::<String>::new();
    let mut token: Option<String> = None;
    let mut response_href = String::new();
    let mut response_status = None;
    let mut response_token: Option<String> = None;
    let mut propstat_status = None;
    let mut propstat_token: Option<String> = None;
    let mut closed_multistatus = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                if name == "response" {
                    response_href.clear();
                    response_status = None;
                    response_token = None;
                } else if name == "propstat" {
                    propstat_status = None;
                    propstat_token = None;
                }
                stack.push(name);
            }
            Ok(Event::End(event)) => {
                let name = local_name(event.local_name().as_ref())?;
                closed_multistatus |= name == "multistatus";
                if name == "propstat"
                    && let Some(value) = propstat_token.take()
                {
                    if value.trim().is_empty() {
                        // An unavailable property is represented by an empty
                        // element in a failed propstat, not by a token value.
                        if !matches!(propstat_status, Some(400..=599)) {
                            return Err(ProviderReadError::UnsafeData);
                        }
                    } else if propstat_status != Some(200)
                        || response_token.replace(value).is_some()
                    {
                        return Err(ProviderReadError::Incomplete);
                    }
                }
                if name == "response"
                    && let Some(value) = response_token.take()
                    && (!crate::domain::remote_mutation::collection_routes_match(
                        &normalize_icloud_response_href(collection_href, &response_href)?,
                        collection_href,
                    ) || response_status.is_some_and(|status| status != 200)
                        || token.replace(value).is_some())
                {
                    return Err(ProviderReadError::UnsafeData);
                }
                stack.pop();
            }
            Ok(Event::Text(text)) => {
                let value = text.as_ref().to_owned();
                match stack.last().map(String::as_str) {
                    Some("sync-token") => {
                        if value.len() > 4 * 1024
                            || (value.trim().is_empty()
                                && stack.iter().rev().nth(1).map(String::as_str) != Some("prop"))
                        {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        match stack.iter().rev().nth(1).map(String::as_str) {
                            Some("multistatus") => {
                                if token.replace(value).is_some() {
                                    return Err(ProviderReadError::UnsafeData);
                                }
                            }
                            Some("prop")
                                if stack
                                    .iter()
                                    .rev()
                                    .nth(2)
                                    .is_some_and(|part| part == "propstat") =>
                            {
                                if propstat_token.replace(value).is_some() {
                                    return Err(ProviderReadError::UnsafeData);
                                }
                            }
                            _ => return Err(ProviderReadError::UnsafeData),
                        }
                    }
                    Some("href")
                        if stack
                            .iter()
                            .rev()
                            .nth(1)
                            .is_some_and(|part| part == "response") =>
                    {
                        response_href = value;
                    }
                    Some("status") => match stack.iter().rev().nth(1).map(String::as_str) {
                        Some("response") => {
                            if response_status.replace(dav_status_code(&value)).is_some() {
                                return Err(ProviderReadError::UnsafeData);
                            }
                        }
                        Some("propstat")
                            if propstat_status.replace(dav_status_code(&value)).is_some() =>
                        {
                            return Err(ProviderReadError::UnsafeData);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ProviderReadError::UnsafeData),
            _ => {}
        }
    }
    if !closed_multistatus || !stack.is_empty() {
        return Err(ProviderReadError::Incomplete);
    }
    Ok(token)
}

fn extract_single_uid(bytes: &[u8]) -> Result<String, ProviderReadError> {
    extract_vevent_series_uid(bytes).map_err(|_| ProviderReadError::UnsafeData)
}

fn mutation_parse_status(raw: &[u8], uid: &str) -> ParseStatus {
    if crate::domain::remote_event_update::validate_event_base_v3(raw, uid, false).is_ok() {
        ParseStatus::Parsed
    } else {
        ParseStatus::ReadOnly
    }
}

/// Chooses the revision label stored with one fetched body.
///
/// A provider is free to answer a later read for the same resource with a
/// different label, or to re-serialize its storage while keeping the label. The
/// exact-target GET label always belongs with the GET body, so it wins over the
/// inventory/REPORT label instead of failing the whole account stage. Both
/// candidates stay bounded. Only an entirely absent GET label falls back to the
/// inventory evidence; a present but empty, oversized or control-bearing label
/// is malformed provider data and still fails the stage.
fn staged_revision(fetched: Option<&str>, inventory: &str) -> Result<String, ProviderReadError> {
    let Some(fetched) = fetched else {
        return Ok(inventory.to_owned());
    };
    if fetched.is_empty()
        || fetched.len() > crate::adapters::caldav_transport::MAX_ETAG_BYTES
        || fetched.chars().any(char::is_control)
    {
        return Err(ProviderReadError::UnsafeData);
    }
    Ok(fetched.to_owned())
}

fn local_name(value: &str) -> Result<String, ProviderReadError> {
    if value.is_empty() {
        return Err(ProviderReadError::UnsafeData);
    }
    Ok(value.to_owned())
}

fn valid_color(value: &str) -> bool {
    let bytes = value.trim().as_bytes();
    matches!(bytes.len(), 7 | 9)
        && bytes.first() == Some(&b'#')
        && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}
