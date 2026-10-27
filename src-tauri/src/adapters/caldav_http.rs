//! Reviewed HTTPS executor with a read-only public CalDAV boundary.
//!
//! The client is pinned to the same stack that produced the Phase 0
//! compatibility evidence: `reqwest` blocking with rustls, no system,
//! environment, or PAC proxy, and no automatic redirect following, so the
//! caller keeps validating every hop against the explicit trusted-host set.
//!
//! Transport classification is deliberately coarse but bounded. Deadlines and
//! name-resolution failures are reported separately; every other connect-class
//! failure (including TLS negotiation failures that the stack surfaces as
//! connect errors) is reported as `Offline`. All of them already collapse into
//! the same transient provider outcome, so no caller can branch on a guess.
//!
//! Private consuming conditional paths serve the unwired title transport and
//! the explicitly authorized standalone diagnostic. This module opens
//! sockets only when a selected executor is explicitly driven.

use super::caldav_transport::{
    ConditionalEventDeleteRequest, ConditionalTitleUpdateRequest, DEFAULT_MAX_RESPONSE_BODY_BYTES,
    DavAuthentication, DavCancellation, DavExecutionError, DavExecutor, DavMethod, DavRequest,
    MAX_CONTENT_TYPE_BYTES, MAX_ETAG_BYTES, MAX_URL_BYTES, RawDavResponse,
};
use crate::ports::credentials::SecretBytes;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Method,
    blocking::Client,
    header::{
        AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, HeaderValue, IF_MATCH,
        IF_NONE_MATCH, LOCATION,
    },
};
use std::{fmt, io::Read, sync::Arc, time::Duration};
use url::Url;
use zeroize::Zeroize;

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
const MIN_CONNECT_TIMEOUT_MS: u64 = 1_000;
const MAX_CONNECT_TIMEOUT_MS: u64 = 30_000;
const MIN_MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024;
const MAX_MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 32 * 1024;
const REQUEST_CONTENT_TYPE: &str = "application/xml; charset=utf-8";
const CALENDAR_CONTENT_TYPE: &str = "text/calendar; charset=utf-8";
const USER_AGENT_VALUE: &str = "calendar-todo/0.1 (+CalDAV read-only)";

/// Non-secret executor configuration. Both deadlines and the response cap are
/// bounded so a caller cannot widen them into an unbounded read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpDavConfig {
    connect_timeout_ms: u64,
    max_response_body_bytes: usize,
}

impl Default for HttpDavConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
        }
    }
}

impl HttpDavConfig {
    pub fn new(
        connect_timeout_ms: u64,
        max_response_body_bytes: usize,
    ) -> Result<Self, DavExecutionError> {
        if !(MIN_CONNECT_TIMEOUT_MS..=MAX_CONNECT_TIMEOUT_MS).contains(&connect_timeout_ms)
            || !(MIN_MAX_RESPONSE_BODY_BYTES..=MAX_MAX_RESPONSE_BODY_BYTES)
                .contains(&max_response_body_bytes)
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        Ok(Self {
            connect_timeout_ms,
            max_response_body_bytes,
        })
    }

    pub fn connect_timeout_ms(self) -> u64 {
        self.connect_timeout_ms
    }

    pub fn max_response_body_bytes(self) -> usize {
        self.max_response_body_bytes
    }
}

/// One HTTPS exchange. The real implementation owns the pinned client; tests
/// inject deterministic responses through the same contract.
pub(crate) trait DavHttpExchange: Send + Sync {
    fn exchange_probe_acquisition_guarded(
        &self,
        _request: &HttpExchangeRequest,
        _cancellation: &DavCancellation,
        _accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        Err(DavExecutionError::InvalidRequest)
    }
    // Unreviewed exchange implementations must not approximate a socket guard.
    fn exchange_probe_metadata_guarded(
        &self,
        _request: &HttpExchangeRequest,
        _cancellation: &DavCancellation,
        _accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        Err(DavExecutionError::InvalidRequest)
    }
    fn exchange(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
    ) -> Result<RawDavResponse, DavExecutionError>;

    fn exchange_exact_get_guarded(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        self.exchange(request, cancellation)
    }

    fn exchange_conditional(
        &self,
        _request: HttpExchangeRequest,
        _cancellation: &DavCancellation,
    ) -> Result<RawDavResponse, DavExecutionError> {
        Err(DavExecutionError::InvalidRequest)
    }

    fn exchange_conditional_guarded(
        &self,
        request: HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        self.exchange_conditional(request, cancellation)
    }
}

/// Fully resolved exchange request. Outside tests it is only produced from an
/// already validated read-only [`DavRequest`]. The private conditional path
/// starts from a separately validated, consuming request and can only enter
/// the exchange through `exchange_conditional`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpRequestMethod {
    Read(DavMethod),
    ConditionalPut,
    ConditionalCreate,
    DedicatedProbeCreate,
    ConditionalDelete,
}

fn probe_metadata_exchange_shape(request: &HttpExchangeRequest) -> bool {
    request.method() == HttpRequestMethod::Read(DavMethod::Propfind)
        && request.depth() == Some("0")
        && request.content_type() == Some(REQUEST_CONTENT_TYPE)
        && request.body() == super::caldav_transport::probe_harness::METADATA_BODY
        && request.timeout() == Duration::from_millis(30_000)
        && request.authorization().is_some()
        && request.if_match().is_none()
        && !request.if_none_match
}

fn probe_acquisition_exchange_shape(request: &HttpExchangeRequest) -> bool {
    use super::production_mutation::probe_preflight::live_acquisition::{
        HOME_BODY, PRINCIPAL_BODY,
    };
    request.method() == HttpRequestMethod::Read(DavMethod::Propfind)
        && ((request.depth() == Some("0")
            && [
                PRINCIPAL_BODY,
                HOME_BODY,
                super::caldav_transport::probe_harness::METADATA_BODY,
            ]
            .contains(&request.body()))
            || (request.depth() == Some("1")
                && request.body() == super::caldav_transport::probe_harness::METADATA_BODY))
        && request.content_type() == Some(REQUEST_CONTENT_TYPE)
        && request.timeout() == Duration::from_millis(30_000)
        && request.authorization().is_some()
        && request.if_match().is_none()
        && !request.if_none_match
}

pub(crate) struct HttpExchangeRequest {
    method: HttpRequestMethod,
    url: Url,
    body: Vec<u8>,
    depth: Option<&'static str>,
    content_type: Option<&'static str>,
    timeout: Duration,
    authorization: Option<SecretBytes>,
    if_match: Option<SecretBytes>,
    if_none_match: bool,
}

impl HttpExchangeRequest {
    fn from_creation(
        request: super::caldav_transport::ConditionalCreateRequest,
        authentication: &DavAuthentication,
    ) -> Result<
        (
            Self,
            crate::domain::remote_creation::RemoteCreationAttemptEvidence,
            crate::application::production_creation_owner::CreationSessionEvidence,
        ),
        DavExecutionError,
    > {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body, attempt, session) = request.into_parts();
        Ok((
            Self {
                method: HttpRequestMethod::ConditionalCreate,
                url,
                body,
                depth: None,
                content_type: Some(CALENDAR_CONTENT_TYPE),
                timeout: Duration::from_millis(30_000),
                authorization: Some(authorization),
                if_match: None,
                if_none_match: true,
            },
            attempt,
            session,
        ))
    }
    #[allow(dead_code)]
    fn from_creation_v2(
        request: super::caldav_transport::ConditionalCreateV2Request,
        authentication: &DavAuthentication,
    ) -> Result<
        (
            Self,
            crate::domain::remote_creation::RemoteCreationAttemptEvidence,
            crate::application::production_creation_owner::CreationSessionEvidence,
        ),
        DavExecutionError,
    > {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body, attempt, session) = request.into_parts();
        Ok((
            Self {
                method: HttpRequestMethod::ConditionalCreate,
                url,
                body,
                depth: None,
                content_type: Some(CALENDAR_CONTENT_TYPE),
                timeout: Duration::from_millis(30_000),
                authorization: Some(authorization),
                if_match: None,
                if_none_match: true,
            },
            attempt,
            session,
        ))
    }
    fn from_boundary(request: &DavRequest, authentication: &DavAuthentication) -> Self {
        let (content_type, depth) = match request.method() {
            DavMethod::Get => (None, None),
            DavMethod::Propfind | DavMethod::Report => {
                (Some(REQUEST_CONTENT_TYPE), Some(request.depth().as_str()))
            }
        };
        Self {
            method: HttpRequestMethod::Read(request.method()),
            url: request.url().clone(),
            body: request.body().to_vec(),
            depth,
            content_type,
            timeout: Duration::from_millis(request.timeout_ms()),
            authorization: basic_authorization(authentication),
            if_match: None,
            if_none_match: false,
        }
    }

    fn from_conditional(
        request: ConditionalTitleUpdateRequest,
        authentication: &DavAuthentication,
    ) -> Result<Self, DavExecutionError> {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body, if_match, timeout_ms) = request.into_exchange_parts();
        Ok(Self {
            method: HttpRequestMethod::ConditionalPut,
            url,
            body,
            depth: None,
            content_type: Some(CALENDAR_CONTENT_TYPE),
            timeout: Duration::from_millis(timeout_ms),
            authorization: Some(authorization),
            if_match: Some(if_match),
            if_none_match: false,
        })
    }

    // Internal v2 path stays outside worker/product composition until B3c.
    #[allow(dead_code)]
    fn from_fields(
        request: super::caldav_transport::ConditionalFieldsUpdateRequest,
        authentication: &DavAuthentication,
    ) -> Result<Self, DavExecutionError> {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body, if_match, timeout_ms) = request.into_exchange_parts();
        Ok(Self {
            method: HttpRequestMethod::ConditionalPut,
            url,
            body,
            depth: None,
            content_type: Some(CALENDAR_CONTENT_TYPE),
            timeout: Duration::from_millis(timeout_ms),
            authorization: Some(authorization),
            if_match: Some(if_match),
            if_none_match: false,
        })
    }

    #[allow(dead_code)]
    fn from_event(
        request: super::caldav_transport::ConditionalEventUpdateRequest,
        authentication: &DavAuthentication,
    ) -> Result<Self, DavExecutionError> {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body, if_match, timeout_ms) = request.into_exchange_parts();
        Ok(Self {
            method: HttpRequestMethod::ConditionalPut,
            url,
            body,
            depth: None,
            content_type: Some(CALENDAR_CONTENT_TYPE),
            timeout: Duration::from_millis(timeout_ms),
            authorization: Some(authorization),
            if_match: Some(if_match),
            if_none_match: false,
        })
    }

    fn from_probe_create(
        request: crate::adapters::mutation_probe::DedicatedProbeCreateRequest,
        authentication: &DavAuthentication,
    ) -> Result<Self, DavExecutionError> {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, body) = request.into_parts();
        Ok(Self {
            method: HttpRequestMethod::DedicatedProbeCreate,
            url,
            body,
            depth: None,
            content_type: Some(CALENDAR_CONTENT_TYPE),
            timeout: Duration::from_millis(30_000),
            authorization: Some(authorization),
            if_match: None,
            if_none_match: true,
        })
    }

    fn from_conditional_delete(
        request: ConditionalEventDeleteRequest,
        authentication: &DavAuthentication,
    ) -> Result<Self, DavExecutionError> {
        let authorization =
            basic_authorization(authentication).ok_or(DavExecutionError::InvalidRequest)?;
        let (url, etag, timeout_ms) = request.into_exchange_parts();
        Ok(Self {
            method: HttpRequestMethod::ConditionalDelete,
            url,
            body: Vec::new(),
            depth: None,
            content_type: None,
            timeout: Duration::from_millis(timeout_ms),
            authorization: Some(authorization),
            if_match: Some(etag),
            if_none_match: false,
        })
    }

    fn method(&self) -> HttpRequestMethod {
        self.method
    }

    fn url(&self) -> &Url {
        &self.url
    }

    fn body(&self) -> &[u8] {
        &self.body
    }

    fn depth(&self) -> Option<&'static str> {
        self.depth
    }

    fn content_type(&self) -> Option<&'static str> {
        self.content_type
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn authorization(&self) -> Option<&SecretBytes> {
        self.authorization.as_ref()
    }

    fn if_match(&self) -> Option<&SecretBytes> {
        self.if_match.as_ref()
    }
}

impl Drop for HttpExchangeRequest {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

impl fmt::Debug for HttpExchangeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpExchangeRequest")
            .field("method", &self.method)
            .field("destination", &"[REDACTED]")
            .field("body_bytes", &self.body.len())
            .field("depth", &self.depth)
            .field("timeout_ms", &self.timeout.as_millis())
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[REDACTED]"),
            )
            .field("if_match", &self.if_match.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Base64 `username:password` for BASIC authentication. Intermediate buffers are
/// zeroized and the result stays opaque.
fn basic_authorization(authentication: &DavAuthentication) -> Option<SecretBytes> {
    if !authentication.is_present() {
        return None;
    }
    let mut raw = Vec::with_capacity(
        authentication.username().len() + 1 + authentication.password_bytes().len(),
    );
    raw.extend_from_slice(authentication.username().as_bytes());
    raw.push(b':');
    raw.extend_from_slice(authentication.password_bytes());
    let mut encoded = format!("Basic {}", STANDARD.encode(&raw)).into_bytes();
    raw.zeroize();
    let secret = SecretBytes::new(std::mem::take(&mut encoded));
    Some(secret)
}

/// Socket-capable executor. Construction is the only place the HTTP stack is
/// selected; every request is injected and bounded by the caller.
pub struct HttpDavExecutor {
    config: HttpDavConfig,
    cancellation: DavCancellation,
    exchange: Arc<dyn DavHttpExchange>,
}

// Minted only after the exact reviewed exchange and its return guard succeed.
pub(crate) struct CompleteProbeMetadataExchange(());
pub(crate) struct CompleteAcquisitionExchange(());

impl HttpDavExecutor {
    pub(crate) fn execute_probe_acquisition_guarded(
        &self,
        permit: super::production_mutation::probe_preflight::live_acquisition::AcquisitionPermit,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if !permit.matches_executor(self) {
            return Err(DavExecutionError::InvalidRequest);
        }
        if self.cancellation.is_cancelled() || !permit.current() {
            return Err(DavExecutionError::Cancelled);
        }
        let (request, auth) = permit.parts();
        let outgoing = HttpExchangeRequest::from_boundary(request, auth);
        if !probe_acquisition_exchange_shape(&outgoing) {
            return Err(DavExecutionError::InvalidRequest);
        }
        let raw = self.exchange.exchange_probe_acquisition_guarded(
            &outgoing,
            &self.cancellation,
            &|| permit.current(),
        )?;
        if self.cancellation.is_cancelled() || !permit.current() {
            return Err(DavExecutionError::Cancelled);
        }
        if raw.body().len()
            > if request.depth() == super::caldav_transport::DavDepth::One {
                64 * 1024
            } else {
                16 * 1024
            }
        {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        permit
            .finish(raw, CompleteAcquisitionExchange(()))
            .map_err(|_| DavExecutionError::InvalidRequest)
    }
    /// A private consuming exact Probe metadata capability, never generic read.
    #[allow(dead_code)]
    pub(crate) fn execute_probe_propfind_guarded(
        &self,
        permit: crate::adapters::production_mutation::probe_preflight::ProbeMetadataSendPermit,
    ) -> Result<
        crate::adapters::production_mutation::probe_preflight::GuardedProbeMetadata,
        DavExecutionError,
    > {
        if !permit.matches_executor(self) {
            return Err(DavExecutionError::InvalidRequest);
        }
        if self.cancellation.is_cancelled() || !permit.current() {
            return Err(DavExecutionError::Cancelled);
        }
        let (request, authentication) = permit.request_and_authentication();
        let exchange_request = HttpExchangeRequest::from_boundary(request, authentication);
        if !probe_metadata_exchange_shape(&exchange_request) {
            return Err(DavExecutionError::InvalidRequest);
        }
        let raw = self.exchange.exchange_probe_metadata_guarded(
            &exchange_request,
            &self.cancellation,
            &|| permit.current(),
        )?;
        if self.cancellation.is_cancelled() || !permit.current() {
            return Err(DavExecutionError::Cancelled);
        }
        if raw.body().len() > 16 * 1024 {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        permit
            .finish(raw, CompleteProbeMetadataExchange(()))
            .map_err(|_| DavExecutionError::InvalidRequest)
    }
    #[allow(dead_code)]
    pub(crate) fn execute_creation_guarded(
        &self,
        request: super::caldav_transport::ConditionalCreateRequest,
        authentication: &DavAuthentication,
        accept: &dyn Fn(
            &crate::domain::remote_creation::RemoteCreationAttemptEvidence,
            &crate::application::production_creation_owner::CreationSessionEvidence,
        ) -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let (request, attempt, session) =
            HttpExchangeRequest::from_creation(request, authentication)?;
        let raw =
            self.exchange
                .exchange_conditional_guarded(request, &self.cancellation, &|| {
                    accept(&attempt, &session)
                })?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }
    #[allow(dead_code)]
    pub(crate) fn execute_creation_v2_guarded(
        &self,
        request: super::caldav_transport::ConditionalCreateV2Request,
        authentication: &DavAuthentication,
        _: &crate::adapters::production_mutation::CompleteCreationV2DispatchAuthorization,
        accept: &dyn Fn(
            &crate::domain::remote_creation::RemoteCreationAttemptEvidence,
            &crate::application::production_creation_owner::CreationSessionEvidence,
        ) -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let (request, attempt, session) =
            HttpExchangeRequest::from_creation_v2(request, authentication)?;
        let raw =
            self.exchange
                .exchange_conditional_guarded(request, &self.cancellation, &|| {
                    accept(&attempt, &session)
                })?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }
    /// One exact GET with a final ownership callback; no redirect loop.
    #[allow(dead_code)]
    pub(crate) fn execute_exact_get_guarded(
        &self,
        request: &DavRequest,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if request.method() != DavMethod::Get
            || !request.body().is_empty()
            || request.depth() != super::caldav_transport::DavDepth::Zero
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_boundary(request, authentication);
        let raw = self.exchange.exchange_exact_get_guarded(
            &exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }
    pub fn new(config: HttpDavConfig) -> Result<Self, DavExecutionError> {
        let exchange = ReqwestDavExchange::new(config)?;
        Ok(Self {
            config,
            cancellation: DavCancellation::new(),
            exchange: Arc::new(exchange),
        })
    }

    pub(in crate::adapters) fn probe_harness(
        config: HttpDavConfig,
        cancellation: DavCancellation,
    ) -> Result<Self, DavExecutionError> {
        let mut executor = Self::new(config)?;
        executor.cancellation = cancellation;
        Ok(executor)
    }

    /// Executes one separately validated conditional request. The request is
    /// consumed into zeroizing exchange storage and is never redirected or
    /// replayed by this layer.
    #[allow(dead_code)] // Activated only when the reviewed M5.1 owner is composed.
    pub(crate) fn execute_conditional(
        &self,
        request: ConditionalTitleUpdateRequest,
        authentication: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError> {
        self.execute_conditional_guarded(request, authentication, &|| true)
    }

    pub(crate) fn execute_conditional_guarded(
        &self,
        request: ConditionalTitleUpdateRequest,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_conditional(request, authentication)?;
        let raw = self.exchange.exchange_conditional_guarded(
            exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    // Internal v2 path stays outside worker/product composition until B3c.
    #[allow(dead_code)]
    pub(crate) fn execute_fields_guarded(
        &self,
        request: super::caldav_transport::ConditionalFieldsUpdateRequest,
        authentication: &DavAuthentication,
        _: &crate::adapters::production_mutation::CompleteFieldsTransportAuthorization,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_fields(request, authentication)?;
        let raw = self.exchange.exchange_conditional_guarded(
            exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    #[allow(dead_code)]
    pub(crate) fn execute_event_guarded(
        &self,
        request: super::caldav_transport::ConditionalEventUpdateRequest,
        authentication: &DavAuthentication,
        _: &crate::adapters::production_mutation::CompleteEventTransportAuthorization,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_event(request, authentication)?;
        let raw = self.exchange.exchange_conditional_guarded(
            exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    pub(crate) fn execute_probe_create_guarded(
        &self,
        request: crate::adapters::mutation_probe::DedicatedProbeCreateRequest,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_probe_create(request, authentication)?;
        let raw = self.exchange.exchange_conditional_guarded(
            exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    pub(crate) fn execute_conditional_delete_guarded(
        &self,
        request: ConditionalEventDeleteRequest,
        authentication: &DavAuthentication,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request =
            HttpExchangeRequest::from_conditional_delete(request, authentication)?;
        let raw = self.exchange.exchange_conditional_guarded(
            exchange_request,
            &self.cancellation,
            accept,
        )?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    #[allow(dead_code)] // Activated only when the reviewed M5.1 owner is composed.
    pub(crate) fn cancel(&self) -> bool {
        self.cancellation.cancel()
    }
}

impl DavExecutor for HttpDavExecutor {
    fn execute(
        &self,
        request: &DavRequest,
        authentication: &DavAuthentication,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if self.cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let exchange_request = HttpExchangeRequest::from_boundary(request, authentication);
        let raw = self
            .exchange
            .exchange(&exchange_request, &self.cancellation)?;
        if raw.body().len() > self.config.max_response_body_bytes {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        Ok(raw)
    }

    fn cancellation(&self) -> Option<DavCancellation> {
        Some(self.cancellation.clone())
    }
}

struct ReqwestDavExchange {
    client: Client,
    max_response_body_bytes: usize,
}

impl ReqwestDavExchange {
    fn new(config: HttpDavConfig) -> Result<Self, DavExecutionError> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .user_agent(USER_AGENT_VALUE)
            .build()
            .map_err(|error| classify_error(&error))?;
        Ok(Self {
            client,
            max_response_body_bytes: config.max_response_body_bytes,
        })
    }

    fn read_bounded_body(
        response: &mut reqwest::blocking::Response,
        cancellation: &DavCancellation,
        limit: usize,
    ) -> Result<Vec<u8>, DavExecutionError> {
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DavExecutionError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            if cancellation.is_cancelled() {
                return Err(DavExecutionError::Cancelled);
            }
            let read = response
                .read(&mut buffer)
                .map_err(|error| classify_read_error(&error))?;
            if read == 0 {
                return Ok(body);
            }
            if body.len() + read > limit {
                return Err(DavExecutionError::ResponseTooLarge);
            }
            body.extend_from_slice(&buffer[..read]);
        }
    }

    fn build_request(
        &self,
        request: &HttpExchangeRequest,
    ) -> Result<reqwest::blocking::Request, DavExecutionError> {
        if !matches!(request.method(), HttpRequestMethod::Read(_)) {
            return Err(DavExecutionError::InvalidRequest);
        }
        self.build_request_inner(request)
    }

    /// Only the separate consuming conditional exchange can invoke this shaper;
    /// the generic read exchange rejects all mutation methods.
    #[allow(dead_code)]
    fn build_conditional_request(
        &self,
        request: HttpExchangeRequest,
    ) -> Result<reqwest::blocking::Request, DavExecutionError> {
        if !matches!(
            request.method(),
            HttpRequestMethod::ConditionalPut
                | HttpRequestMethod::ConditionalCreate
                | HttpRequestMethod::DedicatedProbeCreate
                | HttpRequestMethod::ConditionalDelete
        ) {
            return Err(DavExecutionError::InvalidRequest);
        }
        self.build_request_inner(&request)
    }

    fn build_request_inner(
        &self,
        request: &HttpExchangeRequest,
    ) -> Result<reqwest::blocking::Request, DavExecutionError> {
        let mut builder = self
            .client
            .request(method(request.method())?, request.url().clone())
            .timeout(request.timeout());
        if let Some(content_type) = request.content_type() {
            builder = builder.header(CONTENT_TYPE, content_type);
        }
        if let Some(depth) = request.depth() {
            builder = builder.header("Depth", depth);
        }
        if let Some(authorization) = request.authorization() {
            let mut value = HeaderValue::from_bytes(authorization.expose())
                .map_err(|_| DavExecutionError::InvalidRequest)?;
            value.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, value);
        }
        if let Some(if_match) = request.if_match() {
            let mut value = HeaderValue::from_bytes(if_match.expose())
                .map_err(|_| DavExecutionError::InvalidRequest)?;
            value.set_sensitive(true);
            builder = builder.header(IF_MATCH, value);
        }
        if request.if_none_match {
            builder = builder.header(IF_NONE_MATCH, "*");
        }
        if request.content_type().is_some() {
            builder = builder.body(request.body().to_vec());
        }
        builder.build().map_err(|error| classify_error(&error))
    }

    fn send_built_request(
        &self,
        outgoing: reqwest::blocking::Request,
        cancellation: &DavCancellation,
    ) -> Result<RawDavResponse, DavExecutionError> {
        self.send_built_request_with_limit(outgoing, cancellation, self.max_response_body_bytes)
    }

    fn send_built_request_with_limit(
        &self,
        outgoing: reqwest::blocking::Request,
        cancellation: &DavCancellation,
        response_limit: usize,
    ) -> Result<RawDavResponse, DavExecutionError> {
        let mut response = self
            .client
            .execute(outgoing)
            .map_err(|error| classify_error(&error))?;
        let status = response.status().as_u16();
        let location = bounded_response_header(response.headers(), LOCATION, MAX_URL_BYTES)?;
        let content_type =
            bounded_response_header(response.headers(), CONTENT_TYPE, MAX_CONTENT_TYPE_BYTES)?;
        let etag = bounded_response_header(response.headers(), ETAG, MAX_ETAG_BYTES)?;
        let body = Self::read_bounded_body(&mut response, cancellation, response_limit)?;
        Ok(RawDavResponse::new(status, location, content_type, body).with_etag(etag))
    }
}

impl DavHttpExchange for ReqwestDavExchange {
    fn exchange_probe_acquisition_guarded(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if !probe_acquisition_exchange_shape(request) {
            return Err(DavExecutionError::InvalidRequest);
        }
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_request(request)?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        let limit = if request.depth() == Some("1") {
            64 * 1024
        } else {
            16 * 1024
        };
        let raw = self.send_built_request_with_limit(
            outgoing,
            cancellation,
            self.max_response_body_bytes.min(limit),
        )?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        Ok(raw)
    }
    fn exchange_probe_metadata_guarded(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if !probe_metadata_exchange_shape(request) {
            return Err(DavExecutionError::InvalidRequest);
        }
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_request(request)?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        let raw = self.send_built_request_with_limit(
            outgoing,
            cancellation,
            self.max_response_body_bytes.min(16 * 1024),
        )?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        Ok(raw)
    }
    fn exchange_exact_get_guarded(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if request.method() != HttpRequestMethod::Read(DavMethod::Get) || !request.body().is_empty()
        {
            return Err(DavExecutionError::InvalidRequest);
        }
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_request(request)?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        self.send_built_request(outgoing, cancellation)
    }
    fn exchange(
        &self,
        request: &HttpExchangeRequest,
        cancellation: &DavCancellation,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_request(request)?;
        self.send_built_request(outgoing, cancellation)
    }

    fn exchange_conditional(
        &self,
        request: HttpExchangeRequest,
        cancellation: &DavCancellation,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_conditional_request(request)?;
        self.send_built_request(outgoing, cancellation)
    }

    fn exchange_conditional_guarded(
        &self,
        request: HttpExchangeRequest,
        cancellation: &DavCancellation,
        accept: &dyn Fn() -> bool,
    ) -> Result<RawDavResponse, DavExecutionError> {
        if cancellation.is_cancelled() {
            return Err(DavExecutionError::Cancelled);
        }
        let outgoing = self.build_conditional_request(request)?;
        if cancellation.is_cancelled() || !accept() {
            return Err(DavExecutionError::Cancelled);
        }
        self.send_built_request(outgoing, cancellation)
    }
}

fn bounded_response_header(
    headers: &HeaderMap,
    name: HeaderName,
    max_bytes: usize,
) -> Result<Option<String>, DavExecutionError> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    if value.as_bytes().len() > max_bytes {
        return Err(DavExecutionError::InvalidResponse);
    }
    let value = value
        .to_str()
        .map_err(|_| DavExecutionError::InvalidResponse)?;
    if value.chars().any(char::is_control) {
        return Err(DavExecutionError::InvalidResponse);
    }
    Ok(Some(value.to_owned()))
}

fn method(method: HttpRequestMethod) -> Result<Method, DavExecutionError> {
    match method {
        HttpRequestMethod::Read(DavMethod::Get) => Ok(Method::GET),
        HttpRequestMethod::Read(DavMethod::Propfind) => {
            Method::from_bytes(b"PROPFIND").map_err(|_| DavExecutionError::InvalidRequest)
        }
        HttpRequestMethod::Read(DavMethod::Report) => {
            Method::from_bytes(b"REPORT").map_err(|_| DavExecutionError::InvalidRequest)
        }
        HttpRequestMethod::ConditionalPut
        | HttpRequestMethod::ConditionalCreate
        | HttpRequestMethod::DedicatedProbeCreate => Ok(Method::PUT),
        HttpRequestMethod::ConditionalDelete => Ok(Method::DELETE),
    }
}

/// Bounded transport classification. Kept as a pure decision over the stack's
/// public predicates so precedence is testable without a live network failure.
fn classify_failure(
    timeout: bool,
    dns: bool,
    body: bool,
    invalid_request: bool,
) -> DavExecutionError {
    if timeout {
        DavExecutionError::Timeout
    } else if dns {
        DavExecutionError::Dns
    } else if body {
        DavExecutionError::InvalidResponse
    } else if invalid_request {
        DavExecutionError::InvalidRequest
    } else {
        DavExecutionError::Offline
    }
}

fn classify_error(error: &reqwest::Error) -> DavExecutionError {
    classify_failure(
        error.is_timeout(),
        error.is_dns(),
        error.is_body() || error.is_decode(),
        error.is_builder() || error.is_redirect(),
    )
}

fn classify_read_error(error: &std::io::Error) -> DavExecutionError {
    if error.kind() == std::io::ErrorKind::TimedOut {
        DavExecutionError::Timeout
    } else {
        DavExecutionError::Offline
    }
}
