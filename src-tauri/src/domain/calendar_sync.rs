use serde::Serialize;
use std::collections::{BTreeSet, HashSet};
use thiserror::Error;
use uuid::Uuid;

const MAX_LABEL_LEN: usize = 256;
const MAX_URL_LEN: usize = 4096;
const MAX_ICS_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInventoryStage {
    pub account_id: String,
    pub provider_kind: String,
    pub user_label: String,
    pub credential_ref: String,
    pub region_profile: String,
    pub complete: bool,
    pub authoritative: bool,
    pub observed_at_ms: i64,
    pub calendars: Vec<CalendarInventoryStage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInventoryStage {
    pub calendar_id: String,
    pub collection_url: String,
    pub display_name: String,
    pub server_color: Option<String>,
    pub privileges: String,
    pub sync_token: Option<String>,
    pub resources: Vec<StagedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedResource {
    pub href: String,
    pub etag: String,
    pub content_type: String,
    pub raw_ics: Vec<u8>,
    pub uid: String,
    pub parse_status: ParseStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIncrementalStage {
    pub account_id: String,
    pub complete: bool,
    pub authoritative: bool,
    pub observed_at_ms: i64,
    pub calendars: Vec<CalendarIncrementalStage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarIncrementalStage {
    pub calendar_id: String,
    pub base_sync_token: String,
    pub new_sync_token: String,
    pub changed_resources: Vec<StagedResource>,
    pub removed_hrefs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalSyncRequest {
    pub calendar_id: String,
    pub sync_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ParseStatus {
    Parsed,
    ReadOnly,
}

impl ParseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::ReadOnly => "read_only",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "parsed" => Ok(Self::Parsed),
            "read_only" => Ok(Self::ReadOnly),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedCalendarResource {
    pub resource_id: String,
    pub series_id: String,
    pub calendar_id: String,
    pub href: String,
    pub etag: String,
    pub content_type: String,
    pub raw_ics: Vec<u8>,
    pub uid: String,
    pub parse_status: ParseStatus,
    pub lifecycle: ResourceLifecycle,
    pub remote_deleted_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceLifecycle {
    Active,
    MissingCandidate,
    RemoteDeleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectProbeStatus {
    Found,
    NotFound,
    Gone,
    Unauthorized,
    TransientFailure,
    ServerFailure,
}

impl DirectProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::NotFound => "not_found",
            Self::Gone => "gone",
            Self::Unauthorized => "unauthorized",
            Self::TransientFailure => "transient_failure",
            Self::ServerFailure => "server_failure",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "found" => Ok(Self::Found),
            "not_found" => Ok(Self::NotFound),
            "gone" => Ok(Self::Gone),
            "unauthorized" => Ok(Self::Unauthorized),
            "transient_failure" => Ok(Self::TransientFailure),
            "server_failure" => Ok(Self::ServerFailure),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }

    pub fn is_positive_absence(self) -> bool {
        matches!(self, Self::NotFound | Self::Gone)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionProbe {
    pub resource_id: String,
    pub status: DirectProbeStatus,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDeletionReconciliation {
    pub account_id: String,
    pub complete: bool,
    pub authoritative: bool,
    pub probes: Vec<DeletionProbe>,
}

impl AccountDeletionReconciliation {
    pub fn validate(self) -> Result<Self, CalendarSyncError> {
        validate_uuid(&self.account_id)?;
        if !self.complete || !self.authoritative {
            return Err(CalendarSyncError::IncompleteReconciliation);
        }
        if self.probes.is_empty() {
            return Err(CalendarSyncError::EmptyReconciliation);
        }
        let mut resource_ids = HashSet::new();
        for probe in &self.probes {
            validate_uuid(&probe.resource_id)?;
            if probe.observed_at_ms < 0 {
                return Err(CalendarSyncError::InvalidTimestamp);
            }
            if !resource_ids.insert(&probe.resource_id) {
                return Err(CalendarSyncError::DuplicateIdentity);
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTrigger {
    Startup,
    Manual,
    Periodic,
    Retry,
}

impl SyncTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Manual => "manual",
            Self::Periodic => "periodic",
            Self::Retry => "retry",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "startup" => Ok(Self::Startup),
            "manual" => Ok(Self::Manual),
            "periodic" => Ok(Self::Periodic),
            "retry" => Ok(Self::Retry),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Success,
    Failure,
}

impl SyncOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "success" => Ok(Self::Success),
            "failure" => Ok(Self::Failure),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailureClass {
    None,
    Transient,
    Unauthorized,
    Incomplete,
    UnsafeData,
    Validation,
    Persistence,
}

impl SyncFailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Transient => "transient",
            Self::Unauthorized => "unauthorized",
            Self::Incomplete => "incomplete",
            Self::UnsafeData => "unsafe_data",
            Self::Validation => "validation",
            Self::Persistence => "persistence",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "none" => Ok(Self::None),
            "transient" => Ok(Self::Transient),
            "unauthorized" => Ok(Self::Unauthorized),
            "incomplete" => Ok(Self::Incomplete),
            "unsafe_data" => Ok(Self::UnsafeData),
            "validation" => Ok(Self::Validation),
            "persistence" => Ok(Self::Persistence),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRunSummary {
    pub run_id: String,
    pub account_id: String,
    pub trigger: SyncTrigger,
    pub outcome: SyncOutcome,
    pub failure_class: SyncFailureClass,
    pub calendar_count: u32,
    pub resource_count: u32,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
}

impl ResourceLifecycle {
    pub fn from_storage(value: &str) -> Result<Self, CalendarSyncError> {
        match value {
            "active" => Ok(Self::Active),
            "missing_candidate" => Ok(Self::MissingCandidate),
            "remote_deleted" => Ok(Self::RemoteDeleted),
            _ => Err(CalendarSyncError::CorruptStoredValue),
        }
    }
}

impl AccountInventoryStage {
    pub fn validate(self) -> Result<Self, CalendarSyncError> {
        validate_uuid(&self.account_id)?;
        validate_text(&self.provider_kind, MAX_LABEL_LEN)?;
        validate_text(&self.user_label, MAX_LABEL_LEN)?;
        validate_text(&self.region_profile, MAX_LABEL_LEN)?;
        let credential_id = self
            .credential_ref
            .strip_prefix("calendar-todo/account/")
            .ok_or(CalendarSyncError::InvalidCredentialReference)?;
        if self.credential_ref.len() > MAX_URL_LEN || Uuid::parse_str(credential_id).is_err() {
            return Err(CalendarSyncError::InvalidCredentialReference);
        }
        if !self.complete || !self.authoritative || self.observed_at_ms < 0 {
            return Err(CalendarSyncError::IncompleteStage);
        }
        if self.calendars.is_empty() {
            return Err(CalendarSyncError::EmptyInventory);
        }
        let mut calendar_ids = HashSet::new();
        for calendar in &self.calendars {
            validate_uuid(&calendar.calendar_id)?;
            if !calendar_ids.insert(&calendar.calendar_id) {
                return Err(CalendarSyncError::DuplicateIdentity);
            }
            validate_text(&calendar.collection_url, MAX_URL_LEN)?;
            validate_text(&calendar.display_name, MAX_LABEL_LEN)?;
            validate_text(&calendar.privileges, MAX_LABEL_LEN)?;
            if let Some(sync_token) = &calendar.sync_token {
                validate_text(sync_token, MAX_URL_LEN)?;
            }
            let mut hrefs = HashSet::new();
            for resource in &calendar.resources {
                validate_resource(resource)?;
                if !hrefs.insert(&resource.href) {
                    return Err(CalendarSyncError::DuplicateIdentity);
                }
            }
        }
        Ok(self)
    }
}

impl AccountIncrementalStage {
    pub fn validate(self) -> Result<Self, CalendarSyncError> {
        validate_uuid(&self.account_id)?;
        if !self.complete || !self.authoritative || self.observed_at_ms < 0 {
            return Err(CalendarSyncError::IncompleteStage);
        }
        if self.calendars.is_empty() {
            return Err(CalendarSyncError::EmptyInventory);
        }
        let mut calendar_ids = HashSet::new();
        for calendar in &self.calendars {
            validate_uuid(&calendar.calendar_id)?;
            if !calendar_ids.insert(&calendar.calendar_id) {
                return Err(CalendarSyncError::DuplicateIdentity);
            }
            validate_text(&calendar.base_sync_token, MAX_URL_LEN)?;
            validate_text(&calendar.new_sync_token, MAX_URL_LEN)?;
            let mut hrefs = HashSet::new();
            for resource in &calendar.changed_resources {
                validate_resource(resource)?;
                if !hrefs.insert(&resource.href) {
                    return Err(CalendarSyncError::DuplicateIdentity);
                }
            }
            for href in &calendar.removed_hrefs {
                validate_text(href, MAX_URL_LEN)?;
                if !hrefs.insert(href) {
                    return Err(CalendarSyncError::DuplicateIdentity);
                }
            }
        }
        Ok(self)
    }
}

impl IncrementalSyncRequest {
    pub fn validate(&self) -> Result<(), CalendarSyncError> {
        validate_uuid(&self.calendar_id)?;
        validate_text(&self.sync_token, MAX_URL_LEN)
    }
}

fn validate_resource(resource: &StagedResource) -> Result<(), CalendarSyncError> {
    validate_text(&resource.href, MAX_URL_LEN)?;
    validate_text(&resource.etag, MAX_LABEL_LEN)?;
    validate_text(&resource.content_type, MAX_LABEL_LEN)?;
    validate_text(&resource.uid, MAX_URL_LEN)?;
    if resource.raw_ics.is_empty() || resource.raw_ics.len() > MAX_ICS_BYTES {
        return Err(CalendarSyncError::InvalidRawResource);
    }
    if extract_vevent_series_uid(&resource.raw_ics)? != resource.uid {
        return Err(CalendarSyncError::InvalidRawResource);
    }
    Ok(())
}

pub(crate) fn extract_vevent_series_uid(bytes: &[u8]) -> Result<String, CalendarSyncError> {
    let text = std::str::from_utf8(bytes).map_err(|_| CalendarSyncError::InvalidRawResource)?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut unfolded = Vec::<String>::new();
    for line in normalized.lines() {
        if line.starts_with([' ', '\t']) {
            let previous = unfolded
                .last_mut()
                .ok_or(CalendarSyncError::InvalidRawResource)?;
            previous.push_str(line.trim_start());
        } else {
            unfolded.push(line.to_owned());
        }
    }
    let mut components = Vec::<String>::new();
    let mut uids = BTreeSet::new();
    for line in &unfolded {
        let Some((name_with_parameters, value)) = line.split_once(':') else {
            continue;
        };
        let name = name_with_parameters
            .split_once(';')
            .map_or(name_with_parameters, |(name, _)| name);
        if name.eq_ignore_ascii_case("BEGIN") {
            if value.trim().is_empty() {
                return Err(CalendarSyncError::InvalidRawResource);
            }
            components.push(value.trim().to_ascii_uppercase());
        } else if name.eq_ignore_ascii_case("END") {
            let Some(component) = components.pop() else {
                return Err(CalendarSyncError::InvalidRawResource);
            };
            if !component.eq_ignore_ascii_case(value.trim()) {
                return Err(CalendarSyncError::InvalidRawResource);
            }
        } else if name.eq_ignore_ascii_case("UID")
            && components
                .last()
                .is_some_and(|component| component == "VEVENT")
        {
            let uid = value.trim();
            if !uid.is_empty() {
                uids.insert(uid);
            }
        }
    }
    if !components.is_empty() {
        return Err(CalendarSyncError::InvalidRawResource);
    }
    match (uids.len(), uids.first()) {
        (1, Some(uid)) => Ok((*uid).to_owned()),
        _ => Err(CalendarSyncError::InvalidRawResource),
    }
}

fn validate_uuid(value: &str) -> Result<(), CalendarSyncError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| CalendarSyncError::InvalidIdentity)
}

fn validate_text(value: &str, max: usize) -> Result<(), CalendarSyncError> {
    let length = value.trim().chars().count();
    if length == 0 || length > max {
        return Err(CalendarSyncError::InvalidText);
    }
    Ok(())
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CalendarSyncError {
    #[error("calendar sync identity is invalid")]
    InvalidIdentity,
    #[error("calendar sync text is invalid")]
    InvalidText,
    #[error("credential reference is invalid")]
    InvalidCredentialReference,
    #[error("calendar inventory stage is incomplete")]
    IncompleteStage,
    #[error("calendar inventory is empty")]
    EmptyInventory,
    #[error("calendar inventory contains duplicate or ambiguous identities")]
    DuplicateIdentity,
    #[error("raw calendar resource is invalid")]
    InvalidRawResource,
    #[error("stored calendar sync value is invalid")]
    CorruptStoredValue,
    #[error("deletion reconciliation is incomplete")]
    IncompleteReconciliation,
    #[error("deletion reconciliation is empty")]
    EmptyReconciliation,
    #[error("calendar sync timestamp is invalid")]
    InvalidTimestamp,
}
