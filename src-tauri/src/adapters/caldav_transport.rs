use crate::domain::remote_mutation::{
    RemoteDeletionClaim, RemoteDeletionReadBack, RemoteMutationClaim, RemoteReadBack,
    patch_single_vevent_title, resource_belongs_to_collection, strong_mutation_etag,
};
use crate::ports::credentials::{CredentialRecord, SecretBytes};
use crate::ports::remote_mutation::RemoteMutationTransportError;
use std::{
    collections::{BTreeSet, VecDeque},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use thiserror::Error;
use url::Url;
use zeroize::Zeroize;

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
pub(crate) const DEFAULT_MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_URL_BYTES: usize = 4 * 1024;
pub(crate) const MAX_CONTENT_TYPE_BYTES: usize = 256;
const MAX_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MAX_ETAG_BYTES: usize = 256;

/// DAV `Depth` for collection-style requests. `GET` carries no `Depth` header
/// and must use [`DavDepth::Zero`]; `PROPFIND`/`REPORT` always send an explicit
/// value so a server can never fall back to its own default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DavDepth {
    Zero,
    One,
}

impl DavDepth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::One => "1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DavMethod {
    Get,
    Propfind,
    Report,
}

/// Cooperative cancellation for an in-flight exchange. The flag is shared with
/// the executor, so a shutdown or disable path can interrupt a blocked request
/// instead of waiting for its deadline.
#[derive(Clone, Default)]
pub struct DavCancellation {
    cancelled: Arc<AtomicBool>,
}

impl DavCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when this call was the one that tripped the flag.
    pub fn cancel(&self) -> bool {
        !self.cancelled.swap(true, Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for DavCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DavCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

pub struct DavAuthentication {
    username: String,
    password: SecretBytes,
}

impl DavAuthentication {
    pub fn from_credential(credential: &CredentialRecord) -> Self {
        Self {
            username: credential.username().to_owned(),
            password: SecretBytes::new(credential.password().expose().to_vec()),
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(username: &str, password: &[u8]) -> Self {
        Self {
            username: username.to_owned(),
            password: SecretBytes::new(password.to_vec()),
        }
    }

    pub(crate) fn is_present(&self) -> bool {
        !self.username.is_empty() && !self.password.expose().is_empty()
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn password_bytes(&self) -> &[u8] {
        self.password.expose()
    }
}

impl fmt::Debug for DavAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DavAuthentication([REDACTED])")
    }
}

impl Drop for DavAuthentication {
    fn drop(&mut self) {
        self.username.zeroize();
    }
}

pub struct DavRequest {
    method: DavMethod,
    url: Url,
    body: Vec<u8>,
    depth: DavDepth,
    timeout_ms: u64,
}

impl DavRequest {
    pub fn new(
        method: DavMethod,
        url: Url,
        body: Vec<u8>,
        depth: DavDepth,
        timeout_ms: u64,
    ) -> Result<Self, DavExecutionError> {
        if body.len() > MAX_REQUEST_BODY_BYTES
            || !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms)
            || (method == DavMethod::Get && depth != DavDepth::Zero)
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        validate_https_url(&url)?;
        Ok(Self {
            method,
            url,
            body,
            depth,
            timeout_ms,
        })
    }

    pub fn method(&self) -> DavMethod {
        self.method
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn depth(&self) -> DavDepth {
        self.depth
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
}

impl fmt::Debug for DavRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DavRequest")
            .field("method", &self.method)
            .field("destination", &"[REDACTED]")
            .field("body_bytes", &self.body.len())
            .field("depth", &self.depth)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

pub struct RawDavResponse {
    status: u16,
    location: Option<String>,
    content_type: Option<String>,
    etag: Option<String>,
    body: Vec<u8>,
}

impl RawDavResponse {
    pub fn new(
        status: u16,
        location: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    ) -> Self {
        Self {
            status,
            location,
            content_type,
            etag: None,
            body,
        }
    }

    pub(crate) fn with_etag(mut self, etag: Option<String>) -> Self {
        self.etag = etag;
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for RawDavResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawDavResponse")
            .field("status", &self.status)
            .field("location", &self.location.as_ref().map(|_| "[REDACTED]"))
            .field(
                "content_type",
                &self.content_type.as_ref().map(|_| "[REDACTED]"),
            )
            .field("etag", &self.etag.as_ref().map(|_| "[REDACTED]"))
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

pub struct DavResponse {
    final_url: Url,
    status: u16,
    content_type: Option<String>,
    etag: Option<String>,
    body: Vec<u8>,
}

impl DavResponse {
    pub fn final_url(&self) -> &Url {
        &self.final_url
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for DavResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DavResponse")
            .field("destination", &"[REDACTED]")
            .field("status", &self.status)
            .field("etag", &self.etag.as_ref().map(|_| "[REDACTED]"))
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

pub trait DavExecutor: Send + Sync {
    fn execute(
        &self,
        request: &DavRequest,
        authentication: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError>;

    /// Cancellation handle shared with this executor, when it supports
    /// interrupting an in-flight exchange.
    fn cancellation(&self) -> Option<DavCancellation> {
        None
    }
}

pub struct DavTransportPolicy {
    trusted_hosts: BTreeSet<String>,
    max_redirects: usize,
    max_response_body_bytes: usize,
    icloud: Option<ICloudScope>,
}

#[derive(Clone)]
enum ICloudScope {
    Discovery(Url),
    Bound {
        home: Url,
        collections: BTreeSet<String>,
    },
}

impl DavTransportPolicy {
    pub fn new(trusted_hosts: BTreeSet<String>) -> Result<Self, DavExecutionError> {
        if trusted_hosts.is_empty()
            || trusted_hosts
                .iter()
                .any(|host| host.is_empty() || host != &host.to_ascii_lowercase())
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(Self {
            trusted_hosts,
            max_redirects: 5,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            icloud: None,
        })
    }

    pub(crate) fn icloud_discovery(target: Url) -> Result<Self, DavExecutionError> {
        super::icloud_caldav::validate_url(&target)?;
        Ok(Self {
            trusted_hosts: BTreeSet::new(),
            max_redirects: 5,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            icloud: Some(ICloudScope::Discovery(target)),
        })
    }

    pub(crate) fn icloud_bound(
        binding: &super::icloud_caldav::ICloudBinding,
        routes: impl IntoIterator<Item = String>,
    ) -> Result<Self, DavExecutionError> {
        let home = binding.validate()?;
        let collections = routes
            .into_iter()
            .map(|route| binding.collection(&route).map(|u| u.to_string()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self {
            trusted_hosts: BTreeSet::new(),
            max_redirects: 0,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            icloud: Some(ICloudScope::Bound { home, collections }),
        })
    }

    fn authorized_collection(&self, url: &Url) -> bool {
        match &self.icloud {
            Some(ICloudScope::Bound { collections, .. }) => collections.contains(url.as_str()),
            _ => true,
        }
    }

    fn authorized_request(&self, url: &Url) -> bool {
        match &self.icloud {
            None => true,
            Some(ICloudScope::Discovery(target)) => target == url,
            Some(ICloudScope::Bound { home, collections }) => {
                home == url
                    || collections.iter().any(|route| {
                        let collection = Url::parse(route).expect("validated collection");
                        collection == *url
                            || (url.origin() == collection.origin()
                                && url
                                    .path()
                                    .strip_prefix(collection.path())
                                    .is_some_and(|tail| !tail.is_empty() && !tail.contains('/')))
                    })
            }
        }
    }

    pub fn execute(
        &self,
        executor: &dyn DavExecutor,
        mut request: DavRequest,
        authentication: &DavAuthentication,
    ) -> Result<DavResponse, DavExecutionError> {
        if !self.authorized_request(request.url()) {
            return Err(DavExecutionError::UnsafeRedirect);
        }
        let mut visited = BTreeSet::new();
        for redirect_count in 0..=self.max_redirects {
            if !visited.insert(request.url().to_string()) {
                return Err(DavExecutionError::UnsafeRedirect);
            }
            if let Some(cancellation) = executor.cancellation()
                && cancellation.is_cancelled()
            {
                return Err(DavExecutionError::Cancelled);
            }
            self.validate_destination(request.url())?;
            let raw = executor.execute(&request, authentication)?;
            if !(100..=599).contains(&raw.status) {
                return Err(DavExecutionError::InvalidResponse);
            }
            if raw.body.len() > self.max_response_body_bytes {
                return Err(DavExecutionError::ResponseTooLarge);
            }
            if is_redirect(raw.status) {
                if raw.status == 303 {
                    return Err(DavExecutionError::UnsafeRedirect);
                }
                if redirect_count == self.max_redirects {
                    return Err(DavExecutionError::TooManyRedirects);
                }
                let location = raw
                    .location
                    .as_deref()
                    .ok_or(DavExecutionError::UnsafeRedirect)?;
                if location.len() > MAX_URL_BYTES {
                    return Err(DavExecutionError::UnsafeRedirect);
                }
                let next = if self.icloud.is_some() {
                    super::icloud_caldav::resolve(request.url(), location)?
                } else {
                    request
                        .url()
                        .join(location)
                        .map_err(|_| DavExecutionError::UnsafeRedirect)?
                };
                self.validate_destination(&next)?;
                request = DavRequest::new(
                    request.method(),
                    next,
                    request.body().to_vec(),
                    request.depth(),
                    request.timeout_ms(),
                )?;
                continue;
            }
            if raw.content_type.as_ref().is_some_and(|value| {
                value.len() > MAX_CONTENT_TYPE_BYTES || value.chars().any(char::is_control)
            }) || raw.etag.as_ref().is_some_and(|value| {
                value.len() > MAX_ETAG_BYTES || value.chars().any(char::is_control)
            }) {
                return Err(DavExecutionError::InvalidResponse);
            }
            return Ok(DavResponse {
                final_url: request.url,
                status: raw.status,
                content_type: raw.content_type,
                etag: raw.etag,
                body: raw.body,
            });
        }
        Err(DavExecutionError::TooManyRedirects)
    }

    pub(in crate::adapters) fn validate_destination(
        &self,
        url: &Url,
    ) -> Result<(), DavExecutionError> {
        validate_https_url(url)?;
        if self.icloud.is_some() {
            return super::icloud_caldav::validate_url(url);
        }
        let host = url.host_str().ok_or(DavExecutionError::UnsafeRedirect)?;
        if !self.trusted_hosts.contains(host) {
            return Err(DavExecutionError::UnsafeRedirect);
        }
        Ok(())
    }

    /// Pure exact collection policy check for internally captured Probe evidence.
    /// This constructs no request and grants no executor or routing authority.
    #[allow(dead_code)]
    pub(crate) fn probe_collection_matches(&self, entry: &Url, route: &str) -> bool {
        self.validate_destination(entry).is_ok()
            && entry
                .join(route)
                .is_ok_and(|u| self.authorized_collection(&u))
            && entry.query().is_none()
            && entry.fragment().is_none()
            && entry.join(route).is_ok_and(|target| {
                self.validate_destination(&target).is_ok()
                    && target.origin() == entry.origin()
                    && target.query().is_none()
                    && target.fragment().is_none()
                    && target.path() != "/"
                    && target.path().ends_with('/')
                    && (route == target.path() || route == target.as_str())
            })
    }

    /// Resolve actor-owned routing data without constructing or sending a DAV request.
    /// The entry origin represents the currently attached provider, not a caller-supplied
    /// destination; a later writer must repeat this check under current ownership.
    #[allow(dead_code)]
    pub(crate) fn resolve_exact_mutation_target(
        &self,
        entry_url: &Url,
        collection_href: &str,
        exact_href: &str,
    ) -> Result<ExactMutationTarget, DavExecutionError> {
        self.validate_destination(entry_url)?;
        if !self.authorized_collection(
            &entry_url
                .join(collection_href)
                .map_err(|_| DavExecutionError::InvalidRequest)?,
        ) {
            return Err(DavExecutionError::InvalidRequest);
        }
        if entry_url.query().is_some()
            || !resource_belongs_to_collection(collection_href, exact_href)
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        let collection = entry_url
            .join(collection_href)
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        let resource = entry_url
            .join(exact_href)
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        self.validate_destination(&collection)?;
        self.validate_destination(&resource)?;
        if collection.origin() != entry_url.origin()
            || resource.origin() != collection.origin()
            || collection.query().is_some()
            || resource.query().is_some()
            || resource.path() != exact_href
            || !resource
                .path()
                .strip_prefix(collection.path())
                .is_some_and(|member| !member.is_empty() && !member.contains('/'))
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(ExactMutationTarget { url: resource })
    }

    /// Shape a conditional update only from a leased claim's captured revision
    /// and losslessly patched ICS. This remains inert until a reviewed owner
    /// supplies the current provider entry and an execution lifecycle.
    #[allow(dead_code)]
    pub(crate) fn prepare_claimed_title_update(
        &self,
        entry_url: &Url,
        claim: &RemoteMutationClaim,
        timeout_ms: u64,
    ) -> Result<ConditionalTitleUpdateRequest, DavExecutionError> {
        let target = self.resolve_exact_mutation_target(
            entry_url,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        let patched = patch_single_vevent_title(&claim.base_raw_ics, &claim.title)
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        ConditionalTitleUpdateRequest::new(
            self,
            target,
            claim.base_etag.clone(),
            patched,
            timeout_ms,
        )
    }

    /// Prepare one exact GET for either verify-first recovery or canonical
    /// post-write read-back. The response is accepted only by this request's
    /// own target check, including after a read redirect.
    #[allow(dead_code)]
    pub(crate) fn prepare_claimed_read_back(
        &self,
        entry_url: &Url,
        claim: &RemoteMutationClaim,
        timeout_ms: u64,
    ) -> Result<ClaimReadBackRequest, DavExecutionError> {
        let target = self.resolve_exact_mutation_target(
            entry_url,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        let request = DavRequest::new(
            DavMethod::Get,
            target.url,
            Vec::new(),
            DavDepth::Zero,
            timeout_ms,
        )?;
        Ok(ClaimReadBackRequest { request })
    }

    pub(crate) fn prepare_claimed_delete(
        &self,
        entry_url: &Url,
        claim: &RemoteDeletionClaim,
        timeout_ms: u64,
    ) -> Result<ConditionalEventDeleteRequest, DavExecutionError> {
        let target = self.resolve_exact_mutation_target(
            entry_url,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        ConditionalEventDeleteRequest::new(self, target, claim.base_etag.clone(), timeout_ms)
    }

    pub(crate) fn prepare_conditional_delete(
        &self,
        entry_url: &Url,
        collection_href: &str,
        exact_href: &str,
        base_etag: String,
        timeout_ms: u64,
    ) -> Result<ConditionalEventDeleteRequest, DavExecutionError> {
        let target = self.resolve_exact_mutation_target(entry_url, collection_href, exact_href)?;
        ConditionalEventDeleteRequest::new(self, target, base_etag, timeout_ms)
    }

    pub(crate) fn prepare_claimed_deletion_read_back(
        &self,
        entry_url: &Url,
        claim: &RemoteDeletionClaim,
        timeout_ms: u64,
    ) -> Result<ClaimDeletionReadBackRequest, DavExecutionError> {
        let target = self.resolve_exact_mutation_target(
            entry_url,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        let request = DavRequest::new(
            DavMethod::Get,
            target.url,
            Vec::new(),
            DavDepth::Zero,
            timeout_ms,
        )?;
        Ok(ClaimDeletionReadBackRequest { request })
    }
}

/// Internal target material only. Debug must not reveal its resource URL.
#[allow(dead_code)]
pub(crate) struct ExactMutationTarget {
    url: Url,
}

impl fmt::Debug for ExactMutationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExactMutationTarget(<redacted>)")
    }
}

/// A claim-bound read request and its single-use canonical response check.
/// Independent CREATE envelope; title/probe registration cannot construct it.
#[allow(dead_code)]
pub(crate) struct ConditionalCreateRequest {
    url: Url,
    body: Vec<u8>,
    attempt: Option<crate::domain::remote_creation::RemoteCreationAttemptEvidence>,
    session: Option<crate::application::production_creation_owner::CreationSessionEvidence>,
}
#[allow(dead_code)]
impl ConditionalCreateRequest {
    pub(crate) fn from_attempt(
        policy: &DavTransportPolicy,
        entry: &Url,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        attempt: crate::domain::remote_creation::RemoteCreationAttemptEvidence,
        session: crate::application::production_creation_owner::CreationSessionEvidence,
        _: &crate::adapters::production_mutation::CompleteCreationDispatchAuthorization,
    ) -> Result<Self, DavExecutionError> {
        if claim.codec_version != "utc-v1"
            || !attempt.matches_claim(claim)
            || claim.frozen_content().is_empty()
            || claim.frozen_content().len() > 8192
            || crate::domain::remote_mutation::single_vevent_uid(claim.frozen_content()).is_err()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        let target = policy.resolve_exact_mutation_target(
            entry,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        Ok(Self {
            url: target.url,
            body: claim.frozen_content().to_vec(),
            attempt: Some(attempt),
            session: Some(session),
        })
    }
    pub(crate) fn into_parts(
        mut self,
    ) -> (
        Url,
        Vec<u8>,
        crate::domain::remote_creation::RemoteCreationAttemptEvidence,
        crate::application::production_creation_owner::CreationSessionEvidence,
    ) {
        (
            self.url.clone(),
            std::mem::take(&mut self.body),
            self.attempt.take().expect("private consuming attempt"),
            self.session
                .take()
                .expect("private consuming session proof"),
        )
    }
}
impl fmt::Debug for ConditionalCreateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConditionalCreateRequest(<redacted>)")
    }
}
impl Drop for ConditionalCreateRequest {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

/// A claim-bound read request and its single-use canonical response check.
/// Independent CREATE envelope; title/probe registration cannot construct it.
#[allow(dead_code)]
pub(crate) struct ConditionalCreateV2Request {
    url: Url,
    body: Vec<u8>,
    attempt: Option<crate::domain::remote_creation::RemoteCreationAttemptEvidence>,
    session: Option<crate::application::production_creation_owner::CreationSessionEvidence>,
}
#[allow(dead_code)]
impl ConditionalCreateV2Request {
    pub(crate) fn from_attempt(
        policy: &DavTransportPolicy,
        entry: &Url,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        attempt: crate::domain::remote_creation::RemoteCreationAttemptEvidence,
        session: crate::application::production_creation_owner::CreationSessionEvidence,
        _: &crate::adapters::production_mutation::CompleteCreationV2DispatchAuthorization,
    ) -> Result<Self, DavExecutionError> {
        if claim.codec_version != "utc-v2"
            || !attempt.matches_claim(claim)
            || claim.frozen_content().is_empty()
            || claim.frozen_content().len() > 65536
            || crate::domain::remote_mutation::single_vevent_uid_v2(claim.frozen_content()).is_err()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        let target = policy.resolve_exact_mutation_target(
            entry,
            &claim.collection_href,
            &claim.exact_href,
        )?;
        Ok(Self {
            url: target.url,
            body: claim.frozen_content().to_vec(),
            attempt: Some(attempt),
            session: Some(session),
        })
    }
    pub(crate) fn into_parts(
        mut self,
    ) -> (
        Url,
        Vec<u8>,
        crate::domain::remote_creation::RemoteCreationAttemptEvidence,
        crate::application::production_creation_owner::CreationSessionEvidence,
    ) {
        (
            self.url.clone(),
            std::mem::take(&mut self.body),
            self.attempt.take().expect("private consuming attempt"),
            self.session
                .take()
                .expect("private consuming session proof"),
        )
    }
}
impl fmt::Debug for ConditionalCreateV2Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConditionalCreateV2Request(<redacted>)")
    }
}
impl Drop for ConditionalCreateV2Request {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

/// Canonical evidence from one non-redirecting exact GET on the reviewed executor.
#[allow(dead_code)]
pub(crate) fn read_creation_exact_guarded(
    policy: &DavTransportPolicy,
    entry: &Url,
    claim: &crate::domain::remote_creation::RemoteCreationClaim,
    executor: &crate::adapters::caldav_http::HttpDavExecutor,
    authentication: &DavAuthentication,
    accept: &dyn Fn() -> bool,
) -> Result<crate::domain::remote_creation::RemoteCreationReadBack, RemoteMutationTransportError> {
    let target = policy
        .resolve_exact_mutation_target(entry, &claim.collection_href, &claim.exact_href)
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let request = DavRequest::new(
        DavMethod::Get,
        target.url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let raw = executor
        .execute_exact_get_guarded(&request, authentication, accept)
        .map_err(|e| match e {
            DavExecutionError::Cancelled => RemoteMutationTransportError::Cancelled,
            DavExecutionError::Offline
            | DavExecutionError::Dns
            | DavExecutionError::Timeout
            | DavExecutionError::Tls => RemoteMutationTransportError::Transient,
            _ => RemoteMutationTransportError::UnsafeData,
        })?;
    if raw.location.is_some()
        || raw.body.len() > 4 * 1024 * 1024
        || raw
            .content_type
            .as_ref()
            .is_some_and(|s| s.len() > MAX_CONTENT_TYPE_BYTES)
        || raw.etag.as_ref().is_some_and(|s| s.len() > MAX_ETAG_BYTES)
    {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    if matches!(raw.status, 404 | 410) {
        return Ok(crate::domain::remote_creation::RemoteCreationReadBack::Absent(raw.status));
    }
    let response = DavResponse {
        final_url: target.url.clone(),
        status: raw.status,
        content_type: raw.content_type,
        etag: raw.etag,
        body: raw.body,
    };
    canonical_mutation_read_back(response, &target.url)
        .map(crate::domain::remote_creation::RemoteCreationReadBack::Retained)
}

#[allow(dead_code)]
pub(crate) fn read_creation_v2_exact_guarded(
    policy: &DavTransportPolicy,
    entry: &Url,
    claim: &crate::domain::remote_creation::RemoteCreationClaim,
    executor: &crate::adapters::caldav_http::HttpDavExecutor,
    authentication: &DavAuthentication,
    _: &crate::adapters::production_mutation::CompleteCreationV2DispatchAuthorization,
    accept: &dyn Fn() -> bool,
) -> Result<crate::domain::remote_creation::RemoteCreationReadBack, RemoteMutationTransportError> {
    if claim.codec_version != "utc-v2" {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    let target = policy
        .resolve_exact_mutation_target(entry, &claim.collection_href, &claim.exact_href)
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let request = DavRequest::new(
        DavMethod::Get,
        target.url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let raw = executor
        .execute_exact_get_guarded(&request, authentication, accept)
        .map_err(|e| match e {
            DavExecutionError::Cancelled => RemoteMutationTransportError::Cancelled,
            DavExecutionError::Offline
            | DavExecutionError::Dns
            | DavExecutionError::Timeout
            | DavExecutionError::Tls => RemoteMutationTransportError::Transient,
            _ => RemoteMutationTransportError::UnsafeData,
        })?;
    if raw.location.is_some()
        || raw.body.len() > 4 * 1024 * 1024
        || raw
            .content_type
            .as_ref()
            .is_some_and(|s| s.len() > MAX_CONTENT_TYPE_BYTES)
        || raw.etag.as_ref().is_some_and(|s| s.len() > MAX_ETAG_BYTES)
    {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    if matches!(raw.status, 404 | 410) {
        return Ok(crate::domain::remote_creation::RemoteCreationReadBack::Absent(raw.status));
    }
    let response = DavResponse {
        final_url: target.url.clone(),
        status: raw.status,
        content_type: raw.content_type,
        etag: raw.etag,
        body: raw.body,
    };
    canonical_mutation_read_back(response, &target.url)
        .map(crate::domain::remote_creation::RemoteCreationReadBack::Retained)
}

/// A claim-bound read request and its single-use canonical response check.
#[allow(dead_code)]
pub(crate) struct ClaimReadBackRequest {
    request: DavRequest,
}

#[allow(dead_code)]
impl ClaimReadBackRequest {
    pub(crate) fn request(&self) -> &DavRequest {
        &self.request
    }

    pub(crate) fn accept(
        self,
        response: DavResponse,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        canonical_mutation_read_back(response, self.request.url())
    }

    pub(crate) fn execute_guarded(
        self,
        executor: &crate::adapters::caldav_http::HttpDavExecutor,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        let response = execute_claim_exact_get(&self.request, executor, authentication, accept)?;
        canonical_mutation_read_back(response, self.request.url())
    }
}

impl fmt::Debug for ClaimReadBackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimReadBackRequest(<redacted>)")
    }
}

/// Validated input for a future single-resource conditional title update.
/// This type has no executor method: constructing it cannot send a request.
/// The existing `DavRequest` and `DavMethod` remain read-only.
#[allow(dead_code)] // Deliberately staged before any production writer is registered.
pub(crate) struct ConditionalTitleUpdateRequest {
    url: Url,
    base_etag: String,
    body: Vec<u8>,
    timeout_ms: u64,
}

#[allow(dead_code)]
impl ConditionalTitleUpdateRequest {
    fn new(
        policy: &DavTransportPolicy,
        target: ExactMutationTarget,
        mut base_etag: String,
        mut body: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<Self, DavExecutionError> {
        let url = target.url;
        if let Err(error) = policy.validate_destination(&url) {
            base_etag.zeroize();
            body.zeroize();
            return Err(error);
        }
        let valid_etag = strong_mutation_etag(&base_etag);
        if !valid_etag
            || body.is_empty()
            || body.len() > MAX_REQUEST_BODY_BYTES
            || !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms)
        {
            base_etag.zeroize();
            body.zeroize();
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(Self {
            url,
            base_etag,
            body,
            timeout_ms,
        })
    }

    pub(crate) fn url(&self) -> &Url {
        &self.url
    }
    pub(crate) fn base_etag(&self) -> &str {
        &self.base_etag
    }

    pub(crate) fn into_exchange_parts(mut self) -> (Url, Vec<u8>, SecretBytes, u64) {
        let url = self.url.clone();
        let body = std::mem::take(&mut self.body);
        let etag = SecretBytes::new(std::mem::take(&mut self.base_etag).into_bytes());
        (url, body, etag, self.timeout_ms)
    }
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
    pub(crate) fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
}

/// Independent actor-snapshot envelope; cannot be constructed from title claims.
// Internal v2 path stays outside worker/product composition until B3c.
#[allow(dead_code)]
pub(crate) struct ConditionalFieldsUpdateRequest {
    url: Url,
    base_etag: String,
    body: Vec<u8>,
}
// Internal v2 path stays outside worker/product composition until B3c.
#[allow(dead_code)]
impl ConditionalFieldsUpdateRequest {
    pub(crate) fn from_snapshot(
        policy: &DavTransportPolicy,
        entry: &Url,
        snapshot: crate::domain::remote_mutation::RemoteFieldsSendSnapshot,
        _: &crate::adapters::production_mutation::CompleteFieldsTransportAuthorization,
    ) -> Result<Self, DavExecutionError> {
        let target = policy.resolve_exact_mutation_target(
            entry,
            &snapshot.collection_href,
            &snapshot.exact_href,
        )?;
        if !strong_mutation_etag(&snapshot.base_etag)
            || snapshot.expected_raw_ics.is_empty()
            || snapshot.expected_raw_ics.len() > MAX_REQUEST_BODY_BYTES
            || crate::domain::remote_mutation::single_vevent_uid_v2(&snapshot.expected_raw_ics)
                .is_err()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(Self {
            url: target.url,
            base_etag: snapshot.base_etag,
            body: snapshot.expected_raw_ics,
        })
    }
    pub(crate) fn into_exchange_parts(mut self) -> (Url, Vec<u8>, SecretBytes, u64) {
        (
            self.url.clone(),
            std::mem::take(&mut self.body),
            SecretBytes::new(std::mem::take(&mut self.base_etag).into_bytes()),
            30_000,
        )
    }
}
impl fmt::Debug for ConditionalFieldsUpdateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConditionalFieldsUpdateRequest(<redacted>)")
    }
}
impl Drop for ConditionalFieldsUpdateRequest {
    fn drop(&mut self) {
        self.body.zeroize();
        self.base_etag.zeroize();
    }
}
// Internal v2 path stays outside worker/product composition until B3c.
#[allow(dead_code)]
pub(crate) fn read_fields_exact_guarded(
    policy: &DavTransportPolicy,
    entry: &Url,
    claim: &crate::domain::remote_mutation::RemoteFieldsMutationClaim,
    executor: &crate::adapters::caldav_http::HttpDavExecutor,
    authentication: &DavAuthentication,
    _: &crate::adapters::production_mutation::CompleteFieldsTransportAuthorization,
    accept: &dyn Fn() -> bool,
) -> Result<RemoteReadBack, RemoteMutationTransportError> {
    let target = policy
        .resolve_exact_mutation_target(entry, &claim.collection_href, &claim.exact_href)
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let request = DavRequest::new(
        DavMethod::Get,
        target.url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let response = execute_claim_exact_get(&request, executor, authentication, accept)?;
    canonical_mutation_read_back(response, &target.url)
}

/// Independent event-v3 envelope, minted only by the concrete guarded adapter.
#[allow(dead_code)]
pub(crate) struct ConditionalEventUpdateRequest {
    url: Url,
    base_etag: String,
    body: Vec<u8>,
}
#[allow(dead_code)]
impl ConditionalEventUpdateRequest {
    pub(crate) fn from_snapshot(
        policy: &DavTransportPolicy,
        entry: &Url,
        claim: &crate::domain::remote_event_update::RemoteEventMutationClaimV3,
        snapshot: crate::domain::remote_event_update::RemoteEventUpdateSnapshotV3,
        _: &crate::adapters::production_mutation::CompleteEventTransportAuthorization,
    ) -> Result<Self, DavExecutionError> {
        use crate::domain::remote_event_update::{
            decode_event_intent_v3, patch_single_utc_event_v3,
        };
        let c = &claim.captured;
        let s = &snapshot.captured;
        let fields = decode_event_intent_v3(c.intent_json.as_bytes())
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        let proof = patch_single_utc_event_v3(&c.base_raw_ics, &fields)
            .map_err(|_| DavExecutionError::InvalidRequest)?;
        if c.phase != crate::domain::remote_mutation::RemoteMutationExecutionPhase::Ready
            || c.operation_id != s.operation_id
            || c.account_id != s.account_id
            || c.calendar_id != s.calendar_id
            || c.resource_id != s.resource_id
            || c.collection_href != s.collection_href
            || c.exact_href != s.exact_href
            || c.base_etag != s.base_etag
            || !strong_mutation_etag(&s.base_etag)
            || proof.uid() != c.uid
            || proof.bytes() != s.expected_raw_ics
            || proof.bytes() != c.expected_raw_ics
            || proof.before_time() != snapshot.before_time()
            || proof.after_time() != snapshot.after_time()
            || proof.time_alarm_evidence() != snapshot.time_alarm_evidence()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        for id in [
            &c.operation_id,
            &c.account_id,
            &c.calendar_id,
            &c.resource_id,
            &c.series_id,
        ] {
            uuid::Uuid::parse_str(id).map_err(|_| DavExecutionError::InvalidRequest)?;
        }
        let target =
            policy.resolve_exact_mutation_target(entry, &s.collection_href, &s.exact_href)?;
        Ok(Self {
            url: target.url,
            base_etag: s.base_etag.clone(),
            body: s.expected_raw_ics.clone(),
        })
    }
    pub(crate) fn into_exchange_parts(mut self) -> (Url, Vec<u8>, SecretBytes, u64) {
        (
            self.url.clone(),
            std::mem::take(&mut self.body),
            SecretBytes::new(std::mem::take(&mut self.base_etag).into_bytes()),
            30_000,
        )
    }
}
impl fmt::Debug for ConditionalEventUpdateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConditionalEventUpdateRequest(<redacted>)")
    }
}
impl Drop for ConditionalEventUpdateRequest {
    fn drop(&mut self) {
        self.body.zeroize();
        self.base_etag.zeroize();
    }
}
#[allow(dead_code)]
pub(crate) fn read_event_exact_guarded(
    policy: &DavTransportPolicy,
    entry: &Url,
    claim: &crate::domain::remote_event_update::RemoteEventMutationClaimV3,
    executor: &crate::adapters::caldav_http::HttpDavExecutor,
    authentication: &DavAuthentication,
    _: &crate::adapters::production_mutation::CompleteEventTransportAuthorization,
    accept: &dyn Fn() -> bool,
) -> Result<RemoteReadBack, RemoteMutationTransportError> {
    if !accept() {
        return Err(RemoteMutationTransportError::Cancelled);
    }
    let c = &claim.captured;
    let fields =
        crate::domain::remote_event_update::decode_event_intent_v3(c.intent_json.as_bytes())
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let proof =
        crate::domain::remote_event_update::patch_single_utc_event_v3(&c.base_raw_ics, &fields)
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    if !strong_mutation_etag(&c.base_etag)
        || proof.uid() != c.uid
        || proof.bytes() != c.expected_raw_ics
    {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    let target = policy
        .resolve_exact_mutation_target(entry, &c.collection_href, &c.exact_href)
        .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let request = DavRequest::new(
        DavMethod::Get,
        target.url.clone(),
        Vec::new(),
        DavDepth::Zero,
        30_000,
    )
    .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
    let response = execute_claim_exact_get(&request, executor, authentication, accept)?;
    if response
        .content_type
        .as_ref()
        .is_some_and(|v| v.len() > MAX_CONTENT_TYPE_BYTES)
        || response
            .etag
            .as_ref()
            .is_some_and(|v| v.len() > MAX_ETAG_BYTES)
    {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    canonical_mutation_read_back(response, &target.url)
}

/// Converts only a same-target canonical GET into verification material.
/// The mutation service still checks ICS identity and intended semantics before
/// it can mark an operation applied. No production caller is registered.
pub(crate) fn canonical_mutation_read_back(
    response: DavResponse,
    expected_url: &Url,
) -> Result<RemoteReadBack, RemoteMutationTransportError> {
    if response.final_url != *expected_url {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    match response.status {
        401 | 403 => return Err(RemoteMutationTransportError::Unauthorized),
        500..=599 => return Err(RemoteMutationTransportError::Transient),
        200 => {}
        _ => return Err(RemoteMutationTransportError::UnsafeData),
    }
    if !response.content_type.as_deref().is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/calendar"))
    }) {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    let read_back = RemoteReadBack {
        etag: response
            .etag
            .ok_or(RemoteMutationTransportError::UnsafeData)?,
        raw_ics: response.body,
    };
    if !read_back.has_bounded_strong_revision() {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    Ok(read_back)
}

impl fmt::Debug for ConditionalTitleUpdateRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConditionalTitleUpdateRequest")
            .field("destination", &"[REDACTED]")
            .field("base_etag", &"[REDACTED]")
            .field("body_bytes", &self.body.len())
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// Consuming conditional DELETE shaped only from an actor-owned deletion
/// claim. It carries no body and cannot enter the generic DAV exchange.
pub(crate) struct ConditionalEventDeleteRequest {
    url: Url,
    base_etag: String,
    timeout_ms: u64,
}

impl ConditionalEventDeleteRequest {
    fn new(
        policy: &DavTransportPolicy,
        target: ExactMutationTarget,
        mut base_etag: String,
        timeout_ms: u64,
    ) -> Result<Self, DavExecutionError> {
        if policy.validate_destination(&target.url).is_err()
            || !strong_mutation_etag(&base_etag)
            || !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms)
        {
            base_etag.zeroize();
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(Self {
            url: target.url,
            base_etag,
            timeout_ms,
        })
    }

    pub(crate) fn into_exchange_parts(mut self) -> (Url, SecretBytes, u64) {
        let url = self.url.clone();
        let etag = SecretBytes::new(std::mem::take(&mut self.base_etag).into_bytes());
        (url, etag, self.timeout_ms)
    }
}

impl Drop for ConditionalEventDeleteRequest {
    fn drop(&mut self) {
        self.base_etag.zeroize();
    }
}

impl fmt::Debug for ConditionalEventDeleteRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConditionalEventDeleteRequest(<redacted>)")
    }
}

pub(crate) struct ClaimDeletionReadBackRequest {
    request: DavRequest,
}

impl ClaimDeletionReadBackRequest {
    pub(crate) fn execute_guarded(
        self,
        executor: &crate::adapters::caldav_http::HttpDavExecutor,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RemoteDeletionReadBack, RemoteMutationTransportError> {
        let expected_url = self.request.url().clone();
        let response = execute_claim_exact_get(&self.request, executor, authentication, accept)?;
        match response.status() {
            404 | 410 => RemoteDeletionReadBack::absent(response.status())
                .map_err(|_| RemoteMutationTransportError::UnsafeData),
            200 => canonical_mutation_read_back(response, &expected_url)
                .map(RemoteDeletionReadBack::retained),
            401 | 403 => Err(RemoteMutationTransportError::Unauthorized),
            500..=599 => Err(RemoteMutationTransportError::Transient),
            _ => Err(RemoteMutationTransportError::UnsafeData),
        }
    }
}

impl fmt::Debug for ClaimDeletionReadBackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimDeletionReadBackRequest(<redacted>)")
    }
}

fn execute_claim_exact_get(
    request: &DavRequest,
    executor: &crate::adapters::caldav_http::HttpDavExecutor,
    authentication: &DavAuthentication,
    accept: &dyn Fn() -> bool,
) -> Result<DavResponse, RemoteMutationTransportError> {
    let raw = executor
        .execute_exact_get_guarded(request, authentication, accept)
        .map_err(map_read_execution_error)?;
    if raw.location.is_some() {
        return Err(RemoteMutationTransportError::UnsafeData);
    }
    Ok(DavResponse {
        final_url: request.url().clone(),
        status: raw.status,
        content_type: raw.content_type,
        etag: raw.etag,
        body: raw.body,
    })
}

fn map_read_execution_error(error: DavExecutionError) -> RemoteMutationTransportError {
    match error {
        DavExecutionError::Cancelled => RemoteMutationTransportError::Cancelled,
        DavExecutionError::Offline
        | DavExecutionError::Dns
        | DavExecutionError::Timeout
        | DavExecutionError::Tls => RemoteMutationTransportError::Transient,
        _ => RemoteMutationTransportError::UnsafeData,
    }
}

impl Drop for ConditionalTitleUpdateRequest {
    fn drop(&mut self) {
        self.base_etag.zeroize();
        self.body.zeroize();
    }
}

/// Classifies one attempted conditional PUT without inspecting response text.
/// A redirect is never replayed, and every uncertain delivery requires the
/// mutation service's GET-first recovery before another write can be claimed.
#[allow(dead_code)] // The production write executor remains unavailable.
pub(crate) fn classify_conditional_update_result(
    outcome: Result<RawDavResponse, DavExecutionError>,
) -> Result<(), RemoteMutationTransportError> {
    match outcome {
        Ok(response)
            if !(100..=599).contains(&response.status)
                || response.body.len() > DEFAULT_MAX_RESPONSE_BODY_BYTES =>
        {
            Err(RemoteMutationTransportError::Ambiguous)
        }
        Ok(RawDavResponse {
            status: 200 | 201 | 204,
            ..
        }) => Ok(()),
        Ok(RawDavResponse { status: 412, .. }) => {
            Err(RemoteMutationTransportError::PreconditionFailed)
        }
        Ok(RawDavResponse {
            status: 401 | 403, ..
        }) => Err(RemoteMutationTransportError::Unauthorized),
        Err(DavExecutionError::InvalidRequest) => Err(RemoteMutationTransportError::UnsafeData),
        // A timeout, cancellation, redirect, malformed response, or any other
        // status after attempted delivery does not prove the write was absent.
        Ok(_) | Err(_) => Err(RemoteMutationTransportError::Ambiguous),
    }
}

fn validate_https_url(url: &Url) -> Result<(), DavExecutionError> {
    if url.as_str().len() > MAX_URL_BYTES
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(DavExecutionError::InvalidRequest);
    }
    Ok(())
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DavExecutionError {
    #[error("DAV request is invalid")]
    InvalidRequest,
    #[error("DAV response is invalid")]
    InvalidResponse,
    #[error("DAV redirect is unsafe")]
    UnsafeRedirect,
    #[error("DAV redirect limit was exceeded")]
    TooManyRedirects,
    #[error("DAV response exceeded its configured limit")]
    ResponseTooLarge,
    #[error("DAV request was cancelled")]
    Cancelled,
    #[error("DAV request timed out")]
    Timeout,
    #[error("DAV service is offline")]
    Offline,
    #[error("DAV name resolution failed")]
    Dns,
    #[error("DAV TLS negotiation failed")]
    Tls,
    #[error("DAV scripted execution did not match the expected request")]
    ScriptMismatch,
    #[error("DAV scripted execution state is unavailable")]
    Unavailable,
}

pub struct ScriptedRequest {
    method: DavMethod,
    url: String,
    body: Vec<u8>,
    depth: DavDepth,
    authentication_required: bool,
}

impl ScriptedRequest {
    pub fn new(
        method: DavMethod,
        url: impl Into<String>,
        body: Vec<u8>,
        depth: DavDepth,
        authentication_required: bool,
    ) -> Self {
        Self {
            method,
            url: url.into(),
            body,
            depth,
            authentication_required,
        }
    }
}

impl fmt::Debug for ScriptedRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptedRequest")
            .field("method", &self.method)
            .field("destination", &"[REDACTED]")
            .field("body_bytes", &self.body.len())
            .field("depth", &self.depth)
            .field("authentication_required", &self.authentication_required)
            .finish()
    }
}

pub struct ScriptedStep {
    request: ScriptedRequest,
    outcome: ScriptedOutcome,
}

enum ScriptedOutcome {
    Immediate(Result<RawDavResponse, DavExecutionError>),
    BlockUntilCancelled,
}

impl ScriptedStep {
    pub fn response(request: ScriptedRequest, response: RawDavResponse) -> Self {
        Self {
            request,
            outcome: ScriptedOutcome::Immediate(Ok(response)),
        }
    }

    pub fn error(request: ScriptedRequest, error: DavExecutionError) -> Self {
        Self {
            request,
            outcome: ScriptedOutcome::Immediate(Err(error)),
        }
    }

    pub fn block_until_cancelled(request: ScriptedRequest) -> Self {
        Self {
            request,
            outcome: ScriptedOutcome::BlockUntilCancelled,
        }
    }
}

pub struct ScriptedDavExecutor {
    steps: Mutex<VecDeque<ScriptedStep>>,
    cancellation: DavCancellation,
}

impl ScriptedDavExecutor {
    pub fn new(steps: Vec<ScriptedStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            cancellation: DavCancellation::new(),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.steps.lock().is_ok_and(|steps| steps.is_empty())
    }
}

impl DavExecutor for ScriptedDavExecutor {
    fn execute(
        &self,
        request: &DavRequest,
        authentication: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let mut steps = self
            .steps
            .lock()
            .map_err(|_| DavExecutionError::Unavailable)?;
        let step = steps.pop_front().ok_or(DavExecutionError::ScriptMismatch)?;
        if request.method() != step.request.method
            || request.url().as_str() != step.request.url
            || request.body() != step.request.body
            || request.depth() != step.request.depth
            || (step.request.authentication_required && !authentication.is_present())
        {
            return Err(DavExecutionError::ScriptMismatch);
        }
        drop(steps);
        match step.outcome {
            ScriptedOutcome::Immediate(outcome) => outcome,
            ScriptedOutcome::BlockUntilCancelled => loop {
                if self.cancellation.is_cancelled() {
                    return Err(DavExecutionError::Cancelled);
                }
                thread::sleep(Duration::from_millis(5));
            },
        }
    }

    fn cancellation(&self) -> Option<DavCancellation> {
        Some(self.cancellation.clone())
    }
}

// Offline Probe constraints only; no production owner mint or executor entry.
#[allow(dead_code)]
pub(crate) mod probe_harness;
