//! Read-only actor preview. No schema/import/epoch/ledger/transport changes.
use super::{Connection, PersistenceError};
use crate::adapters::production_mutation::probe_preflight::first_route::{
    CollectionRevisionCapability, FirstRoutePreviewParts,
};

pub(crate) struct FirstRoutePreview {
    pub(crate) revision: CollectionRevisionCapability,
    pub(crate) existing_exact_route: bool,
    pub(crate) first_route_source_available: bool,
    pub(crate) durable_import_available: bool,
    pub(crate) execution_available: bool,
}

pub(super) fn preview(
    connection: &Connection,
    parts: FirstRoutePreviewParts,
) -> Result<FirstRoutePreview, PersistenceError> {
    let tx = connection.unchecked_transaction()?;
    if !parts.actor_owner_current()
        || parts.actor.upgrade().is_none()
        || !uuid::Uuid::parse_str(&parts.account)
            .is_ok_and(|id| !id.is_nil() && id.to_string() == parts.account)
        || !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=?1 AND provider_kind='caldav' AND region_profile='icloud-cn' AND enabled=1 AND credential_configured=1 AND remote_editing_enabled=1 AND removed_at_ms IS NULL)",
            [&parts.account], |r| r.get::<_, bool>(0),
        )?
    {
        return Err(PersistenceError::StaleSyncCapture);
    }
    // Only the two authorized names. Bound materialization before inspection;
    // do not hide an unavailable or foreign-account alias with eligibility.
    let mut stmt = tx.prepare("SELECT CASE WHEN length(CAST(account_id AS BLOB))=36 THEN account_id END, display_name, CASE WHEN length(CAST(collection_url AS BLOB)) BETWEEN 2 AND 2048 THEN collection_url END FROM calendars WHERE display_name IN ('Calendar TODO Probe','Calendar TODO Probe 2') COLLATE BINARY LIMIT 3")?;
    let mut rows = stmt.query([])?;
    let mut count = 0;
    let mut selected_seen = false;
    let mut names = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        count += 1;
        let account: Option<String> = row.get(0)?;
        let name: String = row.get(1)?;
        let route: Option<String> = row.get(2)?;
        let same_target = route.as_deref().is_some_and(|route| {
            parts
                .target
                .join(route)
                .is_ok_and(|url| url == parts.target)
        });
        if count > 2
            || account.as_deref() != Some(&parts.account)
            || !route
                .as_deref()
                .is_some_and(super::probe_target::safe_route)
            || !names.insert(name.clone())
        {
            return Err(PersistenceError::IdentityConflict);
        }
        if name == parts.selection.name() {
            if selected_seen || account.as_deref() != Some(&parts.account) || !same_target {
                return Err(PersistenceError::IdentityConflict);
            }
            selected_seen = true;
        } else if same_target {
            return Err(PersistenceError::IdentityConflict);
        }
    }
    drop(rows);
    drop(stmt);
    if !parts.actor_owner_current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(FirstRoutePreview {
        revision: parts.revision,
        existing_exact_route: selected_seen,
        first_route_source_available: false,
        durable_import_available: false,
        execution_available: false,
    })
}
