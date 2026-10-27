use super::*;
use crate::domain::remote_mutation::{
    RemoteEventTextFieldsV2, patch_single_vevent_text_fields_v2, single_vevent_uid_v2,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UpdateKind {
    Fields,
    EventV3,
}
impl UpdateKind {
    fn storage(self) -> &'static str {
        match self {
            Self::Fields => "update_fields",
            Self::EventV3 => "update_event",
        }
    }
}
fn patch_intent(
    kind: UpdateKind,
    raw: &[u8],
    uid: &str,
    json: &str,
) -> Result<Vec<u8>, PersistenceError> {
    match kind {
        UpdateKind::Fields => {
            let intent = parse_stored_fields_intent(json)?;
            qualify_utc_text_base(raw, uid)?;
            patch_single_vevent_text_fields_v2(raw, &intent.fields)
                .map_err(|_| PersistenceError::RemoteMutationReadOnly)
        }
        UpdateKind::EventV3 => {
            use crate::domain::remote_event_update::{
                decode_event_intent_v3, patch_single_utc_event_v3,
            };
            let fields = decode_event_intent_v3(json.as_bytes())?;
            let patch = patch_single_utc_event_v3(raw, &fields)?;
            if patch.uid() != uid {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            Ok(patch.bytes().to_vec())
        }
    }
}

// Match the byte limit independently enforced on committed sync routes.
pub(super) fn bounded_fields_route(collection: &str, resource: &str) -> bool {
    collection.len() <= 4096
        && resource.len() <= 4096
        && resource_belongs_to_collection(collection, resource)
}

/// Migration 041 rebuilds both FK-linked stores without rewriting any old row.
pub(super) const MIGRATION: &str = r#"
DROP INDEX outbox_one_pending_resource;
DROP INDEX outbox_by_account_state;
DROP INDEX outbox_claim_token;
DROP INDEX outbox_terminal_base_by_resource;
DROP INDEX conflicts_unresolved;
DROP INDEX conflicts_retry_operation;
ALTER TABLE conflicts RENAME TO conflicts_v40;
ALTER TABLE outbox_operations RENAME TO outbox_operations_v40;
CREATE TABLE outbox_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL REFERENCES remote_resources(resource_id) ON DELETE RESTRICT,
    series_id TEXT NOT NULL REFERENCES event_series(series_id) ON DELETE RESTRICT,
    operation_kind TEXT NOT NULL CHECK(operation_kind IN ('update_title','update_fields')),
    base_etag TEXT NOT NULL CHECK(length(base_etag) BETWEEN 1 AND 4096),
    base_raw_ics BLOB NOT NULL CHECK(length(base_raw_ics) BETWEEN 1 AND 4194304),
    intent_json TEXT NOT NULL CHECK(
        (operation_kind='update_title' AND length(intent_json) BETWEEN 2 AND 4096)
        OR (operation_kind='update_fields' AND length(CAST(intent_json AS BLOB)) BETWEEN 2 AND 32768)),
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
INSERT INTO outbox_operations SELECT * FROM outbox_operations_v40;
INSERT INTO conflicts SELECT * FROM conflicts_v40;
DROP TABLE conflicts_v40;
DROP TABLE outbox_operations_v40;
CREATE UNIQUE INDEX outbox_one_pending_resource ON outbox_operations(resource_id) WHERE state='pending';
CREATE INDEX outbox_by_account_state ON outbox_operations(account_id,state,created_at_ms,operation_id);
CREATE UNIQUE INDEX outbox_claim_token ON outbox_operations(claim_token) WHERE claim_token IS NOT NULL;
CREATE INDEX outbox_terminal_base_by_resource ON outbox_operations(resource_id,base_etag);
CREATE INDEX conflicts_unresolved ON conflicts(resolution_state,created_at_ms,conflict_id);
CREATE UNIQUE INDEX conflicts_retry_operation ON conflicts(retry_operation_id) WHERE retry_operation_id IS NOT NULL;
"#;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredFieldsIntent {
    version: String,
    pub(super) fields: RemoteEventTextFieldsV2,
}

pub(super) fn parse_stored_fields_intent(
    json: &str,
) -> Result<StoredFieldsIntent, PersistenceError> {
    if json.len() > 32 * 1024 {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let intent: StoredFieldsIntent =
        serde_json::from_str(json).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    if intent.version != "text-v2"
        || intent.fields.clone().validate().ok().as_ref() != Some(&intent.fields)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(intent)
}

/// Full resource qualification for admission. Text structural proof has already
/// run; this adds product UTC shape without editing times or alarm semantics.
pub(super) fn qualify_utc_text_base(raw: &[u8], uid: &str) -> Result<(), PersistenceError> {
    use crate::domain::calendar_recurrence::{RecurrenceValueKind, canonical_recurrence_identity};
    if !single_vevent_uid_v2(raw).is_ok_and(|parsed| parsed == uid)
        || contains_invitation_property(raw)
        || contains_recurrence_sensitive_property(raw)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let text = std::str::from_utf8(raw).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    let unfolded = text.replace("\r\n ", "").replace("\r\n\t", "");
    let mut stack: Vec<&str> = Vec::new();
    let mut version = None;
    let mut method = None;
    let mut start = None;
    let mut end = None;
    for line in unfolded.split_terminator("\r\n") {
        let Some((header, value)) = line.split_once(':') else {
            return Err(PersistenceError::RemoteMutationReadOnly);
        };
        let name = header.split(';').next().unwrap_or("");
        if name.eq_ignore_ascii_case("BEGIN") {
            if value.eq_ignore_ascii_case("VTIMEZONE") {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            stack.push(value);
            continue;
        }
        if name.eq_ignore_ascii_case("END") {
            stack.pop();
            continue;
        }
        if stack.len() == 2
            && (name.eq_ignore_ascii_case("DURATION") || name.eq_ignore_ascii_case("VTIMEZONE"))
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        if name.eq_ignore_ascii_case("VERSION") || name.eq_ignore_ascii_case("METHOD") {
            if stack.len() != 1
                || !stack[0].eq_ignore_ascii_case("VCALENDAR")
                || header.contains(';')
            {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            if name.eq_ignore_ascii_case("VERSION") {
                if value != "2.0" || version.replace(value).is_some() {
                    return Err(PersistenceError::RemoteMutationReadOnly);
                }
            } else if !value.eq_ignore_ascii_case("PUBLISH") || method.replace(value).is_some() {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
        }
        if stack.len() != 2 || !stack[1].eq_ignore_ascii_case("VEVENT") {
            continue;
        }
        if name.eq_ignore_ascii_case("DTSTART") || name.eq_ignore_ascii_case("DTEND") {
            let params = header.split(';').skip(1).collect::<Vec<_>>();
            if !(params.is_empty()
                || params.len() == 1 && params[0].eq_ignore_ascii_case("VALUE=DATE-TIME"))
                || !value.is_ascii()
            {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let identity = canonical_recurrence_identity(&[], value)
                .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
            if identity.value_kind != RecurrenceValueKind::UtcDateTime
                || !("19700101T000000Z".."21000101T000000Z").contains(&value)
            {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let slot = if name.eq_ignore_ascii_case("DTSTART") {
                &mut start
            } else {
                &mut end
            };
            if slot.replace(value).is_some() {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
        }
    }
    if version.is_none() || !matches!((start,end),(Some(start),Some(end)) if start < end) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(())
}

pub(super) fn queue(
    connection: &mut Connection,
    input: QueueRemoteEventTextUpdateV2,
    now_ms: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    admitted_now_ms(now_ms)?;
    let input = input.validate()?;
    let intent_json = serde_json::to_string(&StoredFieldsIntent {
        version: "text-v2".into(),
        fields: input.fields.clone(),
    })
    .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    if intent_json.len() > 32 * 1024 {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    queue_encoded(
        connection,
        input.idempotency_key,
        input.resource_id,
        intent_json,
        UpdateKind::Fields,
        now_ms,
    )
}
pub(super) fn queue_encoded(
    connection: &mut Connection,
    idempotency_key: String,
    resource_id: String,
    intent_json: String,
    kind: UpdateKind,
    now_ms: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    let transaction = connection.transaction()?;
    let result = queue_encoded_in(
        &transaction,
        idempotency_key,
        resource_id,
        intent_json,
        kind,
        now_ms,
    )?;
    transaction.commit()?;
    Ok(result)
}

pub(super) fn queue_encoded_in(
    transaction: &Transaction<'_>,
    idempotency_key: String,
    resource_id: String,
    intent_json: String,
    kind: UpdateKind,
    now_ms: i64,
) -> Result<RemoteMutationSummary, PersistenceError> {
    admitted_now_ms(now_ms)?;
    validate_account_id(&idempotency_key)?;
    validate_account_id(&resource_id)?;
    let target_resource_id = resource_id.clone();
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE idempotency_key=?1)",
        [&idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_delete_operations WHERE idempotency_key=?1)",
        [&idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }

    let existing = transaction
        .query_row(
            "SELECT operation_id,idempotency_key,account_id,resource_id,state,created_at_ms,updated_at_ms,intent_json,operation_kind
             FROM outbox_operations WHERE idempotency_key=?1",
            [&idempotency_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?;
    if let Some((
        operation_id,
        idempotency_key,
        account_id,
        resource_id,
        state,
        created_at_ms,
        updated_at_ms,
        stored_intent,
        operation_kind,
    )) = existing
    {
        if operation_kind != kind.storage() {
            return Err(PersistenceError::IdempotencyConflict);
        }
        let same_intent = match kind {
            UpdateKind::Fields => {
                parse_stored_fields_intent(&stored_intent)?.fields
                    == parse_stored_fields_intent(&intent_json)?.fields
            }
            UpdateKind::EventV3 => {
                crate::domain::remote_event_update::decode_event_intent_v3(
                    stored_intent.as_bytes(),
                )?;
                stored_intent == intent_json
            }
        };
        if resource_id != target_resource_id || !same_intent {
            return Err(PersistenceError::IdempotencyConflict);
        }
        return Ok(RemoteMutationSummary {
            operation_id,
            idempotency_key,
            account_id,
            resource_id,
            state: RemoteMutationState::from_storage(&state)?,
            created_at_ms,
            updated_at_ms,
        });
    }

    type MutationBase = (
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        String,
        String,
        String,
    );
    let base: MutationBase = transaction
        .query_row(
            "SELECT a.account_id,c.calendar_id,r.series_id,r.etag,r.raw_ics,r.parse_status,
                    r.lifecycle,c.privileges,a.enabled,a.removed_at_ms,c.available,
                    c.collection_url,r.exact_href,s.uid
             FROM remote_resources r
             JOIN calendars c ON c.calendar_id=r.calendar_id
             JOIN accounts a ON a.account_id=c.account_id
             JOIN event_series s ON s.series_id=r.series_id AND s.account_id=a.account_id
             WHERE r.resource_id=?1",
            [&resource_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let (
        account_id,
        calendar_id,
        series_id,
        base_etag,
        base_raw_ics,
        parse_status,
        lifecycle,
        privileges,
        enabled,
        removed_at_ms,
        available,
        collection_href,
        exact_href,
        series_uid,
    ) = base;
    if !enabled || removed_at_ms.is_some() || !available {
        return Err(PersistenceError::AccountDisabled);
    }
    if lifecycle != "active" || (kind == UpdateKind::Fields && parse_status != "parsed") {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let (provider, region, credential_configured, opted_in): (String, String, bool, bool) = transaction.query_row(
        "SELECT provider_kind,region_profile,credential_configured,remote_editing_enabled FROM accounts WHERE account_id=?1",
        [&account_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    if provider != "caldav" || region != "icloud-cn" || !credential_configured || !opted_in {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if kind == UpdateKind::EventV3 {
        for identity in [&account_id, &calendar_id, &series_id] {
            validate_account_id(identity)?;
        }
    }
    patch_intent(kind, &base_raw_ics, &series_uid, &intent_json)?;
    let can_write = collection_content_writable(&privileges);
    if !can_write
        || !bounded_fields_route(&collection_href, &exact_href)
        || !strong_mutation_etag(&base_etag)
        || (kind == UpdateKind::Fields
            && (contains_invitation_property(&base_raw_ics)
                || !single_vevent_uid(&base_raw_ics).is_ok_and(|uid| uid == series_uid)))
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let pending = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending'
            UNION ALL
            SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending'
         )",
        [&resource_id],
        |row| row.get::<_, bool>(0),
    )?;
    if pending || remote_creation::has_pending_target(&transaction, &resource_id)? {
        return Err(PersistenceError::PendingMutationExists);
    }
    if kind == UpdateKind::EventV3 && parse_status != "parsed" {
        transaction.execute(
            "UPDATE remote_resources SET parse_status='parsed' WHERE resource_id=?1",
            [&resource_id],
        )?;
    }

    let operation_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO outbox_operations(
            operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,
            operation_kind,base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms
         ) VALUES(?1,?2,?3,?4,?5,?6,?11,?7,?8,?9,'pending',?10,?10)",
        params![
            operation_id,
            idempotency_key,
            account_id,
            calendar_id,
            resource_id,
            series_id,
            base_etag,
            base_raw_ics,
            intent_json,
            now_ms,
            kind.storage()
        ],
    )?;
    Ok(RemoteMutationSummary {
        operation_id,
        idempotency_key,
        account_id,
        resource_id,
        state: RemoteMutationState::Pending,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

use crate::domain::remote_mutation::{
    RemoteFieldsMutationClaim, RemoteFieldsSendSnapshot, single_vevent_text_values_v2,
};

struct Operation {
    kind: UpdateKind,
    has_claim: bool,
    captured: RemoteFieldsMutationClaim,
    state: String,
    claimed_at_ms: Option<i64>,
    attempts: i64,
    current_etag: String,
    current_raw: Vec<u8>,
    enabled: bool,
    archived: Option<i64>,
    available: bool,
    opted_in: bool,
    configured: bool,
    provider: String,
    region: String,
    writable: String,
    parsed: String,
    lifecycle: String,
}

fn load(
    connection: &Connection,
    operation_id: &str,
    kind: UpdateKind,
) -> Result<Operation, PersistenceError> {
    connection
        .query_row(
            "SELECT o.operation_id,o.account_id,o.calendar_id,o.resource_id,o.series_id,s.uid,
          o.claim_token,c.collection_url,r.exact_href,o.base_etag,
          CASE WHEN length(o.base_raw_ics)<=65536 THEN o.base_raw_ics END,
          CASE WHEN length(CAST(o.intent_json AS BLOB))<=32768 THEN o.intent_json END,
          o.execution_phase,o.state,o.claimed_at_ms,o.attempt_count,r.etag,r.raw_ics,
          a.enabled,a.removed_at_ms,c.available,a.remote_editing_enabled,a.credential_configured,
          a.provider_kind,a.region_profile,c.privileges,r.parse_status,r.lifecycle
         FROM outbox_operations o JOIN remote_resources r ON r.resource_id=o.resource_id
          AND r.calendar_id=o.calendar_id AND r.series_id=o.series_id
         JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id
         JOIN accounts a ON a.account_id=o.account_id
         JOIN event_series s ON s.series_id=o.series_id AND s.account_id=o.account_id
         WHERE o.operation_kind=?2 AND o.operation_id=?1",
            params![operation_id, kind.storage()],
            |r| {
                Ok(Operation {
                    kind,
                    has_claim: r.get::<_, Option<String>>(6)?.is_some(),
                    captured: RemoteFieldsMutationClaim {
                        operation_id: r.get(0)?,
                        account_id: r.get(1)?,
                        calendar_id: r.get(2)?,
                        resource_id: r.get(3)?,
                        series_id: r.get(4)?,
                        uid: r.get(5)?,
                        claim_token: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        sync_epoch: None,
                        collection_href: r.get(7)?,
                        exact_href: r.get(8)?,
                        base_etag: r.get(9)?,
                        base_raw_ics: r.get::<_, Option<Vec<u8>>>(10)?.unwrap_or_default(),
                        intent_json: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                        expected_raw_ics: Vec::new(),
                        phase: RemoteMutationExecutionPhase::from_storage(&r.get::<_, String>(12)?)
                            .map_err(|e| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    12,
                                    rusqlite::types::Type::Text,
                                    Box::new(e),
                                )
                            })?,
                    },
                    state: r.get(13)?,
                    claimed_at_ms: r.get(14)?,
                    attempts: r.get(15)?,
                    current_etag: r.get(16)?,
                    current_raw: r.get(17)?,
                    enabled: r.get(18)?,
                    archived: r.get(19)?,
                    available: r.get(20)?,
                    opted_in: r.get(21)?,
                    configured: r.get(22)?,
                    provider: r.get(23)?,
                    region: r.get(24)?,
                    writable: r.get(25)?,
                    parsed: r.get(26)?,
                    lifecycle: r.get(27)?,
                })
            },
        )
        .optional()?
        .ok_or(PersistenceError::OutboxClaimLost)
}

fn expected(operation: &Operation) -> Result<Vec<u8>, PersistenceError> {
    let claim = &operation.captured;
    for identity in [
        &claim.operation_id,
        &claim.account_id,
        &claim.calendar_id,
        &claim.resource_id,
        &claim.series_id,
    ] {
        Uuid::parse_str(identity).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
    }
    if !strong_mutation_etag(&claim.base_etag) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    patch_intent(
        operation.kind,
        &claim.base_raw_ics,
        &claim.uid,
        &claim.intent_json,
    )
}
fn usable(operation: &Operation) -> Result<(), PersistenceError> {
    let claim = &operation.captured;
    if !operation.enabled || operation.archived.is_some() || !operation.available {
        return Err(PersistenceError::AccountDisabled);
    }
    if !operation.opted_in
        || !operation.configured
        || operation.provider != "caldav"
        || operation.region != "icloud-cn"
        || operation.parsed != "parsed"
        || operation.lifecycle != "active"
        || !collection_content_writable(&operation.writable)
        || !bounded_fields_route(&claim.collection_href, &claim.exact_href)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(())
}
fn same_capture(current: &RemoteFieldsMutationClaim, issued: &RemoteFieldsMutationClaim) -> bool {
    current.operation_id == issued.operation_id
        && current.account_id == issued.account_id
        && current.calendar_id == issued.calendar_id
        && current.resource_id == issued.resource_id
        && current.series_id == issued.series_id
        && current.uid == issued.uid
        && current.claim_token == issued.claim_token
        && current.collection_href == issued.collection_href
        && current.exact_href == issued.exact_href
        && current.base_etag == issued.base_etag
        && current.base_raw_ics == issued.base_raw_ics
        && current.intent_json == issued.intent_json
        && current.phase == issued.phase
        && current.expected_raw_ics == issued.expected_raw_ics
}
fn validated_claim(
    connection: &Connection,
    claim: &RemoteFieldsMutationClaim,
    now: i64,
    kind: UpdateKind,
) -> Result<Operation, PersistenceError> {
    admitted_now_ms(now)?;
    let mut operation = load(connection, &claim.operation_id, kind)?;
    if operation.state != "pending"
        || operation.captured.claim_token.is_empty()
        || !operation
            .claimed_at_ms
            .is_some_and(|t| t <= now && now.saturating_sub(t) < REMOTE_MUTATION_CLAIM_LEASE_MS)
    {
        return Err(PersistenceError::OutboxClaimLost);
    }
    usable(&operation)?;
    operation.captured.expected_raw_ics = expected(&operation)?;
    if !same_capture(&operation.captured, claim)
        || operation.current_etag != claim.base_etag
        || operation.current_raw != claim.base_raw_ics
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(operation)
}

pub(super) fn claim_kind(
    connection: &mut Connection,
    account: &str,
    now: i64,
    kind: UpdateKind,
) -> Result<Option<RemoteFieldsMutationClaim>, PersistenceError> {
    claim_kind_scoped(connection, account, now, kind, None)
}
pub(super) fn claim_kind_scoped(
    connection: &mut Connection,
    account: &str,
    now: i64,
    kind: UpdateKind,
    scope: Option<&super::scoped_event::ScopedEventSnapshot>,
) -> Result<Option<RemoteFieldsMutationClaim>, PersistenceError> {
    claim_kind_scoped_guarded(connection, account, now, kind, scope, &|| true)
}
pub(super) fn claim_kind_scoped_guarded(
    connection: &mut Connection,
    account: &str,
    now: i64,
    kind: UpdateKind,
    scope: Option<&super::scoped_event::ScopedEventSnapshot>,
    current: &dyn Fn() -> bool,
) -> Result<Option<RemoteFieldsMutationClaim>, PersistenceError> {
    admitted_now_ms(now)?;
    validate_account_id(account)?;
    let (operation, calendar, resource) = scope
        .map(|s| {
            let (op, a, c, r) = s.ids();
            if a != account {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            Ok((Some(op), Some(c), Some(r)))
        })
        .transpose()?
        .unwrap_or((None, None, None));
    let tx = connection.transaction()?;
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.execute("UPDATE outbox_operations SET claim_token=NULL,claimed_at_ms=NULL,execution_phase='verify_first',last_outcome='ambiguous',updated_at_ms=?2
       WHERE operation_kind=?4 AND state='pending' AND account_id=?1
         AND ((claim_token IS NOT NULL AND (claimed_at_ms<=?3 OR claimed_at_ms>?2 OR (?5 AND claimed_at_ms IS NULL)))
           OR (?5 AND claim_token IS NULL AND claimed_at_ms IS NOT NULL))
         AND (?6 IS NULL OR (operation_id=?6 AND calendar_id=?7 AND resource_id=?8))",params![account,now,now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),kind.storage(),kind == UpdateKind::EventV3,operation,calendar,resource])?;
    // Preserve the account live-lease exclusion without recovering any other
    // operation's expired/orphan metadata on the scoped path.
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE operation_kind=?2 AND account_id=?1 AND state='pending' AND claim_token IS NOT NULL AND (?3 IS NULL OR (claimed_at_ms>?4 AND claimed_at_ms<=?5)))",params![account,kind.storage(),operation,now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),now],|r|r.get::<_,bool>(0))? {if !current() {return Err(PersistenceError::StaleSyncCapture);}tx.commit()?;return Ok(None);}
    let ids = {
        let mut stmt=tx.prepare("SELECT operation_id FROM outbox_operations WHERE operation_kind=?2 AND account_id=?1 AND state='pending' AND claim_token IS NULL AND (?3 IS NULL OR (operation_id=?3 AND calendar_id=?4 AND resource_id=?5)) ORDER BY created_at_ms,operation_id LIMIT 64")?;
        stmt.query_map(
            params![account, kind.storage(), operation, calendar, resource],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    for id in ids {
        let Ok(mut operation) = load(&tx, &id, kind) else {
            continue;
        };
        if scope.is_some_and(|scope| !scope.matches_fields(&operation.captured))
            || usable(&operation).is_err()
            || operation.current_etag != operation.captured.base_etag
            || operation.current_raw != operation.captured.base_raw_ics
        {
            continue;
        }
        let Ok(body) = expected(&operation) else {
            continue;
        };
        let Some(attempts) = operation
            .attempts
            .checked_add(1)
            .and_then(|n| u32::try_from(n).ok())
        else {
            continue;
        };
        let token = Uuid::new_v4().to_string();
        tx.execute("UPDATE outbox_operations SET claim_token=?2,claimed_at_ms=?3,attempt_count=?4,updated_at_ms=?3 WHERE operation_kind=?5 AND operation_id=?1 AND account_id=?6 AND state='pending' AND claim_token IS NULL AND (?7 IS NULL OR (calendar_id=?8 AND resource_id=?9))",params![id,token,now,attempts,kind.storage(),account,scope.map(|s|s.ids().0),calendar,resource])?;
        operation.captured.claim_token = token;
        operation.captured.expected_raw_ics = body;
        if !current() {
            return Err(PersistenceError::StaleSyncCapture);
        }
        tx.commit()?;
        return Ok(Some(operation.captured));
    }
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(None)
}

pub(super) fn read_allowed_kind(
    connection: &Connection,
    claim: &RemoteFieldsMutationClaim,
    now: i64,
    kind: UpdateKind,
) -> Result<bool, PersistenceError> {
    Ok(validated_claim(connection, claim, now, kind).is_ok())
}

pub(super) fn snapshot_kind(
    connection: &Connection,
    claim: &RemoteFieldsMutationClaim,
    now: i64,
    kind: UpdateKind,
) -> Result<RemoteFieldsSendSnapshot, PersistenceError> {
    let operation = validated_claim(connection, claim, now, kind)?;
    if operation.captured.phase != RemoteMutationExecutionPhase::Ready {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(RemoteFieldsSendSnapshot {
        operation_id: claim.operation_id.clone(),
        account_id: claim.account_id.clone(),
        calendar_id: claim.calendar_id.clone(),
        resource_id: claim.resource_id.clone(),
        collection_href: claim.collection_href.clone(),
        exact_href: claim.exact_href.clone(),
        base_etag: claim.base_etag.clone(),
        expected_raw_ics: operation.captured.expected_raw_ics,
    })
}

/// Token-scoped cleanup remains possible after epoch/ownership loss. It cannot
/// clear another lease or downgrade a durable verify-first phase.
pub(super) fn defer_kind(
    connection: &Connection,
    claim: &RemoteFieldsMutationClaim,
    uncertain: bool,
    now: i64,
    kind: UpdateKind,
) -> Result<(), PersistenceError> {
    admitted_now_ms(now)?;
    let changed=connection.execute("UPDATE outbox_operations SET claim_token=NULL,claimed_at_ms=NULL,
      execution_phase=CASE WHEN execution_phase='verify_first' OR ?4 OR claimed_at_ms>?3 OR claimed_at_ms<=?5 OR (?8 AND claimed_at_ms IS NULL) THEN 'verify_first' ELSE 'ready' END,
      last_outcome=CASE WHEN ?4 THEN 'ambiguous' ELSE 'transient' END,updated_at_ms=?3
      WHERE operation_kind=?7 AND operation_id=?1 AND claim_token=?2 AND account_id=?6 AND state='pending'",
      params![claim.operation_id,claim.claim_token,now,uncertain,now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),claim.account_id,kind.storage(),kind == UpdateKind::EventV3])?;
    if changed != 1 {
        return Err(PersistenceError::OutboxClaimLost);
    }
    Ok(())
}

pub(super) fn restore_base_kind(
    connection: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    now: i64,
    kind: UpdateKind,
) -> Result<RemoteFieldsMutationClaim, PersistenceError> {
    restore_base_kind_guarded(connection, claim, remote, now, kind, &|| true)
}
pub(super) fn restore_base_kind_guarded(
    connection: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    now: i64,
    kind: UpdateKind,
    current: &dyn Fn() -> bool,
) -> Result<RemoteFieldsMutationClaim, PersistenceError> {
    let tx = connection.transaction()?;
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    validated_claim(&tx, claim, now, kind)?;
    if !remote.has_bounded_strong_revision()
        || remote.etag != claim.base_etag
        || remote.raw_ics != claim.base_raw_ics
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    tx.execute("UPDATE outbox_operations SET execution_phase='ready',last_outcome=NULL WHERE operation_kind=?3 AND operation_id=?1 AND claim_token=?2 AND state='pending'",params![claim.operation_id,claim.claim_token,kind.storage()])?;
    let mut renewed = claim.clone();
    renewed.phase = RemoteMutationExecutionPhase::Ready;
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(renewed)
}

fn insert_conflict(
    tx: &Transaction<'_>,
    operation: &Operation,
    remote: &RemoteReadBack,
    now: i64,
) -> Result<(), PersistenceError> {
    let claim = &operation.captured;
    tx.execute("INSERT INTO conflicts(conflict_id,operation_id,reason,base_etag,base_raw_ics,local_intent_json,remote_etag,remote_raw_ics,resolution_state,created_at_ms)
       VALUES(?1,?2,'concurrent_remote_change',?3,?4,?5,?6,?7,'unresolved',?8)",params![Uuid::new_v4().to_string(),claim.operation_id,claim.base_etag,claim.base_raw_ics,claim.intent_json,remote.etag,remote.raw_ics,now])?;
    tx.execute("UPDATE outbox_operations SET state='conflict',claim_token=NULL,claimed_at_ms=NULL,updated_at_ms=?2 WHERE operation_kind=?3 AND operation_id=?1",params![claim.operation_id,now,operation.kind.storage()])?;
    Ok(())
}
fn safe_changed_remote(
    operation: &Operation,
    remote: &RemoteReadBack,
) -> Result<(), PersistenceError> {
    if !remote.has_bounded_strong_revision()
        || remote.etag == operation.captured.base_etag
        || !single_vevent_uid_v2(&remote.raw_ics).is_ok_and(|uid| uid == operation.captured.uid)
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    if operation.kind == UpdateKind::EventV3 {
        let fields = crate::domain::remote_event_update::decode_event_intent_v3(
            operation.captured.intent_json.as_bytes(),
        )?;
        crate::domain::remote_event_update::validate_event_base_v3(
            &remote.raw_ics,
            &operation.captured.uid,
            fields.time.is_some(),
        )
        .map_err(|_| PersistenceError::RemoteReadBackMismatch)?;
    }
    Ok(())
}

pub(super) fn finish(
    connection: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    applied: bool,
    now: i64,
) -> Result<(), PersistenceError> {
    let tx = connection.transaction()?;
    let operation = validated_claim(&tx, claim, now, UpdateKind::Fields)?;
    safe_changed_remote(&operation, remote)?;
    if applied {
        if remote.raw_ics != operation.captured.expected_raw_ics {
            return Err(PersistenceError::RemoteReadBackMismatch);
        }
        let (title, notes, location) = single_vevent_text_values_v2(&remote.raw_ics)?;
        tx.execute("UPDATE remote_resources SET etag=?2,raw_ics=?3,lifecycle='active',remote_deleted_at_ms=NULL,last_seen_at_ms=?4,updated_at_ms=?4 WHERE resource_id=?1",params![claim.resource_id,remote.etag,remote.raw_ics,now])?;
        tx.execute("UPDATE calendar_event_projections SET title=?3,notes=?4,location=?5,source_etag=?6,updated_at_ms=?7 WHERE series_id=?1 AND resource_id=?2",params![claim.series_id,claim.resource_id,title,notes,location,remote.etag,now])?;
        tx.execute("UPDATE outbox_operations SET state='applied',claim_token=NULL,claimed_at_ms=NULL,last_outcome=NULL,updated_at_ms=?2 WHERE operation_kind=?3 AND operation_id=?1",params![claim.operation_id,now,operation.kind.storage()])?;
    } else {
        insert_conflict(&tx, &operation, remote, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// Called inside the existing full/incremental promotion transaction. It
/// terminalizes only unclaimed ready work; the caller promotes remote cache.
pub(super) fn sync_conflict_kind(
    tx: &Transaction<'_>,
    resource: &str,
    remote: &RemoteReadBack,
    now: i64,
    kind: UpdateKind,
) -> Result<(), PersistenceError> {
    let id:Option<String>=tx.query_row("SELECT operation_id FROM outbox_operations WHERE operation_kind=?2 AND resource_id=?1 AND state='pending'",params![resource,kind.storage()],|r|r.get(0)).optional()?;
    let id = id.ok_or(PersistenceError::UnsafeSyncStage)?;
    let operation = load(tx, &id, kind).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    if (kind == UpdateKind::EventV3 && (operation.has_claim || operation.claimed_at_ms.is_some()))
        || !operation.captured.claim_token.is_empty()
        || operation.captured.phase != RemoteMutationExecutionPhase::Ready
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    usable(&operation).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    if operation.current_etag != operation.captured.base_etag
        || operation.current_raw != operation.captured.base_raw_ics
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    expected(&operation).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    safe_changed_remote(&operation, remote).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    insert_conflict(tx, &operation, remote, now)
}

pub(super) fn resolve_kind(
    connection: &mut Connection,
    operation_id: &str,
    choice: ConflictResolutionChoice,
    now: i64,
    kind: UpdateKind,
) -> Result<ConflictResolutionResult, PersistenceError> {
    admitted_now_ms(now)?;
    let tx = connection.transaction()?;
    let operation = load(&tx, operation_id, kind)?;
    let claim = &operation.captured;
    let (resolution, retry, base_etag, base_raw, intent, remote_etag, remote_raw): (
        String,
        Option<String>,
        String,
        Vec<u8>,
        String,
        String,
        Vec<u8>,
    ) = tx
        .query_row(
            "SELECT resolution_state,retry_operation_id,base_etag,
             CASE WHEN length(base_raw_ics)<=65536 THEN base_raw_ics END,
             CASE WHEN length(CAST(local_intent_json AS BLOB))<=32768 THEN local_intent_json END,
             remote_etag,CASE WHEN length(remote_raw_ics)<=65536 THEN remote_raw_ics END
             FROM conflicts WHERE operation_id=?1",
            [operation_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    expected(&operation)?;
    if base_etag != claim.base_etag || base_raw != claim.base_raw_ics || intent != claim.intent_json
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    match kind {
        UpdateKind::Fields => {
            parse_stored_fields_intent(&intent)?;
        }
        UpdateKind::EventV3 => {
            crate::domain::remote_event_update::decode_event_intent_v3(intent.as_bytes())?;
        }
    }
    let recorded_remote = RemoteReadBack {
        etag: remote_etag,
        raw_ics: remote_raw,
    };
    safe_changed_remote(&operation, &recorded_remote)?;
    if resolution != "unresolved" {
        if resolution != choice.as_str()
            || (choice == ConflictResolutionChoice::KeepRemote
                && (retry.is_some() || operation.state != "cancelled"))
            || (choice == ConflictResolutionChoice::RetryLocal
                && (retry.is_none() || operation.state != "conflict"))
        {
            return Err(PersistenceError::ConflictAlreadyResolved);
        }
        let result = ConflictResolutionResult {
            operation_id: operation_id.into(),
            account_id: claim.account_id.clone(),
            choice,
            retry_operation: retry
                .as_deref()
                .map(|id| {
                    let linked = load(&tx, id, kind)?;
                    let linked_claim = &linked.captured;
                    if linked_claim.account_id != claim.account_id
                        || linked_claim.calendar_id != claim.calendar_id
                        || linked_claim.resource_id != claim.resource_id
                        || linked_claim.series_id != claim.series_id
                        || linked_claim.uid != claim.uid
                        || linked_claim.intent_json != claim.intent_json
                    {
                        return Err(PersistenceError::IdempotencyConflict);
                    }
                    expected(&linked)?;
                    tx.query_row("SELECT idempotency_key,created_at_ms,updated_at_ms FROM outbox_operations WHERE operation_kind=?2 AND operation_id=?1", params![id,kind.storage()], |r| {
                        Ok(RemoteMutationSummary {
                            operation_id: id.into(), idempotency_key: r.get(0)?,
                            account_id: linked_claim.account_id.clone(), resource_id: linked_claim.resource_id.clone(),
                            state: RemoteMutationState::from_storage(&linked.state).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0,rusqlite::types::Type::Text,Box::new(e)))?,
                            created_at_ms: r.get(1)?, updated_at_ms: r.get(2)?,
                        })
                    }).map_err(PersistenceError::from)
                })
                .transpose()?,
        };
        return Ok(result);
    }
    if operation.state != "conflict" {
        return Err(PersistenceError::ConflictAlreadyResolved);
    }
    let successor = if kind == UpdateKind::EventV3 {
        tx.query_row("SELECT operation_id,idempotency_key,intent_json,created_at_ms,revision FROM remote_update_successors WHERE predecessor_operation_id=?1 AND state='blocked_conflict'",[operation_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?))).optional()?
    } else {
        None
    };
    let retry_operation = if choice == ConflictResolutionChoice::KeepRemote {
        tx.execute("UPDATE remote_update_successors SET state='cancelled',updated_at_ms=?2 WHERE predecessor_operation_id=?1",params![operation_id,now])?;
        tx.execute("UPDATE outbox_operations SET state='cancelled',updated_at_ms=?2 WHERE operation_kind=?3 AND operation_id=?1",params![operation_id,now,kind.storage()])?;
        None
    } else {
        usable(&operation)?;
        if operation.current_etag == claim.base_etag
            || !strong_mutation_etag(&operation.current_etag)
            || (operation.current_etag == recorded_remote.etag
                && operation.current_raw != recorded_remote.raw_ics)
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        if kind == UpdateKind::EventV3 && operation.current_etag == recorded_remote.etag {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        let retry_intent = if let Some((_, _, json, _, _)) = &successor {
            let head = crate::domain::remote_event_update::decode_event_intent_v3(
                claim.intent_json.as_bytes(),
            )?;
            let latest =
                crate::domain::remote_event_update::decode_event_intent_v3(json.as_bytes())?;
            String::from_utf8(crate::domain::remote_event_update::encode_event_intent_v3(
                &remote_update_successor::merge(head, &latest)?,
            )?)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
        } else {
            claim.intent_json.clone()
        };
        patch_intent(kind, &operation.current_raw, &claim.uid, &retry_intent)?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE resource_id=?1 AND state='pending' UNION ALL SELECT 1 FROM remote_delete_operations WHERE resource_id=?1 AND state='pending')",[&claim.resource_id],|r|r.get::<_,bool>(0))? || remote_creation::has_pending_target(&tx,&claim.resource_id)? {return Err(PersistenceError::PendingMutationExists);}
        let (id, key) = successor
            .as_ref()
            .map(|v| (v.0.clone(), v.1.clone()))
            .unwrap_or_else(|| (Uuid::new_v4().to_string(), Uuid::new_v4().to_string()));
        if successor.is_some() {
            tx.execute(
                "DELETE FROM remote_update_successors WHERE operation_id=?1",
                [&id],
            )?;
        }
        tx.execute("INSERT INTO outbox_operations(operation_id,idempotency_key,account_id,calendar_id,resource_id,series_id,operation_kind,base_etag,base_raw_ics,intent_json,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?11,?7,?8,?9,'pending',?10,?10)",params![id,key,claim.account_id,claim.calendar_id,claim.resource_id,claim.series_id,operation.current_etag,operation.current_raw,retry_intent,now,kind.storage()])?;
        Some(RemoteMutationSummary {
            operation_id: id,
            idempotency_key: key,
            account_id: claim.account_id.clone(),
            resource_id: claim.resource_id.clone(),
            state: RemoteMutationState::Pending,
            created_at_ms: now,
            updated_at_ms: now,
        })
    };
    tx.execute("UPDATE conflicts SET resolution_state=?2,resolved_at_ms=?3,retry_operation_id=?4 WHERE operation_id=?1 AND resolution_state='unresolved'",params![operation_id,choice.as_str(),now,retry_operation.as_ref().map(|op|&op.operation_id)])?;
    let result = ConflictResolutionResult {
        operation_id: operation_id.into(),
        account_id: claim.account_id.clone(),
        choice,
        retry_operation,
    };
    tx.commit()?;
    Ok(result)
}

pub(super) fn claim(
    c: &mut Connection,
    account: &str,
    now: i64,
) -> Result<Option<RemoteFieldsMutationClaim>, PersistenceError> {
    claim_kind(c, account, now, UpdateKind::Fields)
}
pub(super) fn read_allowed(
    c: &Connection,
    claim: &RemoteFieldsMutationClaim,
    now: i64,
) -> Result<bool, PersistenceError> {
    read_allowed_kind(c, claim, now, UpdateKind::Fields)
}
pub(super) fn snapshot(
    c: &Connection,
    claim: &RemoteFieldsMutationClaim,
    now: i64,
) -> Result<RemoteFieldsSendSnapshot, PersistenceError> {
    snapshot_kind(c, claim, now, UpdateKind::Fields)
}
pub(super) fn defer(
    c: &Connection,
    claim: &RemoteFieldsMutationClaim,
    uncertain: bool,
    now: i64,
) -> Result<(), PersistenceError> {
    defer_kind(c, claim, uncertain, now, UpdateKind::Fields)
}
pub(super) fn restore_base(
    c: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    now: i64,
) -> Result<RemoteFieldsMutationClaim, PersistenceError> {
    restore_base_kind(c, claim, remote, now, UpdateKind::Fields)
}
pub(super) fn sync_conflict(
    tx: &Transaction<'_>,
    resource: &str,
    remote: &RemoteReadBack,
    now: i64,
) -> Result<(), PersistenceError> {
    sync_conflict_kind(tx, resource, remote, now, UpdateKind::Fields)
}
pub(super) fn resolve(
    c: &mut Connection,
    id: &str,
    choice: ConflictResolutionChoice,
    now: i64,
) -> Result<ConflictResolutionResult, PersistenceError> {
    resolve_kind(c, id, choice, now, UpdateKind::Fields)
}

pub(super) fn validate_saved_event_base(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<(), PersistenceError> {
    let operation =
        load(tx, id, UpdateKind::EventV3).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    expected(&operation).map_err(|_| PersistenceError::UnsafeSyncStage)?;
    if operation.current_etag != operation.captured.base_etag
        || operation.current_raw != operation.captured.base_raw_ics
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    Ok(())
}

/// Distinct event-v3 terminal boundary. No fields service can invoke this path.
pub(super) fn finish_event(
    connection: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    applied: bool,
    now: i64,
) -> Result<bool, PersistenceError> {
    finish_event_guarded(connection, claim, remote, applied, now, &|| true)
}
pub(super) fn finish_event_guarded(
    connection: &mut Connection,
    claim: &RemoteFieldsMutationClaim,
    remote: &RemoteReadBack,
    applied: bool,
    now: i64,
    current: &dyn Fn() -> bool,
) -> Result<bool, PersistenceError> {
    let tx = connection.transaction()?;
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    let operation = validated_claim(&tx, claim, now, UpdateKind::EventV3)?;
    safe_changed_remote(&operation, remote)?;
    if !applied {
        insert_conflict(&tx, &operation, remote, now)?;
        remote_update_successor::terminal(&tx, claim, remote, false, now)?;
        if !current() {
            return Err(PersistenceError::StaleSyncCapture);
        }
        tx.commit()?;
        return Ok(false);
    }
    if !crate::domain::remote_event_update::event_readback_matches(
        &operation.captured.expected_raw_ics,
        &remote.raw_ics,
    )? {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    let fields = crate::domain::remote_event_update::decode_event_intent_v3(
        operation.captured.intent_json.as_bytes(),
    )?;
    let proof = crate::domain::remote_event_update::patch_single_utc_event_v3(
        &operation.captured.base_raw_ics,
        &fields,
    )?;
    if !crate::domain::remote_event_update::event_readback_matches(proof.bytes(), &remote.raw_ics)?
    {
        return Err(PersistenceError::RemoteReadBackMismatch);
    }
    let recurrence_terminal = tx
        .query_row(
            "SELECT generation,pending_expected_start_at_utc_ms,pending_expected_end_at_utc_ms,
                    pending_next_start_at_utc_ms,pending_next_end_at_utc_ms
             FROM calendar_resource_recurrence
             WHERE resource_id=?1 AND enabled=1 AND pending_operation_id=?2",
            params![claim.resource_id, claim.operation_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let recurrence_generation = recurrence_terminal.as_ref().map(|terminal| terminal.0);
    if let Some((_, expected_start, expected_end, next_start, next_end)) =
        recurrence_terminal.as_ref()
    {
        if proof.before_time().start_at_utc_ms != *expected_start
            || proof.before_time().end_at_utc_ms != *expected_end
            || proof.after_time().start_at_utc_ms != *next_start
            || proof.after_time().end_at_utc_ms != *next_end
        {
            return Err(PersistenceError::RemoteReadBackMismatch);
        }
    }
    let has_source: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM calendar_event_projections WHERE series_id=?1 AND resource_id=?2)", params![claim.series_id, claim.resource_id], |r|r.get(0))?;
    if has_source && fields.time.is_some() {
        let mixed_source: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM calendar_event_projections WHERE series_id=?1 AND resource_id<>?2)", [&claim.series_id, &claim.resource_id], |r|r.get(0))?;
        let normalized = normalize_vevent(&remote.raw_ics, &claim.uid);
        if mixed_source
            || normalized.status != ProjectionStatus::Ready
            || normalized.start_at_utc_ms != Some(proof.after_time().start_at_utc_ms)
            || normalized.end_at_utc_ms != Some(proof.after_time().end_at_utc_ms)
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
    }
    let planned_before = event_planned_fires(&tx, &claim.series_id)?;
    tx.execute("UPDATE remote_resources SET etag=?2,raw_ics=?3,lifecycle='active',remote_deleted_at_ms=NULL,last_seen_at_ms=?4,updated_at_ms=?4 WHERE resource_id=?1",params![claim.resource_id,remote.etag,remote.raw_ics,now])?;
    if has_source {
        if fields.time.is_some() {
            upsert_calendar_event_projection_inner(
                &tx,
                ProjectionSource {
                    series_id: &claim.series_id,
                    resource_id: &claim.resource_id,
                    calendar_id: &claim.calendar_id,
                    etag: &remote.etag,
                    raw_ics: &remote.raw_ics,
                    uid: &claim.uid,
                },
                now,
                standard_horizon_window(now)?,
                false,
            )?;
        } else {
            let (title, notes, location) = single_vevent_text_values_v2(&remote.raw_ics)?;
            tx.execute("UPDATE calendar_event_projections SET title=CASE WHEN ?8 THEN ?3 ELSE title END,notes=CASE WHEN ?9 THEN ?4 ELSE notes END,location=CASE WHEN ?10 THEN ?5 ELSE location END,source_etag=?6,updated_at_ms=?7 WHERE series_id=?1 AND resource_id=?2",params![claim.series_id,claim.resource_id,title,notes,location,remote.etag,now,fields.title.is_some(),fields.description.is_some(),fields.location.is_some()])?;
        }
        if fields.time.is_some() {
            // A time edit is a schedule mutation. Previously planned instants that
            // survived keep their catch-up opportunity; instants first introduced
            // by this edit are suppressed when already due.
            reconcile_calendar_notification_jobs(
                &tx,
                &claim.series_id,
                now,
                NotificationReconcileContext::ScheduleMutation,
            )?;
        }
    }
    tx.execute("UPDATE outbox_operations SET state='applied',claim_token=NULL,claimed_at_ms=NULL,last_outcome=NULL,updated_at_ms=?2 WHERE operation_kind='update_event' AND operation_id=?1",params![claim.operation_id,now])?;
    if let Some(recurrence_generation) = recurrence_generation {
        let changed = tx.execute(
            "UPDATE calendar_resource_recurrence
             SET cursor_start_at_utc_ms=pending_next_start_at_utc_ms,
                 cursor_end_at_utc_ms=pending_next_end_at_utc_ms,
                 pending_operation_id=NULL,pending_idempotency_key=NULL,
                 pending_expected_start_at_utc_ms=NULL,pending_expected_end_at_utc_ms=NULL,
                 pending_next_start_at_utc_ms=NULL,pending_next_end_at_utc_ms=NULL,
                 updated_at_ms=?3
             WHERE resource_id=?1 AND pending_operation_id=?2 AND enabled=1 AND generation=?4",
            params![
                claim.resource_id,
                claim.operation_id,
                now,
                recurrence_generation
            ],
        )?;
        if changed != 1 {
            return Err(PersistenceError::StaleSyncCapture);
        }
        materialize_resource_local_recurrence(&tx, &claim.resource_id, now)?;
    }
    remote_update_successor::terminal(&tx, claim, remote, true, now)?;
    let jobs_changed = has_source
        && fields.time.is_some()
        && planned_before != event_planned_fires(&tx, &claim.series_id)?;
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    tx.commit()?;
    Ok(jobs_changed)
}

pub(super) fn event_planned_fires(
    tx: &Transaction<'_>,
    series: &str,
) -> Result<Vec<(String, i64)>, PersistenceError> {
    let mut stmt = tx.prepare("SELECT DISTINCT occurrence_key,fire_at_utc_ms FROM notification_jobs WHERE source_kind='calendar_occurrence' AND source_id=?1 AND state='planned' ORDER BY occurrence_key,fire_at_utc_ms")?;
    Ok(stmt
        .query_map([series], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?)
}
