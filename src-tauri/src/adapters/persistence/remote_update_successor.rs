//! Durable semantic UPDATE successor. Rows never authorize execution.
use super::*;
use crate::domain::remote_event_update::{
    RemoteEventUpdateFieldsV3, decode_event_intent_v3, encode_event_intent_v3,
    patch_single_utc_event_v3,
};

pub(super) const MIGRATION: &str = r#"
ALTER TABLE outbox_operations ADD COLUMN update_revision INTEGER NOT NULL DEFAULT 1 CHECK(update_revision>=1);
ALTER TABLE outbox_operations ADD COLUMN predecessor_operation_id TEXT REFERENCES outbox_operations(operation_id) ON DELETE RESTRICT;
CREATE TABLE remote_update_successors (
 operation_id TEXT PRIMARY KEY NOT NULL,
 idempotency_key TEXT NOT NULL UNIQUE,
 predecessor_operation_id TEXT NOT NULL UNIQUE REFERENCES outbox_operations(operation_id) ON DELETE RESTRICT,
 account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
 calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
 resource_id TEXT NOT NULL UNIQUE REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
 series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
 intent_json TEXT NOT NULL CHECK(length(CAST(intent_json AS BLOB)) BETWEEN 2 AND 32768),
 revision INTEGER NOT NULL CHECK(revision>=1),
 state TEXT NOT NULL CHECK(state IN ('pending','blocked_conflict','cancelled')),
 created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE remote_update_submissions (
 idempotency_key TEXT PRIMARY KEY NOT NULL,
 resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
 operation_id TEXT NOT NULL,
 intent_json TEXT NOT NULL CHECK(length(CAST(intent_json AS BLOB)) BETWEEN 2 AND 32768)
) STRICT;
CREATE INDEX remote_update_successor_by_account ON remote_update_successors(account_id,updated_at_ms,operation_id);
"#;

pub(super) fn merge(
    mut previous: RemoteEventUpdateFieldsV3,
    next: &RemoteEventUpdateFieldsV3,
) -> Result<RemoteEventUpdateFieldsV3, PersistenceError> {
    if let Some(v) = &next.title {
        previous.title = Some(v.clone());
    }
    if let Some(v) = &next.description {
        previous.description = Some(v.clone());
    }
    if let Some(v) = &next.location {
        previous.location = Some(v.clone());
    }
    if let Some(v) = next.time {
        previous.time = Some(v);
    }
    Ok(previous.validate()?)
}

pub(super) struct Head {
    pub id: String,
    pub account: String,
    pub calendar: String,
    pub series: String,
    pub expected: Vec<u8>,
}
/// Recompute immutable predecessor proof and current identity; never use a UI projection.
pub(super) fn head(c: &Connection, resource: &str) -> Result<Option<Head>, PersistenceError> {
    let row=c.query_row("SELECT o.operation_id,o.account_id,o.calendar_id,o.series_id,o.base_etag,o.base_raw_ics,o.intent_json,r.etag,r.raw_ics,s.uid,c.collection_url,r.exact_href FROM outbox_operations o JOIN remote_resources r ON r.resource_id=o.resource_id AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id WHERE o.resource_id=?1 AND o.state='pending' AND o.operation_kind='update_event' AND r.lifecycle='active'",[resource],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,Vec<u8>>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?,r.get::<_,String>(11)?))).optional()?;
    let Some((
        id,
        account,
        calendar,
        series,
        etag,
        raw,
        json,
        current_etag,
        current_raw,
        uid,
        route,
        href,
    )) = row
    else {
        return Ok(None);
    };
    scoped_event::selected_calendar_route(c, &account, &calendar)?;
    if etag!=current_etag || raw!=current_raw || !strong_mutation_etag(&etag) || !remote_update_fields::bounded_fields_route(&route,&href)
 || c.query_row("SELECT EXISTS(SELECT 1 FROM calendar_resource_recurrence WHERE resource_id=?1 AND pending_operation_id IS NOT NULL) OR EXISTS(SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending')",[resource],|r|r.get::<_,bool>(0))?
 || remote_creation::has_pending_target(c,resource)? { return Err(PersistenceError::RemoteMutationReadOnly); }
    let fields = decode_event_intent_v3(json.as_bytes())?;
    let patch = patch_single_utc_event_v3(&raw, &fields)?;
    if patch.uid() != uid {
        return Err(PersistenceError::IdentityConflict);
    }
    Ok(Some(Head {
        id,
        account,
        calendar,
        series,
        expected: patch.bytes().to_vec(),
    }))
}

pub(super) fn desired(
    c: &Connection,
    resource: &str,
    head: &Head,
) -> Result<Vec<u8>, PersistenceError> {
    let pending=c.query_row("SELECT intent_json FROM remote_update_successors WHERE resource_id=?1 AND predecessor_operation_id=?2 AND state='pending'",params![resource,head.id],|r|r.get::<_,String>(0)).optional()?;
    Ok(match pending {
        Some(json) => {
            patch_single_utc_event_v3(&head.expected, &decode_event_intent_v3(json.as_bytes())?)?
                .bytes()
                .to_vec()
        }
        None => head.expected.clone(),
    })
}

pub(super) fn admit(
    tx: &Transaction<'_>,
    input: QueueRemoteEventUpdateV3,
    now: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    let input = input.validate()?;
    let json = String::from_utf8(encode_event_intent_v3(&input.fields)?)
        .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    if let Some((resource,op,saved))=tx.query_row("SELECT resource_id,operation_id,intent_json FROM remote_update_submissions WHERE idempotency_key=?1",[&input.idempotency_key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()? {
  if resource!=input.resource_id || saved!=json {return Err(PersistenceError::IdempotencyConflict)}
  return summary(tx,&op);
 }
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE idempotency_key=?1 UNION ALL SELECT 1 FROM remote_create_operations WHERE idempotency_key=?1 UNION ALL SELECT 1 FROM remote_delete_operations WHERE idempotency_key=?1)",[&input.idempotency_key],|r|r.get::<_,bool>(0))? {return Err(PersistenceError::IdempotencyConflict)}
    let head = head(tx, &input.resource_id)?.ok_or(PersistenceError::StaleSyncCapture)?;
    let old=tx.query_row("SELECT operation_id,intent_json,revision,state FROM remote_update_successors WHERE resource_id=?1",[&input.resource_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?))).optional()?;
    let (op, fields) = if let Some((op, old, revision, state)) = old {
        if state != "pending" || revision == i64::MAX {
            return Err(PersistenceError::PendingMutationExists);
        }
        (
            op,
            merge(decode_event_intent_v3(old.as_bytes())?, &input.fields)?,
        )
    } else {
        (Uuid::new_v4().to_string(), input.fields.clone())
    };
    patch_single_utc_event_v3(&head.expected, &fields)?;
    let merged = String::from_utf8(encode_event_intent_v3(&fields)?)
        .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    tx.execute("INSERT INTO remote_update_successors(operation_id,idempotency_key,predecessor_operation_id,account_id,calendar_id,resource_id,series_id,intent_json,revision,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,'pending',?9,?9) ON CONFLICT(resource_id) DO UPDATE SET intent_json=excluded.intent_json,revision=remote_update_successors.revision+1,updated_at_ms=excluded.updated_at_ms",params![op,input.idempotency_key,head.id,head.account,head.calendar,input.resource_id,head.series,merged,now])?;
    tx.execute(
        "INSERT INTO remote_update_submissions VALUES(?1,?2,?3,?4)",
        params![input.idempotency_key, input.resource_id, op, json],
    )?;
    summary(tx, &op)
}
fn summary(c: &Connection, op: &str) -> Result<RemoteMutationSummary, PersistenceError> {
    c.query_row("SELECT operation_id,idempotency_key,account_id,resource_id,created_at_ms,updated_at_ms FROM remote_update_successors WHERE operation_id=?1 UNION ALL SELECT operation_id,idempotency_key,account_id,resource_id,created_at_ms,updated_at_ms FROM outbox_operations WHERE operation_id=?1",[op],|r|Ok(RemoteMutationSummary{operation_id:r.get(0)?,idempotency_key:r.get(1)?,account_id:r.get(2)?,resource_id:r.get(3)?,state:RemoteMutationState::Pending,created_at_ms:r.get(4)?,updated_at_ms:r.get(5)?})).map_err(Into::into)
}

/// Invoked only within independently verified event terminal transaction.
pub(super) fn terminal(
    tx: &Transaction<'_>,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    applied: bool,
    now: i64,
) -> Result<(), PersistenceError> {
    if !applied {
        tx.execute("UPDATE remote_update_successors SET state='blocked_conflict',updated_at_ms=?2 WHERE predecessor_operation_id=?1",params![claim.operation_id,now])?;
        return Ok(());
    }
    let row=tx.query_row("SELECT operation_id,idempotency_key,intent_json,account_id,calendar_id,resource_id,series_id,created_at_ms,revision FROM remote_update_successors WHERE predecessor_operation_id=?1 AND state='pending'",[&claim.operation_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?))).optional()?;
    let Some((op, key, json, account, calendar, resource, series, created, revision)) = row else {
        return Ok(());
    };
    if account != claim.account_id
        || calendar != claim.calendar_id
        || resource != claim.resource_id
        || series != claim.series_id
    {
        return Err(PersistenceError::IdentityConflict);
    }
    let fields = decode_event_intent_v3(json.as_bytes())?;
    let patch = patch_single_utc_event_v3(&remote.raw_ics, &fields)?;
    if patch.uid() != claim.uid {
        return Err(PersistenceError::IdentityConflict);
    }
    let state = if patch.bytes() == remote.raw_ics {
        "applied"
    } else {
        "pending"
    };
    tx.execute(
        "DELETE FROM remote_update_successors WHERE operation_id=?1",
        [&op],
    )?;
    tx.execute("INSERT INTO outbox_operations(operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,operation_kind,base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms,update_revision,predecessor_operation_id) VALUES(?1,?2,?3,?4,?5,?6,'update_event',?7,?8,?9,?10,?11,?12,?13,?14)",params![op,key,account,calendar,resource,series,remote.etag,remote.raw_ics,json,state,created,now,revision,claim.operation_id])?;
    Ok(())
}
