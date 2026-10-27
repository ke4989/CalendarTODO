//! Redacted diagnostics only. No identity, routing, body, token or authority.
use serde::Serialize;
#[derive(Clone, Copy, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Input,
    Readiness,
    Configuration,
    Selection,
    Preparation,
    Credential,
    Principal,
    Home,
    Locate,
    First,
    Second,
    Publication,
    Shutdown,
}
#[derive(Clone, Copy, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InvalidInput,
    UnsupportedPlatform,
    PreparationUnavailable,
    SavedReferenceUnavailable,
    SavedReferenceNotFound,
    SavedReferenceAmbiguous,
    SavedReferenceInvalidMetadata,
    SavedReferenceBoundsExceeded,
    CredentialNotFound,
    CredentialInvalid,
    CredentialUnavailable,
    OwnerLostOrCancelled,
    Timeout,
    Offline,
    Dns,
    Tls,
    Redirect,
    HttpRejected,
    UnsafeOrAmbiguousMetadata,
    BoundsExceeded,
    MissingRevision,
    WeakOrMalformedRevision,
    ShutdownUnavailable,
}
#[derive(Clone, Copy, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Revision {
    Missing,
    WeakOrMalformed,
    StrongRepresentationOnly,
}
#[derive(Clone, Copy, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Disabled,
    ReadinessChecksOnly,
    ConfigurationAvailable,
    AcceptedRepresentationObservation,
    UnsupportedRevision,
    Failed,
}
#[derive(Serialize)]
pub struct Report {
    schema: &'static str,
    pub(crate) outcome: Outcome,
    pub(crate) stage: Stage,
    pub(crate) failure: Option<Failure>,
    // Entries are not claimed socket sends: a final guard may reject an entry.
    pub(crate) guarded_executor_entries: u8,
    pub(crate) verified_slots: u8,
    pub(crate) first_revision: Option<Revision>,
    pub(crate) second_revision: Option<Revision>,
    pub(crate) credential_check: &'static str,
    isolation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) configuration_availability:
        Option<crate::adapters::persistence::ConfigurationAvailability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_detail: Option<crate::adapters::persistence::OwnerDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_step: Option<crate::adapters::persistence::OwnerStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fallback_detail: Option<crate::adapters::persistence::LockFallbackObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_access_requests: Option<crate::adapters::persistence::ParentAccessRequests>,
}
impl Report {
    pub(crate) fn new(outcome: Outcome, stage: Stage) -> Self {
        Self {
            schema: "probe-acquisition-report-v1",
            outcome,
            stage,
            failure: None,
            guarded_executor_entries: 0,
            verified_slots: 0,
            first_revision: None,
            second_revision: None,
            credential_check: "deferred_until_execute",
            isolation: "memory_actor_only",
            configuration_availability: None,
            owner_detail: None,
            owner_step: None,
            fallback_detail: None,
            parent_access_requests: None,
        }
    }
    pub(crate) fn fail(&mut self, failure: Failure) {
        self.outcome = Outcome::Failed;
        self.failure = Some(failure);
    }
    pub fn accepted(&self) -> bool {
        self.outcome == Outcome::AcceptedRepresentationObservation
    }
    pub fn failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed | Outcome::UnsupportedRevision)
    }
    pub(crate) fn transport(
        error: crate::adapters::caldav_transport::DavExecutionError,
    ) -> Failure {
        use crate::adapters::caldav_transport::DavExecutionError as E;
        match error {
            E::Cancelled => Failure::OwnerLostOrCancelled,
            E::Timeout => Failure::Timeout,
            E::Offline => Failure::Offline,
            E::Dns => Failure::Dns,
            E::Tls => Failure::Tls,
            E::ResponseTooLarge => Failure::BoundsExceeded,
            E::UnsafeRedirect | E::TooManyRedirects => Failure::Redirect,
            _ => Failure::UnsafeOrAmbiguousMetadata,
        }
    }
}
