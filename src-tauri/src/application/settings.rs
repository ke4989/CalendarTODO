use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::settings::{AppSettings, SettingsError, UpdateAppSettings},
};
use thiserror::Error;

#[derive(Clone)]
pub struct SettingsService {
    database: Database,
}

impl SettingsService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
    pub fn get(&self) -> Result<AppSettings, SettingsServiceError> {
        Ok(self.database.get_app_settings()?)
    }
    pub fn update(&self, input: UpdateAppSettings) -> Result<AppSettings, SettingsServiceError> {
        Ok(self.database.update_app_settings(input.validate()?)?)
    }

    /// Persist the desktop list's position lock. It is owned by the list's own
    /// header control, not by the Settings form.
    pub fn set_desktop_pinned(&self, pinned: bool) -> Result<(), SettingsServiceError> {
        Ok(self.database.set_desktop_pinned(pinned)?)
    }

    /// Persist whether the desktop list was left folded.
    pub fn set_desktop_collapsed(&self, collapsed: bool) -> Result<(), SettingsServiceError> {
        Ok(self.database.set_desktop_collapsed(collapsed)?)
    }
}

#[derive(Debug, Error)]
pub enum SettingsServiceError {
    #[error(transparent)]
    Validation(#[from] SettingsError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}
