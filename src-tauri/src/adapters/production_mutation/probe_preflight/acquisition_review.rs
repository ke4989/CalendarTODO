//! Untrusted review-record/fixture identity boundary, not human authorization.
//! Opaque digest labels and exact-byte matches are not hash verification.
use serde::Deserialize;

const MAX_REVIEW: usize = 2048;
const MAX_CAPTURE: usize = 4096;
const REVIEW_SCHEMA: &str = "probe-offline-review-v1";
const REVIEW_DOMAIN: &str = "calendar-todo/probe-offline-review/v1";
const CAPTURE_DOMAIN: &str = "calendar-todo/probe-revision-capture/v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRecord {
    schema: String,
    review_domain: String,
    capture_schema: String,
    capture_domain: String,
    calendar: String,
    review: String,
    capture_origin: String,
    digest_label_kind: String,
    context_digest_label: String,
    capture_digest_label: String,
    review_digest_label: String,
}

// No Clone/Debug/Serialize or public sensitive record fields. The returned
// record proves only strict syntax; it can never mint the plan below.
pub(crate) struct UntrustedReviewRecord {
    record: ReviewRecord,
    raw: Vec<u8>,
}

fn digest_label_shape(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn inspect_review_record(raw: &[u8]) -> Result<UntrustedReviewRecord, ()> {
    if raw.len() > MAX_REVIEW {
        return Err(());
    }
    let record: ReviewRecord = serde_json::from_slice(raw).map_err(|_| ())?;
    if record.schema != REVIEW_SCHEMA
        || record.review_domain != REVIEW_DOMAIN
        || record.capture_schema != "probe-revision-v1"
        || record.capture_domain != CAPTURE_DOMAIN
        || !matches!(
            record.calendar.as_str(),
            "Calendar TODO Probe" | "Calendar TODO Probe 2"
        )
        || record.review != "recorded-for-offline-fixture-only"
        || record.capture_origin != "synthetic-scripted"
        || record.digest_label_kind != "external-sha256-label-v1"
        || !digest_label_shape(&record.context_digest_label)
        || !digest_label_shape(&record.capture_digest_label)
        || !digest_label_shape(&record.review_digest_label)
    {
        return Err(());
    }
    Ok(UntrustedReviewRecord {
        record,
        raw: raw.to_vec(),
    })
}

// Uninhabited in production. A parsed human-review claim cannot construct it.
enum ExternalApprovalSeal {}

// A single-use fixture identity pin, never a session/source/socket capability.
pub(crate) struct OfflinePlanCapability {
    expected: UntrustedReviewRecord,
    capture: Vec<u8>,
    _approval: ExternalApprovalSeal,
}

pub(crate) struct ReviewPreview {
    pub(crate) record_shape_accepted: bool,
    pub(crate) fixture_identity_matched: bool,
    pub(crate) approval_authenticated: bool,
    pub(crate) source_available: bool,
    pub(crate) session_available: bool,
    pub(crate) socket_available: bool,
    pub(crate) durable_import_available: bool,
    pub(crate) controller_available: bool,
    pub(crate) crud_available: bool,
    pub(crate) completion_available: bool,
}

impl ReviewPreview {
    fn closed(matched: bool) -> Self {
        Self {
            record_shape_accepted: true,
            fixture_identity_matched: matched,
            approval_authenticated: false,
            source_available: false,
            session_available: false,
            socket_available: false,
            durable_import_available: false,
            controller_available: false,
            crud_available: false,
            completion_available: false,
        }
    }
}

impl UntrustedReviewRecord {
    pub(crate) fn preview(&self) -> ReviewPreview {
        ReviewPreview::closed(false)
    }
}

impl OfflinePlanCapability {}

// Consumes only an independently pinned plan. Byte identity and opaque label
// equality reject a substituted fixture; they authenticate no reviewer or hash.
pub(crate) fn ingest_fixture(
    plan: OfflinePlanCapability,
    review: UntrustedReviewRecord,
    capture: &[u8],
) -> Result<ReviewPreview, ()> {
    if capture.len() > MAX_CAPTURE
        || review.raw != plan.expected.raw
        || capture != plan.capture
        || review.record.calendar != plan.expected.record.calendar
        || review.record.context_digest_label != plan.expected.record.context_digest_label
        || review.record.capture_digest_label != plan.expected.record.capture_digest_label
        || review.record.review_digest_label != plan.expected.record.review_digest_label
    {
        return Err(());
    }
    super::revision_contract::inspect_capture_for_selection(capture, &review.record.calendar)?;
    Ok(ReviewPreview::closed(true))
}
