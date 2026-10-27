//! Independent event-v3 pure proof. No persistence, lease, jobs or send authority.
use super::{
    calendar_alarm::{AlarmFire, fires_for_occurrence, parse_utc},
    calendar_event::{ProjectionStatus, normalize_vevent},
    calendar_recurrence::format_utc_timestamp,
    remote_mutation::{
        RemoteEventTextFieldsV2, RemoteMutationError, RemoteOptionalTextUpdateV2, V2TextFields,
        fold_text_property_v2, locate_v2_text_fields, strict_v2_logical_spans, unfold_property,
        v2_property_header, validate_v2_serialized_intent,
    },
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Compare ordered logical content lines, ignoring only RFC 5545 physical folds.
/// No whitespace, escaping, property ordering or value normalization is allowed.
pub(crate) fn event_readback_matches(expected: &[u8], actual: &[u8]) -> Result<bool, Error> {
    if expected.len() > MAX_EVENT_UPDATE_BODY_BYTES || actual.len() > MAX_EVENT_UPDATE_BODY_BYTES {
        return Err(Error::UnsafeCalendarData);
    }
    let left = strict_v2_logical_spans(expected)?;
    let right = strict_v2_logical_spans(actual)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for ((a, b), (c, d)) in left.into_iter().zip(right) {
        if unfold_property(&expected[a..b])? != unfold_property(&actual[c..d])? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub const EVENT_UPDATE_CODEC_V3: &str = "event-v3";
pub const MAX_EVENT_UPDATE_INTENT_BYTES: usize = 32 * 1024;
pub const MAX_EVENT_UPDATE_BODY_BYTES: usize = 64 * 1024;
const UTC_LIMIT_MS: i64 = 4_102_444_800_000;
type Error = RemoteMutationError;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UtcEventTimePair {
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
}
impl UtcEventTimePair {
    pub fn validate(self) -> Result<Self, Error> {
        if !(0..UTC_LIMIT_MS).contains(&self.start_at_utc_ms)
            || !(0..UTC_LIMIT_MS).contains(&self.end_at_utc_ms)
            || self.start_at_utc_ms % 1000 != 0
            || self.end_at_utc_ms % 1000 != 0
            || self.start_at_utc_ms >= self.end_at_utc_ms
        {
            return Err(Error::UnsafeCalendarData);
        }
        Ok(self)
    }
}
impl fmt::Debug for UtcEventTimePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UtcEventTimePair(<redacted>)")
    }
}

/// Omission retains a field; time is a complete UTC pair, never one endpoint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteEventUpdateFieldsV3 {
    pub title: Option<String>,
    pub description: Option<RemoteOptionalTextUpdateV2>,
    pub location: Option<RemoteOptionalTextUpdateV2>,
    pub time: Option<UtcEventTimePair>,
}
impl RemoteEventUpdateFieldsV3 {
    pub fn validate(mut self) -> Result<Self, Error> {
        let text = RemoteEventTextFieldsV2 {
            title: self.title.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
        };
        if text.title.is_some() || text.description.is_some() || text.location.is_some() {
            let text = text.validate()?;
            self.title = text.title;
            self.description = text.description;
            self.location = text.location;
        } else if self.time.is_none() {
            return Err(Error::UnsafeCalendarData);
        }
        if let Some(time) = self.time {
            time.validate()?;
        }
        // Bound the complete durable codec, including its version wrapper.
        validate_v2_serialized_intent(&StoredIntent {
            version: IntentVersion::EventV3,
            fields: self.clone(),
        })?;
        Ok(self)
    }
}
impl fmt::Debug for RemoteEventUpdateFieldsV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteEventUpdateFieldsV3(<redacted>)")
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum IntentVersion {
    #[serde(rename = "event-v3")]
    EventV3,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIntent {
    version: IntentVersion,
    fields: RemoteEventUpdateFieldsV3,
}
/// Exact bounded canonical storage bytes. Unknown versions, duplicate/unknown
/// keys, noncanonical values, whitespace and alternate ordering fail closed.
pub fn encode_event_intent_v3(fields: &RemoteEventUpdateFieldsV3) -> Result<Vec<u8>, Error> {
    let intent = StoredIntent {
        version: IntentVersion::EventV3,
        fields: fields.clone().validate()?,
    };
    let json = serde_json::to_vec(&intent).map_err(|_| Error::UnsafeCalendarData)?;
    if json.len() > MAX_EVENT_UPDATE_INTENT_BYTES {
        return Err(Error::UnsafeCalendarData);
    }
    Ok(json)
}
pub fn decode_event_intent_v3(json: &[u8]) -> Result<RemoteEventUpdateFieldsV3, Error> {
    if json.is_empty() || json.len() > MAX_EVENT_UPDATE_INTENT_BYTES {
        return Err(Error::CorruptStoredValue);
    }
    let stored: StoredIntent =
        serde_json::from_slice(json).map_err(|_| Error::CorruptStoredValue)?;
    if encode_event_intent_v3(&stored.fields).map_err(|_| Error::CorruptStoredValue)? != json {
        return Err(Error::CorruptStoredValue);
    }
    Ok(stored.fields)
}

/// Bounded parser output only; actor must recompute inside its transaction.
/// Alarm fires are semantic evidence, not durable notification jobs.
#[derive(Clone, PartialEq, Eq)]
pub struct TimeAlarmEvidenceV3 {
    pub before: UtcEventTimePair,
    pub after: UtcEventTimePair,
    pub before_fires: Vec<AlarmFire>,
    pub after_fires: Vec<AlarmFire>,
}
impl fmt::Debug for TimeAlarmEvidenceV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TimeAlarmEvidenceV3(<redacted>)")
    }
}
pub struct EventUpdatePatchV3 {
    raw_ics: Vec<u8>,
    uid: String,
    before: UtcEventTimePair,
    after: UtcEventTimePair,
    time_alarm_evidence: Option<TimeAlarmEvidenceV3>,
}
impl EventUpdatePatchV3 {
    pub fn bytes(&self) -> &[u8] {
        &self.raw_ics
    }
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn before_time(&self) -> UtcEventTimePair {
        self.before
    }
    pub fn after_time(&self) -> UtcEventTimePair {
        self.after
    }
    pub fn time_alarm_evidence(&self) -> Option<&TimeAlarmEvidenceV3> {
        self.time_alarm_evidence.as_ref()
    }
}
impl fmt::Debug for EventUpdatePatchV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventUpdatePatchV3(<redacted>)")
    }
}
struct TimeSpan {
    value_start: usize,
    value_end: usize,
    time: i64,
}
enum TimeEncoding {
    Utc,
    Tzid(String),
}
struct Qualified {
    text: V2TextFields,
    start: TimeSpan,
    end: TimeSpan,
    encoding: TimeEncoding,
}
struct Edit {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}
fn canonical_utc_spelling(value: &str) -> bool {
    value.len() == 16
        && value.is_ascii()
        && value.as_bytes()[8] == b'T'
        && value.ends_with('Z')
        && value.as_bytes()[..8]
            .iter()
            .chain(&value.as_bytes()[9..15])
            .all(u8::is_ascii_digit)
}
fn strict_utc(value: &str) -> Result<i64, Error> {
    if !canonical_utc_spelling(value) {
        return Err(Error::UnsafeCalendarData);
    }
    let at = parse_utc(value).map_err(|_| Error::UnsafeCalendarData)?;
    if !(0..UTC_LIMIT_MS).contains(&at) {
        return Err(Error::UnsafeCalendarData);
    }
    Ok(at)
}
fn validate_selected_alarm_utc(raw: &[u8]) -> Result<(), Error> {
    let mut depth = 0usize;
    for (begin, end) in strict_v2_logical_spans(raw)? {
        let line = unfold_property(&raw[begin..end])?;
        let content = line
            .strip_suffix(b"\r\n")
            .ok_or(Error::UnsafeCalendarData)?;
        let (name, separator, _) = v2_property_header(content)?;
        if name.eq_ignore_ascii_case(b"BEGIN") {
            depth += 1;
            continue;
        }
        if name.eq_ignore_ascii_case(b"END") {
            depth -= 1;
            continue;
        }
        if depth == 3 && name.eq_ignore_ascii_case(b"TRIGGER") {
            let header = std::str::from_utf8(&content[..separator])
                .map_err(|_| Error::UnsafeCalendarData)?;
            if header
                .split_once(';')
                .is_some_and(|(_, p)| p.eq_ignore_ascii_case("VALUE=DATE-TIME"))
            {
                let value = std::str::from_utf8(&content[separator + 1..])
                    .map_err(|_| Error::UnsafeCalendarData)?;
                if !canonical_utc_spelling(value) {
                    return Err(Error::UnsafeCalendarData);
                }
            }
        }
    }
    Ok(())
}
/// Pure supported-content qualification; no selected edit or mutation authority.
/// A valid unchanged event is eligible content even though a no-op patch is denied.
fn qualify(raw: &[u8]) -> Result<Qualified, Error> {
    let text = locate_v2_text_fields(raw)?;
    let mut depth = 0usize;
    let mut start = None;
    let mut end = None;
    let mut version = false;
    let mut method = false;
    for (begin, finish) in strict_v2_logical_spans(raw)? {
        let unfolded = unfold_property(&raw[begin..finish])?;
        let content = unfolded
            .strip_suffix(b"\r\n")
            .ok_or(Error::UnsafeCalendarData)?;
        let (name, separator, params) = v2_property_header(content)?;
        let value = std::str::from_utf8(&content[separator + 1..])
            .map_err(|_| Error::UnsafeCalendarData)?;
        if name.eq_ignore_ascii_case(b"BEGIN") {
            if value.eq_ignore_ascii_case("VTIMEZONE") {
                return Err(Error::UnsafeCalendarData);
            }
            depth += 1;
            continue;
        }
        if name.eq_ignore_ascii_case(b"END") {
            depth = depth.checked_sub(1).ok_or(Error::UnsafeCalendarData)?;
            continue;
        }
        // VALARM semantics are deliberately outside text-only qualification.
        // Selected time edits separately reuse the existing semantic parser.
        if depth == 3 {
            continue;
        }
        if [
            b"ATTENDEE".as_slice(),
            b"ORGANIZER",
            b"RRULE",
            b"RDATE",
            b"EXDATE",
            b"RECURRENCE-ID",
        ]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
        {
            return Err(Error::UnsafeCalendarData);
        }
        if name.eq_ignore_ascii_case(b"VERSION") {
            if depth != 1 || params || value != "2.0" || version {
                return Err(Error::UnsafeCalendarData);
            }
            version = true;
        }
        if name.eq_ignore_ascii_case(b"METHOD") {
            if depth != 1 || params || !value.eq_ignore_ascii_case("PUBLISH") || method {
                return Err(Error::UnsafeCalendarData);
            }
            method = true;
        }
        if depth != 2 {
            continue;
        }
        if name.eq_ignore_ascii_case(b"DURATION") || name.eq_ignore_ascii_case(b"VTIMEZONE") {
            return Err(Error::UnsafeCalendarData);
        }
        if name.eq_ignore_ascii_case(b"DTSTART") || name.eq_ignore_ascii_case(b"DTEND") {
            let header = std::str::from_utf8(&content[..separator])
                .map_err(|_| Error::UnsafeCalendarData)?;
            let pieces: Vec<_> = header.split(';').skip(1).collect();
            if !(pieces.is_empty()
                || pieces.len() == 1 && pieces[0].eq_ignore_ascii_case("VALUE=DATE-TIME"))
            {
                return Err(Error::UnsafeCalendarData);
            }
            let value_start = begin
                + raw[begin..finish]
                    .iter()
                    .position(|b| *b == b':')
                    .ok_or(Error::UnsafeCalendarData)?
                + 1;
            let span = TimeSpan {
                value_start,
                value_end: finish - 2,
                time: strict_utc(value)?,
            };
            let slot = if name.eq_ignore_ascii_case(b"DTSTART") {
                &mut start
            } else {
                &mut end
            };
            if slot.replace(span).is_some() {
                return Err(Error::UnsafeCalendarData);
            }
        }
    }
    if !version {
        return Err(Error::UnsafeCalendarData);
    }
    let qualified = Qualified {
        text,
        start: start.ok_or(Error::UnsafeCalendarData)?,
        end: end.ok_or(Error::UnsafeCalendarData)?,
        encoding: TimeEncoding::Utc,
    };
    UtcEventTimePair {
        start_at_utc_ms: qualified.start.time,
        end_at_utc_ms: qualified.end.time,
    }
    .validate()?;
    Ok(qualified)
}

fn qualify_supported(raw: &[u8]) -> Result<Qualified, Error> {
    if let Ok(base) = qualify(raw) {
        return Ok(base);
    }
    let text = locate_v2_text_fields(raw)?;
    let mut depth = 0usize;
    let mut version = false;
    let mut method = false;
    let mut start = None;
    let mut end = None;
    for (begin, finish) in strict_v2_logical_spans(raw)? {
        let unfolded = unfold_property(&raw[begin..finish])?;
        let content = unfolded
            .strip_suffix(b"\r\n")
            .ok_or(Error::UnsafeCalendarData)?;
        let (name, separator, params) = v2_property_header(content)?;
        let value = std::str::from_utf8(&content[separator + 1..])
            .map_err(|_| Error::UnsafeCalendarData)?;
        if name.eq_ignore_ascii_case(b"BEGIN") {
            depth += 1;
            continue;
        }
        if name.eq_ignore_ascii_case(b"END") {
            depth = depth.checked_sub(1).ok_or(Error::UnsafeCalendarData)?;
            continue;
        }
        if depth == 2
            && [
                b"ATTENDEE".as_slice(),
                b"ORGANIZER",
                b"RRULE",
                b"RDATE",
                b"EXDATE",
                b"RECURRENCE-ID",
            ]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        {
            return Err(Error::UnsafeCalendarData);
        }
        if name.eq_ignore_ascii_case(b"VERSION") {
            if depth != 1 || params || value != "2.0" || version {
                return Err(Error::UnsafeCalendarData);
            }
            version = true;
        }
        if name.eq_ignore_ascii_case(b"METHOD") {
            if depth != 1 || params || !value.eq_ignore_ascii_case("PUBLISH") || method {
                return Err(Error::UnsafeCalendarData);
            }
            method = true;
        }
        if depth == 2
            && (name.eq_ignore_ascii_case(b"DTSTART") || name.eq_ignore_ascii_case(b"DTEND"))
        {
            let header = std::str::from_utf8(&content[..separator])
                .map_err(|_| Error::UnsafeCalendarData)?;
            let mut parameters = header.split(';').skip(1);
            let parameter = parameters.next().ok_or(Error::UnsafeCalendarData)?;
            if parameters.next().is_some() {
                return Err(Error::UnsafeCalendarData);
            }
            let (key, timezone) = parameter.split_once('=').ok_or(Error::UnsafeCalendarData)?;
            if !key.eq_ignore_ascii_case("TZID")
                || timezone.is_empty()
                || timezone.len() > 128
                || !timezone.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "/_+-".contains(character)
                })
            {
                return Err(Error::UnsafeCalendarData);
            }
            let value_start = begin
                + raw[begin..finish]
                    .iter()
                    .position(|byte| *byte == b':')
                    .ok_or(Error::UnsafeCalendarData)?
                + 1;
            let span = (value_start, finish - 2, timezone.to_owned());
            let slot = if name.eq_ignore_ascii_case(b"DTSTART") {
                &mut start
            } else {
                &mut end
            };
            if slot.replace(span).is_some() {
                return Err(Error::UnsafeCalendarData);
            }
        }
    }
    if !version {
        return Err(Error::UnsafeCalendarData);
    }
    let event = normalize_vevent(raw, &text.uid);
    if event.status != ProjectionStatus::Ready || event.all_day {
        return Err(Error::UnsafeCalendarData);
    }
    let time = UtcEventTimePair {
        start_at_utc_ms: event.start_at_utc_ms.ok_or(Error::UnsafeCalendarData)?,
        end_at_utc_ms: event.end_at_utc_ms.ok_or(Error::UnsafeCalendarData)?,
    }
    .validate()?;
    let (start_at, start_end, timezone) = start.ok_or(Error::UnsafeCalendarData)?;
    let (end_at, end_end, end_timezone) = end.ok_or(Error::UnsafeCalendarData)?;
    if timezone != end_timezone {
        return Err(Error::UnsafeCalendarData);
    }
    Ok(Qualified {
        text,
        start: TimeSpan {
            value_start: start_at,
            value_end: start_end,
            time: time.start_at_utc_ms,
        },
        end: TimeSpan {
            value_start: end_at,
            value_end: end_end,
            time: time.end_at_utc_ms,
        },
        encoding: TimeEncoding::Tzid(timezone),
    })
}

fn qualify_projected_text(raw: &[u8]) -> Result<(V2TextFields, UtcEventTimePair), Error> {
    let base = qualify_supported(raw)?;
    Ok((
        base.text,
        UtcEventTimePair {
            start_at_utc_ms: base.start.time,
            end_at_utc_ms: base.end.time,
        },
    ))
}

fn format_time_value(time: i64, encoding: &TimeEncoding) -> Result<Vec<u8>, Error> {
    match encoding {
        TimeEncoding::Utc => Ok(format_utc_timestamp(time)
            .map_err(|_| Error::UnsafeCalendarData)?
            .into_bytes()),
        TimeEncoding::Tzid(timezone) => {
            let zone = jiff::tz::TimeZone::get(timezone).map_err(|_| Error::UnsafeCalendarData)?;
            let local = jiff::Timestamp::from_millisecond(time)
                .map_err(|_| Error::UnsafeCalendarData)?
                .to_zoned(zone)
                .datetime();
            Ok(format!(
                "{:04}{:02}{:02}T{:02}{:02}{:02}",
                local.year(),
                local.month(),
                local.day(),
                local.hour(),
                local.minute(),
                local.second()
            )
            .into_bytes())
        }
    }
}

fn alarm_fires(
    raw: &[u8],
    encoding: &TimeEncoding,
    start: i64,
    end: i64,
) -> Result<Vec<AlarmFire>, Error> {
    if matches!(encoding, TimeEncoding::Utc) {
        return fires_for_occurrence(raw, "SINGLETON", start, end)
            .map_err(|_| Error::UnsafeCalendarData);
    }
    let mut body = Vec::with_capacity(raw.len());
    let mut timezone_depth = 0usize;
    for (begin, finish) in strict_v2_logical_spans(raw)? {
        let unfolded = unfold_property(&raw[begin..finish])?;
        let content = unfolded
            .strip_suffix(b"\r\n")
            .ok_or(Error::UnsafeCalendarData)?;
        let (name, separator, params) = v2_property_header(content)?;
        let value = std::str::from_utf8(&content[separator + 1..])
            .map_err(|_| Error::UnsafeCalendarData)?;
        if timezone_depth > 0 {
            if name.eq_ignore_ascii_case(b"BEGIN") {
                timezone_depth += 1;
            } else if name.eq_ignore_ascii_case(b"END") {
                timezone_depth = timezone_depth
                    .checked_sub(1)
                    .ok_or(Error::UnsafeCalendarData)?;
            }
            continue;
        }
        if name.eq_ignore_ascii_case(b"BEGIN") && !params && value.eq_ignore_ascii_case("VTIMEZONE")
        {
            timezone_depth = 1;
            continue;
        }
        body.extend_from_slice(&raw[begin..finish]);
    }
    if timezone_depth != 0 {
        return Err(Error::UnsafeCalendarData);
    }
    fires_for_occurrence(&body, "SINGLETON", start, end).map_err(|_| Error::UnsafeCalendarData)
}

/// Qualification only; no semantic change or execution authority is implied.
pub fn validate_event_base_v3(raw: &[u8], uid: &str, time_selected: bool) -> Result<(), Error> {
    let base = qualify_supported(raw)?;
    if base.text.uid != uid {
        return Err(Error::UnsafeCalendarData);
    }
    if time_selected {
        validate_selected_alarm_utc(raw)?;
        alarm_fires(raw, &base.encoding, base.start.time, base.end.time)?;
    }
    Ok(())
}

pub(crate) fn selected_base_matches_v3(
    raw: &[u8],
    fields: &RemoteEventUpdateFieldsV3,
) -> Result<bool, Error> {
    use super::remote_mutation::RemoteOptionalTextUpdateV2::{Clear, Set};
    if let Some(time) = &fields.time {
        let base = qualify_supported(raw)?;
        if base.start.time != time.start_at_utc_ms || base.end.time != time.end_at_utc_ms {
            return Ok(false);
        }
    } else {
        qualify_projected_text(raw)?;
    }
    let (title, description, location) = super::remote_mutation::single_vevent_text_values_v2(raw)?;
    let matches = |selected: &Option<super::remote_mutation::RemoteOptionalTextUpdateV2>,
                   actual: &Option<String>| match selected {
        None => true,
        Some(Clear) => actual.is_none(),
        Some(Set(value)) => actual.as_ref() == Some(value),
    };
    Ok(fields.title.as_ref().is_none_or(|value| value == &title)
        && matches(&fields.description, &description)
        && matches(&fields.location, &location))
}

pub(crate) fn selected_fields_unchanged_v3(
    before: &[u8],
    after: &[u8],
    fields: &RemoteEventUpdateFieldsV3,
) -> Result<bool, Error> {
    if fields.time.is_some() {
        let before_base = qualify_supported(before)?;
        let after_base = qualify_supported(after)?;
        if before_base.start.time != after_base.start.time
            || before_base.end.time != after_base.end.time
        {
            return Ok(false);
        }
    } else {
        qualify_projected_text(before)?;
        qualify_projected_text(after)?;
    }
    if fields.title.is_none() && fields.description.is_none() && fields.location.is_none() {
        return Ok(true);
    }
    let before_text = super::remote_mutation::single_vevent_text_values_v2(before)?;
    let after_text = super::remote_mutation::single_vevent_text_values_v2(after)?;
    Ok((fields.title.is_none() || before_text.0 == after_text.0)
        && (fields.description.is_none() || before_text.1 == after_text.1)
        && (fields.location.is_none() || before_text.2 == after_text.2))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRemoteEventUpdateV3 {
    pub idempotency_key: String,
    pub resource_id: String,
    pub fields: RemoteEventUpdateFieldsV3,
}
impl QueueRemoteEventUpdateV3 {
    pub fn validate(mut self) -> Result<Self, Error> {
        for id in [&self.idempotency_key, &self.resource_id] {
            uuid::Uuid::parse_str(id).map_err(|_| Error::InvalidIdentity)?;
        }
        self.fields = self.fields.validate()?;
        encode_event_intent_v3(&self.fields)?;
        Ok(self)
    }
}
impl fmt::Debug for QueueRemoteEventUpdateV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueRemoteEventUpdateV3(<redacted>)")
    }
}
/// Distinct actor lease type. The private common store representation is never
/// accepted by a text-v2 actor lookup because persisted operation_kind differs.
#[derive(Clone)]
pub struct RemoteEventMutationClaimV3 {
    pub(crate) captured: super::remote_mutation::RemoteFieldsMutationClaim,
}
impl RemoteEventMutationClaimV3 {
    pub fn operation_id(&self) -> &str {
        self.captured.operation_id()
    }
    pub fn account_id(&self) -> &str {
        self.captured.account_id()
    }
    pub fn phase(&self) -> super::remote_mutation::RemoteMutationExecutionPhase {
        self.captured.phase()
    }
}
impl fmt::Debug for RemoteEventMutationClaimV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteEventMutationClaimV3(<redacted>)")
    }
}
/// Read-only complete actor proof; grants no transport or notification job authority.
#[derive(Clone)]
pub struct RemoteEventUpdateSnapshotV3 {
    pub(crate) captured: super::remote_mutation::RemoteFieldsSendSnapshot,
    pub(crate) before: UtcEventTimePair,
    pub(crate) after: UtcEventTimePair,
    pub(crate) alarms: Option<TimeAlarmEvidenceV3>,
}
impl RemoteEventUpdateSnapshotV3 {
    pub fn operation_id(&self) -> &str {
        self.captured.operation_id()
    }
    pub fn before_time(&self) -> UtcEventTimePair {
        self.before
    }
    pub fn after_time(&self) -> UtcEventTimePair {
        self.after
    }
    pub fn time_alarm_evidence(&self) -> Option<&TimeAlarmEvidenceV3> {
        self.alarms.as_ref()
    }
}
impl fmt::Debug for RemoteEventUpdateSnapshotV3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteEventUpdateSnapshotV3(<redacted>)")
    }
}

/// One deterministic span edit plan for text and the atomic UTC pair. Unselected
/// bytes, supported original time headers, stamps, sequence and VALARM survive.
pub fn patch_single_utc_event_v3(
    raw: &[u8],
    fields: &RemoteEventUpdateFieldsV3,
) -> Result<EventUpdatePatchV3, Error> {
    let fields = fields.clone().validate()?;
    let base = qualify_supported(raw)?;
    let before = UtcEventTimePair {
        start_at_utc_ms: base.start.time,
        end_at_utc_ms: base.end.time,
    };
    let after = fields.time.unwrap_or(before);
    let time_alarm_evidence = if fields.time.is_some() {
        validate_selected_alarm_utc(raw)?;
        Some(TimeAlarmEvidenceV3 {
            before,
            after,
            before_fires: alarm_fires(
                raw,
                &base.encoding,
                before.start_at_utc_ms,
                before.end_at_utc_ms,
            )?,
            after_fires: alarm_fires(
                raw,
                &base.encoding,
                after.start_at_utc_ms,
                after.end_at_utc_ms,
            )?,
        })
    } else {
        None
    };
    let mut edits = Vec::new();
    let mut insertion = Vec::new();
    if let Some(title) = fields.title
        && title != base.text.summary.value
    {
        edits.push(Edit {
            start: base.text.summary.start,
            end: base.text.summary.end,
            replacement: fold_text_property_v2("SUMMARY", &title)?,
        });
    }
    for (name, action, span) in [
        (
            "DESCRIPTION",
            fields.description,
            base.text.description.as_ref(),
        ),
        ("LOCATION", fields.location, base.text.location.as_ref()),
    ] {
        match (action, span) {
            (None, _) | (Some(RemoteOptionalTextUpdateV2::Clear), None) => {}
            (Some(RemoteOptionalTextUpdateV2::Clear), Some(span)) => edits.push(Edit {
                start: span.start,
                end: span.end,
                replacement: Vec::new(),
            }),
            (Some(RemoteOptionalTextUpdateV2::Set(value)), Some(span)) if value == span.value => {}
            (Some(RemoteOptionalTextUpdateV2::Set(value)), Some(span)) => edits.push(Edit {
                start: span.start,
                end: span.end,
                replacement: fold_text_property_v2(name, &value)?,
            }),
            (Some(RemoteOptionalTextUpdateV2::Set(value)), None) => {
                insertion.extend(fold_text_property_v2(name, &value)?)
            }
        }
    }
    if !insertion.is_empty() {
        edits.push(Edit {
            start: base.text.event_end,
            end: base.text.event_end,
            replacement: insertion,
        });
    }
    if fields.time.is_some() {
        for (span, time) in [
            (&base.start, after.start_at_utc_ms),
            (&base.end, after.end_at_utc_ms),
        ] {
            if span.time != time {
                edits.push(Edit {
                    start: span.value_start,
                    end: span.value_end,
                    replacement: format_time_value(time, &base.encoding)?,
                });
            }
        }
    }
    if edits.is_empty() {
        return Err(Error::UnsafeCalendarData);
    }
    edits.sort_by_key(|e| e.start);
    let mut body = Vec::new();
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor {
            return Err(Error::UnsafeCalendarData);
        }
        body.extend_from_slice(&raw[cursor..edit.start]);
        body.extend(edit.replacement);
        cursor = edit.end;
    }
    body.extend_from_slice(&raw[cursor..]);
    if body.len() > MAX_EVENT_UPDATE_BODY_BYTES {
        return Err(Error::UnsafeCalendarData);
    }
    Ok(EventUpdatePatchV3 {
        raw_ics: body,
        uid: base.text.uid,
        before,
        after,
        time_alarm_evidence,
    })
}
