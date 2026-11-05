use super::*;
use rusqlite::{Connection, params};

pub(super) const MIGRATION: &str = r#"
CREATE TABLE calendar_display_hidden (
 calendar_id TEXT PRIMARY KEY NOT NULL
  CHECK(length(calendar_id)=36 AND calendar_id NOT GLOB '*[^0-9a-f-]*')
) STRICT;
"#;

pub(super) fn hidden(c: &Connection) -> Result<Vec<String>, PersistenceError> {
    let mut statement =
        c.prepare("SELECT calendar_id FROM calendar_display_hidden ORDER BY calendar_id")?;
    let rows = statement.query_map([], |row| row.get(0))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(PersistenceError::from)
}

pub(super) fn replace_hidden(c: &mut Connection, ids: &[String]) -> Result<(), PersistenceError> {
    if ids.len() > crate::application::calendar_display_selection::MAX_HIDDEN_CALENDARS {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let tx = c.transaction()?;
    tx.execute("DELETE FROM calendar_display_hidden", [])?;
    for id in ids {
        tx.execute(
            "INSERT INTO calendar_display_hidden(calendar_id) VALUES(?1)",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(())
}
