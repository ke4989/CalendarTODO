//! Explicit standalone validation, never composed by AppState.
//! Only fixed authorized collections and a newly generated resource are writable.
use crate::{
    adapters::{
        caldav::{
            DiscoveredCalendarCollection, ScriptedCalDavSourceFactory, discover_vevent_collections,
        },
        caldav_http::{HttpDavConfig, HttpDavExecutor},
        caldav_transport::{
            DavAuthentication, DavDepth, DavExecutionError, DavMethod, DavRequest,
            DavTransportPolicy, canonical_mutation_read_back,
        },
        credentials::WindowsCredentialStore,
        persistence::{Database, DiscoveredCalendarRecord},
        production_mutation::ProductionMutationSessions,
    },
    application::{
        account_profiles::{AccountProfileService, SourceAttachmentMode},
        account_registry::AccountSourceRegistry,
        attachment_runtime::{
            AttachmentRuntime, AttachmentState, AttachmentStateEmitter, AttachmentView,
        },
        credentials::CredentialService,
        discovery_runtime::{AccountCalendarDiscovery, DiscoveryError},
        recurrence_runtime::{AgendaRefreshEmitter, AgendaRefreshNotice},
        remote_mutation::{RemoteMutationRunOutcome, RemoteMutationService},
        source_factory::{AttachmentCancellation, SourceAttachError, SourceAttacher},
        sync_runtime::{
            ScheduledSyncDeadline, SyncDeadlineScheduler, SyncExecutionRuntime, SyncExecutionView,
            SyncSchedulerError, SyncStateEmitter,
        },
    },
    domain::{
        account::{ProviderKind, RegisterAccountProfile},
        remote_mutation::{
            QueueRemoteTitleUpdate, RemoteReadBack, patch_single_vevent_title, single_vevent_title,
            single_vevent_uid,
        },
    },
    ports::{
        caldav::CalDavSourceFactory,
        clock::{Clock, SystemClock},
        credentials::{CredentialRef, CredentialStore},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const NAMES: [&str; 2] = ["Calendar TODO Probe", "Calendar TODO Probe 2"];
const ENTRY: &str = "https://p235-caldav.icloud.com.cn/";

#[cfg(windows)]
fn acquire_probe_owner()
-> Result<Arc<crate::adapters::single_instance::InstanceOwner>, &'static str> {
    match crate::adapters::single_instance::acquire_mutation_diagnostic()
        .map_err(|_| "diagnostic_owner_unavailable")?
    {
        crate::adapters::single_instance::InstanceRole::Owner(owner) => Ok(owner),
        crate::adapters::single_instance::InstanceRole::AlreadyRunning => {
            Err("diagnostic_already_running")
        }
    }
}
#[cfg(not(windows))]
fn acquire_probe_owner() -> Result<(), &'static str> {
    Err("windows_required")
}

fn hosts() -> BTreeSet<String> {
    BTreeSet::from([
        "caldav.icloud.com".into(),
        "p235-caldav.icloud.com.cn".into(),
    ])
}

/// Cannot be constructed outside this module. No generic creation API exists.
pub(crate) struct DedicatedProbeCreateRequest {
    url: Url,
    body: Zeroizing<Vec<u8>>,
}
impl DedicatedProbeCreateRequest {
    pub(crate) fn new(
        policy: &DavTransportPolicy,
        collection: &str,
        uid: Uuid,
    ) -> Result<Self, DavExecutionError> {
        let entry = Url::parse(ENTRY).map_err(|_| DavExecutionError::InvalidRequest)?;
        let href = probe_href(collection, uid)?;
        policy.resolve_exact_mutation_target(&entry, collection, &href)?;
        let url = entry
            .join(&href)
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        let body = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Calendar TODO//Dedicated M5.1 Probe//EN\r\nBEGIN:VEVENT\r\nUID:calendar-todo-probe-{uid}\r\nDTSTAMP:20260927T000000Z\r\nDTSTART:20260928T010000Z\r\nDTEND:20260928T020000Z\r\nSUMMARY:Calendar TODO dedicated probe\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").into_bytes();
        patch_single_vevent_title(&body, "Calendar TODO dedicated probe changed")
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        Ok(Self {
            url,
            body: Zeroizing::new(body),
        })
    }
    pub(crate) fn into_parts(mut self) -> (Url, Vec<u8>) {
        (self.url.clone(), std::mem::take(&mut *self.body))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DedicatedProbeDeleteOutcome {
    ConfirmedAbsent,
    Retained,
}

fn dedicated_delete_request(
    policy: &DavTransportPolicy,
    collection: &str,
    href: &str,
    etag: String,
) -> Result<crate::adapters::caldav_transport::ConditionalEventDeleteRequest, DavExecutionError> {
    let entry = Url::parse(ENTRY).map_err(|_| DavExecutionError::InvalidRequest)?;
    policy.prepare_conditional_delete(&entry, collection, href, etag, 30_000)
}

/// Sends DELETE at most once, then verifies the same exact target with GET for
/// every transport or HTTP outcome. An uncertain result is never retried.
#[allow(clippy::too_many_arguments)]
fn delete_once_then_get(
    policy: &DavTransportPolicy,
    executor: &HttpDavExecutor,
    authentication: &DavAuthentication,
    request: crate::adapters::caldav_transport::ConditionalEventDeleteRequest,
    collection: &str,
    href: &str,
    expected_uid: &str,
    accept: &dyn Fn() -> bool,
) -> Result<DedicatedProbeDeleteOutcome, &'static str> {
    let delete = executor.execute_conditional_delete_guarded(request, authentication, accept);
    let accepted = delete.as_ref().is_ok_and(|response| {
        matches!(response.status(), 200 | 204) && response.location().is_none()
    });
    println!("mutation_gate stage=delete attempted=true accepted={accepted} attempts=1");

    verify_deleted_target(
        policy,
        executor,
        authentication,
        collection,
        href,
        expected_uid,
    )
}

fn verify_deleted_target(
    policy: &DavTransportPolicy,
    executor: &HttpDavExecutor,
    authentication: &DavAuthentication,
    collection: &str,
    href: &str,
    expected_uid: &str,
) -> Result<DedicatedProbeDeleteOutcome, &'static str> {
    let entry = Url::parse(ENTRY).map_err(|_| "target_unsafe")?;
    policy
        .resolve_exact_mutation_target(&entry, collection, href)
        .map_err(|_| "target_unsafe")?;
    let url = entry.join(href).map_err(|_| "target_unsafe")?;
    let get = DavRequest::new(
        DavMethod::Get,
        url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| "target_unsafe")?;
    let response = policy
        .execute(executor, get, authentication)
        .map_err(|_| "delete_verification_unavailable")?;
    if response.final_url() != &url {
        return Err("delete_verification_wrong_target");
    }
    match response.status() {
        404 | 410 => Ok(DedicatedProbeDeleteOutcome::ConfirmedAbsent),
        200 => {
            let remote = canonical_mutation_read_back(response, &url)
                .map_err(|_| "delete_verification_unsafe")?;
            if single_vevent_uid(&remote.raw_ics).map_err(|_| "delete_verification_unsafe")?
                != expected_uid
            {
                return Err("delete_verification_identity_mismatch");
            }
            Ok(DedicatedProbeDeleteOutcome::Retained)
        }
        _ => Err("delete_verification_unavailable"),
    }
}

fn probe_href(collection: &str, uid: Uuid) -> Result<String, DavExecutionError> {
    let path = if collection.starts_with('/') {
        collection.to_owned()
    } else {
        let url = Url::parse(collection).map_err(|_| DavExecutionError::InvalidRequest)?;
        if url.origin() != Url::parse(ENTRY).unwrap().origin()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        url.path().to_owned()
    };
    if !path.ends_with('/') {
        return Err(DavExecutionError::InvalidRequest);
    }
    Ok(format!("{path}calendar-todo-probe-{uid}.ics"))
}

fn select(
    found: &[DiscoveredCalendarCollection],
) -> Result<&DiscoveredCalendarCollection, &'static str> {
    let mut selected = None;
    for name in NAMES {
        let mut matches = found
            .iter()
            .filter(|calendar| calendar.display_name == name);
        let calendar = matches.next().ok_or("selection_unavailable")?;
        if matches.next().is_some()
            || !calendar
                .privileges
                .iter()
                .any(|p| p == "write" || p == "write-content")
        {
            return Err("selection_not_unique_or_writable");
        }
        if selected.is_none() {
            selected = Some(calendar);
        }
    }
    selected.ok_or("selection_unavailable")
}

#[derive(Serialize)]
struct CreationAttempt<'a> {
    protocol: &'static str,
    account_id: &'a str,
    calendar_id: &'a str,
    collection_href: &'a str,
    exact_href: &'a str,
    generated_uid: String,
}

struct Observer(mpsc::Sender<AttachmentView>);
impl AttachmentStateEmitter for Observer {
    fn emit(&self, view: AttachmentView) {
        let _ = self.0.send(view);
    }
}
impl SyncStateEmitter for Observer {
    fn emit(&self, _: SyncExecutionView) {}
}
impl AgendaRefreshEmitter for Observer {
    fn emit(&self, _: AgendaRefreshNotice) {}
}
struct NoAutomaticSync(std::sync::Mutex<Option<mpsc::Sender<ScheduledSyncDeadline>>>);
impl SyncDeadlineScheduler for NoAutomaticSync {
    fn arm(&self, _: ScheduledSyncDeadline) -> Result<(), SyncSchedulerError> {
        Ok(())
    }
    fn cancel(&self, _: &str) -> Result<(), SyncSchedulerError> {
        Ok(())
    }
    fn shutdown(&self) {
        if let Ok(mut sender) = self.0.lock() {
            sender.take();
        }
    }
}

struct SelectedDiscovery {
    database: Database,
    calendars: Vec<DiscoveredCalendarRecord>,
    clock: Arc<dyn Clock>,
}
impl AccountCalendarDiscovery for SelectedDiscovery {
    fn discover_cancellable(
        &self,
        id: &str,
        token: &AttachmentCancellation,
    ) -> Result<(), DiscoveryError> {
        token
            .run_if_active(|| {
                self.database
                    .replace_discovered_calendars(
                        id.into(),
                        self.calendars.clone(),
                        self.clock
                            .now_utc_ms()
                            .map_err(|_| SourceAttachError::Unavailable)?,
                    )
                    .map_err(|_| SourceAttachError::Unavailable)
            })
            .map_err(|_| DiscoveryError::Superseded)
    }
}
struct SelectedAttacher {
    accounts: Arc<AccountSourceRegistry>,
    database: Database,
    credentials: CredentialService,
    clock: Arc<dyn Clock>,
}
impl SourceAttacher for SelectedAttacher {
    fn attach(&self, _: &str) -> Result<(), SourceAttachError> {
        Err(SourceAttachError::UnsafeConfiguration)
    }
    fn attach_cancellable(
        &self,
        id: &str,
        token: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        let profile = self.accounts.profile(id)?;
        let credential = self.credentials.read(
            &CredentialRef::parse(&profile.credential_ref)
                .map_err(|_| SourceAttachError::UnsafeConfiguration)?,
        )?;
        let executor = Arc::new(
            HttpDavExecutor::new(HttpDavConfig::default())
                .map_err(|_| SourceAttachError::Unavailable)?,
        );
        let interrupted = executor.clone();
        token.register_interrupt(Arc::new(move || {
            interrupted.cancel();
        }));
        let routes = self
            .database
            .available_calendar_ids_by_url(id.into())
            .map_err(|_| SourceAttachError::Unavailable)?;
        if routes.len() != 1 {
            return Err(SourceAttachError::UnsafeConfiguration);
        }
        let source = ScriptedCalDavSourceFactory::new(
            id.into(),
            "icloud-cn".into(),
            ENTRY.into(),
            hosts(),
            routes,
            executor,
            self.clock
                .now_utc_ms()
                .map_err(|_| SourceAttachError::Unavailable)?,
        )
        .map_err(|_| SourceAttachError::Unavailable)?
        .build(&profile, &credential)
        .map_err(|_| SourceAttachError::Unavailable)?;
        token.run_if_active(|| {
            self.accounts
                .attach_source(id, source)
                .map_err(SourceAttachError::from)
        })
    }
}

struct Scope {
    runtime: AttachmentRuntime,
    sync: Arc<SyncExecutionRuntime>,
    creator: Arc<HttpDavExecutor>,
}
impl Drop for Scope {
    fn drop(&mut self) {
        self.creator.cancel();
        self.runtime.shutdown();
        self.sync.shutdown();
        let _ = self.runtime.detach_all();
    }
}

fn get_exact(
    policy: &DavTransportPolicy,
    executor: &HttpDavExecutor,
    authentication: &DavAuthentication,
    collection: &str,
    href: &str,
) -> Result<RemoteReadBack, &'static str> {
    let entry = Url::parse(ENTRY).map_err(|_| "target_unsafe")?;
    policy
        .resolve_exact_mutation_target(&entry, collection, href)
        .map_err(|_| "target_unsafe")?;
    let url = entry.join(href).map_err(|_| "target_unsafe")?;
    let request = DavRequest::new(
        DavMethod::Get,
        url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| "target_unsafe")?;
    let response = policy
        .execute(executor, request, authentication)
        .map_err(|_| "read_back_unavailable")?;
    canonical_mutation_read_back(response, &url).map_err(|_| "read_back_unsafe")
}

#[allow(clippy::too_many_arguments)]
fn create_once_then_get(
    policy: &DavTransportPolicy,
    executor: &HttpDavExecutor,
    authentication: &DavAuthentication,
    request: DedicatedProbeCreateRequest,
    collection: &str,
    href: &str,
    expected_uid: &str,
    accept: &dyn Fn() -> bool,
) -> Result<RemoteReadBack, &'static str> {
    let result = executor.execute_probe_create_guarded(request, authentication, accept);
    let accepted = result
        .as_ref()
        .is_ok_and(|response| response.status() == 201 && response.location().is_none());
    println!("mutation_gate stage=create attempted=true accepted={accepted} attempts=1");
    let remote = get_exact(policy, executor, authentication, collection, href)?;
    if single_vevent_uid(&remote.raw_ics).map_err(|_| "created_identity_unsafe")? != expected_uid {
        return Err("created_identity_mismatch");
    }
    if !accepted {
        return Err("creation_uncertain_get_first_retained");
    }
    Ok(remote)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedAttempt {
    protocol: String,
    account_id: String,
    calendar_id: String,
    collection_href: String,
    exact_href: String,
    generated_uid: String,
}

#[derive(Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DeletionAttempt {
    protocol: String,
    account_id: String,
    calendar_id: String,
    collection_href: String,
    exact_href: String,
    generated_uid: String,
}

fn record_deletion_attempt(output: &Path, saved: &SavedAttempt) -> Result<(), &'static str> {
    let attempt = DeletionAttempt {
        protocol: "m5-2-dedicated-delete-v1".into(),
        account_id: saved.account_id.clone(),
        calendar_id: saved.calendar_id.clone(),
        collection_href: saved.collection_href.clone(),
        exact_href: saved.exact_href.clone(),
        generated_uid: saved.generated_uid.clone(),
    };
    let encoded = Zeroizing::new(serde_json::to_vec(&attempt).map_err(|_| "journal_unavailable")?);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("m5-2-deletion-attempt.json"))
        .map_err(|_| "pending_delete_requires_get_first")?;
    file.write_all(&encoded)
        .and_then(|_| file.sync_all())
        .map_err(|_| "journal_unavailable")
}

fn load_deletion_attempt(output: &Path, account_id: &str) -> Result<DeletionAttempt, &'static str> {
    let file = std::fs::File::open(output.join("m5-2-deletion-attempt.json"))
        .map_err(|_| "delete_attempt_unavailable")?;
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::Read::take(file, 16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| "delete_attempt_invalid")?;
    if bytes.len() > 16_384 {
        return Err("delete_attempt_invalid");
    }
    let saved: DeletionAttempt =
        serde_json::from_slice(&bytes).map_err(|_| "delete_attempt_invalid")?;
    let uid = saved
        .generated_uid
        .strip_prefix("calendar-todo-probe-")
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or("delete_attempt_invalid")?;
    if saved.protocol != "m5-2-dedicated-delete-v1"
        || saved.account_id != account_id
        || Uuid::parse_str(&saved.calendar_id).is_err()
        || saved.exact_href
            != probe_href(&saved.collection_href, uid).map_err(|_| "delete_attempt_invalid")?
    {
        return Err("delete_attempt_invalid");
    }
    Ok(saved)
}

fn load_creation_attempt(
    output: &Path,
    reference: &CredentialRef,
    account_id: &str,
) -> Result<SavedAttempt, &'static str> {
    let file = std::fs::File::open(output.join("m5-1-creation-attempt.json"))
        .map_err(|_| "attempt_unavailable")?;
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::Read::take(file, 16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| "attempt_unavailable")?;
    if bytes.len() > 16_384 {
        return Err("attempt_invalid");
    }
    let saved: SavedAttempt = serde_json::from_slice(&bytes).map_err(|_| "attempt_invalid")?;
    let uid = saved
        .generated_uid
        .strip_prefix("calendar-todo-probe-")
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or("attempt_invalid")?;
    if saved.protocol != "m5-1-dedicated-create-v1"
        || saved.account_id != account_id
        || Uuid::parse_str(&saved.calendar_id).is_err()
        || saved.exact_href
            != probe_href(&saved.collection_href, uid).map_err(|_| "attempt_invalid")?
        || reference.target_name() != format!("calendar-todo/account/{account_id}")
    {
        return Err("attempt_invalid");
    }
    Ok(saved)
}

/// Recovery never sends CREATE, PUT, DELETE, inventory, or an automatic retry.
pub fn recover_dedicated_icloud_probe(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
) -> Result<(), &'static str> {
    let _owner = acquire_probe_owner()?;
    let saved = load_creation_attempt(output, reference, account_id)?;
    let credential = WindowsCredentialStore
        .read(reference)
        .map_err(|_| "credential_unavailable")?;
    let executor = Arc::new(
        HttpDavExecutor::new(HttpDavConfig::default()).map_err(|_| "transport_unavailable")?,
    );
    let authentication = DavAuthentication::from_credential(&credential);
    let found = discover_vevent_collections(
        "https://caldav.icloud.com/",
        hosts(),
        DavAuthentication::from_credential(&credential),
        executor.clone(),
    )
    .map_err(|_| "discovery_unavailable")?;
    drop(credential);
    if select(&found)?.href != saved.collection_href {
        return Err("collection_route_changed");
    }
    let policy = DavTransportPolicy::new(hosts()).map_err(|_| "target_unsafe")?;
    let remote = get_exact(
        &policy,
        &executor,
        &authentication,
        &saved.collection_href,
        &saved.exact_href,
    )?;
    if single_vevent_uid(&remote.raw_ics).map_err(|_| "read_back_unsafe")? != saved.generated_uid {
        return Err("identity_mismatch");
    }
    let title =
        Zeroizing::new(single_vevent_title(&remote.raw_ics).map_err(|_| "read_back_unsafe")?);
    println!(
        "mutation_gate stage=recovery_get_first identity_matched=true original_title={} test_title={} write_attempts=0 appstate_writer=false",
        title.as_str() == "Calendar TODO dedicated probe",
        title.as_str() == "Calendar TODO dedicated probe changed"
    );
    Ok(())
}

fn attach_delete_runtime(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
    saved: &SavedAttempt,
    calendar: &DiscoveredCalendarCollection,
    executor: Arc<HttpDavExecutor>,
) -> Result<(Scope, Arc<AccountSourceRegistry>, u64), &'static str> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let credentials = CredentialService::new(Arc::new(WindowsCredentialStore));
    let database = Database::open_with_clock(
        output.join(format!("m5-2-delete-{}.sqlite3", saved.generated_uid)),
        clock.clone(),
    )
    .map_err(|_| "actor_unavailable")?;
    let accounts = Arc::new(AccountSourceRegistry::new(database.clone()));
    accounts
        .register_profile(RegisterAccountProfile {
            account_id: account_id.into(),
            provider_kind: ProviderKind::CalDav,
            user_label: "Dedicated delete validation".into(),
            credential_ref: reference.target_name().into(),
            region_profile: "icloud-cn".into(),
            enabled: true,
        })
        .map_err(|_| "actor_unavailable")?;
    accounts
        .mark_credential_configured(account_id)
        .map_err(|_| "actor_unavailable")?;
    accounts
        .set_remote_editing_enabled(account_id, true)
        .map_err(|_| "actor_unavailable")?;
    let attacher = Arc::new(SelectedAttacher {
        accounts: accounts.clone(),
        database: database.clone(),
        credentials: credentials.clone(),
        clock: clock.clone(),
    });
    let profiles = AccountProfileService::new(
        accounts.clone(),
        credentials.clone(),
        attacher.clone(),
        attacher,
    );
    let (sender, receiver) = mpsc::channel();
    let observer = Arc::new(Observer(sender));
    let (deadline_sender, deadline_receiver) = mpsc::channel();
    let sync = Arc::new(
        SyncExecutionRuntime::new(
            accounts.clone(),
            observer.clone(),
            observer.clone(),
            clock.clone(),
            Arc::new(NoAutomaticSync(std::sync::Mutex::new(Some(
                deadline_sender,
            )))),
            deadline_receiver,
            15,
        )
        .map_err(|_| "runtime_unavailable")?,
    );
    let sessions = Arc::new(ProductionMutationSessions::new(
        accounts.clone(),
        database.clone(),
        credentials,
        clock.clone(),
    ));
    let runtime = AttachmentRuntime::with_mutation_sessions(
        profiles,
        accounts.clone(),
        sync.clone(),
        Arc::new(SelectedDiscovery {
            database,
            calendars: vec![DiscoveredCalendarRecord {
                calendar_id: saved.calendar_id.clone(),
                collection_url: calendar.href.clone(),
                display_name: calendar.display_name.clone(),
                server_color: calendar.server_color.clone(),
                privileges: calendar.privileges.join(","),
            }],
            clock,
        }),
        observer,
        Some(sessions),
    );
    let scope = Scope {
        runtime,
        sync,
        creator: executor,
    };
    scope
        .runtime
        .submit(account_id, SourceAttachmentMode::IcloudChinaReadOnly, false)
        .map_err(|_| "attachment_unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let view = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "attachment_timeout")?;
        match view.state {
            AttachmentState::Ready => break,
            AttachmentState::Attaching => {}
            _ => return Err("attachment_failed"),
        }
    }
    let generation = accounts
        .attachment_generation(account_id)
        .ok_or("attachment_failed")?;
    Ok((scope, accounts, generation))
}

/// Sends at most one DELETE for the exact M5.1-generated resource. The durable
/// attempt record is synced before dispatch, so a restart can only use the
/// separate GET-only recovery action.
pub fn delete_dedicated_icloud_probe_once(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
) -> Result<(), &'static str> {
    let _owner = acquire_probe_owner()?;
    fs::create_dir_all(output).map_err(|_| "journal_unavailable")?;
    let saved = load_creation_attempt(output, reference, account_id)?;
    if output.join("m5-2-deletion-attempt.json").exists() {
        return Err("pending_delete_requires_get_first");
    }
    let credential = WindowsCredentialStore
        .read(reference)
        .map_err(|_| "credential_unavailable")?;
    let executor = Arc::new(
        HttpDavExecutor::new(HttpDavConfig::default()).map_err(|_| "transport_unavailable")?,
    );
    let authentication = DavAuthentication::from_credential(&credential);
    let found = discover_vevent_collections(
        "https://caldav.icloud.com/",
        hosts(),
        DavAuthentication::from_credential(&credential),
        executor.clone(),
    )
    .map_err(|_| "discovery_unavailable")?;
    drop(credential);
    let calendar = select(&found)?;
    if calendar.href != saved.collection_href {
        return Err("collection_route_changed");
    }
    let policy = DavTransportPolicy::new(hosts()).map_err(|_| "target_unsafe")?;
    let remote = get_exact(
        &policy,
        &executor,
        &authentication,
        &saved.collection_href,
        &saved.exact_href,
    )?;
    if single_vevent_uid(&remote.raw_ics).map_err(|_| "read_back_unsafe")? != saved.generated_uid
        || single_vevent_title(&remote.raw_ics).map_err(|_| "read_back_unsafe")?
            != "Calendar TODO dedicated probe"
    {
        return Err("delete_base_changed");
    }
    let (_scope, accounts, generation) = attach_delete_runtime(
        reference,
        account_id,
        output,
        &saved,
        calendar,
        executor.clone(),
    )?;
    if !accounts.is_current_attachment(account_id, generation) {
        return Err("ownership_lost");
    }
    record_deletion_attempt(output, &saved)?;
    let request = dedicated_delete_request(
        &policy,
        &saved.collection_href,
        &saved.exact_href,
        remote.etag,
    )
    .map_err(|_| "delete_request_unsafe")?;
    let outcome = delete_once_then_get(
        &policy,
        &executor,
        &authentication,
        request,
        &saved.collection_href,
        &saved.exact_href,
        &saved.generated_uid,
        &|| accounts.is_current_attachment(account_id, generation),
    )?;
    println!(
        "mutation_gate stage=delete_complete absent={} retained={} attempts=1 appstate_writer=false",
        outcome == DedicatedProbeDeleteOutcome::ConfirmedAbsent,
        outcome == DedicatedProbeDeleteOutcome::Retained
    );
    match outcome {
        DedicatedProbeDeleteOutcome::ConfirmedAbsent => Ok(()),
        DedicatedProbeDeleteOutcome::Retained => Err("delete_retained_get_first_only"),
    }
}

/// Restart recovery has no DELETE request construction or dispatch path.
pub fn recover_dedicated_icloud_delete_get_only(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
) -> Result<(), &'static str> {
    let _owner = acquire_probe_owner()?;
    let saved = load_deletion_attempt(output, account_id)?;
    if reference.target_name() != format!("calendar-todo/account/{account_id}") {
        return Err("delete_attempt_invalid");
    }
    let credential = WindowsCredentialStore
        .read(reference)
        .map_err(|_| "credential_unavailable")?;
    let executor = Arc::new(
        HttpDavExecutor::new(HttpDavConfig::default()).map_err(|_| "transport_unavailable")?,
    );
    let authentication = DavAuthentication::from_credential(&credential);
    let found = discover_vevent_collections(
        "https://caldav.icloud.com/",
        hosts(),
        DavAuthentication::from_credential(&credential),
        executor.clone(),
    )
    .map_err(|_| "discovery_unavailable")?;
    drop(credential);
    let calendar = select(&found)?;
    if calendar.href != saved.collection_href {
        return Err("collection_route_changed");
    }
    let policy = DavTransportPolicy::new(hosts()).map_err(|_| "target_unsafe")?;
    let outcome = verify_deleted_target(
        &policy,
        &executor,
        &authentication,
        &saved.collection_href,
        &saved.exact_href,
        &saved.generated_uid,
    )?;
    println!(
        "mutation_gate stage=delete_recovery_get_first absent={} retained={} write_attempts=0 appstate_writer=false",
        outcome == DedicatedProbeDeleteOutcome::ConfirmedAbsent,
        outcome == DedicatedProbeDeleteOutcome::Retained
    );
    Ok(())
}

/// Explicit standalone operator action. Installed SQLite is never written.
/// The recovery journal is private local routing evidence, never stdout/log data.
pub fn resume_dedicated_icloud_title_probe(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
) -> Result<(), &'static str> {
    run_protocol(reference, account_id, output, true)
}

pub fn run_dedicated_icloud_title_probe(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
) -> Result<(), &'static str> {
    run_protocol(reference, account_id, output, false)
}

fn validate_dedicated_resource(
    resource: &mut crate::domain::calendar_sync::StagedResource,
    uid: &str,
    now: i64,
) -> Result<(), &'static str> {
    use crate::domain::{
        calendar_recurrence::{RecurrenceHorizon, expand_vevent_recurrence},
        remote_mutation::strong_mutation_etag,
    };
    if crate::adapters::persistence::contains_invitation_property(&resource.raw_ics)
        || !uid
            .strip_prefix("calendar-todo-probe-")
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
        || resource.uid != uid
        || single_vevent_uid(&resource.raw_ics).map_err(|_| "dedicated_parse_unsafe")? != uid
        || !strong_mutation_etag(&resource.etag)
    {
        return Err("dedicated_parse_unsafe");
    }
    patch_single_vevent_title(&resource.raw_ics, "Calendar TODO dedicated probe changed")
        .map_err(|_| "dedicated_patch_unsafe")?;
    let occurrences = expand_vevent_recurrence(
        &resource.raw_ics,
        uid,
        RecurrenceHorizon::standard()
            .window(now)
            .map_err(|_| "dedicated_parse_unsafe")?,
    )
    .map_err(|_| "dedicated_parse_unsafe")?;
    if occurrences.occurrences.len() != 1 {
        return Err("dedicated_parse_unsafe");
    }
    resource.parse_status = crate::domain::calendar_sync::ParseStatus::Parsed;
    Ok(())
}

fn run_protocol(
    reference: &CredentialRef,
    account_id: &str,
    output: &Path,
    resume: bool,
) -> Result<(), &'static str> {
    let _owner = acquire_probe_owner()?;
    Uuid::parse_str(account_id).map_err(|_| "account_invalid")?;
    let expected_reference = format!("calendar-todo/account/{account_id}");
    if CredentialRef::parse(&expected_reference).map_err(|_| "account_invalid")? != *reference {
        return Err("account_invalid");
    }
    fs::create_dir_all(output).map_err(|_| "journal_unavailable")?;
    // A pending journal prevents a second CREATE, including after process crash.
    let journal_path = output.join("m5-1-creation-attempt.json");
    let saved = if resume {
        let file = std::fs::File::open(&journal_path).map_err(|_| "attempt_unavailable")?;
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::Read::take(file, 16_385)
            .read_to_end(&mut bytes)
            .map_err(|_| "attempt_invalid")?;
        if bytes.len() > 16_384 {
            return Err("attempt_invalid");
        }
        let saved: SavedAttempt = serde_json::from_slice(&bytes).map_err(|_| "attempt_invalid")?;
        if saved.protocol != "m5-1-dedicated-create-v1"
            || saved.account_id != account_id
            || Uuid::parse_str(&saved.calendar_id).is_err()
        {
            return Err("attempt_invalid");
        }
        Some(saved)
    } else {
        if journal_path.exists() {
            return Err("pending_attempt_requires_get_first");
        }
        None
    };
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let credentials = CredentialService::new(Arc::new(WindowsCredentialStore));
    let credential = credentials
        .read(reference)
        .map_err(|_| "credential_unavailable")?;
    let creator = Arc::new(
        HttpDavExecutor::new(HttpDavConfig::default()).map_err(|_| "transport_unavailable")?,
    );
    let found = discover_vevent_collections(
        "https://caldav.icloud.com/",
        hosts(),
        DavAuthentication::from_credential(&credential),
        creator.clone(),
    )
    .map_err(|_| "discovery_unsafe_or_unavailable")?;
    let calendar = select(&found)?;
    let uid = match saved.as_ref() {
        Some(saved) => saved
            .generated_uid
            .strip_prefix("calendar-todo-probe-")
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or("attempt_invalid")?,
        None => Uuid::new_v4(),
    };
    let href = probe_href(&calendar.href, uid).map_err(|_| "target_unsafe")?;
    if let Some(saved) = &saved
        && (saved.collection_href != calendar.href || saved.exact_href != href)
    {
        return Err("collection_route_changed");
    }
    let calendar_id = saved.as_ref().map_or_else(
        || Uuid::new_v4().to_string(),
        |saved| saved.calendar_id.clone(),
    );
    let policy = DavTransportPolicy::new(hosts()).map_err(|_| "target_unsafe")?;
    let create = DedicatedProbeCreateRequest::new(&policy, &calendar.href, uid)
        .map_err(|_| "target_unsafe")?;
    let database_path = output.join(format!("m5-1-{uid}.sqlite3"));
    if resume && !database_path.exists() {
        return Err("attempt_actor_unavailable");
    }
    let database =
        Database::open_with_clock(database_path, clock.clone()).map_err(|_| "actor_unavailable")?;
    if !database
        .list_remote_mutations()
        .map_err(|_| "actor_unavailable")?
        .is_empty()
    {
        return Err("existing_mutation_requires_resolution");
    }
    let accounts = Arc::new(AccountSourceRegistry::new(database.clone()));
    accounts
        .register_profile(RegisterAccountProfile {
            account_id: account_id.into(),
            provider_kind: ProviderKind::CalDav,
            user_label: "Dedicated validation".into(),
            credential_ref: expected_reference,
            region_profile: "icloud-cn".into(),
            enabled: true,
        })
        .map_err(|_| "actor_unavailable")?;
    accounts
        .mark_credential_configured(account_id)
        .map_err(|_| "actor_unavailable")?;
    // Explicit authorization is scoped to this isolated diagnostic actor only.
    accounts
        .set_remote_editing_enabled(account_id, true)
        .map_err(|_| "actor_unavailable")?;
    let attacher = Arc::new(SelectedAttacher {
        accounts: accounts.clone(),
        database: database.clone(),
        credentials: credentials.clone(),
        clock: clock.clone(),
    });
    let profiles = AccountProfileService::new(
        accounts.clone(),
        credentials.clone(),
        attacher.clone(),
        attacher,
    );
    let (sender, receiver) = mpsc::channel();
    let observer = Arc::new(Observer(sender));
    let (deadline_sender, deadline_receiver) = mpsc::channel();
    let sync = Arc::new(
        SyncExecutionRuntime::new(
            accounts.clone(),
            observer.clone(),
            observer.clone(),
            clock.clone(),
            Arc::new(NoAutomaticSync(std::sync::Mutex::new(Some(
                deadline_sender,
            )))),
            deadline_receiver,
            15,
        )
        .map_err(|_| "runtime_unavailable")?,
    );
    let sessions = Arc::new(ProductionMutationSessions::new(
        accounts.clone(),
        database.clone(),
        credentials,
        clock.clone(),
    ));
    let runtime = AttachmentRuntime::with_mutation_sessions(
        profiles,
        accounts.clone(),
        sync.clone(),
        Arc::new(SelectedDiscovery {
            database: database.clone(),
            calendars: vec![DiscoveredCalendarRecord {
                calendar_id: calendar_id.clone(),
                collection_url: calendar.href.clone(),
                display_name: calendar.display_name.clone(),
                server_color: calendar.server_color.clone(),
                privileges: calendar.privileges.join(","),
            }],
            clock: clock.clone(),
        }),
        observer,
        Some(sessions.clone()),
    );
    let scope = Scope {
        runtime,
        sync,
        creator: creator.clone(),
    };
    scope
        .runtime
        .submit(account_id, SourceAttachmentMode::IcloudChinaReadOnly, false)
        .map_err(|_| "attachment_unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let view = receiver
            .recv_timeout(left)
            .map_err(|_| "attachment_timeout")?;
        match view.state {
            AttachmentState::Ready => break,
            AttachmentState::Attaching => {}
            _ => return Err("attachment_failed"),
        }
    }
    let generation = accounts
        .attachment_generation(account_id)
        .ok_or("attachment_failed")?;
    let journal = CreationAttempt {
        protocol: "m5-1-dedicated-create-v1",
        account_id,
        calendar_id: &calendar_id,
        collection_href: &calendar.href,
        exact_href: &href,
        generated_uid: format!("calendar-todo-probe-{uid}"),
    };
    if !resume {
        let encoded =
            Zeroizing::new(serde_json::to_vec(&journal).map_err(|_| "journal_unavailable")?);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&journal_path)
            .map_err(|_| "pending_attempt_requires_get_first")?;
        file.write_all(&encoded)
            .and_then(|_| file.sync_all())
            .map_err(|_| "journal_unavailable")?;
    }
    if !accounts.is_current_attachment(account_id, generation) {
        return Err("ownership_lost");
    }
    let authentication = DavAuthentication::from_credential(&credential);
    drop(credential);
    let remote = if resume {
        let remote = get_exact(&policy, &creator, &authentication, &calendar.href, &href)?;
        if single_vevent_uid(&remote.raw_ics).map_err(|_| "read_back_unsafe")?
            != journal.generated_uid
            || single_vevent_title(&remote.raw_ics).map_err(|_| "read_back_unsafe")?
                != "Calendar TODO dedicated probe"
        {
            return Err("recovery_base_changed");
        }
        println!(
            "mutation_gate stage=resume_get_first identity_matched=true original_title=true create_attempts=0"
        );
        remote
    } else {
        create_once_then_get(
            &policy,
            &creator,
            &authentication,
            create,
            &calendar.href,
            &href,
            &journal.generated_uid,
            &|| accounts.is_current_attachment(account_id, generation),
        )?
    };
    let original =
        Zeroizing::new(single_vevent_title(&remote.raw_ics).map_err(|_| "created_event_unsafe")?);
    let patched = Zeroizing::new(
        patch_single_vevent_title(&remote.raw_ics, "Calendar TODO dedicated probe changed")
            .map_err(|_| "created_event_unsafe")?,
    );
    if patched.len() > 64 * 1024 {
        return Err("patch_oversized");
    }
    let profile = accounts
        .profile(account_id)
        .map_err(|_| "actor_unavailable")?;
    let source_credential = WindowsCredentialStore
        .read(reference)
        .map_err(|_| "credential_unavailable")?;
    let source = ScriptedCalDavSourceFactory::new(
        account_id.into(),
        "icloud-cn".into(),
        ENTRY.into(),
        hosts(),
        BTreeMap::from([(calendar.href.clone(), calendar_id.clone())]),
        creator.clone(),
        clock.now_utc_ms().map_err(|_| "clock_unavailable")?,
    )
    .map_err(|_| "source_unavailable")?
    .build(&profile, &source_credential)
    .map_err(|_| "source_unavailable")?;
    drop(source_credential);
    let capture = database
        .begin_sync_capture(account_id.into())
        .map_err(|_| "actor_unavailable")?;
    let mut stage = source
        .collect_full_inventory()
        .map_err(|_| "inventory_unavailable")?;
    let staged = stage
        .calendars
        .iter_mut()
        .flat_map(|calendar| &mut calendar.resources)
        .find(|resource| resource.href == href)
        .ok_or("created_resource_not_in_inventory")?;
    if staged.etag != remote.etag || staged.raw_ics != remote.raw_ics {
        return Err("created_revision_changed");
    }
    validate_dedicated_resource(
        staged,
        &journal.generated_uid,
        clock.now_utc_ms().map_err(|_| "clock_unavailable")?,
    )?;
    database
        .promote_captured_account_inventory(capture, stage)
        .map_err(|_| "inventory_rejected")?;
    let resource_id = database
        .list_cached_calendar_resources()
        .map_err(|_| "actor_unavailable")?
        .into_iter()
        .find(|resource| resource.href == href && resource.calendar_id == calendar_id)
        .ok_or("resource_unavailable")?
        .resource_id;
    let service = RemoteMutationService::new(database.clone(), clock, sessions);
    for (stage, title) in [
        ("title_update", "Calendar TODO dedicated probe changed"),
        ("restore", original.as_str()),
    ] {
        database
            .queue_remote_title_update(QueueRemoteTitleUpdate {
                idempotency_key: Uuid::new_v4().to_string(),
                resource_id: resource_id.clone(),
                title: title.into(),
            })
            .map_err(|_| "intent_rejected")?;
        let outcome = service
            .run_one_for_account_guarded(account_id, &|| {
                accounts.is_current_attachment(account_id, generation)
            })
            .map_err(|_| "mutation_get_first_retained")?;
        println!(
            "mutation_gate stage={stage} applied={} conflict={} deferred={} generation_current={}",
            outcome == RemoteMutationRunOutcome::Applied,
            outcome == RemoteMutationRunOutcome::Conflict,
            outcome == RemoteMutationRunOutcome::Deferred,
            accounts.is_current_attachment(account_id, generation)
        );
        if outcome != RemoteMutationRunOutcome::Applied {
            return Err("mutation_not_applied_no_more_writes");
        }
    }
    let final_remote = get_exact(&policy, &creator, &authentication, &calendar.href, &href)?;
    if single_vevent_uid(&final_remote.raw_ics).map_err(|_| "final_unsafe")?
        != journal.generated_uid
        || single_vevent_title(&final_remote.raw_ics).map_err(|_| "final_unsafe")? != *original
    {
        return Err("restoration_unproven");
    }
    println!(
        "mutation_gate stage=complete restoration_matched=true resource_retained=true installed_acceptance=false appstate_writer=false"
    );
    Ok(())
}
