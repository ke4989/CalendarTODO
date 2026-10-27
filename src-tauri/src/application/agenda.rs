use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::calendar_event::{
        CalendarAgendaItem, CalendarEventError, CalendarSettingsView, CreateCalendarSubtask,
        UpdateCalendarLocalMetadata, UpdateCalendarSubtask, validate_calendar_display_range,
        validate_identifier,
    },
};
use thiserror::Error;

#[derive(Clone)]
pub struct AgendaService {
    database: Database,
}

impl AgendaService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn list_calendar_items(
        &self,
        now_utc_ms: i64,
    ) -> Result<Vec<CalendarAgendaItem>, AgendaServiceError> {
        Ok(self
            .database
            .list_calendar_agenda_items_in_display_window(now_utc_ms)?)
    }

    pub fn list_calendar_settings(&self) -> Result<Vec<CalendarSettingsView>, AgendaServiceError> {
        Ok(self.database.list_calendar_settings()?)
    }

    pub fn set_calendar_color_override(
        &self,
        calendar_id: String,
        color: Option<String>,
    ) -> Result<(), AgendaServiceError> {
        validate_identifier(&calendar_id)?;
        Ok(self
            .database
            .set_calendar_color_override(calendar_id, color)?)
    }
    pub fn set_calendar_visible(
        &self,
        account_id: String,
        calendar_id: String,
        visible: bool,
    ) -> Result<(), AgendaServiceError> {
        validate_identifier(&account_id)?;
        validate_identifier(&calendar_id)?;
        Ok(self
            .database
            .set_calendar_visible(account_id, calendar_id, visible)?)
    }

    pub fn set_calendar_display_range(
        &self,
        account_id: String,
        calendar_id: String,
        display_past_days: Option<i64>,
        display_future_days: Option<i64>,
    ) -> Result<(), AgendaServiceError> {
        validate_identifier(&account_id)?;
        validate_identifier(&calendar_id)?;
        validate_calendar_display_range(display_past_days, display_future_days)?;
        Ok(self.database.set_calendar_display_range(
            account_id,
            calendar_id,
            display_past_days,
            display_future_days,
        )?)
    }

    pub fn set_completed(
        &self,
        series_id: String,
        recurrence_key: String,
        completed: bool,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        validate_identifier(&series_id)?;
        crate::domain::calendar_event::validate_recurrence_key(&recurrence_key)?;
        Ok(self
            .database
            .set_calendar_completed(series_id, recurrence_key, completed)?)
    }

    pub fn set_important(
        &self,
        series_id: String,
        recurrence_key: String,
        important: bool,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        validate_identifier(&series_id)?;
        crate::domain::calendar_event::validate_recurrence_key(&recurrence_key)?;
        Ok(self
            .database
            .set_calendar_important(series_id, recurrence_key, important)?)
    }

    pub fn update_local_metadata(
        &self,
        input: UpdateCalendarLocalMetadata,
    ) -> Result<(), AgendaServiceError> {
        Ok(self
            .database
            .update_calendar_local_metadata(input.validate()?)?)
    }

    pub fn create_subtask(
        &self,
        input: CreateCalendarSubtask,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        Ok(self.database.create_calendar_subtask(input.validate()?)?)
    }

    pub fn update_subtask(
        &self,
        input: UpdateCalendarSubtask,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        Ok(self.database.update_calendar_subtask(input.validate()?)?)
    }

    pub fn set_subtask_completed(
        &self,
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
        completed: bool,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        validate_identifier(&series_id)?;
        crate::domain::calendar_event::validate_recurrence_key(&recurrence_key)?;
        validate_identifier(&subtask_id)?;
        Ok(self.database.set_calendar_subtask_completed(
            series_id,
            recurrence_key,
            subtask_id,
            completed,
        )?)
    }

    pub fn delete_subtask(
        &self,
        series_id: String,
        recurrence_key: String,
        subtask_id: String,
    ) -> Result<CalendarAgendaItem, AgendaServiceError> {
        validate_identifier(&series_id)?;
        crate::domain::calendar_event::validate_recurrence_key(&recurrence_key)?;
        validate_identifier(&subtask_id)?;
        Ok(self
            .database
            .delete_calendar_subtask(series_id, recurrence_key, subtask_id)?)
    }
}

#[derive(Debug, Error)]
pub enum AgendaServiceError {
    #[error(transparent)]
    Validation(#[from] CalendarEventError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}
