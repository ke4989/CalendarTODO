use serde::Serialize;
use std::{collections::BTreeSet, sync::Mutex};
use thiserror::Error;
use uuid::Uuid;

/// Upper bound for one hidden-calendar set. Far above any realistic account
/// while keeping the process-local snapshot and its event payload small.
pub const MAX_HIDDEN_CALENDARS: usize = 256;
pub const CALENDAR_DISPLAY_SELECTION_EVENT: &str =
    "calendar-todo://calendar-display-selection-changed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarDisplaySelection {
    pub version: u64,
    pub hidden_calendar_ids: Vec<String>,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CalendarDisplaySelectionError {
    #[error("calendar display selection is invalid")]
    Invalid,
    #[error("calendar display selection could not be stored")]
    Persistence,
}

/// Presentation choice shared by the main window and desktop list. The
/// caller stores the hidden set; the version only orders events in this process.
#[derive(Default)]
pub struct CalendarDisplaySelectionState {
    current: Mutex<CalendarDisplaySelection>,
    write: Mutex<()>,
}

impl Default for CalendarDisplaySelection {
    fn default() -> Self {
        Self {
            version: 0,
            hidden_calendar_ids: Vec::new(),
        }
    }
}

impl CalendarDisplaySelectionState {
    pub fn get(&self) -> CalendarDisplaySelection {
        self.current.lock().expect("display selection").clone()
    }

    /// Installs a previously stored set at version 0. Invalid input leaves the
    /// current snapshot unchanged.
    pub fn restore(&self, stored: Vec<String>) -> Result<(), CalendarDisplaySelectionError> {
        let hidden = canonicalize(stored)?;
        let _write = self.write.lock().expect("display selection");
        let mut current = self.current.lock().expect("display selection");
        current.hidden_calendar_ids = hidden;
        Ok(())
    }

    /// Replaces the complete hidden set. The store callback runs only after
    /// validation and only when the set changed. Its failure leaves the snapshot
    /// unchanged.
    pub fn replace(
        &self,
        hidden_calendar_ids: Vec<String>,
        persist: impl FnOnce(&[String]) -> Result<(), ()>,
    ) -> Result<(CalendarDisplaySelection, bool), CalendarDisplaySelectionError> {
        let _write = self.write.lock().expect("display selection");
        let hidden = canonicalize(hidden_calendar_ids)?;
        {
            let current = self.current.lock().expect("display selection");
            if current.hidden_calendar_ids == hidden {
                return Ok((current.clone(), false));
            }
        }
        persist(&hidden).map_err(|_| CalendarDisplaySelectionError::Persistence)?;
        let mut current = self.current.lock().expect("display selection");
        current.version += 1;
        current.hidden_calendar_ids = hidden;
        Ok((current.clone(), true))
    }
}

fn canonicalize(
    hidden_calendar_ids: Vec<String>,
) -> Result<Vec<String>, CalendarDisplaySelectionError> {
    if hidden_calendar_ids.len() > MAX_HIDDEN_CALENDARS {
        return Err(CalendarDisplaySelectionError::Invalid);
    }
    let mut canonical = BTreeSet::new();
    for id in hidden_calendar_ids {
        if !Uuid::parse_str(&id).is_ok_and(|parsed| parsed.to_string() == id) {
            return Err(CalendarDisplaySelectionError::Invalid);
        }
        canonical.insert(id);
    }
    Ok(canonical.into_iter().collect())
}
