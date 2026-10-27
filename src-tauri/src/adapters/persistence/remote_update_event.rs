//! Actor-only event-v3 wrappers; no terminal cache/projection/jobs or sender.
use super::*;
use remote_update_fields::UpdateKind;

pub(super) const MIGRATION: &str = r#"
DROP INDEX outbox_one_pending_resource;
DROP INDEX outbox_by_account_state;
DROP INDEX outbox_claim_token;
DROP INDEX outbox_terminal_base_by_resource;
DROP INDEX conflicts_unresolved;
DROP INDEX conflicts_retry_operation;
ALTER TABLE conflicts RENAME TO conflicts_v42;
ALTER TABLE outbox_operations RENAME TO outbox_operations_v42;
CREATE TABLE outbox_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    operation_kind TEXT NOT NULL CHECK(operation_kind IN ('update_title','update_fields','update_event')),
    base_etag TEXT NOT NULL CHECK(length(base_etag) BETWEEN 1 AND 4096),
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    intent_json TEXT NOT NULL CHECK(
        (operation_kind='update_title' AND length(intent_json) BETWEEN 2 AND 4096)
        OR (operation_kind IN ('update_fields','update_event') AND length(CAST(intent_json AS BLOB)) BETWEEN 2 AND 32768)),
    state TEXT NOT NULL CHECK(state IN ('pending','conflict','applied','cancelled')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    execution_phase TEXT NOT NULL DEFAULT 'ready' CHECK(execution_phase IN ('ready','verify_first')),
    claim_token TEXT,
    claimed_at_ms INTEGER,
    last_outcome TEXT CHECK(last_outcome IS NULL OR last_outcome IN ('transient','ambiguous','unauthorized','unsafe'))
) STRICT;
CREATE TABLE conflicts (
    conflict_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE REFERENCES outbox_operations(operation_id) ON DELETE CASCADE,
    reason TEXT NOT NULL CHECK(reason IN ('concurrent_remote_change')),
    base_etag TEXT NOT NULL,
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    local_intent_json TEXT NOT NULL CHECK(length(CAST(local_intent_json AS BLOB)) BETWEEN 2 AND 32768),
    remote_etag TEXT NOT NULL,
    remote_raw_ics BLOB NOT NULL CHECK(length(remote_raw_ics) BETWEEN 1 AND 4194304),
    resolution_state TEXT NOT NULL CHECK(resolution_state IN ('unresolved','keep_remote','retry_local')),
    created_at_ms INTEGER NOT NULL,
    resolved_at_ms INTEGER,
    retry_operation_id TEXT REFERENCES outbox_operations(operation_id) ON DELETE RESTRICT
) STRICT;
INSERT INTO outbox_operations SELECT * FROM outbox_operations_v42;
INSERT INTO conflicts SELECT * FROM conflicts_v42;
DROP TABLE conflicts_v42;
DROP TABLE outbox_operations_v42;
CREATE UNIQUE INDEX outbox_one_pending_resource ON outbox_operations(resource_id) WHERE state='pending';
CREATE INDEX outbox_by_account_state ON outbox_operations(account_id,state,created_at_ms,operation_id);
CREATE UNIQUE INDEX outbox_claim_token ON outbox_operations(claim_token) WHERE claim_token IS NOT NULL;
CREATE INDEX outbox_terminal_base_by_resource ON outbox_operations(resource_id,base_etag);
CREATE INDEX conflicts_unresolved ON conflicts(resolution_state,created_at_ms,conflict_id);
CREATE UNIQUE INDEX conflicts_retry_operation ON conflicts(retry_operation_id) WHERE retry_operation_id IS NOT NULL;
"#;

pub(super) fn queue(
    connection: &mut Connection,
    input: QueueRemoteEventUpdateV3,
    now: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    let input = input.validate()?;
    let json = crate::domain::remote_event_update::encode_event_intent_v3(&input.fields)?;
    let json = String::from_utf8(json).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    remote_update_fields::queue_encoded(
        connection,
        input.idempotency_key,
        input.resource_id,
        json,
        UpdateKind::EventV3,
        now,
    )
}
pub(super) fn claim(
    c: &mut Connection,
    account: &str,
    now: i64,
) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
    Ok(
        remote_update_fields::claim_kind(c, account, now, UpdateKind::EventV3)?
            .map(|captured| RemoteEventMutationClaimV3 { captured }),
    )
}
pub(super) fn read_allowed(
    c: &Connection,
    claim: &RemoteEventMutationClaimV3,
    now: i64,
) -> Result<bool, PersistenceError> {
    remote_update_fields::read_allowed_kind(c, &claim.captured, now, UpdateKind::EventV3)
}
pub(super) fn snapshot(
    c: &Connection,
    claim: &RemoteEventMutationClaimV3,
    now: i64,
) -> Result<RemoteEventUpdateSnapshotV3, PersistenceError> {
    let captured =
        remote_update_fields::snapshot_kind(c, &claim.captured, now, UpdateKind::EventV3)?;
    let fields = crate::domain::remote_event_update::decode_event_intent_v3(
        claim.captured.intent_json.as_bytes(),
    )?;
    let proof = crate::domain::remote_event_update::patch_single_utc_event_v3(
        &claim.captured.base_raw_ics,
        &fields,
    )?;
    if proof.bytes() != captured.expected_raw_ics {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(RemoteEventUpdateSnapshotV3 {
        captured,
        before: proof.before_time(),
        after: proof.after_time(),
        alarms: proof.time_alarm_evidence().cloned(),
    })
}
pub(super) fn defer(
    c: &Connection,
    claim: &RemoteEventMutationClaimV3,
    uncertain: bool,
    now: i64,
) -> Result<(), PersistenceError> {
    remote_update_fields::defer_kind(c, &claim.captured, uncertain, now, UpdateKind::EventV3)
}
pub(super) fn restore_base(
    c: &mut Connection,
    claim: &RemoteEventMutationClaimV3,
    remote: &RemoteReadBack,
    now: i64,
) -> Result<RemoteEventMutationClaimV3, PersistenceError> {
    Ok(RemoteEventMutationClaimV3 {
        captured: remote_update_fields::restore_base_kind(
            c,
            &claim.captured,
            remote,
            now,
            UpdateKind::EventV3,
        )?,
    })
}
pub(super) fn sync_conflict(
    tx: &Transaction<'_>,
    resource: &str,
    remote: &RemoteReadBack,
    now: i64,
) -> Result<(), PersistenceError> {
    remote_update_fields::sync_conflict_kind(tx, resource, remote, now, UpdateKind::EventV3)
}
pub(super) fn resolve(
    c: &mut Connection,
    id: &str,
    choice: ConflictResolutionChoice,
    now: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    remote_update_fields::resolve_kind(c, id, choice, now, UpdateKind::EventV3)
}

/// Process-local event claims cannot survive actor restart. Preserve intent/base
/// and monotonically require exact-base recovery before future ready work.
pub(super) fn recover_after_restart(
    connection: &Connection,
    now: Option<i64>,
) -> Result<(), PersistenceError> {
    connection.execute("UPDATE outbox_operations SET claim_token=NULL, claimed_at_ms=NULL, execution_phase='verify_first', last_outcome='ambiguous', updated_at_ms=COALESCE(?1,updated_at_ms) WHERE operation_kind='update_event' AND state='pending' AND (claim_token IS NOT NULL OR claimed_at_ms IS NOT NULL)", [now])?;
    Ok(())
}

pub(super) fn finish(
    c: &mut Connection,
    claim: &RemoteEventMutationClaimV3,
    remote: &RemoteReadBack,
    applied: bool,
    now: i64,
) -> Result<bool, PersistenceError> {
    remote_update_fields::finish_event(c, &claim.captured, remote, applied, now)
}
