//! Durable local source exclusion, never remote asset cleanup or send authority.

use super::{Connection, PersistenceError};

pub(super) const MIGRATION: &str = r#"
CREATE TABLE probe_source_lifecycle (
    selection INTEGER PRIMARY KEY CHECK(selection IN (1,2)),
    registration TEXT NOT NULL CHECK(length(registration)=36),
    account_id TEXT NOT NULL CHECK(length(account_id)=36),
    calendar_id TEXT NOT NULL CHECK(length(calendar_id)=36),
    state TEXT NOT NULL CHECK(state IN ('active','unresolved','retired'))
) STRICT;
"#;

// No recovery reconstruction, Clone, formatting, serialization or exposed fields.

// Cloneable read-check metadata, never a retirement token. Each use must query
// the exact actor/registration/slot state; copies confer no cleanup authority.

pub(super) fn recover(connection: &Connection) -> Result<(), PersistenceError> {
    connection.execute(
        "UPDATE probe_source_lifecycle SET state='unresolved' WHERE state='active'",
        [],
    )?;
    Ok(())
}
