//! Consuming existing-route observation and untrusted redacted fixture contract.
//! Neither path can construct collection coverage or first-route provenance.
use super::{first_route::CollectionRevisionCapability, seed::ProbeSeedEnvelope};
use serde::Deserialize;

pub(crate) struct RevisionContractView {
    pub(crate) guarded_representation_observed: bool,
    pub(crate) fixture_shape_accepted: bool,
    pub(crate) metadata_coverage_available: bool,
    pub(crate) exact_only_source_available: bool,
    pub(crate) durable_import_available: bool,
}

impl RevisionContractView {
    fn closed(guarded: bool, fixture: bool) -> Self {
        Self {
            guarded_representation_observed: guarded,
            fixture_shape_accepted: fixture,
            metadata_coverage_available: false,
            exact_only_source_available: false,
            durable_import_available: false,
        }
    }
}

// Only the executor-sealed same-concrete-owner seed envelope can enter this
// observation path; capture claims, raw bytes and callback traits cannot.
pub(crate) fn observe_guarded(envelope: ProbeSeedEnvelope) -> Result<RevisionContractView, ()> {
    let (candidate, revision, owner) = envelope.into_parts()?;
    if super::first_route::classify_collection_revision(Some(&revision))
        != CollectionRevisionCapability::StrongRepresentationOnly
        || !owner.current(&candidate)
    {
        return Err(());
    }
    Ok(RevisionContractView::closed(true, false))
}

// No runtime API accepts this capture. Strict comparison enums intentionally
// contain no identity, revision, route, body, auth, token or credential bytes.
#[derive(Deserialize, PartialEq)]
#[serde(try_from = "String")]
enum Same {
    Same,
}
impl TryFrom<String> for Same {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "same" => Ok(Self::Same),
            _ => Err("incomplete captured ownership"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedOwnership {
    account: Same,
    session: Same,
    runtime: Same,
    executor: Same,
    authentication: Same,
    target: Same,
    trusted_host: Same,
    before_socket: Same,
    after_response: Same,
    final_publication: Same,
}

#[derive(Deserialize, PartialEq)]
#[serde(try_from = "String")]
enum ValidatorKind {
    HttpRepresentationEtag,
    DavSyncToken,
    CalendarCtag,
}
impl TryFrom<String> for ValidatorKind {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "http-representation-etag" => Ok(Self::HttpRepresentationEtag),
            "dav-sync-token" => Ok(Self::DavSyncToken),
            "calendar-ctag" => Ok(Self::CalendarCtag),
            _ => Err("invalid validator kind"),
        }
    }
}
#[derive(Deserialize, PartialEq)]
#[serde(try_from = "String")]
enum ValidatorShape {
    Strong,
    Opaque,
}
impl TryFrom<String> for ValidatorShape {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "strong" => Ok(Self::Strong),
            "opaque" => Ok(Self::Opaque),
            _ => Err("invalid validator shape"),
        }
    }
}
#[derive(Deserialize)]
#[serde(try_from = "String")]
enum ValidatorRelation {
    Changed,
    Same,
    Unknown,
}
impl TryFrom<String> for ValidatorRelation {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "changed" => Ok(Self::Changed),
            "same" => Ok(Self::Same),
            "unknown" => Ok(Self::Unknown),
            _ => Err("invalid validator relation"),
        }
    }
}
#[derive(Deserialize)]
#[serde(try_from = "String")]
enum MetadataField {
    Unchanged,
    DisplayName,
    ResourceType,
    Privileges,
    CollectionRoute,
}
impl TryFrom<String> for MetadataField {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "unchanged" => Ok(Self::Unchanged),
            "display-name" => Ok(Self::DisplayName),
            "resource-type" => Ok(Self::ResourceType),
            "privileges" => Ok(Self::Privileges),
            "collection-route" => Ok(Self::CollectionRoute),
            _ => Err("invalid metadata field"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    schema: String,
    calendar: String,
    request_profile: String,
    redaction: String,
    status: u16,
    response_count: u8,
    exchange_count: u8,
    redirect_count: u8,
    timeout_ms: u32,
    body_bytes: u32,
    content_type_bytes: u16,
    validator_bytes: u16,
    validator_kind: ValidatorKind,
    validator_shape: ValidatorShape,
    validator_relation: ValidatorRelation,
    metadata_field: MetadataField,
    ownership: CapturedOwnership,
}

pub(crate) fn inspect_capture(raw: &[u8]) -> Result<RevisionContractView, ()> {
    inspect_capture_core(raw, None)
}

pub(super) fn inspect_capture_for_selection(
    raw: &[u8],
    selection: &str,
) -> Result<RevisionContractView, ()> {
    inspect_capture_core(raw, Some(selection))
}

fn inspect_capture_core(raw: &[u8], selection: Option<&str>) -> Result<RevisionContractView, ()> {
    if raw.len() > 4096 {
        return Err(());
    }
    let capture: Capture = serde_json::from_slice(raw).map_err(|_| ())?;
    if capture.schema != "probe-revision-v1"
        || !matches!(
            capture.calendar.as_str(),
            "Calendar TODO Probe" | "Calendar TODO Probe 2"
        )
        || selection.is_some_and(|expected| capture.calendar != expected)
        || capture.request_profile != "same-owner-depth-zero-single-exchange-v1"
        || capture.redaction != "opaque-summary-only-v1"
        || capture.status != 207
        || capture.response_count != 1
        || capture.exchange_count != 1
        || capture.redirect_count != 0
        || capture.timeout_ms != 30_000
        || capture.body_bytes == 0
        || capture.body_bytes > 16 * 1024
        || capture.content_type_bytes == 0
        || capture.content_type_bytes > 128
        || capture.validator_bytes == 0
        || capture.validator_bytes > 256
        || (capture.validator_kind == ValidatorKind::HttpRepresentationEtag)
            != (capture.validator_shape == ValidatorShape::Strong)
    {
        return Err(());
    }
    // Deserialize every required ownership comparison. "same" is an untrusted
    // report, never a concrete pointer check or production capability.
    let CapturedOwnership {
        account,
        session,
        runtime,
        executor,
        authentication,
        target,
        trusted_host,
        before_socket,
        after_response,
        final_publication,
    } = capture.ownership;
    let _comparisons = (
        account,
        session,
        runtime,
        executor,
        authentication,
        target,
        trusted_host,
        before_socket,
        after_response,
        final_publication,
    );
    let _reported_relation = (capture.validator_relation, capture.metadata_field);
    Ok(RevisionContractView::closed(false, true))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionPlan {
    schema: String,
    calendar: String,
    plan: String,
}

// Parsing the two semantic selections performs no discovery, actor read,
// credential acquisition or request construction. No production source mint.
pub(crate) fn inspect_acquisition_plan(raw: &[u8]) -> Result<RevisionContractView, ()> {
    if raw.len() > 512 {
        return Err(());
    }
    let plan: AcquisitionPlan = serde_json::from_slice(raw).map_err(|_| ())?;
    if plan.schema != "probe-acquisition-plan-v1"
        || !matches!(
            plan.calendar.as_str(),
            "Calendar TODO Probe" | "Calendar TODO Probe 2"
        )
        || plan.plan != "review-exact-only-source"
    {
        return Err(());
    }
    Ok(RevisionContractView::closed(false, true))
}
