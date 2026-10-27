//! Actor-owned committed target candidates, not Probe source/session authority.
//! No persisted schema currently attests discovery's concrete session owner.

use super::{Connection, PersistenceError, collection_content_writable};
use std::{collections::HashMap, sync::Weak};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum ProbeTargetSelection {
    First,
    Second,
}

impl ProbeTargetSelection {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::First => "Calendar TODO Probe",
            Self::Second => "Calendar TODO Probe 2",
        }
    }
}

#[derive(PartialEq, Eq)]
struct TargetRow {
    account: String,
    calendar: String,
    route: String,
    privileges: String,
    available: bool,
}

// No Clone, Debug, Serialize, public fields or caller-controlled constructor.
pub(crate) struct CommittedProbeTarget {
    selection: ProbeTargetSelection,
    row: TargetRow,
    epoch: Uuid,
    actor: Weak<()>,
}

// Product routing is captured separately from diagnostic target candidates.
// This is a current committed pair, never historical discovery/socket proof.
pub(crate) struct CommittedExactProbeCollections {
    rows: [TargetRow; 2],
    epoch: Uuid,
    actor: Weak<()>,
}

impl CommittedExactProbeCollections {
    pub(crate) fn matches_actor(&self, actor: &Weak<()>) -> bool {
        self.actor.upgrade().is_some() && Weak::ptr_eq(&self.actor, actor)
    }
    pub(in crate::adapters) fn account_and_routes(&self) -> (&str, [&str; 2]) {
        (
            &self.rows[0].account,
            [&self.rows[0].route, &self.rows[1].route],
        )
    }
}

fn exact_pair(connection: &Connection) -> Result<[TargetRow; 2], PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT c.display_name,
         CASE WHEN length(CAST(c.account_id AS BLOB))=36 THEN c.account_id END,
         CASE WHEN length(CAST(c.calendar_id AS BLOB))=36 THEN c.calendar_id END,
         CASE WHEN length(CAST(c.collection_url AS BLOB)) BETWEEN 2 AND 2048 THEN c.collection_url END,
         CASE WHEN length(CAST(c.privileges AS BLOB)) BETWEEN 1 AND 512 THEN c.privileges END,
         c.available,
         a.enabled=1 AND a.credential_configured=1 AND a.remote_editing_enabled=1
           AND a.provider_kind='caldav' AND a.region_profile='icloud-cn' AND a.removed_at_ms IS NULL
           AND a.credential_ref='calendar-todo/account/' || c.account_id
         FROM calendars c LEFT JOIN accounts a ON a.account_id=c.account_id
         WHERE c.display_name IN ('Calendar TODO Probe','Calendar TODO Probe 2') COLLATE BINARY
           AND a.removed_at_ms IS NULL
         LIMIT 3")?;
    let mut query = statement.query([])?;
    let mut slots: [Option<(TargetRow, bool)>; 2] = [None, None];
    while let Some(row) = query.next()? {
        let slot = if row.get::<_, String>(0)? == ProbeTargetSelection::First.name() {
            0
        } else {
            1
        };
        if slots[slot].is_some() {
            return Err(PersistenceError::IdentityConflict);
        }
        // A bounded-out value cannot qualify, but must not hide a later
        // duplicate name. Delay readiness/shape rejection until both slots
        // have been checked across all non-archived candidates.
        let bounded = |index| {
            row.get::<_, Option<String>>(index)
                .map(Option::unwrap_or_default)
        };
        slots[slot] = Some((
            TargetRow {
                account: bounded(1)?,
                calendar: bounded(2)?,
                route: bounded(3)?,
                privileges: bounded(4)?,
                available: row.get(5)?,
            },
            row.get::<_, Option<bool>>(6)?.unwrap_or(false),
        ));
    }
    let [
        Some((first, first_eligible)),
        Some((second, second_eligible)),
    ] = slots
    else {
        return Err(PersistenceError::NotFound);
    };
    // Only explicit archival removes historical candidates; no readiness
    // filter may hide a current duplicate, foreign account or slot alias.
    if first.account != second.account
        || first.calendar == second.calendar
        || crate::domain::remote_mutation::collection_routes_match(&first.route, &second.route)
    {
        return Err(PersistenceError::IdentityConflict);
    }
    if !first_eligible
        || !second_eligible
        || [&first, &second].into_iter().any(|row| {
            [&row.account, &row.calendar].into_iter().any(|id| {
                !Uuid::parse_str(id).is_ok_and(|value| !value.is_nil() && value.to_string() == *id)
            }) || !row.available
                || !collection_content_writable(&row.privileges)
                || !safe_route(&row.route)
        })
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    Ok([first, second])
}

pub(super) fn scoped_calendar_route(
    connection: &Connection,
    account: &str,
    calendar: &str,
) -> Result<String, PersistenceError> {
    exact_pair(connection)?
        .into_iter()
        .find(|row| row.account == account && row.calendar == calendar)
        .map(|row| row.route)
        .ok_or(PersistenceError::RemoteMutationReadOnly)
}

pub(super) fn capture_exact_pair(
    connection: &Connection,
    epochs: &mut HashMap<String, Uuid>,
    actor: Weak<()>,
) -> Result<CommittedExactProbeCollections, PersistenceError> {
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let tx = connection.unchecked_transaction()?;
    let rows = exact_pair(&tx)?;
    tx.commit()?;
    let epoch = *epochs
        .entry(rows[0].account.clone())
        .or_insert_with(Uuid::new_v4);
    Ok(CommittedExactProbeCollections { rows, epoch, actor })
}

pub(super) fn revalidate_exact_pair(
    connection: &Connection,
    epochs: &HashMap<String, Uuid>,
    snapshot: CommittedExactProbeCollections,
) -> Result<CommittedExactProbeCollections, PersistenceError> {
    let tx = connection.unchecked_transaction()?;
    if snapshot.actor.upgrade().is_none()
        || epochs.get(&snapshot.rows[0].account) != Some(&snapshot.epoch)
        || exact_pair(&tx)? != snapshot.rows
    {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(snapshot)
}

pub(crate) struct ProbeTargetView {
    pub(crate) committed_candidate: bool,
    pub(crate) production_binding_available: bool,
    pub(crate) network_authorized: bool,
}

impl CommittedProbeTarget {
    pub(super) fn ledger_binding(&self) -> (Uuid, &str, bool) {
        (self.epoch, &self.row.privileges, self.row.available)
    }
    pub(super) fn matches_ledger_binding(
        &self,
        epoch: Uuid,
        route: &str,
        privileges: &str,
        available: bool,
    ) -> bool {
        self.epoch == epoch
            && self.row.route == route
            && self.row.privileges == privileges
            && self.row.available == available
    }
    pub(crate) fn matches_actor(&self, actor: &Weak<()>) -> bool {
        self.actor.upgrade().is_some() && Weak::ptr_eq(&self.actor, actor)
    }

    // Internal checked parts only, never a presentation or caller route API.
    pub(crate) fn checked_parts(&self) -> (&str, &str, &str, &'static str) {
        (
            &self.row.account,
            &self.row.calendar,
            &self.row.route,
            self.selection.name(),
        )
    }

    pub(crate) fn view(&self) -> ProbeTargetView {
        ProbeTargetView {
            committed_candidate: true,
            production_binding_available: false,
            network_authorized: false,
        }
    }
}

fn target(
    connection: &Connection,
    selection: ProbeTargetSelection,
) -> Result<TargetRow, PersistenceError> {
    // Exactly one authorized display name; never enumerate other calendars.
    // CASE bounds text materialization independently of stored row validity.
    // Availability/privileges are checked after uniqueness so an ambiguous
    // read-only/unavailable alias cannot be hidden by a readiness filter.
    let mut statement = connection.prepare(
        "SELECT CASE WHEN length(CAST(a.account_id AS BLOB))=36 THEN a.account_id END,
                CASE WHEN length(CAST(c.calendar_id AS BLOB))=36 THEN c.calendar_id END,
                CASE WHEN length(CAST(c.collection_url AS BLOB)) BETWEEN 2 AND 2048 THEN c.collection_url END,
                CASE WHEN length(CAST(c.privileges AS BLOB)) BETWEEN 1 AND 512 THEN c.privileges END,
                c.available
         FROM calendars c JOIN accounts a ON a.account_id=c.account_id
         WHERE c.display_name=?1 COLLATE BINARY
           AND a.provider_kind='caldav' AND a.region_profile='icloud-cn'
           AND a.enabled=1 AND a.credential_configured=1 AND a.removed_at_ms IS NULL
         LIMIT 2")?;
    let mut rows = statement.query([selection.name()])?;
    let row = rows.next()?.ok_or(PersistenceError::NotFound)?;
    let value = TargetRow {
        account: row
            .get::<_, Option<String>>(0)?
            .ok_or(PersistenceError::UnsafeSyncStage)?,
        calendar: row
            .get::<_, Option<String>>(1)?
            .ok_or(PersistenceError::UnsafeSyncStage)?,
        route: row
            .get::<_, Option<String>>(2)?
            .ok_or(PersistenceError::UnsafeSyncStage)?,
        privileges: row
            .get::<_, Option<String>>(3)?
            .ok_or(PersistenceError::UnsafeSyncStage)?,
        available: row.get(4)?,
    };
    if rows.next()?.is_some() {
        return Err(PersistenceError::IdentityConflict);
    }
    if Uuid::parse_str(&value.account).is_err()
        || Uuid::parse_str(&value.calendar).is_err()
        || !value.available
        || !collection_content_writable(&value.privileges)
        || !safe_route(&value.route)
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    Ok(value)
}

pub(in crate::adapters) fn safe_route(route: &str) -> bool {
    if route.contains(['%', '\\', '?', '#'])
        || route.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let path = if route.starts_with('/') && !route.starts_with("//") {
        route
    } else {
        let Ok(url) = url::Url::parse(route) else {
            return false;
        };
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
            || url.port_or_known_default() != Some(443)
            || url.as_str() != route
        {
            return false;
        }
        return safe_path(url.path());
    };
    safe_path(path)
}

fn safe_path(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && path.ends_with('/')
        && !path.contains("//")
        && !path.split('/').any(|s| s == "." || s == "..")
}

pub(super) fn capture(
    connection: &Connection,
    epochs: &mut HashMap<String, Uuid>,
    selection: ProbeTargetSelection,
    actor: Weak<()>,
) -> Result<CommittedProbeTarget, PersistenceError> {
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let row = target(connection, selection)?;
    let epoch = *epochs
        .entry(row.account.clone())
        .or_insert_with(Uuid::new_v4);
    Ok(CommittedProbeTarget {
        selection,
        row,
        epoch,
        actor,
    })
}

pub(super) fn revalidate(
    connection: &Connection,
    epochs: &HashMap<String, Uuid>,
    snapshot: CommittedProbeTarget,
) -> Result<CommittedProbeTarget, PersistenceError> {
    if snapshot.actor.upgrade().is_none()
        || epochs.get(&snapshot.row.account) != Some(&snapshot.epoch)
        || target(connection, snapshot.selection)? != snapshot.row
    {
        return Err(PersistenceError::StaleSyncCapture);
    }
    Ok(snapshot)
}
