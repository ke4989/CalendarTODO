use super::calendar_recurrence::format_utc_timestamp;
use super::remote_mutation::{RemoteMutationError, validate_title};
use super::remote_mutation::{RemoteMutationExecutionPhase, RemoteReadBack, single_vevent_uid};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Semantic UTC-only admission. No remote routing or caller-generated UID.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRemoteEventCreate {
    pub idempotency_key: String,
    pub calendar_id: String,
    pub title: String,
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
}

impl fmt::Debug for QueueRemoteEventCreate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueRemoteEventCreate(<redacted>)")
    }
}

impl QueueRemoteEventCreate {
    pub fn validate(mut self) -> Result<Self, RemoteMutationError> {
        for id in [&mut self.idempotency_key, &mut self.calendar_id] {
            *id = Uuid::parse_str(id)
                .map_err(|_| RemoteMutationError::InvalidIdentity)?
                .to_string();
        }
        self.title = self.title.trim().to_owned();
        validate_title(&self.title)?;
        // Millisecond API, whole-second UTC VEVENT representation, 1970..2100.
        if self.title.chars().any(char::is_control)
            || self.start_at_utc_ms < 0
            || self.end_at_utc_ms >= 4_102_444_800_000
            || self.end_at_utc_ms <= self.start_at_utc_ms
            || self.start_at_utc_ms % 1000 != 0
            || self.end_at_utc_ms % 1000 != 0
        {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        Ok(self)
    }
}

/// Independent UTC-v2 semantic input. It cannot alter legacy utc-v1 bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRemoteEventCreateV2 {
    pub idempotency_key: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
}

impl fmt::Debug for QueueRemoteEventCreateV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueRemoteEventCreateV2(<redacted>)")
    }
}

impl QueueRemoteEventCreateV2 {
    pub fn validate(mut self) -> Result<Self, RemoteMutationError> {
        use super::remote_mutation::{
            normalize_v2_description, validate_v2_location, validate_v2_serialized_intent,
            validate_v2_title,
        };
        for id in [&mut self.idempotency_key, &mut self.calendar_id] {
            *id = Uuid::parse_str(id)
                .map_err(|_| RemoteMutationError::InvalidIdentity)?
                .to_string();
        }
        if self.title.chars().any(char::is_control) {
            return Err(RemoteMutationError::InvalidTitle);
        }
        self.title = self.title.trim().to_owned();
        validate_v2_title(&self.title)?;
        if let Some(description) = &mut self.description {
            *description = normalize_v2_description(description)?;
        }
        if let Some(location) = &self.location {
            validate_v2_location(location)?;
        }
        if self.start_at_utc_ms < 0
            || self.end_at_utc_ms >= 4_102_444_800_000
            || self.end_at_utc_ms <= self.start_at_utc_ms
            || self.start_at_utc_ms % 1000 != 0
            || self.end_at_utc_ms % 1000 != 0
        {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        validate_v2_serialized_intent(&self)?;
        Ok(self)
    }
}

/// Pure deterministic utc-v2 content. It supplies no actor or send authority.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedCreationContentV2 {
    raw_ics: Vec<u8>,
    uid: String,
}

impl fmt::Debug for GeneratedCreationContentV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GeneratedCreationContentV2(<redacted>)")
    }
}

impl GeneratedCreationContentV2 {
    pub const CODEC: &'static str = "utc-v2";

    pub fn generate(
        input: QueueRemoteEventCreateV2,
        actor_uid: &str,
        original_created_at_ms: i64,
    ) -> Result<Self, RemoteMutationError> {
        use super::remote_mutation::{fold_text_property_v2, single_vevent_uid_v2};
        let input = input.validate()?;
        let uid = Uuid::parse_str(actor_uid)
            .map_err(|_| RemoteMutationError::InvalidIdentity)?
            .to_string();
        if !(0..4_102_444_800_000).contains(&original_created_at_ms) {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        let format =
            |ms| format_utc_timestamp(ms).map_err(|_| RemoteMutationError::UnsafeCalendarData);
        let mut raw_ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Calendar TODO//UTC Event Creation v2//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\n",
            format(original_created_at_ms / 1000 * 1000)?, format(input.start_at_utc_ms)?, format(input.end_at_utc_ms)?
        ).into_bytes();
        raw_ics.extend(fold_text_property_v2("SUMMARY", &input.title)?);
        if let Some(description) = input.description {
            raw_ics.extend(fold_text_property_v2("DESCRIPTION", &description)?);
        }
        if let Some(location) = input.location {
            raw_ics.extend(fold_text_property_v2("LOCATION", &location)?);
        }
        raw_ics.extend_from_slice(b"END:VEVENT\r\nEND:VCALENDAR\r\n");
        if raw_ics.len() > 64 * 1024 || single_vevent_uid_v2(&raw_ics)? != uid {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        Ok(Self { raw_ics, uid })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.raw_ics
    }

    pub fn matches_read_back(&self, remote: &RemoteReadBack) -> bool {
        remote.has_bounded_strong_revision()
            && super::remote_mutation::single_vevent_uid_v2(&remote.raw_ics)
                .is_ok_and(|uid| uid == self.uid)
            && remote.raw_ics == self.raw_ics
    }
}

/// Bounded admission receipt; it carries no generated UID, route, or intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCreationSummary {
    pub operation_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub created_at_ms: i64,
}

/// Account history allowlist; never exposes intent, route, revision or lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCreationStatus {
    pub operation_id: String,
    pub calendar_id: String,
    pub resource_id: String,
    pub state: super::remote_mutation::RemoteMutationState,
    #[serde(serialize_with = "serialize_creation_phase")]
    pub phase: RemoteMutationExecutionPhase,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCreationAccountView {
    pub account_id: String,
    pub operation_count: u32,
    pub operations: Vec<RemoteCreationStatus>,
    pub production_execution_available: bool,
}

fn serialize_creation_phase<S: serde::Serializer>(
    phase: &RemoteMutationExecutionPhase,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(phase.as_str())
}

/// Actor-issued lease material. It provides no network dispatch capability.
#[derive(Clone)]
pub struct RemoteCreationClaim {
    pub(crate) codec_version: String,
    pub(crate) operation_id: String,
    pub(crate) account_id: String,
    pub(crate) claim_token: String,
    pub(crate) frozen_raw_ics: Vec<u8>,
    pub(crate) phase: RemoteMutationExecutionPhase,
    pub(crate) calendar_id: String,
    pub(crate) collection_href: String,
    pub(crate) exact_href: String,
    pub(crate) sync_epoch: Option<Uuid>,
}

/// A single durable first-send reservation. No Clone/serde and no routing,
/// body, credential, executor or dispatch authority. Dropping it loses the
/// reservation object forever; recovery remains GET-only.
pub struct RemoteCreationAttemptEvidence {
    pub(crate) operation_id: String,
    pub(crate) account_id: String,
    pub(crate) claim_token: String,
    pub(crate) attempt_id: Uuid,
    pub(crate) attempted_at_ms: i64,
    pub(crate) sync_epoch: Uuid,
}

impl RemoteCreationAttemptEvidence {
    pub(crate) fn matches_claim(&self, claim: &RemoteCreationClaim) -> bool {
        self.operation_id == claim.operation_id
            && self.account_id == claim.account_id
            && self.claim_token == claim.claim_token
            && Some(self.sync_epoch) == claim.sync_epoch
            && claim.phase == RemoteMutationExecutionPhase::VerifyFirst
            && !self.attempt_id.is_nil()
    }
}

impl fmt::Debug for RemoteCreationAttemptEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCreationAttemptEvidence(<redacted>)")
    }
}

/// Exact-target transport evidence. Absence is unresolved, never resend authority.
#[derive(Clone)]
pub enum RemoteCreationReadBack {
    Absent(u16),
    Retained(RemoteReadBack),
}

impl fmt::Debug for RemoteCreationReadBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCreationReadBack(<redacted>)")
    }
}

/// Nonterminal content observation, not success, conflict or execution authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCreationObservation {
    ExactContent,
    AbsentUnresolved,
    UnsafeUnresolved,
}

impl RemoteCreationClaim {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn phase(&self) -> RemoteMutationExecutionPhase {
        self.phase
    }
    pub fn frozen_content(&self) -> &[u8] {
        &self.frozen_raw_ics
    }
}

impl fmt::Debug for RemoteCreationClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCreationClaim(<redacted>)")
    }
}

/// Pure generated content, not admission or permission to send a request.
/// Actor identity and its original creation timestamp must be reused on retry.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedCreationContent {
    raw_ics: Vec<u8>,
    uid: String,
}

impl fmt::Debug for GeneratedCreationContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GeneratedCreationContent(<redacted>)")
    }
}

impl GeneratedCreationContent {
    pub fn generate(
        input: QueueRemoteEventCreate,
        actor_uid: &str,
        original_created_at_ms: i64,
    ) -> Result<Self, RemoteMutationError> {
        let input = input.validate()?;
        let uid = Uuid::parse_str(actor_uid)
            .map_err(|_| RemoteMutationError::InvalidIdentity)?
            .to_string();
        if !(0..4_102_444_800_000).contains(&original_created_at_ms) {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        let format =
            |ms| format_utc_timestamp(ms).map_err(|_| RemoteMutationError::UnsafeCalendarData);
        let stamp = format(original_created_at_ms / 1000 * 1000)?;
        let start = format(input.start_at_utc_ms)?;
        let end = format(input.end_at_utc_ms)?;
        let title = input
            .title
            .replace('\\', "\\\\")
            .replace(';', "\\;")
            .replace(',', "\\,");
        let mut raw = String::new();
        for line in [
            "BEGIN:VCALENDAR".to_owned(),
            "VERSION:2.0".to_owned(),
            "PRODID:-//Calendar TODO//UTC Event Creation v1//EN".to_owned(),
            "BEGIN:VEVENT".to_owned(),
            format!("UID:{uid}"),
            format!("DTSTAMP:{stamp}"),
            format!("DTSTART:{start}"),
            format!("DTEND:{end}"),
            format!("SUMMARY:{title}"),
            "END:VEVENT".to_owned(),
            "END:VCALENDAR".to_owned(),
        ] {
            let mut octets = 0;
            for ch in line.chars() {
                if octets + ch.len_utf8() > 75 {
                    raw.push_str("\r\n ");
                    octets = 1;
                }
                raw.push(ch);
                octets += ch.len_utf8();
            }
            raw.push_str("\r\n");
        }
        let raw_ics = raw.into_bytes();
        if !single_vevent_uid(&raw_ics).is_ok_and(|parsed| parsed == uid) {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        Ok(Self { raw_ics, uid })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.raw_ics
    }

    /// Content comparison only; exact-target transport/lease/epoch ownership
    /// must be checked independently before a future actor terminal transition.
    pub fn matches_read_back(&self, remote: &RemoteReadBack) -> bool {
        remote.has_bounded_strong_revision()
            && single_vevent_uid(&remote.raw_ics).is_ok_and(|uid| uid == self.uid)
            && remote.raw_ics == self.raw_ics
    }
}
