//! Durable two-slot exact-route witnesses; no route/calendar creation or authority.
use super::{
    Connection, PersistenceError, probe_ledger,
    probe_target::{self, CommittedProbeTarget},
};
use crate::adapters::production_mutation::probe_preflight::seed::ProbeSeedOwner;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use uuid::Uuid;

pub(super) const MIGRATION: &str = r#"
CREATE TABLE probe_exact_route_witness (
 selection INTEGER PRIMARY KEY CHECK(selection IN (1,2)),
 account_id TEXT NOT NULL CHECK(length(account_id)=36),
 calendar_id TEXT NOT NULL CHECK(length(calendar_id)=36),
 route TEXT NOT NULL CHECK(length(CAST(route AS BLOB)) BETWEEN 2 AND 2048),
 target TEXT NOT NULL UNIQUE CHECK(length(CAST(target AS BLOB)) BETWEEN 2 AND 4096),
 revision TEXT NOT NULL CHECK(length(CAST(revision AS BLOB)) BETWEEN 3 AND 256),
 UNIQUE(account_id,calendar_id)
) STRICT;
"#;

pub(super) fn import(
    connection: &Connection,
    epochs: &HashMap<String, Uuid>,
    candidate: CommittedProbeTarget,
    revision: String,
    owner: ProbeSeedOwner,
) -> Result<(), PersistenceError> {
    let tx = connection.unchecked_transaction()?;
    let candidate = probe_target::revalidate(&tx, epochs, candidate)?;
    let ledger = owner.ledger().ok_or(PersistenceError::StaleSyncCapture)?;
    if !owner.current(&candidate)
        || !ledger.matches_target(&candidate)
        || !probe_ledger::active(&tx, ledger)?
        || !crate::domain::remote_mutation::strong_mutation_etag(&revision)
    {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let (account, calendar, route, name) = candidate.checked_parts();
    let opted_in: bool = tx.query_row(
        "SELECT remote_editing_enabled=1 FROM accounts WHERE account_id=?1",
        [account],
        |r| r.get(0),
    )?;
    if !opted_in {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let slot = if name == "Calendar TODO Probe" { 1 } else { 2 };
    // A second fixed-name calendar alias cannot be hidden by slot separation.
    let mut stmt = tx.prepare("SELECT CASE WHEN length(CAST(calendar_id AS BLOB))=36 THEN calendar_id END, CASE WHEN length(CAST(collection_url AS BLOB)) BETWEEN 2 AND 2048 THEN collection_url END FROM calendars WHERE display_name IN ('Calendar TODO Probe','Calendar TODO Probe 2') COLLATE BINARY LIMIT 3")?;
    let mut rows = stmt.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > 2 {
            return Err(PersistenceError::IdentityConflict);
        }
        let id: String = row.get(0)?;
        let other: Option<String> = row.get(1)?;
        if id != calendar && other.as_deref().is_none_or(|r| owner.same_target_route(r)) {
            return Err(PersistenceError::IdentityConflict);
        }
    }
    drop(rows);
    drop(stmt);
    let saved: Option<(String,String,String,String,String)> = tx.query_row(
    "SELECT account_id,calendar_id,route,target,revision FROM probe_exact_route_witness WHERE selection=?1",
    [slot], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
 ).optional()?;
    if let Some(saved) = saved {
        if saved
            != (
                account.into(),
                calendar.into(),
                route.into(),
                owner.target().into(),
                revision.clone(),
            )
        {
            return Err(PersistenceError::IdentityConflict);
        }
    } else {
        let collision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM probe_exact_route_witness WHERE target=?1 OR (account_id=?2 AND calendar_id=?3))", rusqlite::params![owner.target(),account,calendar], |r| r.get(0))?;
        if collision {
            return Err(PersistenceError::IdentityConflict);
        }
        tx.execute("INSERT INTO probe_exact_route_witness(selection,account_id,calendar_id,route,target,revision) VALUES(?1,?2,?3,?4,?5,?6)", rusqlite::params![slot,account,calendar,route,owner.target(),revision])?;
    }
    // Late owner/cancellation loss rolls the local witness back.
    if !owner.current(&candidate) || !probe_ledger::active(&tx, ledger)? {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(())
}
