use crate::domain::calendar_recurrence::canonical_recurrence_identity;
use std::collections::BTreeSet;

const MAX_RAW: usize = 1024 * 1024;
const MAX_LINES: usize = 10_000;
const MAX_LINE: usize = 16 * 1024;
const MAX_ALARMS: usize = 16;
const MAX_FIRES: usize = 64;
const MAX_REPEAT: u32 = 8;
const MAX_OFFSET_MS: i64 = 366 * 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmError {
    Invalid,
    Unsupported,
    Limit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmFire {
    pub alarm_index: usize,
    pub repeat_index: u32,
    pub fire_at_utc_ms: i64,
}

#[derive(Default)]
struct Event {
    recurrence_key: Option<String>,
    alarms: Vec<Alarm>,
}

#[derive(Default)]
struct Alarm {
    trigger: Option<Trigger>,
    action: Option<String>,
    repeat: Option<u32>,
    duration_ms: Option<i64>,
    unsupported: bool,
}

enum Trigger {
    Start(i64),
    End(i64),
    Absolute(i64),
}

/// Read-only, bounded VALARM projection. Raw ICS remains authoritative. A
/// detached VEVENT replaces the master's alarms for its original recurrence
/// slot. This codec reports only explicit alarms; notification policy is separate.
pub fn fires_for_occurrence(
    raw: &[u8],
    recurrence_key: &str,
    start: i64,
    end: i64,
) -> Result<Vec<AlarmFire>, AlarmError> {
    if raw.is_empty() || raw.len() > MAX_RAW || end < start {
        return Err(AlarmError::Invalid);
    }
    let text = std::str::from_utf8(raw).map_err(|_| AlarmError::Invalid)?;
    let mut lines: Vec<String> = Vec::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let prior = lines.last_mut().ok_or(AlarmError::Invalid)?;
            prior.push_str(&line[1..]);
            if prior.len() > MAX_LINE {
                return Err(AlarmError::Limit);
            }
        } else {
            if lines.len() >= MAX_LINES || line.len() > MAX_LINE {
                return Err(AlarmError::Limit);
            }
            lines.push(line.trim_end_matches('\r').to_owned());
        }
    }
    let mut stack: Vec<&str> = Vec::new();
    let mut events = Vec::<Event>::new();
    let mut current: Option<Event> = None;
    let mut alarm: Option<Alarm> = None;
    for line in &lines {
        let (head, value) = line.split_once(':').ok_or(AlarmError::Invalid)?;
        let mut split = head.split(';');
        let name = split.next().ok_or(AlarmError::Invalid)?;
        let params: Vec<&str> = split.collect();
        if name.eq_ignore_ascii_case("BEGIN") {
            if !params.is_empty() {
                return Err(AlarmError::Invalid);
            }
            match value {
                "VCALENDAR" if stack.is_empty() => {}
                "VEVENT" if stack.last() == Some(&"VCALENDAR") => current = Some(Event::default()),
                "VALARM" if stack.last() == Some(&"VEVENT") => alarm = Some(Alarm::default()),
                "VTIMEZONE" if stack.last() == Some(&"VCALENDAR") => {}
                "STANDARD" | "DAYLIGHT" if stack.last() == Some(&"VTIMEZONE") => {}
                _ => return Err(AlarmError::Invalid),
            }
            stack.push(value);
            continue;
        }
        if name.eq_ignore_ascii_case("END") {
            if !params.is_empty() || stack.pop() != Some(value) {
                return Err(AlarmError::Invalid);
            }
            if value == "VALARM" {
                let a = alarm.take().ok_or(AlarmError::Invalid)?;
                let event = current.as_mut().ok_or(AlarmError::Invalid)?;
                if event.alarms.len() >= MAX_ALARMS {
                    return Err(AlarmError::Limit);
                }
                event.alarms.push(a);
            } else if value == "VEVENT" {
                events.push(current.take().ok_or(AlarmError::Invalid)?);
            }
            continue;
        }
        match stack.last().copied() {
            Some("VEVENT") if name.eq_ignore_ascii_case("RECURRENCE-ID") => {
                let event = current.as_mut().ok_or(AlarmError::Invalid)?;
                if event.recurrence_key.is_some() || !value.is_ascii() {
                    return Err(AlarmError::Invalid);
                }
                event.recurrence_key = Some(
                    canonical_recurrence_identity(&params, value)
                        .map_err(|_| AlarmError::Unsupported)?
                        .canonical_key,
                );
            }
            Some("VALARM") => {
                let a = alarm.as_mut().ok_or(AlarmError::Invalid)?;
                match name.to_ascii_uppercase().as_str() {
                    "TRIGGER" => {
                        if a.trigger.is_some() {
                            return Err(AlarmError::Invalid);
                        }
                        a.trigger = Some(parse_trigger(&params, value)?);
                    }
                    "ACTION" => {
                        if !params.is_empty() || a.action.replace(value.to_owned()).is_some() {
                            return Err(AlarmError::Invalid);
                        }
                    }
                    "REPEAT" => {
                        if !params.is_empty()
                            || a.repeat
                                .replace(value.parse().map_err(|_| AlarmError::Invalid)?)
                                .is_some()
                        {
                            return Err(AlarmError::Invalid);
                        }
                    }
                    "DURATION" => {
                        if !params.is_empty()
                            || a.duration_ms
                                .replace(parse_duration(value, false)?)
                                .is_some()
                        {
                            return Err(AlarmError::Invalid);
                        }
                    }
                    "DESCRIPTION" | "SUMMARY" | "ATTENDEE" | "ATTACH" => {}
                    _ => a.unsupported = true,
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() || current.is_some() || alarm.is_some() || events.is_empty() {
        return Err(AlarmError::Invalid);
    }
    let mut master = None;
    let mut selected = None;
    let mut keys = BTreeSet::new();
    for event in &events {
        if let Some(key) = &event.recurrence_key {
            if !keys.insert(key.clone()) {
                return Err(AlarmError::Invalid);
            }
            if key == recurrence_key {
                selected = Some(event);
            }
        } else if master.replace(event).is_some() {
            return Err(AlarmError::Invalid);
        }
    }
    let event = if recurrence_key == "SINGLETON" {
        master
    } else {
        selected.or(master)
    }
    .ok_or(AlarmError::Invalid)?;
    let mut fires = Vec::new();
    for (index, a) in event.alarms.iter().enumerate() {
        if a.unsupported || a.action.as_deref() != Some("DISPLAY") {
            return Err(AlarmError::Unsupported);
        }
        let first = match a.trigger.as_ref().ok_or(AlarmError::Invalid)? {
            Trigger::Start(offset) => start.checked_add(*offset),
            Trigger::End(offset) => end.checked_add(*offset),
            Trigger::Absolute(at) => Some(*at),
        }
        .ok_or(AlarmError::Invalid)?;
        let repeats = match (a.repeat, a.duration_ms) {
            (None, None) => 0,
            (Some(count), Some(duration)) if count <= MAX_REPEAT && duration > 0 => count,
            _ => return Err(AlarmError::Unsupported),
        };
        for repeat_index in 0..=repeats {
            if fires.len() >= MAX_FIRES {
                return Err(AlarmError::Limit);
            }
            let offset = i64::from(repeat_index)
                .checked_mul(a.duration_ms.unwrap_or(0))
                .ok_or(AlarmError::Invalid)?;
            fires.push(AlarmFire {
                alarm_index: index,
                repeat_index,
                fire_at_utc_ms: first.checked_add(offset).ok_or(AlarmError::Invalid)?,
            });
        }
    }
    Ok(fires)
}

/// Notification policy only: a safely parsed occurrence without VALARM gets one
/// start reminder. Invalid or unsupported alarms still suppress the whole set.
/// Mutation proof continues to use the explicit-alarm codec above.
pub fn notification_fires_for_occurrence(
    raw: &[u8],
    recurrence_key: &str,
    start: i64,
    end: i64,
) -> Result<Vec<AlarmFire>, AlarmError> {
    let mut fires = fires_for_occurrence(raw, recurrence_key, start, end)?;
    if fires.is_empty() {
        fires.push(AlarmFire {
            alarm_index: 0,
            repeat_index: 0,
            fire_at_utc_ms: start,
        });
    }
    Ok(fires)
}

fn parse_trigger(params: &[&str], value: &str) -> Result<Trigger, AlarmError> {
    if params.len() == 1 && params[0].eq_ignore_ascii_case("VALUE=DATE-TIME") {
        return Ok(Trigger::Absolute(parse_utc(value)?));
    }
    let offset = parse_duration(value, true)?;
    match params {
        [] => Ok(Trigger::Start(offset)),
        [related] if related.eq_ignore_ascii_case("RELATED=END") => Ok(Trigger::End(offset)),
        [related] if related.eq_ignore_ascii_case("RELATED=START") => Ok(Trigger::Start(offset)),
        _ => Err(AlarmError::Unsupported),
    }
}

fn parse_duration(value: &str, signed: bool) -> Result<i64, AlarmError> {
    let (negative, body) = if let Some(rest) = value.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix('+') {
        (false, rest)
    } else {
        (false, value)
    };
    if negative && !signed {
        return Err(AlarmError::Unsupported);
    }
    let body = body.strip_prefix('P').ok_or(AlarmError::Unsupported)?;
    let (digits, scale) = if let Some(rest) = body.strip_prefix('T') {
        if let Some(n) = rest.strip_suffix('H') {
            (n, 3_600_000)
        } else if let Some(n) = rest.strip_suffix('M') {
            (n, 60_000)
        } else if let Some(n) = rest.strip_suffix('S') {
            (n, 1_000)
        } else {
            return Err(AlarmError::Unsupported);
        }
    } else if let Some(n) = body.strip_suffix('D') {
        (n, 86_400_000)
    } else {
        return Err(AlarmError::Unsupported);
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AlarmError::Unsupported);
    }
    let count: i64 = digits.parse().map_err(|_| AlarmError::Limit)?;
    let total = count.checked_mul(scale).ok_or(AlarmError::Limit)?;
    if total > MAX_OFFSET_MS {
        return Err(AlarmError::Unsupported);
    }
    Ok(if negative { -total } else { total })
}

pub(super) fn parse_utc(value: &str) -> Result<i64, AlarmError> {
    if value.len() != 16
        || !value.is_ascii()
        || !value.ends_with('Z')
        || value.as_bytes()[8] != b'T'
    {
        return Err(AlarmError::Unsupported);
    }
    let year: i32 = value[0..4].parse().map_err(|_| AlarmError::Invalid)?;
    let month: u32 = value[4..6].parse().map_err(|_| AlarmError::Invalid)?;
    let day: u32 = value[6..8].parse().map_err(|_| AlarmError::Invalid)?;
    let hour: u32 = value[9..11].parse().map_err(|_| AlarmError::Invalid)?;
    let minute: u32 = value[11..13].parse().map_err(|_| AlarmError::Invalid)?;
    let second: u32 = value[13..15].parse().map_err(|_| AlarmError::Invalid)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(AlarmError::Invalid),
    };
    if year < 1970 || day == 0 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return Err(AlarmError::Invalid);
    }
    let days = days_from_civil(year, month, day);
    days.checked_mul(86_400_000)
        .and_then(|v| v.checked_add(i64::from(hour * 3_600 + minute * 60 + second) * 1000))
        .ok_or(AlarmError::Invalid)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
