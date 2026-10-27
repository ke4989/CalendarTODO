use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

const MAX_TITLE_LEN: usize = 512;
const MAX_MUTATION_ETAG_BYTES: usize = 256;
const MAX_MUTATION_READ_BACK_ICS_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn strong_mutation_etag(value: &str) -> bool {
    value.len() > 2
        && value.len() <= MAX_MUTATION_ETAG_BYTES
        && value.starts_with('"')
        && value.ends_with('"')
        && !value[1..value.len() - 1]
            .bytes()
            .any(|byte| byte == b'"' || byte < 0x21 || byte == 0x7f)
}

/// The first Phase 3 intent is deliberately semantic rather than rewritten ICS.
/// A later codec must apply it to the retained base bytes without dropping
/// unknown properties before any network writer may claim the operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemoteTitleUpdate {
    pub idempotency_key: String,
    pub resource_id: String,
    pub title: String,
}

impl fmt::Debug for QueueRemoteTitleUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueRemoteTitleUpdate(<redacted>)")
    }
}

impl QueueRemoteTitleUpdate {
    pub fn validate(mut self) -> Result<Self, RemoteMutationError> {
        validate_uuid(&self.idempotency_key)?;
        validate_uuid(&self.resource_id)?;
        self.title = self.title.trim().to_owned();
        validate_title(&self.title)?;
        Ok(self)
    }
}

/// Semantic request to delete one exact committed DAV resource. The actor
/// derives every revision and routing field from its own cache.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemoteEventDelete {
    pub idempotency_key: String,
    pub resource_id: String,
}

impl fmt::Debug for QueueRemoteEventDelete {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueRemoteEventDelete(<redacted>)")
    }
}

impl QueueRemoteEventDelete {
    pub fn validate(self) -> Result<Self, RemoteMutationError> {
        validate_uuid(&self.idempotency_key)?;
        validate_uuid(&self.resource_id)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteMutationState {
    Pending,
    Conflict,
    Applied,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMutationExecutionPhase {
    Ready,
    VerifyFirst,
}

impl RemoteMutationExecutionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::VerifyFirst => "verify_first",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, RemoteMutationError> {
        match value {
            "ready" => Ok(Self::Ready),
            "verify_first" => Ok(Self::VerifyFirst),
            _ => Err(RemoteMutationError::CorruptStoredValue),
        }
    }
}

/// Actor-owned execution material. It is deliberately not serializable or
/// formattable because it carries href, ETag, raw ICS, and semantic intent.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteMutationClaim {
    pub operation_id: String,
    pub claim_token: String,
    pub account_id: String,
    pub calendar_id: String,
    pub resource_id: String,
    pub collection_href: String,
    pub exact_href: String,
    pub base_etag: String,
    pub base_raw_ics: Vec<u8>,
    pub title: String,
    pub phase: RemoteMutationExecutionPhase,
    pub attempt_count: u32,
}

/// Actor-owned lease for one exact DAV resource deletion. It is separate from
/// `RemoteMutationClaim` so a title writer can never interpret deletion work.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteDeletionClaim {
    pub operation_id: String,
    pub claim_token: String,
    pub account_id: String,
    pub calendar_id: String,
    pub resource_id: String,
    pub collection_href: String,
    pub exact_href: String,
    pub base_etag: String,
    pub base_raw_ics: Vec<u8>,
    pub phase: RemoteMutationExecutionPhase,
    pub attempt_count: u32,
}

impl fmt::Debug for RemoteDeletionClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteDeletionClaim(<redacted>)")
    }
}

/// Canonical exact-target GET evidence after a deletion attempt or while
/// recovering a `verify_first` lease. Constructors keep absence limited to the
/// two DAV statuses that can prove this exact resource is gone.
#[derive(Clone, PartialEq, Eq)]
pub enum RemoteDeletionReadBack {
    Absent { status: u16 },
    Retained(RemoteReadBack),
}

impl RemoteDeletionReadBack {
    pub fn absent(status: u16) -> Result<Self, RemoteMutationError> {
        match status {
            404 | 410 => Ok(Self::Absent { status }),
            _ => Err(RemoteMutationError::CorruptStoredValue),
        }
    }

    pub fn retained(remote: RemoteReadBack) -> Self {
        Self::Retained(remote)
    }
}

impl fmt::Debug for RemoteDeletionReadBack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteDeletionReadBack(<redacted>)")
    }
}

/// A DAV resource must be an immediate member of the committed collection.
/// This checks stored routing data before the actor grants an execution lease;
/// a future transport must still validate the resolved HTTPS target and host.
pub fn resource_belongs_to_collection(collection_href: &str, exact_href: &str) -> bool {
    collection_path(collection_href).is_some_and(|path| valid_collection_member(path, exact_href))
}

/// Discovery may store a relative href while inventory carries its absolute
/// request URL. Both must describe the same safe path; two absolute routes
/// must also retain the same origin.
pub(crate) fn collection_routes_match(committed: &str, staged: &str) -> bool {
    let (Some(committed_path), Some(staged_path)) =
        (collection_path(committed), collection_path(staged))
    else {
        return false;
    };
    if committed_path != staged_path {
        return false;
    }
    if !committed.starts_with('/') && !staged.starts_with('/') {
        return url::Url::parse(committed)
            .ok()
            .zip(url::Url::parse(staged).ok())
            .is_some_and(|(left, right)| left.origin() == right.origin());
    }
    true
}

fn collection_path(collection_href: &str) -> Option<&str> {
    let collection_path = if collection_href.starts_with('/') {
        collection_href
    } else {
        let Ok(url) = url::Url::parse(collection_href) else {
            return None;
        };
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let raw_path = collection_href
            .split_once("://")
            .and_then(|(_, authority_and_path)| {
                authority_and_path
                    .find('/')
                    .map(|index| &authority_and_path[index..])
            })?;
        if raw_path != url.path() {
            return None;
        }
        raw_path
    };
    (safe_dav_path(collection_path) && collection_path.ends_with('/')).then_some(collection_path)
}

fn valid_collection_member(collection_path: &str, exact_href: &str) -> bool {
    if !safe_dav_path(collection_path)
        || !safe_dav_path(exact_href)
        || !collection_path.ends_with('/')
    {
        return false;
    }
    let Some(member) = exact_href.strip_prefix(collection_path) else {
        return false;
    };
    !member.is_empty() && !member.contains('/')
}

fn safe_dav_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains("//")
        && !path.contains(['?', '#', '\\'])
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|segment| {
            segment == "."
                || segment == ".."
                || ["%2f", "%5c", "%2e", "%3f", "%23"]
                    .iter()
                    .any(|encoded| segment.to_ascii_lowercase().contains(encoded))
        })
}

impl fmt::Debug for RemoteMutationClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteMutationClaim(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RemoteReadBack {
    pub etag: String,
    pub raw_ics: Vec<u8>,
}

impl RemoteReadBack {
    pub(crate) fn has_bounded_strong_revision(&self) -> bool {
        strong_mutation_etag(&self.etag)
            && !self.raw_ics.is_empty()
            && self.raw_ics.len() <= MAX_MUTATION_READ_BACK_ICS_BYTES
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolutionChoice {
    KeepRemote,
    RetryLocal,
}

impl ConflictResolutionChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeepRemote => "keep_remote",
            Self::RetryLocal => "retry_local",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolutionResult {
    pub operation_id: String,
    pub account_id: String,
    pub choice: ConflictResolutionChoice,
    pub retry_operation: Option<RemoteMutationSummary>,
}

impl fmt::Debug for RemoteReadBack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteReadBack(<redacted>)")
    }
}

impl RemoteMutationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Conflict => "conflict",
            Self::Applied => "applied",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, RemoteMutationError> {
        match value {
            "pending" => Ok(Self::Pending),
            "conflict" => Ok(Self::Conflict),
            "applied" => Ok(Self::Applied),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(RemoteMutationError::CorruptStoredValue),
        }
    }
}

/// Bounded metadata safe for application state. Raw ICS, hrefs, ETags, and the
/// semantic intent stay behind the Rust persistence boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMutationSummary {
    pub operation_id: String,
    pub idempotency_key: String,
    pub account_id: String,
    pub resource_id: String,
    pub state: RemoteMutationState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolutionState {
    Unresolved,
    KeepRemote,
    RetryLocal,
}

impl ConflictResolutionState {
    pub fn from_storage(value: &str) -> Result<Self, RemoteMutationError> {
        match value {
            "unresolved" => Ok(Self::Unresolved),
            "keep_remote" => Ok(Self::KeepRemote),
            "retry_local" => Ok(Self::RetryLocal),
            _ => Err(RemoteMutationError::CorruptStoredValue),
        }
    }
}

/// Redacted conflict metadata. Revision evidence and semantic intent remain in SQLite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConflictSummary {
    pub operation_id: String,
    pub account_id: String,
    pub resource_id: String,
    pub resolution_state: ConflictResolutionState,
    pub retry_operation_id: Option<String>,
    pub detected_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMutationError {
    #[error("remote mutation identity is invalid")]
    InvalidIdentity,
    #[error("remote event title is invalid")]
    InvalidTitle,
    #[error("stored remote mutation state is invalid")]
    CorruptStoredValue,
    #[error("calendar data cannot be patched losslessly")]
    UnsafeCalendarData,
}

pub fn patch_single_vevent_title(raw: &[u8], title: &str) -> Result<Vec<u8>, RemoteMutationError> {
    let title = validate_title(title)?;
    let summary = locate_single_event_fields(raw)?.summary;
    let escaped = escape_text(title);
    let mut patched = Vec::with_capacity(raw.len() + escaped.len());
    patched.extend_from_slice(&raw[..summary.start]);
    patched.extend_from_slice(&raw[summary.prefix_start..summary.value_start]);
    patched.extend_from_slice(escaped.as_bytes());
    patched.extend_from_slice(b"\r\n");
    patched.extend_from_slice(&raw[summary.end..]);
    Ok(patched)
}

pub fn single_vevent_title(raw: &[u8]) -> Result<String, RemoteMutationError> {
    let summary = locate_single_event_fields(raw)?.summary;
    let logical = unfold_property(&raw[summary.start..summary.end])?;
    let separator = logical
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(RemoteMutationError::UnsafeCalendarData)?;
    unescape_text(&logical[separator + 1..])
}

pub fn single_vevent_uid(raw: &[u8]) -> Result<String, RemoteMutationError> {
    Ok(locate_single_event_fields(raw)?.uid)
}

struct SingleEventFields {
    summary: SummarySpan,
    uid: String,
}

struct SummarySpan {
    start: usize,
    end: usize,
    prefix_start: usize,
    value_start: usize,
}

fn locate_single_event_fields(raw: &[u8]) -> Result<SingleEventFields, RemoteMutationError> {
    if raw.is_empty() || !raw.ends_with(b"\r\n") || std::str::from_utf8(raw).is_err() {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    let mut physical = Vec::new();
    let mut start = 0;
    while start < raw.len() {
        let relative_end = raw[start..]
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or(RemoteMutationError::UnsafeCalendarData)?;
        let end = start + relative_end + 2;
        if raw[start..end - 2].contains(&b'\r') || raw[start..end - 2].contains(&b'\n') {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        physical.push((start, end));
        start = end;
    }

    let mut logical = Vec::new();
    for (line_start, line_end) in physical {
        let continuation = matches!(raw.get(line_start), Some(b' ' | b'\t'));
        if continuation {
            let Some((_, previous_end)) = logical.last_mut() else {
                return Err(RemoteMutationError::UnsafeCalendarData);
            };
            *previous_end = line_end;
        } else {
            logical.push((line_start, line_end));
        }
    }

    let mut stack: Vec<String> = Vec::new();
    let mut event_count = 0usize;
    let mut summary = None;
    let mut uid = None;
    for (line_start, line_end) in logical {
        let bytes = unfold_property(&raw[line_start..line_end])?;
        let content = bytes.strip_suffix(b"\r\n").unwrap_or(&bytes);
        if let Some(value) = ascii_property_value(content, b"BEGIN") {
            let component = std::str::from_utf8(value)
                .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
                .to_ascii_uppercase();
            if component == "VEVENT" {
                event_count += 1;
            }
            stack.push(component);
            continue;
        }
        if let Some(value) = ascii_property_value(content, b"END") {
            let component = std::str::from_utf8(value)
                .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
                .to_ascii_uppercase();
            if stack.pop().as_deref() != Some(component.as_str()) {
                return Err(RemoteMutationError::UnsafeCalendarData);
            }
            continue;
        }
        if stack.last().is_some_and(|component| component == "VEVENT") {
            let separator = content
                .iter()
                .position(|byte| *byte == b':')
                .ok_or(RemoteMutationError::UnsafeCalendarData)?;
            let name_end = content[..separator]
                .iter()
                .position(|byte| *byte == b';')
                .unwrap_or(separator);
            if content[..name_end].eq_ignore_ascii_case(b"SUMMARY") {
                if summary.is_some() {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
                let physical_separator = raw[line_start..line_end]
                    .iter()
                    .position(|byte| *byte == b':')
                    .ok_or(RemoteMutationError::UnsafeCalendarData)?;
                summary = Some(SummarySpan {
                    start: line_start,
                    end: line_end,
                    prefix_start: line_start,
                    value_start: line_start + physical_separator + 1,
                });
            } else if content[..name_end].eq_ignore_ascii_case(b"UID") {
                if uid.is_some() {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
                let value = &content[separator + 1..];
                if value.is_empty() || value.len() > 4096 || value.iter().any(u8::is_ascii_control)
                {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
                uid = Some(
                    std::str::from_utf8(value)
                        .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
                        .to_owned(),
                );
            }
        }
    }
    if !stack.is_empty() || event_count != 1 {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(SingleEventFields {
        summary: summary.ok_or(RemoteMutationError::UnsafeCalendarData)?,
        uid: uid.ok_or(RemoteMutationError::UnsafeCalendarData)?,
    })
}

pub(super) fn unfold_property(raw: &[u8]) -> Result<Vec<u8>, RemoteMutationError> {
    let mut unfolded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index..].starts_with(b"\r\n ") || raw[index..].starts_with(b"\r\n\t") {
            index += 3;
        } else {
            unfolded.push(raw[index]);
            index += 1;
        }
    }
    Ok(unfolded)
}

fn ascii_property_value<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let separator = line.iter().position(|byte| *byte == b':')?;
    let name_end = line[..separator]
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(separator);
    line[..name_end]
        .eq_ignore_ascii_case(name)
        .then_some(&line[separator + 1..])
}

pub(crate) fn validate_title(title: &str) -> Result<&str, RemoteMutationError> {
    if title.is_empty() || title.chars().count() > MAX_TITLE_LEN || title.contains('\0') {
        Err(RemoteMutationError::InvalidTitle)
    } else {
        Ok(title)
    }
}

fn escape_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

fn unescape_text(value: &[u8]) -> Result<String, RemoteMutationError> {
    let value = std::str::from_utf8(value).map_err(|_| RemoteMutationError::UnsafeCalendarData)?;
    let value = value.strip_suffix("\r\n").unwrap_or(value);
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => decoded.push('\n'),
            Some('\\') => decoded.push('\\'),
            Some(';') => decoded.push(';'),
            Some(',') => decoded.push(','),
            _ => return Err(RemoteMutationError::UnsafeCalendarData),
        }
    }
    Ok(decoded)
}

fn validate_uuid(value: &str) -> Result<(), RemoteMutationError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| RemoteMutationError::InvalidIdentity)
}

/// Explicit action for an optional text property; omitted fields stay unchanged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteOptionalTextUpdateV2 {
    Set(String),
    Clear,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteEventTextFieldsV2 {
    pub title: Option<String>,
    pub description: Option<RemoteOptionalTextUpdateV2>,
    pub location: Option<RemoteOptionalTextUpdateV2>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRemoteEventTextUpdateV2 {
    pub idempotency_key: String,
    pub resource_id: String,
    pub fields: RemoteEventTextFieldsV2,
}

impl fmt::Debug for RemoteOptionalTextUpdateV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteOptionalTextUpdateV2(<redacted>)")
    }
}
impl fmt::Debug for RemoteEventTextFieldsV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteEventTextFieldsV2(<redacted>)")
    }
}
impl fmt::Debug for QueueRemoteEventTextUpdateV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueRemoteEventTextUpdateV2(<redacted>)")
    }
}

impl QueueRemoteEventTextUpdateV2 {
    pub fn validate(mut self) -> Result<Self, RemoteMutationError> {
        validate_uuid(&self.idempotency_key)?;
        validate_uuid(&self.resource_id)?;
        self.fields = self.fields.validate()?;
        validate_v2_serialized_intent(&self)?;
        Ok(self)
    }
}
impl RemoteEventTextFieldsV2 {
    pub fn validate(mut self) -> Result<Self, RemoteMutationError> {
        if self.title.is_none() && self.description.is_none() && self.location.is_none() {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        if let Some(title) = &mut self.title {
            if title.chars().any(char::is_control) {
                return Err(RemoteMutationError::InvalidTitle);
            }
            *title = title.trim().to_owned();
            validate_v2_title(title)?;
        }
        if let Some(RemoteOptionalTextUpdateV2::Set(description)) = &mut self.description {
            *description = normalize_v2_description(description)?;
        }
        if let Some(RemoteOptionalTextUpdateV2::Set(location)) = &self.location {
            validate_v2_location(location)?;
        }
        validate_v2_serialized_intent(&self)?;
        Ok(self)
    }
}

pub(crate) fn validate_v2_serialized_intent<T: Serialize>(
    value: &T,
) -> Result<(), RemoteMutationError> {
    if serde_json::to_vec(value)
        .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
        .len()
        > 32 * 1024
    {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(())
}
pub(crate) fn validate_v2_title(title: &str) -> Result<&str, RemoteMutationError> {
    if title.is_empty()
        || title.chars().count() > 512
        || title.len() > 2048
        || title.chars().any(char::is_control)
    {
        return Err(RemoteMutationError::InvalidTitle);
    }
    Ok(title)
}
pub(crate) fn validate_v2_location(location: &str) -> Result<&str, RemoteMutationError> {
    if location.is_empty()
        || location.chars().count() > 512
        || location.len() > 2048
        || location.chars().any(char::is_control)
    {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(location)
}
pub(crate) fn normalize_v2_description(description: &str) -> Result<String, RemoteMutationError> {
    let normalized = description.replace("\r\n", "\n");
    if normalized.is_empty()
        || normalized.chars().count() > 4096
        || normalized.len() > 12 * 1024
        || normalized.chars().any(|ch| ch.is_control() && ch != '\n')
        || "DESCRIPTION:".len() + escape_text(&normalized).len() > 16 * 1024
    {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(normalized)
}
pub(crate) fn fold_text_property_v2(
    property: &str,
    value: &str,
) -> Result<Vec<u8>, RemoteMutationError> {
    let line = format!("{property}:{}", escape_text(value));
    if line.len() > 16 * 1024 {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    let mut folded = String::new();
    let mut octets = 0;
    for ch in line.chars() {
        if octets + ch.len_utf8() > 75 {
            folded.push_str("\r\n ");
            octets = 1;
        }
        folded.push(ch);
        octets += ch.len_utf8();
    }
    folded.push_str("\r\n");
    Ok(folded.into_bytes())
}

pub(super) struct V2PropertySpan {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) value: String,
}
pub(super) struct V2TextFields {
    pub(super) summary: V2PropertySpan,
    pub(super) description: Option<V2PropertySpan>,
    pub(super) location: Option<V2PropertySpan>,
    pub(super) event_end: usize,
    pub(super) uid: String,
}
struct V2Edit {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}

fn unescape_text_v2(value: &[u8]) -> Result<String, RemoteMutationError> {
    // Raw comma/semicolon are not TEXT characters. Inspect escapes before the
    // legacy decoder so malformed input cannot be silently rewritten by v2.
    let mut escaped = false;
    for byte in value {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if matches!(byte, b',' | b';') {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
    }
    unescape_text(value)
}

/// Pure lossless text patch. Product qualification and actor ownership are separate.
pub fn patch_single_vevent_text_fields_v2(
    raw: &[u8],
    fields: &RemoteEventTextFieldsV2,
) -> Result<Vec<u8>, RemoteMutationError> {
    let fields = fields.clone().validate()?;
    let located = locate_v2_text_fields(raw)?;
    let mut edits = Vec::new();
    let mut insertions = Vec::new();
    if let Some(title) = fields.title
        && title != located.summary.value
    {
        edits.push(V2Edit {
            start: located.summary.start,
            end: located.summary.end,
            replacement: fold_text_property_v2("SUMMARY", &title)?,
        });
    }
    for (property, action, existing) in [
        (
            "DESCRIPTION",
            fields.description,
            located.description.as_ref(),
        ),
        ("LOCATION", fields.location, located.location.as_ref()),
    ] {
        match (action, existing) {
            (None, _) | (Some(RemoteOptionalTextUpdateV2::Clear), None) => {}
            (Some(RemoteOptionalTextUpdateV2::Clear), Some(span)) => edits.push(V2Edit {
                start: span.start,
                end: span.end,
                replacement: Vec::new(),
            }),
            (Some(RemoteOptionalTextUpdateV2::Set(value)), Some(span)) if value == span.value => {}
            (Some(RemoteOptionalTextUpdateV2::Set(value)), Some(span)) => edits.push(V2Edit {
                start: span.start,
                end: span.end,
                replacement: fold_text_property_v2(property, &value)?,
            }),
            (Some(RemoteOptionalTextUpdateV2::Set(value)), None) => {
                insertions.extend(fold_text_property_v2(property, &value)?)
            }
        }
    }
    if !insertions.is_empty() {
        edits.push(V2Edit {
            start: located.event_end,
            end: located.event_end,
            replacement: insertions,
        });
    }
    if edits.is_empty() {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    edits.sort_by_key(|edit| edit.start);
    let mut patched = Vec::new();
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        patched.extend_from_slice(&raw[cursor..edit.start]);
        patched.extend(edit.replacement);
        cursor = edit.end;
    }
    patched.extend_from_slice(&raw[cursor..]);
    if patched.len() > 64 * 1024 {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(patched)
}

/// Structural v2 identity check; it grants no product eligibility or write authority.
pub fn single_vevent_uid_v2(raw: &[u8]) -> Result<String, RemoteMutationError> {
    Ok(locate_v2_text_fields(raw)?.uid)
}

fn is_top_level_event(stack: &[String]) -> bool {
    matches!(stack, [calendar, event] if calendar == "VCALENDAR" && event == "VEVENT")
}
pub(super) fn locate_v2_text_fields(raw: &[u8]) -> Result<V2TextFields, RemoteMutationError> {
    if raw.len() > 64 * 1024 {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    let logical = strict_v2_logical_spans(raw)?;
    let mut stack: Vec<String> = Vec::new();
    let mut calendar_count = 0usize;
    let mut event_count = 0usize;
    let mut summary = None;
    let mut description = None;
    let mut location = None;
    let mut uid = None;
    let mut event_end = None;
    for (start, end) in logical {
        let unfolded = unfold_property(&raw[start..end])?;
        let content = unfolded.strip_suffix(b"\r\n").unwrap_or(&unfolded);
        let (name, separator, parameterized) = v2_property_header(content)?;
        let value = &content[separator + 1..];
        if name.eq_ignore_ascii_case(b"BEGIN") || name.eq_ignore_ascii_case(b"END") {
            if parameterized
                || raw[start..end]
                    .windows(3)
                    .any(|w| w == b"\r\n " || w == b"\r\n\t")
            {
                return Err(RemoteMutationError::UnsafeCalendarData);
            }
            let component = std::str::from_utf8(value)
                .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
                .to_ascii_uppercase();
            if name.eq_ignore_ascii_case(b"BEGIN") {
                match stack.as_slice() {
                    [] if component == "VCALENDAR" => calendar_count += 1,
                    [calendar] if calendar == "VCALENDAR" && component == "VEVENT" => {
                        event_count += 1
                    }
                    [calendar] if calendar == "VCALENDAR" && component == "VTIMEZONE" => {}
                    [calendar, timezone]
                        if calendar == "VCALENDAR"
                            && timezone == "VTIMEZONE"
                            && matches!(component.as_str(), "STANDARD" | "DAYLIGHT") => {}
                    [calendar, event]
                        if calendar == "VCALENDAR"
                            && event == "VEVENT"
                            && component == "VALARM" => {}
                    _ => return Err(RemoteMutationError::UnsafeCalendarData),
                }
                stack.push(component);
            } else {
                if component == "VEVENT" && is_top_level_event(&stack) {
                    event_end = Some(start);
                }
                if stack.pop().as_deref() != Some(component.as_str()) {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
            }
            continue;
        }
        if stack.is_empty() {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        if !is_top_level_event(&stack) {
            continue;
        }
        if ![b"SUMMARY".as_slice(), b"DESCRIPTION", b"LOCATION", b"UID"]
            .iter()
            .any(|field| name.eq_ignore_ascii_case(field))
        {
            continue;
        }
        if parameterized {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        if name.eq_ignore_ascii_case(b"UID") {
            if uid.is_some() || value.is_empty() || value.len() > 4096 {
                return Err(RemoteMutationError::UnsafeCalendarData);
            }
            if unescape_text_v2(value)?.chars().any(char::is_control) {
                return Err(RemoteMutationError::UnsafeCalendarData);
            }
            uid = Some(
                std::str::from_utf8(value)
                    .map_err(|_| RemoteMutationError::UnsafeCalendarData)?
                    .to_owned(),
            );
            continue;
        }
        let span = V2PropertySpan {
            start,
            end,
            value: unescape_text_v2(value)?,
        };
        if span
            .value
            .chars()
            .any(|ch| ch.is_control() && !(name.eq_ignore_ascii_case(b"DESCRIPTION") && ch == '\n'))
        {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        let slot = if name.eq_ignore_ascii_case(b"SUMMARY") {
            &mut summary
        } else if name.eq_ignore_ascii_case(b"DESCRIPTION") {
            &mut description
        } else {
            &mut location
        };
        if slot.replace(span).is_some() {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
    }
    if !stack.is_empty() || calendar_count != 1 || event_count != 1 {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    Ok(V2TextFields {
        summary: summary.ok_or(RemoteMutationError::UnsafeCalendarData)?,
        description,
        location,
        event_end: event_end.ok_or(RemoteMutationError::UnsafeCalendarData)?,
        uid: uid.ok_or(RemoteMutationError::UnsafeCalendarData)?,
    })
}

pub(super) fn strict_v2_logical_spans(
    raw: &[u8],
) -> Result<Vec<(usize, usize)>, RemoteMutationError> {
    if raw.is_empty() || !raw.ends_with(b"\r\n") || std::str::from_utf8(raw).is_err() {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    let mut logical: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    let mut physical_count = 0;
    while start < raw.len() {
        let end = start
            + raw[start..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or(RemoteMutationError::UnsafeCalendarData)?
            + 2;
        if raw[start..end - 2].contains(&b'\r') || raw[start..end - 2].contains(&b'\n') {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        physical_count += 1;
        if physical_count > 10_000 {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        if matches!(raw.get(start), Some(b' ' | b'\t')) {
            let Some((_, prior_end)) = logical.last_mut() else {
                return Err(RemoteMutationError::UnsafeCalendarData);
            };
            *prior_end = end;
        } else {
            logical.push((start, end));
        }
        start = end;
    }
    Ok(logical)
}

/// Inspect every content line, including wrapper and alarm lines. Unknown valid
/// properties remain byte-identical; quoted parameter colons are not delimiters.
pub(super) fn v2_property_header(
    content: &[u8],
) -> Result<(&[u8], usize, bool), RemoteMutationError> {
    let text = std::str::from_utf8(content).map_err(|_| RemoteMutationError::UnsafeCalendarData)?;
    if content.len() > 16 * 1024 || text.chars().any(char::is_control) {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    let mut quoted = false;
    let mut separator = None;
    for (index, byte) in content.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b':' if !quoted => {
                separator = Some(index);
                break;
            }
            _ => {}
        }
    }
    let separator = separator.ok_or(RemoteMutationError::UnsafeCalendarData)?;
    let name_end = content[..separator]
        .iter()
        .position(|b| *b == b';')
        .unwrap_or(separator);
    let name = &content[..name_end];
    if name.is_empty() || !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-') {
        return Err(RemoteMutationError::UnsafeCalendarData);
    }
    if name_end != separator {
        let header = &text[name_end + 1..separator];
        let mut beginning = 0;
        let mut quoted = false;
        for (index, ch) in header
            .char_indices()
            .chain(std::iter::once((header.len(), ';')))
        {
            if ch == '"' {
                quoted = !quoted;
            }
            if ch == ';' && !quoted {
                let (key, value) = header[beginning..index]
                    .split_once('=')
                    .ok_or(RemoteMutationError::UnsafeCalendarData)?;
                if key.is_empty()
                    || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || value.is_empty()
                {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
                validate_v2_parameter_values(value)?;
                beginning = index + 1;
            }
        }
        if quoted {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
    }
    Ok((name, separator, name_end != separator))
}

fn validate_v2_parameter_values(value: &str) -> Result<(), RemoteMutationError> {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            cursor += 1;
            let end = bytes[cursor..]
                .iter()
                .position(|b| *b == b'"')
                .ok_or(RemoteMutationError::UnsafeCalendarData)?;
            cursor += end + 1;
        } else {
            let beginning = cursor;
            while cursor < bytes.len() && bytes[cursor] != b',' {
                if matches!(bytes[cursor], b'"' | b':' | b';') {
                    return Err(RemoteMutationError::UnsafeCalendarData);
                }
                cursor += 1;
            }
            if cursor == beginning {
                return Err(RemoteMutationError::UnsafeCalendarData);
            }
        }
        if cursor == bytes.len() {
            return Ok(());
        }
        if bytes[cursor] != b',' || cursor + 1 == bytes.len() {
            return Err(RemoteMutationError::UnsafeCalendarData);
        }
        cursor += 1;
    }
    Err(RemoteMutationError::UnsafeCalendarData)
}

/// Actor-issued v2 lease. No serde or transport authority; retained evidence is
/// private to Rust and binds the exact intent/base/route and process-local epoch.
#[derive(Clone)]
pub struct RemoteFieldsMutationClaim {
    pub(crate) operation_id: String,
    pub(crate) account_id: String,
    pub(crate) calendar_id: String,
    pub(crate) resource_id: String,
    pub(crate) series_id: String,
    pub(crate) uid: String,
    pub(crate) claim_token: String,
    pub(crate) sync_epoch: Option<Uuid>,
    pub(crate) collection_href: String,
    pub(crate) exact_href: String,
    pub(crate) base_etag: String,
    pub(crate) base_raw_ics: Vec<u8>,
    pub(crate) intent_json: String,
    pub(crate) expected_raw_ics: Vec<u8>,
    pub(crate) phase: RemoteMutationExecutionPhase,
}
impl RemoteFieldsMutationClaim {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn phase(&self) -> RemoteMutationExecutionPhase {
        self.phase
    }
}
impl fmt::Debug for RemoteFieldsMutationClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteFieldsMutationClaim(<redacted>)")
    }
}

/// Read-only actor evidence for a later independently guarded sender. This
/// snapshot neither claims work nor grants network/lifecycle authority.
#[derive(Clone)]
#[allow(dead_code)] // Prepared bounded evidence; production sender is deliberately uncomposed.
pub struct RemoteFieldsSendSnapshot {
    pub(crate) operation_id: String,
    pub(crate) account_id: String,
    pub(crate) calendar_id: String,
    pub(crate) resource_id: String,
    pub(crate) collection_href: String,
    pub(crate) exact_href: String,
    pub(crate) base_etag: String,
    pub(crate) expected_raw_ics: Vec<u8>,
}
impl fmt::Debug for RemoteFieldsSendSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteFieldsSendSnapshot(<redacted>)")
    }
}
impl RemoteFieldsSendSnapshot {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

/// Independently parse top-level v2 text for source-scoped projection updates.
/// Time and alarm projection remain unchanged by this text-only codec.
pub(crate) fn single_vevent_text_values_v2(
    raw: &[u8],
) -> Result<(String, Option<String>, Option<String>), RemoteMutationError> {
    let fields = locate_v2_text_fields(raw)?;
    Ok((
        fields.summary.value,
        fields.description.map(|span| span.value),
        fields.location.map(|span| span.value),
    ))
}
