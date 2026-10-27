use super::*;
use crate::domain::remote_crud::{RemoteCrudKind as Kind, RemoteCrudOperation, RemoteCrudStatus};
fn canonical_id(id: &str) -> Result<(), PersistenceError> {
    if !Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(())
}
fn source(kind: Kind) -> (&'static str, &'static str, &'static str) {
    match kind {
        Kind::CreateUtcV2 => (
            "remote_create_operations",
            "intent_version='utc-v2'",
            "NULL,NULL",
        ),
        Kind::UpdateEventV3 => (
            "outbox_operations",
            "operation_kind='update_event'",
            "c.resolution_state,c.retry_operation_id",
        ),
        Kind::Delete => (
            "remote_delete_operations",
            "1=1",
            "o.resolution_state,o.retry_operation_id",
        ),
    }
}
pub(super) fn status(
    connection: &mut Connection,
    account: &str,
    kind: Kind,
) -> Result<RemoteCrudStatus, PersistenceError> {
    canonical_id(account)?;
    let tx = connection.transaction()?;
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=?1)",
        [account],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::NotFound);
    }
    let (table, selector, conflict) = source(kind);
    let count: i64 = tx.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE account_id=?1 AND {selector}"),
        [account],
        |r| r.get(0),
    )?;
    let join = if kind == Kind::UpdateEventV3 {
        "LEFT JOIN conflicts c ON c.operation_id=o.operation_id"
    } else {
        ""
    };
    let operations = {
        let mut stmt=tx.prepare(&format!("SELECT o.operation_id,o.calendar_id,o.resource_id,o.state,o.created_at_ms,o.updated_at_ms,{conflict} FROM {table} o {join} WHERE o.account_id=?1 AND {selector} ORDER BY o.updated_at_ms DESC,o.operation_id LIMIT 100"))?;
        let rows = stmt
            .query_map([account], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(
                    operation_id,
                    calendar_id,
                    resource_id,
                    state,
                    created_at_ms,
                    updated_at_ms,
                    resolution,
                    retry_operation_id,
                )| {
                    for id in [&operation_id, &calendar_id, &resource_id] {
                        canonical_id(id)?;
                    }
                    if let Some(id) = &retry_operation_id {
                        canonical_id(id)?;
                    }
                    Ok(RemoteCrudOperation {
                        revision: 1,
                        predecessor_operation_id: None,
                        stage: state.clone(),
                        operation_id,
                        calendar_id,
                        resource_id,
                        state: RemoteMutationState::from_storage(&state)?,
                        created_at_ms,
                        updated_at_ms,
                        resolution_state: resolution
                            .as_deref()
                            .map(ConflictResolutionState::from_storage)
                            .transpose()?,
                        retry_operation_id,
                    })
                },
            )
            .collect::<Result<Vec<_>, PersistenceError>>()?
    };
    tx.commit()?;
    Ok(RemoteCrudStatus {
        account_id: account.into(),
        kind,
        operation_count: u32::try_from(count).unwrap_or(u32::MAX),
        operations,
        production_execution_available: false,
    })
}
pub(super) fn resolve(
    connection: &mut Connection,
    kind: Kind,
    id: &str,
    choice: ConflictResolutionChoice,
    now: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    canonical_id(id)?;
    if kind == Kind::CreateUtcV2 {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let (table, selector, _) = source(kind);
    if !connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE operation_id=?1 AND {selector})"),
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::NotFound);
    }
    match kind {
        Kind::UpdateEventV3 => remote_update_event::resolve(connection, id, choice, now),
        Kind::Delete => resolve_remote_deletion_conflict_scoped(connection, id, choice, now, true),
        Kind::CreateUtcV2 => unreachable!(),
    }
}
