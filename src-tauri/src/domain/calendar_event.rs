use crate::domain::local_todo::{RecurrenceFrequency, Subtask, Tag, normalize_tag_name};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_TITLE_CHARS: usize = 512;
const MAX_NOTES_CHARS: usize = 10_000;
const MAX_LOCATION_CHARS: usize = 512;
const DAY_MS: i64 = 86_400_000;
const MAX_SUBTASK_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionStatus {
    Ready,
    Unsupported,
}

impl ProjectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnsupportedReason {
    InvalidStructure,
    UnsupportedRecurrence,
    UnsupportedTime,
    InvalidData,
}

impl UnsupportedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidStructure => "invalid_structure",
            Self::UnsupportedRecurrence => "unsupported_recurrence",
            Self::UnsupportedTime => "unsupported_time",
            Self::InvalidData => "invalid_data",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "invalid_structure" => Some(Self::InvalidStructure),
            "unsupported_recurrence" => Some(Self::UnsupportedRecurrence),
            "unsupported_time" => Some(Self::UnsupportedTime),
            "invalid_data" => Some(Self::InvalidData),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCalendarEvent {
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: Option<i64>,
    pub end_at_utc_ms: Option<i64>,
    pub all_day: bool,
    pub cancelled: bool,
    pub status: ProjectionStatus,
    pub unsupported_reason: Option<UnsupportedReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarAgendaItem {
    pub series_id: String,
    /// Opaque source identity used for actor-derived mutation admission. DAV
    /// routing and revision evidence remain private to persistence.
    pub resource_id: String,
    pub recurrence_key: String,
    pub calendar_id: String,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: Option<i64>,
    pub end_at_utc_ms: Option<i64>,
    pub all_day: bool,
    pub status: ProjectionStatus,
    pub unsupported_reason: Option<UnsupportedReason>,
    pub work_item_id: Option<String>,
    pub completed: bool,
    pub completed_at_ms: Option<i64>,
    pub important: bool,
    pub tags: Vec<Tag>,
    pub recurrence: Option<CalendarFixedRecurrence>,
    pub subtasks: Vec<Subtask>,
    pub remote_edit_eligible: bool,
    pub remote_edit_blocked_reason: Option<String>,
    pub remote_time_edit_eligible: bool,
    /// Non-authoritative view over one durable remote mutation. This never
    /// proves that the server or authoritative calendar cache has changed.
    pub pending_remote: Option<PendingRemoteAgendaState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingRemoteAgendaKind {
    CreateUtcV2,
    UpdateEventV3,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingRemoteAgendaStatus {
    Pending,
    RetryNeeded,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRemoteAgendaState {
    pub operation_id: String,
    pub account_id: String,
    pub kind: PendingRemoteAgendaKind,
    pub status: PendingRemoteAgendaStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarFixedRecurrence {
    pub frequency: RecurrenceFrequency,
    pub interval: u32,
    pub options: crate::domain::local_recurrence::RecurrenceOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCalendarLocalMetadata {
    pub resource_id: String,
    pub tag_names: Vec<String>,
    pub recurrence: Option<CalendarFixedRecurrenceInput>,
    pub important: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarFixedRecurrenceInput {
    pub frequency: RecurrenceFrequency,
    pub interval: u32,
    #[serde(default)]
    pub options: crate::domain::local_recurrence::RecurrenceOptions,
}

impl UpdateCalendarLocalMetadata {
    pub fn validate(mut self) -> Result<Self, CalendarEventError> {
        validate_identifier(&self.resource_id)?;
        if self.tag_names.len() > 32 {
            return Err(CalendarEventError::TooManyTags);
        }
        let mut seen = std::collections::HashSet::new();
        let mut tags = Vec::with_capacity(self.tag_names.len());
        for name in self.tag_names {
            let name = name.trim().to_owned();
            if name.is_empty() || name.chars().count() > 64 {
                return Err(CalendarEventError::InvalidTag);
            }
            if seen.insert(normalize_tag_name(&name)) {
                tags.push(name);
            }
        }
        if let Some(value) = &self.recurrence {
            crate::domain::local_recurrence::validate_rule_shape(
                value.frequency,
                value.interval,
                &value.options,
            )
            .map_err(|_| CalendarEventError::InvalidRecurrence)?;
        }
        self.tag_names = tags;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarSettingsView {
    pub remote_session_ready: bool,
    pub calendar_id: String,
    pub account_id: String,
    pub display_name: String,
    pub server_color: Option<String>,
    pub color_override: Option<String>,
    pub effective_color: Option<String>,
    pub available: bool,
    pub visible: bool,
    pub display_past_days: Option<i64>,
    pub display_future_days: Option<i64>,
    pub remote_create_eligible: bool,
    pub remote_create_blocked_reason: Option<String>,
}

pub const MAX_CALENDAR_DISPLAY_DAYS: i64 = 3_650;

pub fn validate_calendar_display_range(
    display_past_days: Option<i64>,
    display_future_days: Option<i64>,
) -> Result<(), CalendarEventError> {
    if display_past_days.is_none_or(|value| (0..=MAX_CALENDAR_DISPLAY_DAYS).contains(&value))
        && display_future_days.is_none_or(|value| (0..=MAX_CALENDAR_DISPLAY_DAYS).contains(&value))
    {
        Ok(())
    } else {
        Err(CalendarEventError::InvalidDisplayRange)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCalendarSubtask {
    pub series_id: String,
    pub recurrence_key: String,
    pub text: String,
}

impl CreateCalendarSubtask {
    pub fn validate(mut self) -> Result<Self, CalendarEventError> {
        validate_identifier(&self.series_id)?;
        validate_recurrence_key(&self.recurrence_key)?;
        self.text = validate_subtask_text(self.text)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCalendarSubtask {
    pub series_id: String,
    pub recurrence_key: String,
    pub subtask_id: String,
    pub text: String,
}

impl UpdateCalendarSubtask {
    pub fn validate(mut self) -> Result<Self, CalendarEventError> {
        validate_identifier(&self.series_id)?;
        validate_recurrence_key(&self.recurrence_key)?;
        validate_identifier(&self.subtask_id)?;
        self.text = validate_subtask_text(self.text)?;
        Ok(self)
    }
}

pub fn validate_identifier(value: &str) -> Result<(), CalendarEventError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| CalendarEventError::InvalidIdentifier)
}

pub fn validate_recurrence_key(value: &str) -> Result<(), CalendarEventError> {
    let valid_prefix = value.is_ascii()
        && (value == "SINGLETON"
            || value.strip_prefix("UTC:").is_some_and(|slot| {
                slot.len() == 16
                    && slot.as_bytes().get(8) == Some(&b'T')
                    && slot.ends_with('Z')
                    && slot[..8].bytes().all(|byte| byte.is_ascii_digit())
                    && slot[9..15].bytes().all(|byte| byte.is_ascii_digit())
            })
            || value.strip_prefix("DATE:").is_some_and(|slot| {
                slot.len() == 8 && slot.bytes().all(|byte| byte.is_ascii_digit())
            })
            || value.strip_prefix("TZID:").is_some_and(|tail| {
                tail.split_once(':').is_some_and(|(zone, local)| {
                    !zone.is_empty()
                        && zone.len() <= 128
                        && zone
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"/_+-".contains(&c))
                        && crate::domain::calendar_recurrence::canonical_recurrence_identity(
                            &[&format!("TZID={zone}")],
                            local,
                        )
                        .is_ok_and(|identity| identity.canonical_key == value)
                })
            }));
    if valid_prefix && value.len() <= 255 && !value.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(CalendarEventError::InvalidRecurrenceKey)
    }
}

fn validate_subtask_text(text: String) -> Result<String, CalendarEventError> {
    let text = text.trim().to_owned();
    let length = text.chars().count();
    if length == 0 {
        return Err(CalendarEventError::EmptySubtask);
    }
    if length > MAX_SUBTASK_CHARS {
        return Err(CalendarEventError::SubtaskTooLong);
    }
    Ok(text)
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CalendarEventError {
    #[error("calendar event identifier is invalid")]
    InvalidIdentifier,
    #[error("calendar recurrence key is invalid")]
    InvalidRecurrenceKey,
    #[error("calendar subtask is empty")]
    EmptySubtask,
    #[error("calendar subtask is too long")]
    SubtaskTooLong,
    #[error("calendar local metadata has too many tags")]
    TooManyTags,
    #[error("calendar local metadata tag is invalid")]
    InvalidTag,
    #[error("calendar local recurrence is invalid")]
    InvalidRecurrence,
    #[error("calendar display range is invalid")]
    InvalidDisplayRange,
}

impl NormalizedCalendarEvent {
    fn unsupported(reason: UnsupportedReason) -> Self {
        Self {
            title: "无法安全解析的日历事件".into(),
            notes: None,
            location: None,
            start_at_utc_ms: None,
            end_at_utc_ms: None,
            all_day: false,
            cancelled: false,
            status: ProjectionStatus::Unsupported,
            unsupported_reason: Some(reason),
        }
    }
}

pub fn normalize_vevent(raw: &[u8], expected_uid: &str) -> NormalizedCalendarEvent {
    normalize_vevent_inner(raw, expected_uid).unwrap_or_else(NormalizedCalendarEvent::unsupported)
}

fn normalize_vevent_inner(
    raw: &[u8],
    expected_uid: &str,
) -> Result<NormalizedCalendarEvent, UnsupportedReason> {
    let text = std::str::from_utf8(raw).map_err(|_| UnsupportedReason::InvalidData)?;
    let lines = unfold(text)?;
    let declared_timezones = declared_timezones(&lines)?;
    // Recurrence dispatch must not depend on which property fails singleton parsing first.
    let mut scope = Vec::new();
    for line in &lines {
        let (head, value) = line
            .split_once(':')
            .ok_or(UnsupportedReason::InvalidStructure)?;
        let name = head.split(';').next().unwrap_or("").to_ascii_uppercase();
        match name.as_str() {
            "BEGIN" => scope.push(value.to_ascii_uppercase()),
            "END" => {
                scope.pop();
            }
            "RRULE" | "RDATE" | "EXDATE" | "RECURRENCE-ID"
                if scope.last().map(String::as_str) == Some("VEVENT") =>
            {
                return Err(UnsupportedReason::UnsupportedRecurrence);
            }
            _ => {}
        }
    }
    let mut component_stack: Vec<String> = Vec::new();
    let mut calendar_count = 0_u8;
    let mut event_count = 0_u8;
    let mut summary = None;
    let mut notes = None;
    let mut location = None;
    let mut start = None;
    let mut end = None;
    let mut start_timezone = None;
    let mut end_timezone = None;
    let mut all_day = false;
    let mut cancelled = false;
    let mut status_seen = false;
    let mut uid = None;

    for line in lines {
        let (head, value) = line
            .split_once(':')
            .ok_or(UnsupportedReason::InvalidStructure)?;
        let mut parts = head.split(';');
        let name = parts
            .next()
            .ok_or(UnsupportedReason::InvalidStructure)?
            .to_ascii_uppercase();
        let params = parts.collect::<Vec<_>>();
        if name == "BEGIN" {
            if !params.is_empty() {
                return Err(UnsupportedReason::InvalidStructure);
            }
            let component = value.to_ascii_uppercase();
            if component == "VCALENDAR" {
                if !component_stack.is_empty() {
                    return Err(UnsupportedReason::InvalidStructure);
                }
                calendar_count = calendar_count.saturating_add(1);
            }
            if component == "VEVENT" {
                if component_stack.last().map(String::as_str) != Some("VCALENDAR") {
                    return Err(UnsupportedReason::InvalidStructure);
                }
                event_count = event_count.saturating_add(1);
            }
            component_stack.push(component);
            continue;
        }
        if name == "END" {
            if !params.is_empty() {
                return Err(UnsupportedReason::InvalidStructure);
            }
            let component = value.to_ascii_uppercase();
            if component_stack.pop().as_deref() != Some(component.as_str()) {
                return Err(UnsupportedReason::InvalidStructure);
            }
            continue;
        }
        if component_stack.last().map(String::as_str) != Some("VEVENT") {
            continue;
        }
        // New explicit timed headers are accepted only inside the existing
        // strict event-v3 envelope; legacy no-parameter/DATE parsing is unchanged.
        if matches!(name.as_str(), "DTSTART" | "DTEND")
            && params.len() == 1
            && params[0].eq_ignore_ascii_case("VALUE=DATE-TIME")
        {
            super::remote_event_update::validate_event_base_v3(raw, expected_uid, false)
                .map_err(|_| UnsupportedReason::UnsupportedTime)?;
        }
        match name.as_str() {
            "RRULE" | "RDATE" | "EXDATE" | "RECURRENCE-ID" => {
                return Err(UnsupportedReason::UnsupportedRecurrence);
            }
            "DURATION" => return Err(UnsupportedReason::UnsupportedTime),
            "UID" => {
                ensure_no_params(&params)?;
                set_once(&mut uid, decode_text(value, 4_096)?)?;
            }
            "SUMMARY" => {
                ensure_no_params(&params)?;
                set_once(&mut summary, decode_text(value, MAX_TITLE_CHARS)?)?;
            }
            "DESCRIPTION" => {
                ensure_no_params(&params)?;
                set_once(&mut notes, decode_text(value, MAX_NOTES_CHARS)?)?;
            }
            "LOCATION" => {
                ensure_no_params(&params)?;
                set_once(&mut location, decode_text(value, MAX_LOCATION_CHARS)?)?;
            }
            "DTSTART" => {
                if start.is_some() {
                    return Err(UnsupportedReason::InvalidData);
                }
                let parsed = parse_time(&params, value, &declared_timezones)?;
                all_day = parsed.1;
                start = Some(parsed.0);
                start_timezone = parsed.2;
            }
            "DTEND" => {
                if end.is_some() {
                    return Err(UnsupportedReason::InvalidData);
                }
                let parsed = parse_time(&params, value, &declared_timezones)?;
                end_timezone = parsed.2.clone();
                end = Some((parsed.0, parsed.1));
            }
            "STATUS" => {
                ensure_no_params(&params)?;
                if status_seen {
                    return Err(UnsupportedReason::InvalidData);
                }
                status_seen = true;
                cancelled = value.eq_ignore_ascii_case("CANCELLED");
            }
            _ => {}
        }
    }
    if !component_stack.is_empty() || calendar_count != 1 || event_count != 1 {
        return Err(UnsupportedReason::InvalidStructure);
    }
    if uid.as_deref() != Some(expected_uid) {
        return Err(UnsupportedReason::InvalidData);
    }
    let start = start.ok_or(UnsupportedReason::UnsupportedTime)?;
    if let Some((_, end_all_day)) = end
        && (end_all_day != all_day || end_timezone != start_timezone)
    {
        return Err(UnsupportedReason::UnsupportedTime);
    }
    let end_at = end.map(|value| value.0).unwrap_or_else(|| {
        if all_day {
            start.checked_add(DAY_MS).unwrap_or(start)
        } else {
            start
        }
    });
    if end_at < start || (all_day && end_at == start) {
        return Err(UnsupportedReason::InvalidData);
    }
    Ok(NormalizedCalendarEvent {
        title: summary
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "无标题事件".into()),
        notes: notes.filter(|value| !value.is_empty()),
        location: location.filter(|value| !value.is_empty()),
        start_at_utc_ms: Some(start),
        end_at_utc_ms: Some(end_at),
        all_day,
        cancelled,
        status: ProjectionStatus::Ready,
        unsupported_reason: None,
    })
}

fn unfold(text: &str) -> Result<Vec<String>, UnsupportedReason> {
    let mut output: Vec<String> = Vec::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            output
                .last_mut()
                .ok_or(UnsupportedReason::InvalidStructure)?
                .push_str(&line[1..]);
        } else {
            output.push(line.trim_end_matches('\r').to_owned());
        }
    }
    Ok(output)
}

fn set_once(slot: &mut Option<String>, value: String) -> Result<(), UnsupportedReason> {
    if slot.replace(value).is_some() {
        return Err(UnsupportedReason::InvalidData);
    }
    Ok(())
}

fn ensure_no_params(params: &[&str]) -> Result<(), UnsupportedReason> {
    if params.is_empty() {
        Ok(())
    } else {
        Err(UnsupportedReason::InvalidData)
    }
}

fn decode_text(value: &str, max: usize) -> Result<String, UnsupportedReason> {
    let mut decoded = String::new();
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
            _ => return Err(UnsupportedReason::InvalidData),
        }
    }
    if decoded.chars().count() > max {
        return Err(UnsupportedReason::InvalidData);
    }
    Ok(decoded)
}

fn parse_time(
    params: &[&str],
    value: &str,
    declared_timezones: &std::collections::BTreeSet<String>,
) -> Result<(i64, bool, Option<String>), UnsupportedReason> {
    let explicit_utc = params.len() == 1 && params[0].eq_ignore_ascii_case("VALUE=DATE-TIME");
    if explicit_utc
        && (value.len() != 16
            || !value.is_ascii()
            || value.as_bytes()[8] != b'T'
            || !value.ends_with('Z')
            || !value.as_bytes()[..8]
                .iter()
                .chain(&value.as_bytes()[9..15])
                .all(u8::is_ascii_digit))
    {
        return Err(UnsupportedReason::UnsupportedTime);
    }
    if params.len() == 1
        && let Some((key, timezone)) = params[0].split_once('=')
        && key.eq_ignore_ascii_case("TZID")
    {
        if timezone.is_empty()
            || timezone.len() > 128
            || !timezone
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "/_+-".contains(character))
            || !declared_timezones.contains(timezone)
        {
            return Err(UnsupportedReason::UnsupportedTime);
        }
        let local = parse_local_datetime(value)?;
        let zone =
            jiff::tz::TimeZone::get(timezone).map_err(|_| UnsupportedReason::UnsupportedTime)?;
        let zoned = zone
            .to_ambiguous_zoned(local)
            .unambiguous()
            .map_err(|_| UnsupportedReason::UnsupportedTime)?;
        return Ok((
            zoned.timestamp().as_millisecond(),
            false,
            Some(timezone.to_owned()),
        ));
    }
    let mut date_value = false;
    for param in params {
        if param.eq_ignore_ascii_case("VALUE=DATE") {
            date_value = true;
        } else if !explicit_utc {
            return Err(UnsupportedReason::UnsupportedTime);
        }
    }
    if date_value || value.len() == 8 {
        if !date_value && !params.is_empty() {
            return Err(UnsupportedReason::UnsupportedTime);
        }
        return Ok((parse_date(value)?, true, None));
    }
    if (!params.is_empty() && !explicit_utc)
        || value.len() != 16
        || !value.ends_with('Z')
        || &value[8..9] != "T"
    {
        return Err(UnsupportedReason::UnsupportedTime);
    }
    let date = parse_date(&value[..8])?;
    let hour = parse_digits(&value[9..11])?;
    let minute = parse_digits(&value[11..13])?;
    let second = parse_digits(&value[13..15])?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(UnsupportedReason::InvalidData);
    }
    let offset = i64::from(hour * 3_600 + minute * 60 + second)
        .checked_mul(1_000)
        .ok_or(UnsupportedReason::InvalidData)?;
    Ok((
        date.checked_add(offset)
            .ok_or(UnsupportedReason::InvalidData)?,
        false,
        None,
    ))
}

pub(super) fn parse_local_datetime(
    value: &str,
) -> Result<jiff::civil::DateTime, UnsupportedReason> {
    if value.len() != 15 || !value.is_ascii() || value.as_bytes()[8] != b'T' {
        return Err(UnsupportedReason::UnsupportedTime);
    }
    let year =
        i16::try_from(parse_digits(&value[0..4])?).map_err(|_| UnsupportedReason::InvalidData)?;
    let month =
        i8::try_from(parse_digits(&value[4..6])?).map_err(|_| UnsupportedReason::InvalidData)?;
    let day =
        i8::try_from(parse_digits(&value[6..8])?).map_err(|_| UnsupportedReason::InvalidData)?;
    let hour =
        i8::try_from(parse_digits(&value[9..11])?).map_err(|_| UnsupportedReason::InvalidData)?;
    let minute =
        i8::try_from(parse_digits(&value[11..13])?).map_err(|_| UnsupportedReason::InvalidData)?;
    let second =
        i8::try_from(parse_digits(&value[13..15])?).map_err(|_| UnsupportedReason::InvalidData)?;
    jiff::civil::DateTime::new(year, month, day, hour, minute, second, 0)
        .map_err(|_| UnsupportedReason::InvalidData)
}

pub(super) fn declared_timezones(
    lines: &[String],
) -> Result<std::collections::BTreeSet<String>, UnsupportedReason> {
    let mut stack = Vec::<String>::new();
    let mut timezones = std::collections::BTreeSet::new();
    for line in lines {
        let (head, value) = line
            .split_once(':')
            .ok_or(UnsupportedReason::InvalidStructure)?;
        let mut pieces = head.split(';');
        let name = pieces
            .next()
            .ok_or(UnsupportedReason::InvalidStructure)?
            .to_ascii_uppercase();
        let has_params = pieces.next().is_some();
        if name == "BEGIN" {
            if has_params {
                return Err(UnsupportedReason::InvalidStructure);
            }
            stack.push(value.to_ascii_uppercase());
        } else if name == "END" {
            if has_params || stack.pop().as_deref() != Some(value.to_ascii_uppercase().as_str()) {
                return Err(UnsupportedReason::InvalidStructure);
            }
        } else if stack.last().map(String::as_str) == Some("VTIMEZONE") && name == "TZID" {
            if has_params
                || value.is_empty()
                || value.len() > 128
                || !value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "/_+-".contains(character)
                })
                || !timezones.insert(value.to_owned())
            {
                return Err(UnsupportedReason::UnsupportedTime);
            }
        }
    }
    if !stack.is_empty() {
        return Err(UnsupportedReason::InvalidStructure);
    }
    Ok(timezones)
}

fn parse_date(value: &str) -> Result<i64, UnsupportedReason> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(UnsupportedReason::InvalidData);
    }
    let year = parse_digits(&value[..4])? as i32;
    let month = parse_digits(&value[4..6])?;
    let day = parse_digits(&value[6..8])?;
    if !(1600..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Err(UnsupportedReason::InvalidData);
    }
    let month_days = [
        31,
        28 + u32::from(is_leap(year)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > month_days[(month - 1) as usize] {
        return Err(UnsupportedReason::InvalidData);
    }
    days_from_civil(year, month, day)
        .checked_mul(DAY_MS)
        .ok_or(UnsupportedReason::InvalidData)
}

fn parse_digits(value: &str) -> Result<u32, UnsupportedReason> {
    value.parse().map_err(|_| UnsupportedReason::InvalidData)
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i32::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i32::try_from(month).unwrap_or(0) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i32::try_from(day).unwrap_or(0) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}
