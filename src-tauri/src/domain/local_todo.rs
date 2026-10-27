use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

pub const SINGLETON_OCCURRENCE_KEY: &str = "SINGLETON";
pub const MAX_TITLE_CHARS: usize = 512;
pub const MAX_NOTES_CHARS: usize = 10_000;
pub const MAX_LOCATION_CHARS: usize = 512;
pub const MAX_TAG_CHARS: usize = 64;
pub const MAX_TAGS: usize = 32;
pub const MAX_SUBTASK_CHARS: usize = 512;
pub const MAX_RECURRENCE_INTERVAL: u32 = 365;
pub const MAX_CALENDAR_NAME_CHARS: usize = 128;
pub const MAX_CALENDAR_DISPLAY_DAYS: u16 = 3650;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCalendar {
    pub calendar_id: String,
    pub name: String,
    pub color: String,
    pub is_default: bool,
    /// Hidden calendars keep every task and every piece of local state; they
    /// simply stop contributing Agenda rows and reminders. At least one local
    /// calendar must stay visible.
    pub hidden: bool,
    pub display_past_days: Option<u16>,
    pub display_future_days: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveLocalCalendar {
    pub calendar_id: Option<String>,
    pub name: String,
    pub color: String,
    pub display_past_days: Option<u16>,
    pub display_future_days: Option<u16>,
}

impl SaveLocalCalendar {
    pub fn validate(mut self) -> Result<Self, LocalTodoError> {
        if let Some(id) = &self.calendar_id {
            validate_uuid(id)?;
        }
        self.name = required_text(
            self.name,
            MAX_CALENDAR_NAME_CHARS,
            LocalTodoError::EmptyCalendarName,
            LocalTodoError::CalendarNameTooLong,
        )?;
        if !valid_color(&self.color) {
            return Err(LocalTodoError::InvalidCalendarColor);
        }
        if self
            .display_past_days
            .is_some_and(|days| days > MAX_CALENDAR_DISPLAY_DAYS)
            || self
                .display_future_days
                .is_some_and(|days| days > MAX_CALENDAR_DISPLAY_DAYS)
        {
            return Err(LocalTodoError::InvalidCalendarDisplayRange);
        }
        self.color.make_ascii_uppercase();
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLocalCalendarHidden {
    pub calendar_id: String,
    pub hidden: bool,
}

impl SetLocalCalendarHidden {
    pub fn validate(self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.calendar_id)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteLocalCalendar {
    pub calendar_id: String,
}

impl DeleteLocalCalendar {
    pub fn validate(self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.calendar_id)?;
        Ok(self)
    }
}

/// Bounded result of deleting one local calendar. Deletion never destroys a
/// task: every task owned by the removed calendar is reassigned first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedLocalCalendar {
    pub moved_todos: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    Low,
    Normal,
    High,
}

pub use crate::domain::local_recurrence::{
    RecurrenceEndKind, RecurrenceFrequency, RecurrenceOptions,
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixedRecurrence {
    pub series_id: String,
    pub frequency: RecurrenceFrequency,
    pub interval: u32,
    pub options: RecurrenceOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixedRecurrenceInput {
    pub frequency: RecurrenceFrequency,
    pub interval: u32,
    #[serde(default)]
    pub options: RecurrenceOptions,
}

impl FixedRecurrenceInput {
    fn validate(&self) -> Result<(), LocalTodoError> {
        if self.interval == 0 || self.interval > MAX_RECURRENCE_INTERVAL {
            return Err(LocalTodoError::InvalidRecurrence);
        }
        crate::domain::local_recurrence::validate_rule_shape(
            self.frequency,
            self.interval,
            &self.options,
        )
        .map_err(|_| LocalTodoError::InvalidRecurrence)?;
        Ok(())
    }
}

impl Priority {
    pub fn rank(self) -> i64 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::High => 2,
        }
    }
    pub fn from_rank(rank: i64) -> Result<Self, LocalTodoError> {
        match rank {
            0 => Ok(Self::Low),
            1 => Ok(Self::Normal),
            2 => Ok(Self::High),
            _ => Err(LocalTodoError::CorruptPriority),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub tag_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subtask {
    pub subtask_id: String,
    pub work_item_id: String,
    pub text: String,
    pub completed: bool,
    pub sort_order: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTodo {
    pub todo_id: String,
    pub calendar_id: String,
    pub calendar_name: String,
    pub calendar_color: String,
    pub work_item_id: Option<String>,
    pub occurrence_key: String,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: Option<i64>,
    pub due_at_utc_ms: Option<i64>,
    pub priority: Priority,
    pub important: bool,
    pub completed: bool,
    pub completed_at_ms: Option<i64>,
    pub recurrence: Option<FixedRecurrence>,
    pub backlog_count: u64,
    pub tags: Vec<Tag>,
    pub subtasks: Vec<Subtask>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateLocalTodo {
    pub title: String,
    pub priority: Priority,
    pub important: bool,
}

impl CreateLocalTodo {
    pub fn validate(mut self) -> Result<Self, LocalTodoError> {
        self.title = required_text(
            self.title,
            MAX_TITLE_CHARS,
            LocalTodoError::EmptyTitle,
            LocalTodoError::TitleTooLong,
        )?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLocalTodo {
    pub todo_id: String,
    #[serde(default)]
    pub selected_occurrence_key: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: Option<i64>,
    pub due_at_utc_ms: Option<i64>,
    pub priority: Priority,
    pub important: bool,
    pub tag_names: Vec<String>,
    pub recurrence: Option<FixedRecurrenceInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveLocalTodo {
    pub todo_id: Option<String>,
    #[serde(default)]
    pub selected_occurrence_key: Option<String>,
    pub calendar_id: String,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub start_at_utc_ms: Option<i64>,
    pub due_at_utc_ms: Option<i64>,
    pub priority: Priority,
    pub important: bool,
    pub tag_names: Vec<String>,
    pub recurrence: Option<FixedRecurrenceInput>,
}

impl SaveLocalTodo {
    pub fn validate(self) -> Result<Self, LocalTodoError> {
        if let Some(todo_id) = &self.todo_id {
            validate_uuid(todo_id)?;
        }
        validate_uuid(&self.calendar_id)?;
        let details = UpdateLocalTodo {
            todo_id: self
                .todo_id
                .clone()
                .unwrap_or_else(|| "00000000-0000-4000-8000-000000000001".into()),
            selected_occurrence_key: self.selected_occurrence_key.clone(),
            title: self.title,
            notes: self.notes,
            location: self.location,
            start_at_utc_ms: self.start_at_utc_ms,
            due_at_utc_ms: self.due_at_utc_ms,
            priority: self.priority,
            important: self.important,
            tag_names: self.tag_names,
            recurrence: self.recurrence,
        }
        .validate()?;
        Ok(Self {
            todo_id: self.todo_id,
            selected_occurrence_key: details.selected_occurrence_key,
            calendar_id: self.calendar_id,
            title: details.title,
            notes: details.notes,
            location: details.location,
            start_at_utc_ms: details.start_at_utc_ms,
            due_at_utc_ms: details.due_at_utc_ms,
            priority: details.priority,
            important: details.important,
            tag_names: details.tag_names,
            recurrence: details.recurrence,
        })
    }
}

impl UpdateLocalTodo {
    pub fn validate(mut self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.todo_id)?;
        if let Some(key) = &self.selected_occurrence_key {
            validate_occurrence_key(key)?;
        }
        self.title = required_text(
            self.title,
            MAX_TITLE_CHARS,
            LocalTodoError::EmptyTitle,
            LocalTodoError::TitleTooLong,
        )?;
        self.notes = optional_text(self.notes, MAX_NOTES_CHARS, LocalTodoError::NotesTooLong)?;
        self.location = optional_text(
            self.location,
            MAX_LOCATION_CHARS,
            LocalTodoError::LocationTooLong,
        )?;
        if matches!((self.start_at_utc_ms, self.due_at_utc_ms), (Some(start), Some(due)) if start > due)
        {
            return Err(LocalTodoError::StartAfterDue);
        }
        if let Some(recurrence) = &self.recurrence {
            recurrence.validate()?;
            if self.due_at_utc_ms.is_none() {
                return Err(LocalTodoError::RecurrenceRequiresDue);
            }
        }
        if self.tag_names.len() > MAX_TAGS {
            return Err(LocalTodoError::TooManyTags);
        }
        let mut normalized = HashSet::new();
        let mut tags = Vec::with_capacity(self.tag_names.len());
        for name in self.tag_names {
            let name = required_text(
                name,
                MAX_TAG_CHARS,
                LocalTodoError::EmptyTag,
                LocalTodoError::TagTooLong,
            )?;
            if normalized.insert(normalize_tag_name(&name)) {
                tags.push(name);
            }
        }
        self.tag_names = tags;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSubtask {
    pub todo_id: String,
    pub occurrence_key: String,
    pub text: String,
}

impl CreateSubtask {
    pub fn validate(mut self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.todo_id)?;
        validate_occurrence_key(&self.occurrence_key)?;
        self.text = validate_subtask_text(self.text)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSubtask {
    pub todo_id: String,
    pub subtask_id: String,
    pub text: String,
}

impl UpdateSubtask {
    pub fn validate(mut self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.todo_id)?;
        validate_uuid(&self.subtask_id)?;
        self.text = validate_subtask_text(self.text)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtaskMoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoveLocalSubtask {
    pub todo_id: String,
    pub subtask_id: String,
    pub direction: SubtaskMoveDirection,
    // deserialize_with makes this nullable field required (no implicit Option default).
    #[serde(deserialize_with = "required_neighbor")]
    pub expected_neighbor_id: Option<String>,
}

fn required_neighbor<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl MoveLocalSubtask {
    pub fn validate(self) -> Result<Self, LocalTodoError> {
        validate_uuid(&self.todo_id)?;
        validate_uuid(&self.subtask_id)?;
        if let Some(id) = &self.expected_neighbor_id {
            validate_uuid(id)?;
            if id == &self.subtask_id {
                return Err(LocalTodoError::InvalidIdentifier);
            }
        }
        Ok(self)
    }
}

pub fn validate_identifier(value: &str) -> Result<(), LocalTodoError> {
    validate_uuid(value)
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub fn validate_occurrence_key(value: &str) -> Result<(), LocalTodoError> {
    if value == SINGLETON_OCCURRENCE_KEY {
        return Ok(());
    }
    let mut parts = value.split(':');
    if parts.next() != Some("FIXED") {
        return Err(LocalTodoError::InvalidOccurrenceKey);
    }
    validate_uuid(parts.next().ok_or(LocalTodoError::InvalidOccurrenceKey)?)?;
    parts
        .next()
        .ok_or(LocalTodoError::InvalidOccurrenceKey)?
        .parse::<u64>()
        .map_err(|_| LocalTodoError::InvalidOccurrenceKey)?;
    if parts.next().is_some() {
        return Err(LocalTodoError::InvalidOccurrenceKey);
    }
    Ok(())
}

pub fn fixed_occurrence_key(series_id: &str, sequence: u64) -> String {
    format!("FIXED:{series_id}:{sequence}")
}
pub fn normalize_tag_name(name: &str) -> String {
    name.to_lowercase()
}

fn validate_subtask_text(text: String) -> Result<String, LocalTodoError> {
    required_text(
        text,
        MAX_SUBTASK_CHARS,
        LocalTodoError::EmptySubtask,
        LocalTodoError::SubtaskTooLong,
    )
}

fn validate_uuid(value: &str) -> Result<(), LocalTodoError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| LocalTodoError::InvalidIdentifier)
}

fn optional_text(
    value: Option<String>,
    limit: usize,
    too_long: LocalTodoError,
) -> Result<Option<String>, LocalTodoError> {
    match value {
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                Ok(None)
            } else if value.chars().count() > limit {
                Err(too_long)
            } else {
                Ok(Some(value))
            }
        }
        None => Ok(None),
    }
}

fn required_text(
    value: String,
    limit: usize,
    empty: LocalTodoError,
    too_long: LocalTodoError,
) -> Result<String, LocalTodoError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(empty)
    } else if value.chars().count() > limit {
        Err(too_long)
    } else {
        Ok(value)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum LocalTodoError {
    #[error("an identifier is invalid")]
    InvalidIdentifier,
    #[error("title is required")]
    EmptyTitle,
    #[error("title exceeds 512 characters")]
    TitleTooLong,
    #[error("notes exceed 10000 characters")]
    NotesTooLong,
    #[error("location exceeds 512 characters")]
    LocationTooLong,
    #[error("start must not be later than due")]
    StartAfterDue,
    #[error("tag is required")]
    EmptyTag,
    #[error("tag exceeds 64 characters")]
    TagTooLong,
    #[error("a TODO cannot have more than 32 tags")]
    TooManyTags,
    #[error("subtask text is required")]
    EmptySubtask,
    #[error("subtask text exceeds 512 characters")]
    SubtaskTooLong,
    #[error("stored priority is invalid")]
    CorruptPriority,
    #[error("recurrence requires a due time")]
    RecurrenceRequiresDue,
    #[error("fixed recurrence is invalid")]
    InvalidRecurrence,
    #[error("occurrence key is invalid")]
    InvalidOccurrenceKey,
    #[error("local calendar name is required")]
    EmptyCalendarName,
    #[error("local calendar name exceeds 128 characters")]
    CalendarNameTooLong,
    #[error("local calendar color is invalid")]
    InvalidCalendarColor,
    #[error("local calendar display range is invalid")]
    InvalidCalendarDisplayRange,
    #[error("at least one local calendar must stay visible")]
    LastVisibleCalendar,
    #[error("the last local calendar cannot be deleted")]
    LastCalendar,
}
