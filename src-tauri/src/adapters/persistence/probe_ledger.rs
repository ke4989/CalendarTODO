//! Durable local source exclusion, never remote asset cleanup or send authority.

use super::{Connection, PersistenceError, probe_target::CommittedProbeTarget};
use std::{collections::HashMap, sync::Weak};
use uuid::Uuid;

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
pub(crate) struct ProbeLedgerToken {
    pub(super) scope: ProbeLedgerScope,
}

// Cloneable read-check metadata, never a retirement token. Each use must query
// the exact actor/registration/slot state; copies confer no cleanup authority.
#[derive(Clone)]
pub(crate) struct ProbeLedgerScope {
    pub(super) selection: i64,
    pub(super) registration: String,
    account: String,
    calendar: String,
    epoch: Uuid,
    route: String,
    privileges: String,
    available: bool,
    actor: Weak<()>,
}

impl ProbeLedgerToken {
    pub(super) fn matches_actor(&self, actor: &Weak<()>) -> bool {
        self.scope.matches_actor(actor)
    }
}
impl ProbeLedgerScope {
    pub(crate) fn matches_target(&self, target: &CommittedProbeTarget) -> bool {
        let (account, calendar, _, name) = target.checked_parts();
        self.account == account
            && self.calendar == calendar
            && match self.selection {
                1 => name == "Calendar TODO Probe",
                2 => name == "Calendar TODO Probe 2",
                _ => false,
            }
            && target.matches_actor(&self.actor)
            && target.matches_ledger_binding(
                self.epoch,
                &self.route,
                &self.privileges,
                self.available,
            )
    }
    pub(super) fn matches_actor(&self, actor: &Weak<()>) -> bool {
        self.actor.upgrade().is_some() && Weak::ptr_eq(&self.actor, actor)
    }
}

pub(crate) struct ProbeReconciliationEvidence {
    pub(super) scope: ProbeLedgerScope,
}

pub(super) fn active(
    connection: &Connection,
    scope: &ProbeLedgerScope,
) -> Result<bool, PersistenceError> {
    if scope.actor.upgrade().is_none() {
        return Ok(false);
    }
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM probe_source_lifecycle WHERE selection=?1 AND registration=?2
         AND account_id=?3 AND calendar_id=?4 AND state='active')",
        rusqlite::params![
            scope.selection,
            scope.registration,
            scope.account,
            scope.calendar
        ],
        |r| r.get(0),
    )?)
}

pub(super) fn record_closure(
    connection: &Connection,
    scope: ProbeLedgerScope,
) -> Result<ProbeReconciliationEvidence, PersistenceError> {
    if scope.actor.upgrade().is_none() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let changed = connection.execute(
        "UPDATE probe_source_lifecycle SET state='unresolved' WHERE selection=?1 AND registration=?2
         AND account_id=?3 AND calendar_id=?4 AND state IN ('active','unresolved')",
        rusqlite::params![scope.selection, scope.registration, scope.account, scope.calendar],
    )?;
    if changed != 1 {
        return Err(PersistenceError::StaleSyncCapture);
    }
    Ok(ProbeReconciliationEvidence { scope })
}

pub(super) fn reconcile(
    connection: &Connection,
    evidence: ProbeReconciliationEvidence,
) -> Result<(), PersistenceError> {
    let scope = evidence.scope;
    if scope.actor.upgrade().is_none() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let changed = connection.execute(
        "UPDATE probe_source_lifecycle SET state='retired' WHERE selection=?1 AND registration=?2
         AND account_id=?3 AND calendar_id=?4 AND state='unresolved'",
        rusqlite::params![
            scope.selection,
            scope.registration,
            scope.account,
            scope.calendar
        ],
    )?;
    if changed != 1 {
        return Err(PersistenceError::StaleSyncCapture);
    }
    Ok(())
}

pub(super) fn begin(
    connection: &mut Connection,
    epochs: &HashMap<String, Uuid>,
    candidate: CommittedProbeTarget,
    actor: Weak<()>,
) -> Result<(CommittedProbeTarget, ProbeLedgerToken), PersistenceError> {
    if !candidate.matches_actor(&actor) {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let tx = connection.transaction()?;
    let candidate = super::probe_target::revalidate(&tx, epochs, candidate)?;
    let (account, calendar, _, name) = candidate.checked_parts();
    let selection = match name {
        "Calendar TODO Probe" => 1,
        "Calendar TODO Probe 2" => 2,
        _ => return Err(PersistenceError::StaleSyncCapture),
    };
    let registration = Uuid::new_v4().to_string();
    let changed = tx.execute(
        "INSERT INTO probe_source_lifecycle(selection,registration,account_id,calendar_id,state)
         VALUES (?1,?2,?3,?4,'active') ON CONFLICT(selection) DO UPDATE SET
         registration=excluded.registration,account_id=excluded.account_id,
         calendar_id=excluded.calendar_id,state='active'
         WHERE probe_source_lifecycle.state='retired'",
        rusqlite::params![selection, registration, account, calendar],
    )?;
    if changed != 1 {
        return Err(PersistenceError::IdentityConflict);
    }
    let (epoch, privileges, available) = candidate.ledger_binding();
    let scope = ProbeLedgerScope {
        selection,
        registration,
        account: account.to_owned(),
        calendar: calendar.to_owned(),
        actor,
        epoch,
        route: candidate.checked_parts().2.to_owned(),
        privileges: privileges.to_owned(),
        available,
    };
    tx.commit()?;
    Ok((candidate, ProbeLedgerToken { scope }))
}

pub(super) fn finish(
    connection: &Connection,
    scope: &ProbeLedgerScope,
    retired: bool,
) -> Result<(), PersistenceError> {
    // Cleanup never needs current target eligibility: disabled/renamed targets
    // still need their exact local registration retired. It cannot execute HTTP.
    if scope.actor.upgrade().is_none() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let state = if retired { "retired" } else { "unresolved" };
    let changed = connection.execute(
        "UPDATE probe_source_lifecycle SET state=?3 WHERE selection=?1
         AND registration=?2 AND account_id=?4 AND calendar_id=?5 AND state IN ('active',?3)",
        rusqlite::params![
            scope.selection,
            scope.registration,
            state,
            scope.account,
            scope.calendar
        ],
    )?;
    if changed != 1 {
        return Err(PersistenceError::StaleSyncCapture);
    }
    Ok(())
}

pub(super) fn recover(connection: &Connection) -> Result<(), PersistenceError> {
    connection.execute(
        "UPDATE probe_source_lifecycle SET state='unresolved' WHERE state='active'",
        [],
    )?;
    Ok(())
}
