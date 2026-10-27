use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::local_todo::{
        CreateLocalTodo, CreateSubtask, DeleteLocalCalendar, DeletedLocalCalendar, LocalCalendar,
        LocalTodo, LocalTodoError, MoveLocalSubtask, SaveLocalCalendar, SaveLocalTodo,
        SetLocalCalendarHidden, UpdateLocalTodo, UpdateSubtask, validate_identifier,
        validate_occurrence_key,
    },
};
use thiserror::Error;

#[derive(Clone)]
pub struct LocalTodoService {
    database: Database,
}

impl LocalTodoService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn create(&self, input: CreateLocalTodo) -> Result<LocalTodo, LocalTodoServiceError> {
        let input = input.validate()?;
        Ok(self
            .database
            .create_local_todo(input.title, input.priority, input.important)?)
    }

    pub fn list(&self) -> Result<Vec<LocalTodo>, LocalTodoServiceError> {
        Ok(self.database.list_local_todos()?)
    }

    pub fn list_calendars(&self) -> Result<Vec<LocalCalendar>, LocalTodoServiceError> {
        Ok(self.database.list_local_calendars()?)
    }

    pub fn save_calendar(
        &self,
        input: SaveLocalCalendar,
    ) -> Result<LocalCalendar, LocalTodoServiceError> {
        Ok(self.database.save_local_calendar(input.validate()?)?)
    }

    /// Hide or reveal one local calendar. Hiding keeps every task, its
    /// completion, and its subtasks; it only removes the calendar from the
    /// Agenda lists and reminder planning.
    pub fn set_calendar_hidden(
        &self,
        input: SetLocalCalendarHidden,
    ) -> Result<LocalCalendar, LocalTodoServiceError> {
        let input = input.validate()?;
        Ok(self
            .database
            .set_local_calendar_hidden(input.calendar_id, input.hidden)?)
    }

    /// Delete one local calendar after reassigning its tasks to a remaining
    /// visible calendar. No task is ever deleted implicitly.
    pub fn delete_calendar(
        &self,
        input: DeleteLocalCalendar,
    ) -> Result<DeletedLocalCalendar, LocalTodoServiceError> {
        let input = input.validate()?;
        Ok(self.database.delete_local_calendar(input.calendar_id)?)
    }

    pub fn move_to_calendar(
        &self,
        todo_id: String,
        calendar_id: String,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_identifier(&calendar_id)?;
        Ok(self.database.move_local_todo(todo_id, calendar_id)?)
    }

    pub fn update(&self, input: UpdateLocalTodo) -> Result<LocalTodo, LocalTodoServiceError> {
        Ok(self.database.update_local_todo(input.validate()?)?)
    }

    pub fn save(&self, input: SaveLocalTodo) -> Result<LocalTodo, LocalTodoServiceError> {
        Ok(self.database.save_local_todo(input.validate()?)?)
    }

    pub fn set_completed(
        &self,
        todo_id: String,
        occurrence_key: String,
        completed: bool,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_occurrence_key(&occurrence_key)?;
        Ok(self
            .database
            .set_completed(todo_id, occurrence_key, completed)?)
    }

    pub fn create_subtask(&self, input: CreateSubtask) -> Result<LocalTodo, LocalTodoServiceError> {
        Ok(self.database.create_subtask(input.validate()?)?)
    }

    pub fn move_subtask(
        &self,
        input: MoveLocalSubtask,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        Ok(self.database.move_local_subtask(input.validate()?)?)
    }

    pub fn update_subtask(&self, input: UpdateSubtask) -> Result<LocalTodo, LocalTodoServiceError> {
        Ok(self.database.update_subtask(input.validate()?)?)
    }

    pub fn set_subtask_completed(
        &self,
        todo_id: String,
        subtask_id: String,
        completed: bool,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_identifier(&subtask_id)?;
        Ok(self
            .database
            .set_subtask_completed(todo_id, subtask_id, completed)?)
    }

    pub fn delete_subtask(
        &self,
        todo_id: String,
        subtask_id: String,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_identifier(&subtask_id)?;
        Ok(self.database.delete_subtask(todo_id, subtask_id)?)
    }

    pub fn delete(&self, todo_id: String) -> Result<(), LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        Ok(self.database.soft_delete(todo_id)?)
    }
    pub fn delete_occurrence(
        &self,
        todo_id: String,
        occurrence_key: String,
    ) -> Result<(), LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_occurrence_key(&occurrence_key)?;
        Ok(self
            .database
            .soft_delete_occurrence(todo_id, occurrence_key)?)
    }
    pub fn restore_recently_deleted_occurrence(
        &self,
        todo_id: String,
        occurrence_key: String,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        validate_occurrence_key(&occurrence_key)?;
        Ok(self
            .database
            .restore_recently_deleted_occurrence(todo_id, occurrence_key)?)
    }
    pub fn restore_recently_deleted(
        &self,
        todo_id: String,
    ) -> Result<LocalTodo, LocalTodoServiceError> {
        validate_identifier(&todo_id)?;
        Ok(self.database.restore_recently_deleted(todo_id)?)
    }
}

#[derive(Debug, Error)]
pub enum LocalTodoServiceError {
    #[error(transparent)]
    Validation(#[from] LocalTodoError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}
