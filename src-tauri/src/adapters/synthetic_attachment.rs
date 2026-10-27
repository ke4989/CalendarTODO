use crate::{
    adapters::{
        caldav::ScriptedCalDavSourceFactory,
        caldav_transport::{
            DavAuthentication, DavCancellation, DavDepth, DavExecutionError, DavExecutor,
            DavMethod, DavRequest, RawDavResponse,
        },
    },
    application::{
        account_registry::AccountSourceRegistry,
        credentials::CredentialService,
        source_factory::{
            AttachmentCancellation, SourceAttachError, SourceAttacher, SourceFactoryOrchestrator,
        },
    },
    ports::credentials::{
        CredentialError, CredentialRecord, CredentialRef, CredentialStore, SecretBytes,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use uuid::Uuid;

const ENTRY_URL: &str = "https://synthetic.invalid/.well-known/caldav";
const COLLECTION_URL: &str = "https://synthetic.invalid/home/calendar/";
const RESOURCE_URL: &str = "https://synthetic.invalid/home/calendar/event.ics";
const HOME_URL: &str = "https://synthetic.invalid/home/";
const DISCOVERY_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:current-user-principal/><d:principal-URL/><c:calendar-home-set/></d:prop></d:propfind>"#;
const CALENDARS_BODY: &[u8] = br#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/"><d:prop><d:displayname/><d:resourcetype/><a:calendar-color/><d:current-user-privilege-set/><c:supported-calendar-component-set/></d:prop></d:propfind>"#;
const INVENTORY_BODY: &[u8] =
    br#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:sync-token/></d:prop></d:propfind>"#;

/// Runtime checkpoint adapter with deliberately no platform or socket capability.
/// It creates the reviewed CalDAV source from fixed fake credentials and an
/// exact in-memory response script; the Windows store and HTTP executor are not
/// fields and cannot be selected by this path.
pub struct SyntheticSourceAttacher {
    registry: Arc<AccountSourceRegistry>,
}

impl SyntheticSourceAttacher {
    pub fn new(registry: Arc<AccountSourceRegistry>) -> Self {
        Self { registry }
    }
}

impl SourceAttacher for SyntheticSourceAttacher {
    fn attach(&self, account_id: &str) -> Result<(), SourceAttachError> {
        self.attach_cancellable(account_id, &AttachmentCancellation::new())
    }

    fn attach_cancellable(
        &self,
        account_id: &str,
        cancellation: &AttachmentCancellation,
    ) -> Result<(), SourceAttachError> {
        let profile = self
            .registry
            .profile(account_id)
            .map_err(SourceAttachError::from)?;
        let mut calendar_bytes = Uuid::parse_str(account_id)
            .map_err(|_| SourceAttachError::UnsafeConfiguration)?
            .as_u128()
            .wrapping_add(0x53fe_8aa2_707b_40de_ae4e_2b46_087e_1421)
            .to_be_bytes();
        calendar_bytes[6] = (calendar_bytes[6] & 0x0f) | 0x80;
        calendar_bytes[8] = (calendar_bytes[8] & 0x3f) | 0x80;
        let calendar_id = Uuid::from_bytes(calendar_bytes).to_string();
        let executor = Arc::new(SyntheticDavExecutor::new(account_id));
        let factory = Arc::new(ScriptedCalDavSourceFactory::new(
            profile.account_id.clone(),
            profile.region_profile.clone(),
            ENTRY_URL.into(),
            BTreeSet::from(["synthetic.invalid".into()]),
            BTreeMap::from([("/home/calendar/".into(), calendar_id)]),
            executor,
            1_779_120_000_000,
        )?);
        SourceFactoryOrchestrator::new(
            self.registry.clone(),
            CredentialService::new(Arc::new(SyntheticCredentialStore)),
            factory,
        )
        .build_and_attach_cancellable(account_id, cancellation)
    }
}

struct SyntheticCredentialStore;

impl CredentialStore for SyntheticCredentialStore {
    fn read(&self, _: &CredentialRef) -> Result<CredentialRecord, CredentialError> {
        CredentialRecord::new(
            "fixture@example.invalid".into(),
            SecretBytes::new(b"synthetic-only".to_vec()),
        )
    }

    fn write(&self, _: &CredentialRef, _: &CredentialRecord) -> Result<(), CredentialError> {
        Err(CredentialError::InvalidReference)
    }

    fn delete(&self, _: &CredentialRef) -> Result<(), CredentialError> {
        Err(CredentialError::InvalidReference)
    }
}

/// A repeatable, offline fixture. Exact request matching keeps unexpected DAV
/// traffic visible as a bounded failure, while later periodic syncs can return
/// a stable no-change token indefinitely.
struct SyntheticDavExecutor {
    account_id: String,
    cancellation: DavCancellation,
}

impl SyntheticDavExecutor {
    fn new(account_id: &str) -> Self {
        Self {
            account_id: account_id.to_owned(),
            cancellation: DavCancellation::new(),
        }
    }
}

impl DavExecutor for SyntheticDavExecutor {
    fn execute(
        &self,
        request: &DavRequest,
        authentication: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        if !authentication.is_present() {
            return Err(DavExecutionError::ScriptMismatch);
        }
        let url = request.url().as_str();
        let body = request.body();
        let reply = match (request.method(), url, request.depth()) {
            (DavMethod::Propfind, ENTRY_URL, DavDepth::Zero) if body == DISCOVERY_BODY => {
                RawDavResponse::new(207, None, Some("application/xml".into()), br#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:propstat><d:prop><d:current-user-principal><d:href>/principal/</d:href></d:current-user-principal><c:calendar-home-set><d:href>/home/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#.to_vec())
            }
            (DavMethod::Propfind, HOME_URL, DavDepth::One) if body == CALENDARS_BODY => {
                RawDavResponse::new(207, None, Some("application/xml".into()), br#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/home/calendar/</d:href><d:propstat><d:prop><d:displayname>Synthetic Calendar</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#.to_vec())
            }
            (DavMethod::Propfind, COLLECTION_URL, DavDepth::One) if body == INVENTORY_BODY => {
                RawDavResponse::new(207, None, Some("application/xml".into()), br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/home/calendar/</d:href><d:status>HTTP/1.1 200 OK</d:status></d:response><d:response><d:href>/home/calendar/event.ics</d:href><d:propstat><d:prop><d:getetag>&quot;synthetic-etag-2&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:sync-token>synthetic-token-2</d:sync-token></d:multistatus>"#.to_vec())
            }
            (DavMethod::Get, RESOURCE_URL, DavDepth::Zero) if body.is_empty() => {
                let event = format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:synthetic-{}\r\nSUMMARY:Synthetic incremental boundary\r\nDTSTART:20260918T010000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", self.account_id);
                RawDavResponse::new(200, None, Some("text/calendar; charset=utf-8".into()), event.into_bytes())
            }
            (DavMethod::Report, COLLECTION_URL, DavDepth::One)
                if body == sync_report_body("synthetic-token-1") =>
            {
                RawDavResponse::new(207, None, Some("application/xml".into()), br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/home/calendar/event.ics</d:href><d:propstat><d:prop><d:getetag>&quot;synthetic-etag-2&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:sync-token>synthetic-token-2</d:sync-token></d:multistatus>"#.to_vec())
            }
            (DavMethod::Report, COLLECTION_URL, DavDepth::One)
                if body == sync_report_body("synthetic-token-2") =>
            {
                RawDavResponse::new(207, None, Some("application/xml".into()), br#"<d:multistatus xmlns:d="DAV:"><d:sync-token>synthetic-token-2</d:sync-token></d:multistatus>"#.to_vec())
            }
            _ => return Err(DavExecutionError::ScriptMismatch),
        };
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        Ok(reply)
    }

    fn cancellation(&self) -> Option<DavCancellation> {
        Some(self.cancellation.clone())
    }
}

fn sync_report_body(token: &str) -> Vec<u8> {
    format!("<d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>").into_bytes()
}
