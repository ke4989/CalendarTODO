use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const DAY_MS: i64 = 86_400_000;
const MAX_RAW_BYTES: usize = 1024 * 1024;
const MAX_UNFOLDED_LINES: usize = 10_000;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_TEXT_CHARS: usize = 16_384;
const MAX_WINDOW_MS: i64 = 5 * 366 * DAY_MS;
const MAX_OCCURRENCES: usize = 512;
const MAX_GENERATED_CANDIDATES: usize = 100_000;
const MAX_SCANNED_DAYS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceValueKind {
    Date,
    UtcDateTime,
    TzidDateTime,
    FloatingDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceIdentity {
    pub canonical_key: String,
    pub original_value: String,
    pub value_kind: RecurrenceValueKind,
    pub timezone_context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpansionWindow {
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
    pub max_occurrences: usize,
}

impl ExpansionWindow {
    pub fn validate(self) -> Result<Self, RecurrenceError> {
        let span = self
            .end_at_utc_ms
            .checked_sub(self.start_at_utc_ms)
            .ok_or(RecurrenceError::InvalidWindow)?;
        if span <= 0
            || span > MAX_WINDOW_MS
            || !(1..=MAX_OCCURRENCES).contains(&self.max_occurrences)
        {
            return Err(RecurrenceError::InvalidWindow);
        }
        Ok(self)
    }
}

/// Policy that derives the rebuildable projection window from the current time.
///
/// The horizon is the only place that decides how far before and after "now" a
/// projection extends. It is validated at construction, so a stored horizon can
/// never select an unbounded or reversed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecurrenceHorizon {
    past_ms: i64,
    future_ms: i64,
}

impl Default for RecurrenceHorizon {
    fn default() -> Self {
        Self::standard()
    }
}

impl RecurrenceHorizon {
    /// The 90-day past / 400-day future window accepted by ADR 0006.
    pub const fn standard() -> Self {
        Self {
            past_ms: 90 * DAY_MS,
            future_ms: 400 * DAY_MS,
        }
    }

    pub fn new(past_ms: i64, future_ms: i64) -> Result<Self, RecurrenceError> {
        Self { past_ms, future_ms }.validate()
    }

    pub const fn past_ms(self) -> i64 {
        self.past_ms
    }

    pub const fn future_ms(self) -> i64 {
        self.future_ms
    }

    pub fn validate(self) -> Result<Self, RecurrenceError> {
        let span = self
            .past_ms
            .checked_add(self.future_ms)
            .ok_or(RecurrenceError::InvalidWindow)?;
        if self.past_ms < 0 || self.future_ms <= 0 || span > MAX_WINDOW_MS {
            return Err(RecurrenceError::InvalidWindow);
        }
        Ok(self)
    }

    pub fn window(self, now_ms: i64) -> Result<ExpansionWindow, RecurrenceError> {
        ExpansionWindow {
            start_at_utc_ms: now_ms.saturating_sub(self.past_ms),
            end_at_utc_ms: now_ms.saturating_add(self.future_ms),
            max_occurrences: MAX_OCCURRENCES,
        }
        .validate()
    }

    /// Next UTC midnight strictly after `now_ms`.
    ///
    /// This is the earliest instant at which a day-granularity horizon can admit
    /// or drop an occurrence, so it is the deadline a runtime should arm instead
    /// of polling.
    pub fn next_boundary_at_utc_ms(self, now_ms: i64) -> Result<i64, RecurrenceError> {
        now_ms
            .div_euclid(DAY_MS)
            .checked_add(1)
            .and_then(|days| days.checked_mul(DAY_MS))
            .ok_or(RecurrenceError::InvalidWindow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedOccurrence {
    pub identity: RecurrenceIdentity,
    pub effective_start_at_utc_ms: i64,
    pub effective_end_at_utc_ms: i64,
    pub all_day: bool,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceExpansion {
    pub preserved_raw_ics: Vec<u8>,
    pub occurrences: Vec<ExpandedOccurrence>,
    pub window: ExpansionWindow,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RecurrenceError {
    #[error("recurrence input structure is invalid")]
    InvalidStructure,
    #[error("recurrence data is invalid or ambiguous")]
    InvalidData,
    #[error("recurrence time semantics are unsupported")]
    UnsupportedTime,
    #[error("recurrence rule is unsupported")]
    UnsupportedRule,
    #[error("recurrence expansion window is invalid")]
    InvalidWindow,
    #[error("recurrence expansion exceeded its safety limit")]
    ExpansionLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedKind {
    Date,
    UtcDateTime,
    TzidDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TemporalValue {
    kind: SupportedKind,
    original: String,
    timezone: Option<String>,
    timestamp_ms: i64,
}

#[derive(Debug, Default)]
struct EventDefinition {
    uid: Option<String>,
    summary: Option<String>,
    notes: Option<String>,
    location: Option<String>,
    start: Option<TemporalValue>,
    end: Option<TemporalValue>,
    duration_ms: Option<i64>,
    recurrence_id: Option<(RecurrenceIdentity, TemporalValue)>,
    rrule: Option<String>,
    rdates: Vec<TemporalValue>,
    exdates: Vec<TemporalValue>,
    cancelled: bool,
    status_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frequency {
    Daily,
    Weekly,
}

#[derive(Debug)]
struct ParsedRule {
    frequency: Frequency,
    interval: u32,
    count: Option<u32>,
    until: Option<TemporalValue>,
    by_days: Vec<u8>,
    week_start: u8,
}

#[derive(Debug)]
struct Slot {
    identity: RecurrenceIdentity,
    effective_start_at_utc_ms: i64,
    effective_end_at_utc_ms: i64,
    title: String,
    notes: Option<String>,
    location: Option<String>,
    cancelled: bool,
}

pub fn canonical_recurrence_identity(
    params: &[&str],
    value: &str,
) -> Result<RecurrenceIdentity, RecurrenceError> {
    if !value.is_ascii() {
        return Err(RecurrenceError::InvalidData);
    }
    let mut date_value = false;
    let mut tzid = None;
    for param in params {
        if param.eq_ignore_ascii_case("VALUE=DATE") {
            if date_value {
                return Err(RecurrenceError::InvalidData);
            }
            date_value = true;
        } else if let Some((name, value)) = param.split_once('=')
            && name.eq_ignore_ascii_case("TZID")
        {
            if value.is_empty()
                || value.len() > 255
                || value
                    .chars()
                    .any(|character| character.is_control() || character == ':')
                || tzid.replace(value.to_owned()).is_some()
            {
                return Err(RecurrenceError::InvalidData);
            }
        } else {
            return Err(RecurrenceError::UnsupportedTime);
        }
    }
    let (value_kind, timezone_context, prefix) = if date_value || value.len() == 8 {
        if tzid.is_some() || (value.len() == 8 && !params.is_empty() && !date_value) {
            return Err(RecurrenceError::InvalidData);
        }
        parse_date(value)?;
        (RecurrenceValueKind::Date, None, "DATE")
    } else if value.len() == 16 && value.ends_with('Z') {
        if date_value || tzid.is_some() {
            return Err(RecurrenceError::InvalidData);
        }
        parse_utc(value)?;
        (RecurrenceValueKind::UtcDateTime, None, "UTC")
    } else if value.len() == 15 && &value[8..9] == "T" {
        parse_local_shape(value)?;
        if let Some(tzid) = tzid {
            (
                RecurrenceValueKind::TzidDateTime,
                Some(tzid.clone()),
                "TZID",
            )
        } else {
            (RecurrenceValueKind::FloatingDateTime, None, "FLOATING")
        }
    } else {
        return Err(RecurrenceError::InvalidData);
    };
    let canonical_key = match timezone_context.as_deref() {
        Some(tzid) => format!("{prefix}:{tzid}:{value}"),
        None => format!("{prefix}:{value}"),
    };
    Ok(RecurrenceIdentity {
        canonical_key,
        original_value: value.to_owned(),
        value_kind,
        timezone_context,
    })
}

pub fn expand_vevent_recurrence(
    raw: &[u8],
    expected_uid: &str,
    window: ExpansionWindow,
) -> Result<RecurrenceExpansion, RecurrenceError> {
    let window = window.validate()?;
    if raw.len() > MAX_RAW_BYTES {
        return Err(RecurrenceError::ExpansionLimit);
    }
    if raw.is_empty() || expected_uid.is_empty() || expected_uid.len() > 4_096 {
        return Err(RecurrenceError::InvalidData);
    }
    let events = parse_events(raw)?;
    let mut masters = events.iter().filter(|event| event.recurrence_id.is_none());
    let master = masters.next().ok_or(RecurrenceError::InvalidStructure)?;
    if masters.next().is_some() {
        return Err(RecurrenceError::InvalidStructure);
    }
    validate_uid(master, expected_uid)?;
    let master_start = master.start.as_ref().ok_or(RecurrenceError::InvalidData)?;
    let duration_ms = event_duration(master, None)?;
    let rule = master
        .rrule
        .as_deref()
        .map(|value| parse_rule(value, master_start.kind))
        .transpose()?;

    let mut maximum_generation_ms = window.end_at_utc_ms;
    for event in events.iter().filter(|event| event.recurrence_id.is_some()) {
        validate_uid(event, expected_uid)?;
        let (_, recurrence_id) = event.recurrence_id.as_ref().expect("filtered above");
        ensure_same_kind(master_start, recurrence_id)?;
        maximum_generation_ms = maximum_generation_ms.max(
            recurrence_id
                .timestamp_ms
                .checked_add(1)
                .ok_or(RecurrenceError::InvalidData)?,
        );
    }

    let mut slots = BTreeMap::<i64, Slot>::new();
    let master_identity = identity_for_supported(master_start);
    insert_slot(
        &mut slots,
        master_start.timestamp_ms,
        Slot::from_event(
            master,
            master_identity,
            duration_ms,
            master_start.timestamp_ms,
        )?,
    )?;
    if let Some(rule) = &rule {
        generate_rule_slots(
            &mut slots,
            master,
            master_start,
            duration_ms,
            rule,
            maximum_generation_ms,
        )?;
    }
    for rdate in &master.rdates {
        ensure_same_kind(master_start, rdate)?;
        insert_slot(
            &mut slots,
            rdate.timestamp_ms,
            Slot::from_event(
                master,
                identity_for_supported(rdate),
                duration_ms,
                rdate.timestamp_ms,
            )?,
        )?;
    }

    let mut excluded = BTreeSet::new();
    for exdate in &master.exdates {
        if !(master_start.kind == SupportedKind::TzidDateTime
            && exdate.kind == SupportedKind::UtcDateTime)
        {
            ensure_same_kind(master_start, exdate)?;
        }
        if !excluded.insert(exdate.timestamp_ms) {
            return Err(RecurrenceError::InvalidData);
        }
        slots.remove(&exdate.timestamp_ms);
    }

    let mut exception_ids = BTreeSet::new();
    for exception in events.iter().filter(|event| event.recurrence_id.is_some()) {
        if exception.rrule.is_some()
            || !exception.rdates.is_empty()
            || !exception.exdates.is_empty()
        {
            return Err(RecurrenceError::UnsupportedRule);
        }
        let (identity, recurrence_id) = exception
            .recurrence_id
            .as_ref()
            .ok_or(RecurrenceError::InvalidData)?;
        if !exception_ids.insert(identity.canonical_key.clone())
            || excluded.contains(&recurrence_id.timestamp_ms)
            || !slots.contains_key(&recurrence_id.timestamp_ms)
        {
            return Err(RecurrenceError::InvalidData);
        }
        let effective_start = exception
            .start
            .as_ref()
            .ok_or(RecurrenceError::InvalidData)?;
        ensure_same_kind(master_start, effective_start)?;
        let exception_duration = event_duration(exception, Some(duration_ms))?;
        slots.insert(
            recurrence_id.timestamp_ms,
            Slot::from_exception(
                exception,
                master,
                identity.clone(),
                exception_duration,
                effective_start.timestamp_ms,
            )?,
        );
    }

    let mut occurrences = slots
        .into_values()
        .filter(|slot| {
            slot.effective_start_at_utc_ms >= window.start_at_utc_ms
                && slot.effective_start_at_utc_ms < window.end_at_utc_ms
        })
        .map(|slot| ExpandedOccurrence {
            identity: slot.identity,
            effective_start_at_utc_ms: slot.effective_start_at_utc_ms,
            effective_end_at_utc_ms: slot.effective_end_at_utc_ms,
            all_day: master_start.kind == SupportedKind::Date,
            title: slot.title,
            notes: slot.notes,
            location: slot.location,
            cancelled: slot.cancelled,
        })
        .collect::<Vec<_>>();
    occurrences.sort_by(|left, right| {
        left.effective_start_at_utc_ms
            .cmp(&right.effective_start_at_utc_ms)
            .then_with(|| {
                left.identity
                    .canonical_key
                    .cmp(&right.identity.canonical_key)
            })
    });
    if occurrences.len() > window.max_occurrences {
        return Err(RecurrenceError::ExpansionLimit);
    }
    Ok(RecurrenceExpansion {
        preserved_raw_ics: raw.to_vec(),
        occurrences,
        window,
    })
}

impl Slot {
    fn from_event(
        event: &EventDefinition,
        identity: RecurrenceIdentity,
        duration_ms: i64,
        effective_start_at_utc_ms: i64,
    ) -> Result<Self, RecurrenceError> {
        Ok(Self {
            identity,
            effective_start_at_utc_ms,
            effective_end_at_utc_ms: effective_start_at_utc_ms
                .checked_add(duration_ms)
                .ok_or(RecurrenceError::InvalidData)?,
            title: event
                .summary
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "无标题事件".into()),
            notes: event.notes.clone().filter(|value| !value.is_empty()),
            location: event.location.clone().filter(|value| !value.is_empty()),
            cancelled: event.cancelled,
        })
    }

    fn from_exception(
        exception: &EventDefinition,
        master: &EventDefinition,
        identity: RecurrenceIdentity,
        duration_ms: i64,
        effective_start_at_utc_ms: i64,
    ) -> Result<Self, RecurrenceError> {
        let mut slot =
            Self::from_event(exception, identity, duration_ms, effective_start_at_utc_ms)?;
        slot.title = exception
            .summary
            .clone()
            .or_else(|| master.summary.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "无标题事件".into());
        slot.notes = exception.notes.clone().or_else(|| master.notes.clone());
        slot.location = exception
            .location
            .clone()
            .or_else(|| master.location.clone());
        slot.cancelled = exception.cancelled || master.cancelled;
        Ok(slot)
    }
}

fn parse_events(raw: &[u8]) -> Result<Vec<EventDefinition>, RecurrenceError> {
    let text = std::str::from_utf8(raw).map_err(|_| RecurrenceError::InvalidData)?;
    let lines = unfold(text)?;
    let timezones = crate::domain::calendar_event::declared_timezones(&lines).map_err(
        |reason| match reason {
            crate::domain::calendar_event::UnsupportedReason::InvalidStructure => {
                RecurrenceError::InvalidStructure
            }
            _ => RecurrenceError::UnsupportedTime,
        },
    )?;
    let mut stack = Vec::<String>::new();
    let mut calendar_count = 0_u8;
    let mut current = None;
    let mut events = Vec::new();
    for line in lines {
        let (head, value) = line
            .split_once(':')
            .ok_or(RecurrenceError::InvalidStructure)?;
        let mut parts = head.split(';');
        let name = parts
            .next()
            .ok_or(RecurrenceError::InvalidStructure)?
            .to_ascii_uppercase();
        let params = parts.collect::<Vec<_>>();
        if name == "BEGIN" {
            if !params.is_empty() {
                return Err(RecurrenceError::InvalidStructure);
            }
            let component = value.to_ascii_uppercase();
            match component.as_str() {
                "VCALENDAR" => {
                    if !stack.is_empty() {
                        return Err(RecurrenceError::InvalidStructure);
                    }
                    calendar_count = calendar_count.saturating_add(1);
                }
                "VEVENT" => {
                    if stack.last().map(String::as_str) != Some("VCALENDAR") || current.is_some() {
                        return Err(RecurrenceError::InvalidStructure);
                    }
                    current = Some(EventDefinition::default());
                }
                "VTIMEZONE" if stack.last().map(String::as_str) == Some("VCALENDAR") => {}
                "VTIMEZONE" => return Err(RecurrenceError::InvalidStructure),
                _ if stack.last().map(String::as_str) == Some("VCALENDAR") => {
                    return Err(RecurrenceError::InvalidStructure);
                }
                _ => {}
            }
            stack.push(component);
            continue;
        }
        if name == "END" {
            if !params.is_empty() {
                return Err(RecurrenceError::InvalidStructure);
            }
            let component = value.to_ascii_uppercase();
            if stack.pop().as_deref() != Some(component.as_str()) {
                return Err(RecurrenceError::InvalidStructure);
            }
            if component == "VEVENT" {
                events.push(current.take().ok_or(RecurrenceError::InvalidStructure)?);
            }
            continue;
        }
        if stack.last().map(String::as_str) != Some("VEVENT") {
            continue;
        }
        let event = current.as_mut().ok_or(RecurrenceError::InvalidStructure)?;
        match name.as_str() {
            "UID" => set_once(&mut event.uid, parse_text(&params, value)?)?,
            "SUMMARY" => set_once(&mut event.summary, parse_text(&params, value)?)?,
            "DESCRIPTION" => set_once(&mut event.notes, parse_text(&params, value)?)?,
            "LOCATION" => set_once(&mut event.location, parse_text(&params, value)?)?,
            "DTSTART" => set_once(
                &mut event.start,
                parse_supported_time(&params, value, &timezones)?,
            )?,
            "DTEND" => set_once(
                &mut event.end,
                parse_supported_time(&params, value, &timezones)?,
            )?,
            "DURATION" => {
                ensure_no_params(&params)?;
                set_once(&mut event.duration_ms, parse_event_duration(value)?)?;
            }
            "RECURRENCE-ID" => {
                if params
                    .iter()
                    .any(|param| param.eq_ignore_ascii_case("RANGE=THISANDFUTURE"))
                {
                    return Err(RecurrenceError::UnsupportedRule);
                }
                let identity = canonical_recurrence_identity(&params, value)?;
                let temporal = parse_supported_time(&params, value, &timezones)?;
                set_once(&mut event.recurrence_id, (identity, temporal))?;
            }
            "RRULE" => {
                ensure_no_params(&params)?;
                set_once(&mut event.rrule, value.to_owned())?;
            }
            "RDATE" => event
                .rdates
                .extend(parse_temporal_list(&params, value, &timezones)?),
            "EXDATE" => event
                .exdates
                .extend(parse_temporal_list(&params, value, &timezones)?),
            "STATUS" => {
                ensure_no_params(&params)?;
                if event.status_seen {
                    return Err(RecurrenceError::InvalidData);
                }
                event.status_seen = true;
                event.cancelled = value.eq_ignore_ascii_case("CANCELLED");
            }
            _ => {}
        }
    }
    if !stack.is_empty() || calendar_count != 1 || current.is_some() || events.is_empty() {
        return Err(RecurrenceError::InvalidStructure);
    }
    Ok(events)
}

fn unfold(text: &str) -> Result<Vec<String>, RecurrenceError> {
    let mut output = Vec::<String>::new();
    for raw_line in text.replace("\r\n", "\n").split('\n') {
        if raw_line.is_empty() {
            continue;
        }
        if raw_line.starts_with([' ', '\t']) {
            let prior = output.last_mut().ok_or(RecurrenceError::InvalidStructure)?;
            prior.push_str(&raw_line[1..]);
            if prior.len() > MAX_LINE_BYTES {
                return Err(RecurrenceError::ExpansionLimit);
            }
        } else {
            if output.len() >= MAX_UNFOLDED_LINES || raw_line.len() > MAX_LINE_BYTES {
                return Err(RecurrenceError::ExpansionLimit);
            }
            output.push(raw_line.trim_end_matches('\r').to_owned());
        }
    }
    Ok(output)
}

fn parse_text(params: &[&str], value: &str) -> Result<String, RecurrenceError> {
    ensure_no_params(params)?;
    let mut decoded = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
        } else {
            match chars.next() {
                Some('n' | 'N') => decoded.push('\n'),
                Some('\\') => decoded.push('\\'),
                Some(';') => decoded.push(';'),
                Some(',') => decoded.push(','),
                _ => return Err(RecurrenceError::InvalidData),
            }
        }
        if decoded.chars().count() > MAX_TEXT_CHARS {
            return Err(RecurrenceError::ExpansionLimit);
        }
    }
    Ok(decoded)
}

fn parse_temporal_list(
    params: &[&str],
    value: &str,
    timezones: &BTreeSet<String>,
) -> Result<Vec<TemporalValue>, RecurrenceError> {
    if value.is_empty() {
        return Err(RecurrenceError::InvalidData);
    }
    value
        .split(',')
        .map(|item| parse_supported_time(params, item, timezones))
        .collect()
}

fn parse_supported_time(
    params: &[&str],
    value: &str,
    timezones: &BTreeSet<String>,
) -> Result<TemporalValue, RecurrenceError> {
    let identity = canonical_recurrence_identity(params, value)?;
    let (kind, timestamp_ms) = match identity.value_kind {
        RecurrenceValueKind::Date => (SupportedKind::Date, parse_date(value)?),
        RecurrenceValueKind::UtcDateTime => (SupportedKind::UtcDateTime, parse_utc(value)?),
        RecurrenceValueKind::TzidDateTime => {
            let zone = identity
                .timezone_context
                .as_deref()
                .ok_or(RecurrenceError::UnsupportedTime)?;
            if zone.len() > 128 || !timezones.contains(zone) {
                return Err(RecurrenceError::UnsupportedTime);
            }
            (SupportedKind::TzidDateTime, resolve_local(zone, value)?)
        }
        RecurrenceValueKind::FloatingDateTime => return Err(RecurrenceError::UnsupportedTime),
    };
    Ok(TemporalValue {
        kind,
        original: value.to_owned(),
        timezone: identity.timezone_context,
        timestamp_ms,
    })
}

fn parse_rule(value: &str, kind: SupportedKind) -> Result<ParsedRule, RecurrenceError> {
    let mut fields = BTreeMap::<String, &str>::new();
    for part in value.split(';') {
        let (name, field_value) = part.split_once('=').ok_or(RecurrenceError::InvalidData)?;
        let name = name.to_ascii_uppercase();
        if field_value.is_empty() || fields.insert(name, field_value).is_some() {
            return Err(RecurrenceError::InvalidData);
        }
    }
    let frequency = match fields.remove("FREQ") {
        Some(value) if value.eq_ignore_ascii_case("DAILY") => Frequency::Daily,
        Some(value) if value.eq_ignore_ascii_case("WEEKLY") => Frequency::Weekly,
        Some(_) => return Err(RecurrenceError::UnsupportedRule),
        None => return Err(RecurrenceError::InvalidData),
    };
    let interval = fields
        .remove("INTERVAL")
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| RecurrenceError::InvalidData)
        })
        .transpose()?
        .unwrap_or(1);
    if !(1..=366).contains(&interval) {
        return Err(RecurrenceError::InvalidData);
    }
    let count = fields
        .remove("COUNT")
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| RecurrenceError::InvalidData)
        })
        .transpose()?;
    if count.is_some_and(|value| value == 0 || value as usize > MAX_GENERATED_CANDIDATES) {
        return Err(RecurrenceError::ExpansionLimit);
    }
    let until = fields
        .remove("UNTIL")
        .map(|value| parse_supported_time(&[], value, &BTreeSet::new()))
        .transpose()?;
    if count.is_some() && until.is_some() {
        return Err(RecurrenceError::InvalidData);
    }
    if until.as_ref().is_some_and(|value| {
        value.kind
            != if kind == SupportedKind::TzidDateTime {
                SupportedKind::UtcDateTime
            } else {
                kind
            }
    }) {
        return Err(RecurrenceError::InvalidData);
    }
    let by_days = match fields.remove("BYDAY") {
        Some(value) if frequency == Frequency::Weekly => parse_by_days(value)?,
        Some(_) => return Err(RecurrenceError::UnsupportedRule),
        None => Vec::new(),
    };
    let week_start = match fields.remove("WKST") {
        Some(value) => {
            let days = parse_by_days(value)?;
            if days.len() != 1 {
                return Err(RecurrenceError::UnsupportedRule);
            }
            days[0]
        }
        None => 0,
    };
    if !fields.is_empty() {
        return Err(RecurrenceError::UnsupportedRule);
    }
    Ok(ParsedRule {
        frequency,
        interval,
        count,
        until,
        by_days,
        week_start,
    })
}

fn parse_by_days(value: &str) -> Result<Vec<u8>, RecurrenceError> {
    let mut days = BTreeSet::new();
    for item in value.split(',') {
        let day = match item.to_ascii_uppercase().as_str() {
            "MO" => 0,
            "TU" => 1,
            "WE" => 2,
            "TH" => 3,
            "FR" => 4,
            "SA" => 5,
            "SU" => 6,
            _ => return Err(RecurrenceError::UnsupportedRule),
        };
        if !days.insert(day) {
            return Err(RecurrenceError::InvalidData);
        }
    }
    if days.is_empty() {
        return Err(RecurrenceError::InvalidData);
    }
    Ok(days.into_iter().collect())
}

fn generate_rule_slots(
    slots: &mut BTreeMap<i64, Slot>,
    master: &EventDefinition,
    start: &TemporalValue,
    duration_ms: i64,
    rule: &ParsedRule,
    generation_end_ms: i64,
) -> Result<(), RecurrenceError> {
    let candidates = match rule.frequency {
        Frequency::Daily => generate_daily(start, rule, generation_end_ms)?,
        Frequency::Weekly => generate_weekly(start, rule, generation_end_ms)?,
    };
    for candidate in candidates {
        let timestamp_ms = candidate.timestamp_ms;
        insert_slot(
            slots,
            timestamp_ms,
            Slot::from_event(
                master,
                identity_for_supported(&candidate),
                duration_ms,
                timestamp_ms,
            )?,
        )?;
    }
    Ok(())
}

fn generate_daily(
    start: &TemporalValue,
    rule: &ParsedRule,
    generation_end_ms: i64,
) -> Result<Vec<TemporalValue>, RecurrenceError> {
    let period = i64::from(rule.interval) * DAY_MS;
    let mut output = Vec::new();
    let mut nominal = nominal_timestamp(start)?;
    let cutoff = generation_cutoff(rule, generation_end_ms)?;
    let nominal_end = civil_scan_cap(start, cutoff)?;
    while nominal < nominal_end {
        if rule
            .count
            .is_some_and(|count| output.len() >= count as usize)
        {
            break;
        }
        let candidate = nominal;
        nominal = nominal
            .checked_add(period)
            .ok_or(RecurrenceError::InvalidData)?;
        let Some(current) = temporal_at_nominal(start, candidate, cutoff)? else {
            continue;
        };
        if output.len() >= MAX_GENERATED_CANDIDATES {
            return Err(RecurrenceError::ExpansionLimit);
        }
        output.push(current);
    }
    Ok(output)
}

fn generate_weekly(
    start: &TemporalValue,
    rule: &ParsedRule,
    generation_end_ms: i64,
) -> Result<Vec<TemporalValue>, RecurrenceError> {
    let nominal_start = nominal_timestamp(start)?;
    let start_day = nominal_start.div_euclid(DAY_MS);
    let time_of_day = nominal_start.rem_euclid(DAY_MS);
    let start_weekday = weekday(start_day);
    let by_days = if rule.by_days.is_empty() {
        vec![start_weekday]
    } else {
        rule.by_days.clone()
    };
    if !by_days.contains(&start_weekday) {
        return Err(RecurrenceError::InvalidData);
    }
    let week_start_day =
        start_day - (i64::from(start_weekday) - i64::from(rule.week_start)).rem_euclid(7);
    let cutoff = generation_cutoff(rule, generation_end_ms)?;
    let nominal_end = civil_scan_cap(start, cutoff)?;
    let end_day = nominal_end.div_euclid(DAY_MS).saturating_add(1);
    let scanned_days = end_day.saturating_sub(week_start_day);
    if scanned_days < 0 || scanned_days as usize > MAX_SCANNED_DAYS {
        return Err(RecurrenceError::ExpansionLimit);
    }
    let mut output = Vec::new();
    for day in week_start_day..end_day {
        if rule
            .count
            .is_some_and(|count| output.len() >= count as usize)
        {
            break;
        }
        let weeks = (day - week_start_day).div_euclid(7);
        if weeks.rem_euclid(i64::from(rule.interval)) != 0 || !by_days.contains(&weekday(day)) {
            continue;
        }
        let nominal = day
            .checked_mul(DAY_MS)
            .and_then(|value| value.checked_add(time_of_day))
            .ok_or(RecurrenceError::InvalidData)?;
        if nominal < nominal_start {
            continue;
        }
        if nominal >= nominal_end {
            break;
        }
        let Some(candidate) = temporal_at_nominal(start, nominal, cutoff)? else {
            continue;
        };
        if output.len() >= MAX_GENERATED_CANDIDATES {
            return Err(RecurrenceError::ExpansionLimit);
        }
        output.push(candidate);
    }
    Ok(output)
}

fn insert_slot(
    slots: &mut BTreeMap<i64, Slot>,
    timestamp_ms: i64,
    slot: Slot,
) -> Result<(), RecurrenceError> {
    match slots.get(&timestamp_ms) {
        Some(existing) if existing.identity.canonical_key != slot.identity.canonical_key => {
            Err(RecurrenceError::InvalidData)
        }
        Some(_) => Ok(()),
        None => {
            slots.insert(timestamp_ms, slot);
            Ok(())
        }
    }
}

fn validate_uid(event: &EventDefinition, expected_uid: &str) -> Result<(), RecurrenceError> {
    if event.uid.as_deref() == Some(expected_uid) {
        Ok(())
    } else {
        Err(RecurrenceError::InvalidData)
    }
}

fn event_duration(event: &EventDefinition, inherited: Option<i64>) -> Result<i64, RecurrenceError> {
    let start = event.start.as_ref().ok_or(RecurrenceError::InvalidData)?;
    if let Some(duration) = event.duration_ms {
        if event.end.is_some() || start.kind == SupportedKind::Date {
            return Err(RecurrenceError::InvalidData);
        }
        return Ok(duration);
    }
    match event.end.as_ref() {
        Some(end) => {
            ensure_same_kind(start, end)?;
            let duration = end
                .timestamp_ms
                .checked_sub(start.timestamp_ms)
                .ok_or(RecurrenceError::InvalidData)?;
            if duration < 0 || (start.kind == SupportedKind::Date && duration == 0) {
                return Err(RecurrenceError::InvalidData);
            }
            Ok(duration)
        }
        None => Ok(inherited.unwrap_or(if start.kind == SupportedKind::Date {
            DAY_MS
        } else {
            0
        })),
    }
}

fn ensure_same_kind(left: &TemporalValue, right: &TemporalValue) -> Result<(), RecurrenceError> {
    if left.kind == right.kind && left.timezone == right.timezone {
        Ok(())
    } else {
        Err(RecurrenceError::InvalidData)
    }
}

fn identity_for_supported(value: &TemporalValue) -> RecurrenceIdentity {
    RecurrenceIdentity {
        canonical_key: match value.kind {
            SupportedKind::Date => format!("DATE:{}", value.original),
            SupportedKind::UtcDateTime => format!("UTC:{}", value.original),
            SupportedKind::TzidDateTime => format!(
                "TZID:{}:{}",
                value.timezone.as_deref().expect("TZID temporal value"),
                value.original
            ),
        },
        original_value: value.original.clone(),
        value_kind: match value.kind {
            SupportedKind::Date => RecurrenceValueKind::Date,
            SupportedKind::UtcDateTime => RecurrenceValueKind::UtcDateTime,
            SupportedKind::TzidDateTime => RecurrenceValueKind::TzidDateTime,
        },
        timezone_context: value.timezone.clone(),
    }
}

// Civil milliseconds are used only for calendar arithmetic, never as UTC identities.
fn nominal_timestamp(value: &TemporalValue) -> Result<i64, RecurrenceError> {
    if value.kind == SupportedKind::TzidDateTime {
        parse_utc(&format!("{}Z", value.original))
    } else {
        Ok(value.timestamp_ms)
    }
}

fn generation_cutoff(rule: &ParsedRule, end_ms: i64) -> Result<i64, RecurrenceError> {
    match &rule.until {
        Some(until) => Ok(end_ms.min(
            until
                .timestamp_ms
                .checked_add(1)
                .ok_or(RecurrenceError::InvalidData)?,
        )),
        None => Ok(end_ms),
    }
}

fn civil_scan_cap(start: &TemporalValue, cutoff: i64) -> Result<i64, RecurrenceError> {
    // jiff offsets are strictly smaller than two days. This is a scan bound,
    // not a local endpoint: civil time is non-monotonic across a backward transition.
    cutoff
        .checked_add(if start.timezone.is_some() {
            2 * DAY_MS
        } else {
            0
        })
        .ok_or(RecurrenceError::InvalidData)
}

fn temporal_at_nominal(
    start: &TemporalValue,
    nominal_ms: i64,
    cutoff: i64,
) -> Result<Option<TemporalValue>, RecurrenceError> {
    let original = if start.kind == SupportedKind::TzidDateTime {
        format_timestamp(SupportedKind::UtcDateTime, nominal_ms)?
            .trim_end_matches('Z')
            .to_owned()
    } else {
        format_timestamp(start.kind, nominal_ms)?
    };
    let timestamp_ms = match start.timezone.as_deref() {
        Some(zone) => {
            let local = crate::domain::calendar_event::parse_local_datetime(&original)
                .map_err(|_| RecurrenceError::UnsupportedTime)?;
            let zone =
                jiff::tz::TimeZone::get(zone).map_err(|_| RecurrenceError::UnsupportedTime)?;
            let ambiguous = zone.to_ambiguous_timestamp(local);
            // Earlier is used only to reject outside-cutoff candidates. It never
            // selects an occurrence time: an in-range gap/fold must fail below.
            if ambiguous
                .earlier()
                .map_err(|_| RecurrenceError::UnsupportedTime)?
                .as_millisecond()
                >= cutoff
            {
                return Ok(None);
            }
            ambiguous
                .unambiguous()
                .map_err(|_| RecurrenceError::UnsupportedTime)?
                .as_millisecond()
        }
        None if nominal_ms >= cutoff => return Ok(None),
        None => nominal_ms,
    };
    Ok(Some(TemporalValue {
        kind: start.kind,
        original,
        timezone: start.timezone.clone(),
        timestamp_ms,
    }))
}

fn resolve_local(zone: &str, value: &str) -> Result<i64, RecurrenceError> {
    let local = crate::domain::calendar_event::parse_local_datetime(value)
        .map_err(|_| RecurrenceError::UnsupportedTime)?;
    let zone = jiff::tz::TimeZone::get(zone).map_err(|_| RecurrenceError::UnsupportedTime)?;
    Ok(zone
        .to_ambiguous_zoned(local)
        .unambiguous()
        .map_err(|_| RecurrenceError::UnsupportedTime)?
        .timestamp()
        .as_millisecond())
}

fn parse_event_duration(value: &str) -> Result<i64, RecurrenceError> {
    let mut rest = value
        .strip_prefix("PT")
        .ok_or(RecurrenceError::UnsupportedTime)?;
    let mut total = 0_i64;
    let mut previous = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(RecurrenceError::UnsupportedTime);
        }
        let number: i64 = rest[..digits]
            .parse()
            .map_err(|_| RecurrenceError::InvalidData)?;
        let (order, unit) = match rest.as_bytes().get(digits) {
            Some(b'H') => (1, 3_600_000),
            Some(b'M') => (2, 60_000),
            Some(b'S') => (3, 1_000),
            _ => return Err(RecurrenceError::UnsupportedTime),
        };
        if order <= previous {
            return Err(RecurrenceError::InvalidData);
        }
        previous = order;
        total = number
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or(RecurrenceError::InvalidData)?;
        rest = &rest[digits + 1..];
    }
    if total <= 0 || total > MAX_WINDOW_MS {
        return Err(RecurrenceError::UnsupportedTime);
    }
    Ok(total)
}

fn ensure_no_params(params: &[&str]) -> Result<(), RecurrenceError> {
    if params.is_empty() {
        Ok(())
    } else {
        Err(RecurrenceError::InvalidData)
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), RecurrenceError> {
    if slot.replace(value).is_some() {
        Err(RecurrenceError::InvalidData)
    } else {
        Ok(())
    }
}

fn parse_utc(value: &str) -> Result<i64, RecurrenceError> {
    if value.len() != 16 || !value.ends_with('Z') || &value[8..9] != "T" {
        return Err(RecurrenceError::InvalidData);
    }
    parse_local_shape(&value[..15])?;
    let date = parse_date(&value[..8])?;
    let hour = parse_digits(&value[9..11])?;
    let minute = parse_digits(&value[11..13])?;
    let second = parse_digits(&value[13..15])?;
    let offset = i64::from(hour * 3_600 + minute * 60 + second)
        .checked_mul(1_000)
        .ok_or(RecurrenceError::InvalidData)?;
    date.checked_add(offset).ok_or(RecurrenceError::InvalidData)
}

fn parse_local_shape(value: &str) -> Result<(), RecurrenceError> {
    if value.len() != 15
        || &value[8..9] != "T"
        || !value[..8].bytes().all(|byte| byte.is_ascii_digit())
        || !value[9..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(RecurrenceError::InvalidData);
    }
    parse_date(&value[..8])?;
    let hour = parse_digits(&value[9..11])?;
    let minute = parse_digits(&value[11..13])?;
    let second = parse_digits(&value[13..15])?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(RecurrenceError::InvalidData);
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<i64, RecurrenceError> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RecurrenceError::InvalidData);
    }
    let year = parse_digits(&value[..4])? as i32;
    let month = parse_digits(&value[4..6])?;
    let day = parse_digits(&value[6..8])?;
    if !(1600..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Err(RecurrenceError::InvalidData);
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
        return Err(RecurrenceError::InvalidData);
    }
    days_from_civil(year, month, day)
        .checked_mul(DAY_MS)
        .ok_or(RecurrenceError::InvalidData)
}

fn parse_digits(value: &str) -> Result<u32, RecurrenceError> {
    value.parse().map_err(|_| RecurrenceError::InvalidData)
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

fn civil_from_days(days: i64) -> Result<(i32, u32, u32), RecurrenceError> {
    let z = days
        .checked_add(719_468)
        .ok_or(RecurrenceError::InvalidData)?;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Ok((
        i32::try_from(year).map_err(|_| RecurrenceError::InvalidData)?,
        u32::try_from(month).map_err(|_| RecurrenceError::InvalidData)?,
        u32::try_from(day).map_err(|_| RecurrenceError::InvalidData)?,
    ))
}

pub(crate) fn format_utc_timestamp(timestamp_ms: i64) -> Result<String, RecurrenceError> {
    format_timestamp(SupportedKind::UtcDateTime, timestamp_ms)
}

fn format_timestamp(kind: SupportedKind, timestamp_ms: i64) -> Result<String, RecurrenceError> {
    if timestamp_ms.rem_euclid(1_000) != 0 {
        return Err(RecurrenceError::InvalidData);
    }
    let days = timestamp_ms.div_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days)?;
    if !(1600..=9999).contains(&year) {
        return Err(RecurrenceError::InvalidData);
    }
    let date = format!("{year:04}{month:02}{day:02}");
    if kind == SupportedKind::Date {
        if timestamp_ms.rem_euclid(DAY_MS) != 0 {
            return Err(RecurrenceError::InvalidData);
        }
        return Ok(date);
    }
    let seconds = timestamp_ms.rem_euclid(DAY_MS) / 1_000;
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    Ok(format!("{date}T{hour:02}{minute:02}{second:02}Z"))
}

fn weekday(days_since_epoch: i64) -> u8 {
    u8::try_from((days_since_epoch + 3).rem_euclid(7)).unwrap_or(0)
}

#[cfg(test)]
#[path = "calendar_recurrence_tzid_tests.rs"]
mod tzid_tests;
