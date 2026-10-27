use jiff::{Timestamp, civil::DateTime};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_RECURRENCE_OCCURRENCES: u16 = 100;
pub const MAX_RECURRENCE_YEARS: i16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceFrequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    CustomWeekly,
    CustomMonthly,
    CustomYearly,
}

impl RecurrenceFrequency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
            Self::CustomWeekly => "custom_weekly",
            Self::CustomMonthly => "custom_monthly",
            Self::CustomYearly => "custom_yearly",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, crate::domain::local_todo::LocalTodoError> {
        match value {
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "monthly" => Ok(Self::Monthly),
            "yearly" => Ok(Self::Yearly),
            "custom_weekly" => Ok(Self::CustomWeekly),
            "custom_monthly" => Ok(Self::CustomMonthly),
            "custom_yearly" => Ok(Self::CustomYearly),
            _ => Err(crate::domain::local_todo::LocalTodoError::InvalidRecurrence),
        }
    }

    pub fn period_ms(
        self,
        interval: u32,
    ) -> Result<i64, crate::domain::local_todo::LocalTodoError> {
        let days = match self {
            Self::Daily => 1_i64,
            Self::Weekly => 7_i64,
            _ => return Err(crate::domain::local_todo::LocalTodoError::InvalidRecurrence),
        };
        days.checked_mul(i64::from(interval))
            .and_then(|value| value.checked_mul(86_400_000))
            .ok_or(crate::domain::local_todo::LocalTodoError::InvalidRecurrence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RecurrenceEndKind {
    Count,
    Until,
}

impl RecurrenceEndKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Until => "until",
        }
    }
    pub fn from_storage(value: &str) -> Result<Self, RecurrenceGenerationError> {
        match value {
            "count" => Ok(Self::Count),
            "until" => Ok(Self::Until),
            _ => Err(RecurrenceGenerationError::InvalidRule),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecurrenceOptions {
    pub timezone: String,
    #[serde(default)]
    pub weekdays: Vec<u8>,
    #[serde(default)]
    pub month_days: Vec<u8>,
    #[serde(default)]
    pub year_months: Vec<u8>,
    pub end_kind: RecurrenceEndKind,
    pub end_count: Option<u16>,
    pub end_until_at_utc_ms: Option<i64>,
}

impl Default for RecurrenceOptions {
    fn default() -> Self {
        Self {
            timezone: "UTC".into(),
            weekdays: vec![],
            month_days: vec![],
            year_months: vec![],
            end_kind: RecurrenceEndKind::Count,
            end_count: Some(MAX_RECURRENCE_OCCURRENCES),
            end_until_at_utc_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneratedOccurrence {
    pub nominal_start_at_utc_ms: Option<i64>,
    pub nominal_due_at_utc_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RecurrenceGenerationError {
    #[error("invalid recurrence rule")]
    InvalidRule,
    #[error("recurrence is outside the supported range")]
    OutOfRange,
}

pub fn validate_options(
    frequency: RecurrenceFrequency,
    interval: u32,
    options: &RecurrenceOptions,
    anchor_due_at_utc_ms: i64,
) -> Result<(), RecurrenceGenerationError> {
    validate_rule_shape(frequency, interval, options)?;
    let anchor = Timestamp::from_millisecond(anchor_due_at_utc_ms)
        .map_err(|_| RecurrenceGenerationError::OutOfRange)?
        .in_tz(&options.timezone)
        .map_err(|_| RecurrenceGenerationError::InvalidRule)?;
    let limit = recurrence_limit_ms(&anchor, &options.timezone)?;
    if let Some(until) = options.end_until_at_utc_ms {
        if until < anchor_due_at_utc_ms || until > limit {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
    }
    Ok(())
}

fn recurrence_limit_ms(
    anchor: &jiff::Zoned,
    timezone: &str,
) -> Result<i64, RecurrenceGenerationError> {
    let target_year = anchor
        .year()
        .checked_add(MAX_RECURRENCE_YEARS)
        .ok_or(RecurrenceGenerationError::OutOfRange)?;
    (1..=anchor.day())
        .rev()
        .find_map(|day| {
            DateTime::new(
                target_year,
                anchor.month(),
                day,
                anchor.hour(),
                anchor.minute(),
                anchor.second(),
                anchor.subsec_nanosecond(),
            )
            .ok()?
            .in_tz(timezone)
            .ok()
            .map(|value| value.timestamp().as_millisecond())
        })
        .ok_or(RecurrenceGenerationError::OutOfRange)
}

pub fn validate_rule_shape(
    frequency: RecurrenceFrequency,
    interval: u32,
    options: &RecurrenceOptions,
) -> Result<(), RecurrenceGenerationError> {
    if interval == 0
        || interval > 365
        || options.timezone.is_empty()
        || options.timezone.len() > 128
        || jiff::tz::TimeZone::get(&options.timezone).is_err()
    {
        return Err(RecurrenceGenerationError::InvalidRule);
    }
    match frequency {
        RecurrenceFrequency::CustomWeekly
            if options.weekdays.is_empty()
                || options.weekdays.len() > 7
                || options.weekdays.iter().any(|day| *day > 6) =>
        {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
        RecurrenceFrequency::CustomMonthly
            if options.month_days.is_empty()
                || options.month_days.len() > 31
                || options.month_days.iter().any(|day| !(1..=31).contains(day)) =>
        {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
        RecurrenceFrequency::CustomYearly
            if options.year_months.is_empty()
                || options.year_months.len() > 12
                || options
                    .year_months
                    .iter()
                    .any(|month| !(1..=12).contains(month)) =>
        {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
        _ => {}
    }
    match options.end_kind {
        RecurrenceEndKind::Count
            if !matches!(options.end_count, Some(1..=MAX_RECURRENCE_OCCURRENCES))
                || options.end_until_at_utc_ms.is_some() =>
        {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
        RecurrenceEndKind::Until
            if options.end_count.is_some() || options.end_until_at_utc_ms.is_none() =>
        {
            return Err(RecurrenceGenerationError::InvalidRule);
        }
        _ => {}
    }
    Ok(())
}

pub fn generate_occurrences(
    frequency: RecurrenceFrequency,
    interval: u32,
    options: &RecurrenceOptions,
    anchor_start_at_utc_ms: Option<i64>,
    anchor_due_at_utc_ms: i64,
) -> Result<Vec<GeneratedOccurrence>, RecurrenceGenerationError> {
    validate_options(frequency, interval, options, anchor_due_at_utc_ms)?;
    let anchor = Timestamp::from_millisecond(anchor_due_at_utc_ms)
        .map_err(|_| RecurrenceGenerationError::OutOfRange)?
        .in_tz(&options.timezone)
        .map_err(|_| RecurrenceGenerationError::InvalidRule)?;
    let horizon = recurrence_limit_ms(&anchor, &options.timezone)?;
    let duration = anchor_start_at_utc_ms.map(|start| start - anchor_due_at_utc_ms);
    let count_limit = options.end_count.unwrap_or(MAX_RECURRENCE_OCCURRENCES) as usize;
    let until = options.end_until_at_utc_ms.unwrap_or(i64::MAX);
    let mut values: Vec<GeneratedOccurrence> = Vec::with_capacity(count_limit);
    let max_days = 366_i64 * i64::from(MAX_RECURRENCE_YEARS) + 2;
    for day_offset in 0..=max_days {
        if values.len() >= count_limit {
            break;
        }
        let due = match frequency {
            RecurrenceFrequency::Daily | RecurrenceFrequency::Weekly => {
                let period = frequency
                    .period_ms(interval)
                    .map_err(|_| RecurrenceGenerationError::InvalidRule)?;
                let step = if frequency == RecurrenceFrequency::Daily {
                    day_offset
                } else if day_offset % 7 == 0 {
                    day_offset / 7
                } else {
                    continue;
                };
                if step % i64::from(interval) != 0 {
                    continue;
                }
                anchor_due_at_utc_ms
                    .checked_add(
                        step.checked_mul(period / i64::from(interval))
                            .ok_or(RecurrenceGenerationError::OutOfRange)?,
                    )
                    .ok_or(RecurrenceGenerationError::OutOfRange)?
            }
            _ => {
                let date = anchor
                    .date()
                    .checked_add(jiff::Span::new().days(day_offset))
                    .map_err(|_| RecurrenceGenerationError::OutOfRange)?;
                let months = i64::from(date.year() - anchor.year()) * 12
                    + i64::from(date.month() - anchor.month());
                let matches = day_offset == 0
                    || match frequency {
                        RecurrenceFrequency::Monthly => {
                            months >= 0
                                && months % i64::from(interval) == 0
                                && date.day() == anchor.day()
                        }
                        RecurrenceFrequency::Yearly => {
                            date.year() >= anchor.year()
                                && (date.year() - anchor.year()) % interval as i16 == 0
                                && date.month() == anchor.month()
                                && date.day() == anchor.day()
                        }
                        RecurrenceFrequency::CustomWeekly => {
                            day_offset / 7 % i64::from(interval) == 0
                                && options
                                    .weekdays
                                    .contains(&(date.weekday().to_monday_zero_offset() as u8))
                        }
                        RecurrenceFrequency::CustomMonthly => {
                            months >= 0
                                && months % i64::from(interval) == 0
                                && options.month_days.contains(&(date.day() as u8))
                        }
                        RecurrenceFrequency::CustomYearly => {
                            date.year() >= anchor.year()
                                && (date.year() - anchor.year()) % interval as i16 == 0
                                && options.year_months.contains(&(date.month() as u8))
                                && date.day() == anchor.day()
                        }
                        _ => false,
                    };
                if !matches {
                    continue;
                }
                DateTime::new(
                    date.year(),
                    date.month(),
                    date.day(),
                    anchor.hour(),
                    anchor.minute(),
                    anchor.second(),
                    anchor.subsec_nanosecond(),
                )
                .map_err(|_| RecurrenceGenerationError::OutOfRange)?
                .in_tz(&options.timezone)
                .map_err(|_| RecurrenceGenerationError::OutOfRange)?
                .timestamp()
                .as_millisecond()
            }
        };
        if due > horizon {
            break;
        }
        if due < anchor_due_at_utc_ms || due > until {
            continue;
        }
        if day_offset == 0
            || values
                .iter()
                .all(|value| value.nominal_due_at_utc_ms != due)
        {
            values.push(GeneratedOccurrence {
                nominal_start_at_utc_ms: duration.and_then(|offset| due.checked_add(offset)),
                nominal_due_at_utc_ms: due,
            });
        }
    }
    if values.is_empty() {
        return Err(RecurrenceGenerationError::InvalidRule);
    }
    Ok(values)
}
