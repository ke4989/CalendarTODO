use super::*;
use crate::domain::remote_creation::{
    GeneratedCreationContent, GeneratedCreationContentV2, QueueRemoteEventCreate,
    QueueRemoteEventCreateV2, RemoteCreationClaim, RemoteCreationSummary,
};

pub(super) const MIGRATION: &str = r#"
CREATE TABLE remote_create_operations (
    operation_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL UNIQUE,
    uid TEXT NOT NULL UNIQUE,
    collection_href TEXT NOT NULL,
    exact_href TEXT NOT NULL,
    intent_json TEXT NOT NULL CHECK(length(intent_json)<=8192),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','applied','conflict','cancelled')),
    execution_phase TEXT NOT NULL DEFAULT 'ready' CHECK(execution_phase IN ('ready','verify_first')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(calendar_id,exact_href)
);
CREATE INDEX remote_create_account_pending
    ON remote_create_operations(account_id,state,created_at_ms,operation_id);
"#;

pub(super) const LEASE_MIGRATION: &str = r#"
ALTER TABLE remote_create_operations ADD COLUMN frozen_raw_ics BLOB
    CHECK(frozen_raw_ics IS NULL OR (length(frozen_raw_ics)>0 AND length(frozen_raw_ics)<=4194304));
ALTER TABLE remote_create_operations ADD COLUMN codec_version TEXT
    CHECK(codec_version IS NULL OR codec_version='utc-v1');
ALTER TABLE remote_create_operations ADD COLUMN claim_token TEXT;
ALTER TABLE remote_create_operations ADD COLUMN claimed_at_ms INTEGER;
"#;

pub(super) const COMPLETION_MIGRATION: &str = r#"
ALTER TABLE remote_create_operations ADD COLUMN applied_etag TEXT
    CHECK(applied_etag IS NULL OR (length(CAST(applied_etag AS BLOB))>2 AND length(CAST(applied_etag AS BLOB))<=256));
ALTER TABLE remote_create_operations ADD COLUMN applied_raw_ics BLOB
    CHECK(applied_raw_ics IS NULL OR (length(applied_raw_ics)>0 AND length(applied_raw_ics)<=4194304));
ALTER TABLE remote_create_operations ADD COLUMN applied_at_ms INTEGER;
"#;

/// Every old row is uncertain, including historical zero-lease rows.
pub(super) const FIRST_SEND_MIGRATION: &str = r#"
ALTER TABLE remote_create_operations ADD COLUMN send_attempt_id TEXT;
ALTER TABLE remote_create_operations ADD COLUMN send_attempted_at_ms INTEGER;
ALTER TABLE remote_create_operations ADD COLUMN send_state TEXT NOT NULL DEFAULT 'legacy_unknown'
    CHECK(send_state IN ('not_started','attempt_recorded','legacy_unknown'))
    CHECK((send_state='attempt_recorded' AND send_attempt_id IS NOT NULL
           AND length(send_attempt_id)=36 AND send_attempted_at_ms IS NOT NULL
           AND send_attempted_at_ms>=0 AND send_attempted_at_ms<4102444800000)
       OR (send_state IN ('not_started','legacy_unknown')
           AND send_attempt_id IS NULL AND send_attempted_at_ms IS NULL));
UPDATE remote_create_operations SET execution_phase='verify_first'
    WHERE state='pending';
"#;

/// Atomic versioning; historical v1 BLOB/intent schema limits remain copyable.
pub(super) const VERSIONED_MIGRATION: &str = r#"
CREATE TABLE remote_create_operations_v42 (
    operation_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    calendar_id TEXT NOT NULL REFERENCES calendars(calendar_id) ON DELETE RESTRICT,
    resource_id TEXT NOT NULL UNIQUE,
    uid TEXT NOT NULL UNIQUE,
    collection_href TEXT NOT NULL,
    exact_href TEXT NOT NULL,
    intent_json TEXT NOT NULL,
    intent_version TEXT NOT NULL DEFAULT 'utc-v1' CHECK(intent_version IN ('utc-v1','utc-v2')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','applied','conflict','cancelled')),
    execution_phase TEXT NOT NULL DEFAULT 'ready' CHECK(execution_phase IN ('ready','verify_first')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    frozen_raw_ics BLOB CHECK(frozen_raw_ics IS NULL OR (length(frozen_raw_ics)>0 AND
        length(frozen_raw_ics)<=CASE WHEN intent_version='utc-v2' THEN 65536 ELSE 4194304 END)),
    codec_version TEXT CHECK(codec_version IS NULL OR
        (intent_version='utc-v1' AND codec_version='utc-v1') OR
        (intent_version='utc-v2' AND codec_version='utc-v2')),
    claim_token TEXT,
    claimed_at_ms INTEGER,
    applied_etag TEXT CHECK(applied_etag IS NULL OR (length(CAST(applied_etag AS BLOB))>2 AND length(CAST(applied_etag AS BLOB))<=256)),
    applied_raw_ics BLOB CHECK(applied_raw_ics IS NULL OR (length(applied_raw_ics)>0 AND length(applied_raw_ics)<=4194304)),
    applied_at_ms INTEGER,
    send_attempt_id TEXT,
    send_attempted_at_ms INTEGER,
    send_state TEXT NOT NULL DEFAULT 'legacy_unknown'
        CHECK(send_state IN ('not_started','attempt_recorded','legacy_unknown'))
        CHECK((send_state='attempt_recorded' AND send_attempt_id IS NOT NULL
               AND length(send_attempt_id)=36 AND send_attempted_at_ms IS NOT NULL
               AND send_attempted_at_ms>=0 AND send_attempted_at_ms<4102444800000)
           OR (send_state IN ('not_started','legacy_unknown')
               AND send_attempt_id IS NULL AND send_attempted_at_ms IS NULL)),
    CHECK((intent_version='utc-v1' AND length(intent_json)<=8192)
       OR (intent_version='utc-v2' AND length(CAST(intent_json AS BLOB)) BETWEEN 2 AND 32768)),
    UNIQUE(calendar_id,exact_href)
);
INSERT INTO remote_create_operations_v42(operation_id,idempotency_key,account_id,calendar_id,resource_id,uid,collection_href,exact_href,intent_json,state,execution_phase,attempts,created_at_ms,updated_at_ms,frozen_raw_ics,codec_version,claim_token,claimed_at_ms,applied_etag,applied_raw_ics,applied_at_ms,send_attempt_id,send_attempted_at_ms,send_state) SELECT operation_id,idempotency_key,account_id,calendar_id,resource_id,uid,collection_href,exact_href,intent_json,state,execution_phase,attempts,created_at_ms,updated_at_ms,frozen_raw_ics,codec_version,claim_token,claimed_at_ms,applied_etag,applied_raw_ics,applied_at_ms,send_attempt_id,send_attempted_at_ms,send_state FROM remote_create_operations;
DROP TABLE remote_create_operations;
ALTER TABLE remote_create_operations_v42 RENAME TO remote_create_operations;
CREATE INDEX remote_create_account_pending ON remote_create_operations(account_id,state,created_at_ms,operation_id);
"#;

fn valid_send_record(state: &str, id: Option<&str>, time: Option<i64>) -> bool {
    match (state, id, time) {
        ("not_started" | "legacy_unknown", None, None) => true,
        ("attempt_recorded", Some(id), Some(time)) => {
            Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id && !uuid.is_nil())
                && (0..4_102_444_800_000).contains(&time)
        }
        _ => false,
    }
}

pub(super) fn account_view(
    connection: &mut Connection,
    account: &str,
) -> Result<crate::domain::remote_creation::RemoteCreationAccountView, PersistenceError> {
    use crate::domain::remote_creation::{RemoteCreationAccountView, RemoteCreationStatus};
    if !Uuid::parse_str(account).is_ok_and(|id| id.to_string() == account && !id.is_nil()) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let tx = connection.transaction()?;
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=?1)",
        [account],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::NotFound);
    }
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM remote_create_operations WHERE account_id=?1",
        [account],
        |r| r.get(0),
    )?;
    let operations = {
        let mut stmt = tx.prepare("SELECT operation_id,calendar_id,resource_id,state,execution_phase,created_at_ms,updated_at_ms
            FROM remote_create_operations WHERE account_id=?1 ORDER BY updated_at_ms DESC,operation_id LIMIT 100")?;
        let rows = stmt
            .query_map([account], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
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
                    phase,
                    created_at_ms,
                    updated_at_ms,
                )| {
                    for id in [&operation_id, &calendar_id, &resource_id] {
                        if !Uuid::parse_str(id)
                            .is_ok_and(|parsed| parsed.to_string() == *id && !parsed.is_nil())
                        {
                            return Err(PersistenceError::RemoteMutationReadOnly);
                        }
                    }
                    if !(0..4_102_444_800_000).contains(&created_at_ms)
                        || !(created_at_ms..4_102_444_800_000).contains(&updated_at_ms)
                    {
                        return Err(PersistenceError::RemoteMutationReadOnly);
                    }
                    Ok(RemoteCreationStatus {
                        operation_id,
                        calendar_id,
                        resource_id,
                        state: RemoteMutationState::from_storage(&state)?,
                        phase: RemoteMutationExecutionPhase::from_storage(&phase)?,
                        created_at_ms,
                        updated_at_ms,
                    })
                },
            )
            .collect::<Result<Vec<_>, PersistenceError>>()?
    };
    tx.commit()?;
    Ok(RemoteCreationAccountView {
        account_id: account.to_owned(),
        operation_count: u32::try_from(count).unwrap_or(u32::MAX),
        operations,
        production_execution_available: false,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIntent {
    title: String,
    start_at_utc_ms: i64,
    end_at_utc_ms: i64,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIntentV2 {
    version: String,
    title: String,
    description: Option<String>,
    location: Option<String>,
    start_at_utc_ms: i64,
    end_at_utc_ms: i64,
}

/// Internal codec dispatch; no transport authorization.
enum Content {
    V1(GeneratedCreationContent),
    V2(GeneratedCreationContentV2),
}
fn versioned_route_is_bounded(version: &str, collection: &str, href: &str) -> bool {
    version != "utc-v2" || (collection.len() <= 4096 && href.len() <= 4096)
}
impl Content {
    fn codec(&self) -> &'static str {
        match self {
            Self::V1(_) => "utc-v1",
            Self::V2(_) => "utc-v2",
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Self::V1(c) => c.bytes(),
            Self::V2(c) => c.bytes(),
        }
    }
    fn matches_read_back(&self, remote: &RemoteReadBack) -> bool {
        match self {
            Self::V1(c) => c.matches_read_back(remote),
            Self::V2(c) => c.matches_read_back(remote),
        }
    }
}
fn regenerate(
    json: &str,
    version: &str,
    key: &str,
    calendar: &str,
    uid: &str,
    created: i64,
) -> Result<Content, PersistenceError> {
    match version {
        "utc-v1" => {
            if json.len() > 8192 {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let intent: StoredIntent =
                serde_json::from_str(json).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
            if intent.title.trim() != intent.title {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let content = GeneratedCreationContent::generate(
                QueueRemoteEventCreate {
                    idempotency_key: key.into(),
                    calendar_id: calendar.into(),
                    title: intent.title,
                    start_at_utc_ms: intent.start_at_utc_ms,
                    end_at_utc_ms: intent.end_at_utc_ms,
                },
                uid,
                created,
            )?;
            if content.bytes().len() > 8192 {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            Ok(Content::V1(content))
        }
        "utc-v2" => {
            if json.len() > 32768 {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let intent: StoredIntentV2 =
                serde_json::from_str(json).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
            if intent.version != "utc-v2" {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            let input = QueueRemoteEventCreateV2 {
                idempotency_key: key.into(),
                calendar_id: calendar.into(),
                title: intent.title.clone(),
                description: intent.description.clone(),
                location: intent.location.clone(),
                start_at_utc_ms: intent.start_at_utc_ms,
                end_at_utc_ms: intent.end_at_utc_ms,
            };
            let canonical = input.clone().validate()?;
            if canonical != input {
                return Err(PersistenceError::RemoteMutationReadOnly);
            }
            Ok(Content::V2(GeneratedCreationContentV2::generate(
                input, uid, created,
            )?))
        }
        _ => Err(PersistenceError::RemoteMutationReadOnly),
    }
}

/// Resolve reserved creation identity inside the read promotion transaction.
/// This never terminalizes creation or gives permission to resend.
#[allow(clippy::too_many_arguments)]
pub(super) fn staged_identity(
    tx: &Transaction<'_>,
    account: &str,
    calendar: &str,
    route: &str,
    resource: &crate::domain::calendar_sync::StagedResource,
    existing: Option<&str>,
    series: &str,
    now: i64,
) -> Result<String, PersistenceError> {
    // Absence lookup uses stable legacy columns, including historical migration fixtures.
    if !tx.query_row("SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE calendar_id=?1 AND exact_href=?2 AND state='pending')",params![calendar,resource.href],|r|r.get::<_,bool>(0))? {
        return Ok(existing.map(str::to_owned).unwrap_or_else(||Uuid::new_v4().to_string()));
    }
    let saved = tx
        .query_row(
            "SELECT operation_id,account_id,resource_id,uid,collection_href,COALESCE(CASE WHEN (intent_version='utc-v1' AND length(CAST(intent_json AS BLOB))<=8192) OR (intent_version='utc-v2' AND length(CAST(intent_json AS BLOB))<=32768) THEN intent_json END,''),
            idempotency_key,created_at_ms,execution_phase,claim_token,claimed_at_ms,
            CASE WHEN (intent_version='utc-v1' AND codec_version='utc-v1' AND length(frozen_raw_ics)<=8192) OR (intent_version='utc-v2' AND codec_version='utc-v2' AND length(frozen_raw_ics)<=65536) THEN frozen_raw_ics END,CASE WHEN frozen_raw_ics IS NOT NULL AND codec_version IS NULL THEN 'invalid' ELSE codec_version END,
            send_state,send_attempt_id,send_attempted_at_ms,intent_version
         FROM remote_create_operations WHERE calendar_id=?1 AND exact_href=?2 AND state='pending'",
            params![calendar, resource.href],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<Vec<u8>>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, String>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, String>(16)?,
                ))
            },
        )
        .optional()?;
    let Some((
        op,
        owner,
        id,
        uid,
        saved_route,
        json,
        key,
        created,
        phase,
        token,
        claimed,
        frozen,
        codec,
        send_state,
        send_id,
        send_time,
        version,
    )) = saved
    else {
        return Ok(existing
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string()));
    };
    if !valid_send_record(&send_state, send_id.as_deref(), send_time)
        || (send_state != "not_started" && phase != "verify_first")
        || owner != account
        || token.is_some()
        || claimed.is_some()
        || !matches!(phase.as_str(), "ready" | "verify_first")
        || !collection_routes_match(&saved_route, route)
        || !versioned_route_is_bounded(&version, &saved_route, &resource.href)
        || !versioned_route_is_bounded(&version, route, &resource.href)
        || !resource_belongs_to_collection(route, &resource.href)
        || resource.uid != uid
        || !Uuid::parse_str(&uid).is_ok_and(|u| u.to_string() == uid)
        || Uuid::parse_str(&id).is_err()
        || Uuid::parse_str(&op).is_err()
        || existing.is_some_and(|old| old != id)
    {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    let content = regenerate(&json, &version, &key, calendar, &uid, created)
        .map_err(|_| PersistenceError::UnsafeSyncStage)?;
    if codec.as_deref() != Some(content.codec()) {
        return Err(PersistenceError::UnsafeSyncStage);
    }
    if frozen.as_deref()!=Some(content.bytes()) || !content.matches_read_back(&RemoteReadBack {
        etag:resource.etag.clone(),raw_ics:resource.raw_ics.clone(),
    }) || tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE resource_id=?1
            AND (calendar_id<>?2 OR exact_href<>?3 OR series_id<>?4)
         UNION ALL SELECT 1 FROM identity_aliases WHERE calendar_id=?2 AND exact_href=?3 AND series_id<>?4)",
        params![id,calendar,resource.href,series],|r|r.get::<_,bool>(0),
    )? {return Err(PersistenceError::UnsafeSyncStage);}
    tx.execute(
        "UPDATE remote_create_operations SET execution_phase='verify_first',updated_at_ms=?2
        WHERE operation_id=?1 AND execution_phase='ready'",
        params![op, now],
    )?;
    Ok(id)
}

pub(super) fn has_pending_target(
    connection: &Connection,
    resource_id: &str,
) -> Result<bool, PersistenceError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations o JOIN remote_resources r
         ON r.calendar_id=o.calendar_id AND r.exact_href=o.exact_href
         WHERE r.resource_id=?1 AND o.state='pending')",
        [resource_id],
        |r| r.get(0),
    )?)
}

pub(super) fn removal_allowed(
    connection: &Connection,
    calendar: &str,
    href: &str,
) -> Result<bool, PersistenceError> {
    Ok(!connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE calendar_id=?1 AND exact_href=?2
         AND state='pending' AND (claim_token IS NOT NULL OR claimed_at_ms IS NOT NULL OR execution_phase<>'ready'))",
        params![calendar,href],|r|r.get::<_,bool>(0),
    )?)
}

struct Candidate {
    version: String,
    operation_id: String,
    key: String,
    calendar_id: String,
    uid: String,
    intent: String,
    created_at_ms: i64,
    phase: String,
    attempts: i64,
    saved_route: String,
    current_route: String,
    href: String,
    frozen: Option<Vec<u8>>,
    codec: Option<String>,
    send_state: String,
    send_id: Option<String>,
    send_time: Option<i64>,
}

struct ValidatedCandidate {
    candidate: Candidate,
    phase: RemoteMutationExecutionPhase,
    attempt: u32,
    frozen: Vec<u8>,
    codec: String,
}

fn next_candidate(
    tx: &Transaction<'_>,
    account: &str,
    now: i64,
    version: Option<&str>,
    scope: Option<&scoped_event::ScopedEventSnapshot>,
) -> Result<Option<ValidatedCandidate>, PersistenceError> {
    let candidates = {
        let mut stmt = tx.prepare(
            "SELECT o.operation_id,o.idempotency_key,o.calendar_id,o.uid,COALESCE(CASE WHEN (o.intent_version='utc-v1' AND length(CAST(o.intent_json AS BLOB))<=8192) OR (o.intent_version='utc-v2' AND length(CAST(o.intent_json AS BLOB))<=32768) THEN o.intent_json END,''),
             o.created_at_ms,CASE WHEN o.claim_token IS NOT NULL THEN 'verify_first' ELSE o.execution_phase END,o.attempts,o.collection_href,c.collection_url,
             o.exact_href,CASE WHEN (o.intent_version='utc-v1' AND o.codec_version='utc-v1' AND length(o.frozen_raw_ics)<=8192) OR (o.intent_version='utc-v2' AND o.codec_version='utc-v2' AND length(o.frozen_raw_ics)<=65536) THEN o.frozen_raw_ics END,CASE WHEN o.frozen_raw_ics IS NOT NULL AND o.codec_version IS NULL THEN 'invalid' ELSE o.codec_version END,
             o.send_state,o.send_attempt_id,o.send_attempted_at_ms,o.intent_version
             FROM remote_create_operations o JOIN calendars c ON c.calendar_id=o.calendar_id
             JOIN accounts a ON a.account_id=o.account_id AND a.account_id=c.account_id
             WHERE o.account_id=?1 AND o.state='pending' AND (o.claim_token IS NULL AND o.claimed_at_ms IS NULL
                 OR o.claim_token IS NOT NULL AND (o.claimed_at_ms IS NULL OR o.claimed_at_ms<=?2 OR o.claimed_at_ms>?3)) AND a.enabled=1 AND a.removed_at_ms IS NULL
               AND a.remote_editing_enabled=1 AND c.available=1
               AND (?4 IS NULL OR o.intent_version=?4)
               AND (?5 IS NULL OR (o.operation_id=?5 AND o.calendar_id=?6))
               AND (o.intent_version='utc-v1' OR (a.credential_configured=1 AND a.provider_kind='caldav' AND a.region_profile='icloud-cn'))
               AND (','||replace(c.privileges,' ','')||',' LIKE '%,write,%'
                    OR ','||replace(c.privileges,' ','')||',' LIKE '%,write-content,%')
             ORDER BY o.created_at_ms,o.operation_id LIMIT 32",
        )?;
        stmt.query_map(
            params![
                account,
                now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
                now,
                version,
                scoped_event::scope_ids(scope).0,
                scoped_event::scope_ids(scope).1
            ],
            |r| {
                Ok(Candidate {
                    version: r.get(16)?,
                    operation_id: r.get(0)?,
                    key: r.get(1)?,
                    calendar_id: r.get(2)?,
                    uid: r.get(3)?,
                    intent: r.get(4)?,
                    created_at_ms: r.get(5)?,
                    phase: r.get(6)?,
                    attempts: r.get(7)?,
                    saved_route: r.get(8)?,
                    current_route: r.get(9)?,
                    href: r.get(10)?,
                    frozen: r.get(11)?,
                    codec: r.get(12)?,
                    send_state: r.get(13)?,
                    send_id: r.get(14)?,
                    send_time: r.get(15)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    for c in candidates {
        let Ok(phase) = RemoteMutationExecutionPhase::from_storage(&c.phase) else {
            continue;
        };
        if !valid_send_record(&c.send_state, c.send_id.as_deref(), c.send_time)
            || (c.send_state != "not_started" && phase != RemoteMutationExecutionPhase::VerifyFirst)
        {
            continue;
        }
        let Some(attempt) = u32::try_from(c.attempts)
            .ok()
            .and_then(|n| n.checked_add(1))
        else {
            continue;
        };
        if !collection_routes_match(&c.saved_route, &c.current_route)
            || !versioned_route_is_bounded(&c.version, &c.saved_route, &c.href)
            || !versioned_route_is_bounded(&c.version, &c.current_route, &c.href)
            || !resource_belongs_to_collection(&c.current_route, &c.href)
        {
            continue;
        }
        if Uuid::parse_str(&c.operation_id).is_err()
            || !Uuid::parse_str(&c.uid).is_ok_and(|uid| uid.to_string() == c.uid)
        {
            continue;
        }
        let Ok(generated) = regenerate(
            &c.intent,
            &c.version,
            &c.key,
            &c.calendar_id,
            &c.uid,
            c.created_at_ms,
        ) else {
            continue;
        };
        let frozen = match (c.frozen.clone(), c.codec.as_deref()) {
            (Some(raw), Some(codec)) if codec == generated.codec() && raw == generated.bytes() => {
                raw
            }
            (None, None)
                if c.attempts == 0
                    && (phase == RemoteMutationExecutionPhase::Ready
                        || c.send_state == "legacy_unknown") =>
            {
                generated.bytes().to_vec()
            }
            _ => continue,
        };
        if phase == RemoteMutationExecutionPhase::Ready && tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2)",
            params![c.calendar_id,c.href],|r|r.get::<_,bool>(0),
        )? {continue;}
        return Ok(Some(ValidatedCandidate {
            candidate: c,
            phase,
            attempt,
            frozen,
            codec: generated.codec().into(),
        }));
    }
    Ok(None)
}

/// Queue selection is read-only, bounded, and shares claim admission/order.
pub(super) fn select(
    connection: &mut Connection,
    account: &str,
    now: i64,
    version: &str,
) -> Result<Option<String>, PersistenceError> {
    Uuid::parse_str(account)
        .map_err(|_| crate::domain::remote_mutation::RemoteMutationError::InvalidIdentity)?;
    admitted_now_ms(now)?;
    let tx = connection.transaction()?;
    let live: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE account_id=?1 AND state='pending' AND claim_token IS NOT NULL AND claimed_at_ms>?2 AND claimed_at_ms<=?3)", params![account,now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),now], |r|r.get(0))?;
    let operation = if live {
        None
    } else {
        next_candidate(&tx, account, now, Some(version), None)?
            .map(|validated| validated.candidate.operation_id)
    };
    tx.commit()?;
    Ok(operation)
}

pub(super) fn claim(
    connection: &mut Connection,
    account: &str,
    now: i64,
    version: Option<&str>,
) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
    claim_scoped(connection, account, now, version, None)
}
pub(super) fn claim_scoped(
    connection: &mut Connection,
    account: &str,
    now: i64,
    version: Option<&str>,
    scope: Option<&scoped_event::ScopedEventSnapshot>,
) -> Result<Option<RemoteCreationClaim>, PersistenceError> {
    if let Some(scope) = scope {
        scoped_event::current_route(connection, scope)?;
    }
    Uuid::parse_str(account)
        .map_err(|_| crate::domain::remote_mutation::RemoteMutationError::InvalidIdentity)?;
    admitted_now_ms(now)?;
    let tx = connection.transaction()?;
    tx.execute(
        "UPDATE remote_create_operations SET claim_token=NULL,claimed_at_ms=NULL,
             execution_phase='verify_first',updated_at_ms=?2
         WHERE account_id=?1 AND state='pending' AND claim_token IS NOT NULL
           AND (claimed_at_ms IS NULL OR claimed_at_ms<=?3 OR claimed_at_ms>?2)
           AND (?4 IS NULL OR (operation_id=?4 AND calendar_id=?5))",
        params![
            account,
            now,
            now.saturating_sub(REMOTE_MUTATION_CLAIM_LEASE_MS),
            scoped_event::scope_ids(scope).0,
            scoped_event::scope_ids(scope).1
        ],
    )?;
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_create_operations
         WHERE account_id=?1 AND state='pending' AND claim_token IS NOT NULL AND (?2 IS NULL OR (operation_id=?2 AND calendar_id=?3)))",
        params![account,scoped_event::scope_ids(scope).0,scoped_event::scope_ids(scope).1],
        |r| r.get::<_, bool>(0),
    )? {
        tx.commit()?;
        return Ok(None);
    }
    if let Some(ValidatedCandidate {
        candidate: c,
        phase,
        attempt,
        frozen,
        codec,
    }) = next_candidate(&tx, account, now, version, scope)?
    {
        let token = Uuid::new_v4().to_string();
        tx.execute(
            "UPDATE remote_create_operations SET frozen_raw_ics=COALESCE(frozen_raw_ics,?2),codec_version=COALESCE(codec_version,?6),
             claim_token=?3,claimed_at_ms=?4,attempts=?5,updated_at_ms=?4 WHERE operation_id=?1",
            params![c.operation_id, frozen, token, now, attempt, codec],
        )?;
        tx.commit()?;
        return Ok(Some(RemoteCreationClaim {
            operation_id: c.operation_id,
            account_id: account.into(),
            claim_token: token,
            frozen_raw_ics: frozen,
            codec_version: codec,
            phase,
            calendar_id: c.calendar_id,
            collection_href: c.saved_route,
            exact_href: c.href,
            sync_epoch: None,
        }));
    }
    tx.commit()?;
    Ok(None)
}

/// Rejoin committed ownership and reconstruct the frozen intent independently.
pub(super) fn read_allowed(
    connection: &Connection,
    claim: &RemoteCreationClaim,
    now: i64,
) -> Result<bool, PersistenceError> {
    Ok(validated_content(connection, claim, now)?.is_some())
}

fn validated_content(
    connection: &Connection,
    claim: &RemoteCreationClaim,
    now: i64,
) -> Result<Option<Content>, PersistenceError> {
    admitted_now_ms(now)?;
    let row = connection
        .query_row(
            "SELECT o.idempotency_key,o.uid,COALESCE(CASE WHEN (o.intent_version='utc-v1' AND length(CAST(o.intent_json AS BLOB))<=8192) OR (o.intent_version='utc-v2' AND length(CAST(o.intent_json AS BLOB))<=32768) THEN o.intent_json END,''),o.created_at_ms,o.claimed_at_ms,
            o.collection_href,c.collection_url,o.exact_href,o.execution_phase,
            CASE WHEN (o.intent_version='utc-v1' AND o.codec_version='utc-v1' AND length(o.frozen_raw_ics)<=8192) OR (o.intent_version='utc-v2' AND o.codec_version='utc-v2' AND length(o.frozen_raw_ics)<=65536) THEN o.frozen_raw_ics END,CASE WHEN o.frozen_raw_ics IS NOT NULL AND o.codec_version IS NULL THEN 'invalid' ELSE o.codec_version END,
            o.send_state,o.send_attempt_id,o.send_attempted_at_ms,o.intent_version
         FROM remote_create_operations o
         JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id
         JOIN accounts a ON a.account_id=o.account_id
         WHERE o.operation_id=?1 AND o.account_id=?2 AND o.calendar_id=?3
           AND o.claim_token=?4 AND o.state='pending' AND a.enabled=1
           AND a.removed_at_ms IS NULL AND a.remote_editing_enabled=1 AND c.available=1
           AND (o.intent_version='utc-v1' OR (a.credential_configured=1 AND a.provider_kind='caldav' AND a.region_profile='icloud-cn'))
           AND (','||replace(c.privileges,' ','')||',' LIKE '%,write,%'
                OR ','||replace(c.privileges,' ','')||',' LIKE '%,write-content,%')",
            params![
                claim.operation_id,
                claim.account_id,
                claim.calendar_id,
                claim.claim_token
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<Vec<u8>>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, String>(14)?,
                ))
            },
        )
        .optional()?;
    let Some((
        key,
        uid,
        json,
        created,
        claimed,
        saved,
        current,
        href,
        phase,
        frozen,
        codec,
        send_state,
        send_id,
        send_time,
        version,
    )) = row
    else {
        return Ok(None);
    };
    if !valid_send_record(&send_state, send_id.as_deref(), send_time)
        || (send_state != "not_started" && phase != "verify_first")
        || !claimed
            .is_some_and(|t| t <= now && now.saturating_sub(t) < REMOTE_MUTATION_CLAIM_LEASE_MS)
        || saved != claim.collection_href
        || href != claim.exact_href
        || !collection_routes_match(&saved, &current)
        || !versioned_route_is_bounded(&version, &saved, &href)
        || !versioned_route_is_bounded(&version, &current, &href)
        || !resource_belongs_to_collection(&current, &href)
        || RemoteMutationExecutionPhase::from_storage(&phase)? != claim.phase
        || !Uuid::parse_str(&uid).is_ok_and(|id| id.to_string() == uid)
    {
        return Ok(None);
    }
    let Ok(content) = regenerate(&json, &version, &key, &claim.calendar_id, &uid, created) else {
        return Ok(None);
    };
    if codec.as_deref() != Some(content.codec()) || claim.codec_version != content.codec() {
        return Ok(None);
    }
    if frozen.as_deref() != Some(content.bytes()) || claim.frozen_raw_ics != content.bytes() {
        return Ok(None);
    }
    Ok(Some(content))
}

/// Read-only attempt-specific final snapshot. Does not consume or dispatch.
pub(super) fn send_allowed(
    connection: &mut Connection,
    claim: &RemoteCreationClaim,
    attempt: Uuid,
    attempted_at_ms: i64,
    now: i64,
) -> Result<bool, PersistenceError> {
    let tx = connection.transaction()?;
    if attempt.is_nil()
        || claim.phase != RemoteMutationExecutionPhase::VerifyFirst
        || validated_content(&tx, claim, now)?.is_none()
    {
        return Ok(false);
    }
    let (id, uid, saved_attempt, attempted): (String, String, Option<String>, Option<i64>) = tx.query_row(
        "SELECT resource_id,uid,send_attempt_id,send_attempted_at_ms FROM remote_create_operations
         WHERE operation_id=?1 AND send_state='attempt_recorded' AND execution_phase='verify_first'",
        [&claim.operation_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional()?.unwrap_or_default();
    let path = if claim.collection_href.starts_with('/') {
        claim.collection_href.clone()
    } else {
        url::Url::parse(&claim.collection_href)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
            .path()
            .to_owned()
    };
    Ok(saved_attempt.as_deref() == Some(attempt.to_string().as_str())
        && attempted == Some(attempted_at_ms) && attempted_at_ms <= now
        && [&id,&uid,&claim.operation_id,&claim.account_id,&claim.calendar_id,&claim.claim_token].iter()
            .all(|s| Uuid::parse_str(s).is_ok_and(|u| !u.is_nil() && u.to_string() == **s))
        && claim.exact_href == format!("{}/{}.ics", path.trim_end_matches('/'),id)
        && tx.query_row("SELECT credential_configured=1 FROM accounts WHERE account_id=?1", [&claim.account_id], |r| r.get::<_,bool>(0))?
        && !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE resource_id=?1 OR (calendar_id=?2 AND exact_href=?3)
             UNION ALL SELECT 1 FROM identity_aliases WHERE calendar_id=?2 AND exact_href=?3
             UNION ALL SELECT 1 FROM event_series WHERE account_id=?4 AND uid=?5
             UNION ALL SELECT 1 FROM outbox_operations WHERE resource_id=?1
             UNION ALL SELECT 1 FROM remote_delete_operations WHERE resource_id=?1
             UNION ALL SELECT 1 FROM remote_create_operations WHERE operation_id<>?6 AND (resource_id=?1 OR uid=?5 OR (calendar_id=?2 AND exact_href=?3)))",
            params![id,claim.calendar_id,claim.exact_href,claim.account_id,uid,claim.operation_id], |r| r.get::<_,bool>(0))?)
}

/// Record only once, before a future sender can be given an object to consume.
/// The caller/actor checks process-local epoch; transaction failures do not rotate it.
pub(super) fn record_first_send(
    connection: &mut Connection,
    claim: &RemoteCreationClaim,
    now: i64,
) -> Result<Uuid, PersistenceError> {
    let tx = connection.transaction()?;
    if claim.phase != RemoteMutationExecutionPhase::Ready
        || validated_content(&tx, claim, now)?.is_none()
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let (id, uid, send_state, send_id, send_time): (
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
    ) = tx.query_row(
        "SELECT resource_id,uid,send_state,send_attempt_id,send_attempted_at_ms
         FROM remote_create_operations WHERE operation_id=?1",
        [&claim.operation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let collection_path = if claim.collection_href.starts_with('/') {
        claim.collection_href.clone()
    } else {
        url::Url::parse(&claim.collection_href)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
            .path()
            .to_owned()
    };
    if send_state != "not_started" || send_id.is_some() || send_time.is_some()
        || !Uuid::parse_str(&id).is_ok_and(|uuid| uuid.to_string() == id && !uuid.is_nil())
        || Uuid::parse_str(&claim.operation_id).is_err()
        || claim.exact_href != format!("{}/{}.ics", collection_path.trim_end_matches('/'), id)
        || tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_resources
                 WHERE resource_id=?1 OR (calendar_id=?2 AND exact_href=?3)
             UNION ALL SELECT 1 FROM identity_aliases WHERE calendar_id=?2 AND exact_href=?3
             UNION ALL SELECT 1 FROM event_series WHERE account_id=?4 AND uid=?5
             UNION ALL SELECT 1 FROM outbox_operations WHERE resource_id=?1
             UNION ALL SELECT 1 FROM remote_delete_operations WHERE resource_id=?1
             UNION ALL SELECT 1 FROM remote_create_operations
                 WHERE operation_id<>?6 AND (resource_id=?1 OR uid=?5 OR (calendar_id=?2 AND exact_href=?3)))",
            params![id,claim.calendar_id,claim.exact_href,claim.account_id,uid,claim.operation_id],
            |r|r.get::<_,bool>(0),
        )?
    {
        return Err(PersistenceError::IdentityConflict);
    }
    let attempt = Uuid::new_v4();
    let changed = tx.execute(
        "UPDATE remote_create_operations SET send_state='attempt_recorded',send_attempt_id=?2,
         send_attempted_at_ms=?3,execution_phase='verify_first',updated_at_ms=?3
         WHERE operation_id=?1 AND send_state='not_started' AND send_attempt_id IS NULL
           AND send_attempted_at_ms IS NULL AND claim_token=?4 AND execution_phase='ready'",
        params![
            claim.operation_id,
            attempt.to_string(),
            now,
            claim.claim_token
        ],
    )?;
    if changed != 1 {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    tx.commit()?;
    Ok(attempt)
}

/// No terminal row, cache or projection is written. Every observation retains
/// verify-first, including exact bytes and exact-target absence.
pub(super) fn observe(
    connection: &mut Connection,
    claim: RemoteCreationClaim,
    evidence: crate::domain::remote_creation::RemoteCreationReadBack,
    now: i64,
) -> Result<crate::domain::remote_creation::RemoteCreationObservation, PersistenceError> {
    use crate::domain::remote_creation::{
        RemoteCreationObservation as O, RemoteCreationReadBack as R,
    };
    let tx = connection.transaction()?;
    let content =
        validated_content(&tx, &claim, now)?.ok_or(PersistenceError::RemoteMutationReadOnly)?;
    let observation = match evidence {
        R::Absent(404 | 410) => O::AbsentUnresolved,
        R::Retained(remote) if content.matches_read_back(&remote) => O::ExactContent,
        _ => O::UnsafeUnresolved,
    };
    tx.execute(
        "UPDATE remote_create_operations SET claim_token=NULL,claimed_at_ms=NULL,
        execution_phase='verify_first',updated_at_ms=?2 WHERE operation_id=?1",
        params![claim.operation_id, now],
    )?;
    tx.commit()?;
    Ok(observation)
}

/// Exact-content terminal recovery. Epoch ownership is checked by the actor.
///
/// Returns whether this commit changed the series' planned reminder set, so a
/// request-scoped create can rearm the reminder deadline immediately instead of
/// waiting for an unrelated refresh.
pub(super) fn complete(
    connection: &mut Connection,
    claim: &RemoteCreationClaim,
    remote: RemoteReadBack,
    now: i64,
) -> Result<bool, PersistenceError> {
    let tx = connection.transaction()?;
    let content =
        validated_content(&tx, claim, now)?.ok_or(PersistenceError::RemoteMutationReadOnly)?;
    if !content.matches_read_back(&remote) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let (id, uid): (String, String) = tx.query_row(
        "SELECT resource_id,uid FROM remote_create_operations WHERE operation_id=?1",
        [&claim.operation_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if Uuid::parse_str(&id).is_err() || tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id
         WHERE s.account_id=?1 AND s.uid=?2 AND (r.resource_id<>?3 OR r.calendar_id<>?4 OR r.exact_href<>?5)
         UNION ALL SELECT 1 FROM remote_resources WHERE resource_id=?3 AND (calendar_id<>?4 OR exact_href<>?5)
         UNION ALL SELECT 1 FROM outbox_operations WHERE resource_id=?3 AND state='pending'
         UNION ALL SELECT 1 FROM remote_delete_operations WHERE resource_id=?3 AND state='pending')",
        params![claim.account_id,uid,id,claim.calendar_id,claim.exact_href],|r|r.get::<_,bool>(0),
    )? {return Err(PersistenceError::IdentityConflict);}
    let cached=tx.query_row(
        "SELECT r.resource_id,r.series_id,s.account_id,s.uid,r.etag,r.raw_ics,r.lifecycle,r.parse_status
         FROM remote_resources r JOIN event_series s ON s.series_id=r.series_id
         WHERE r.calendar_id=?1 AND r.exact_href=?2",
        params![claim.calendar_id,claim.exact_href],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,
            r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Vec<u8>>(5)?,
            r.get::<_,String>(6)?,r.get::<_,String>(7)?)),
    ).optional()?;
    let series = if let Some((cached_id, series, owner, cached_uid, etag, raw, lifecycle, parsed)) =
        cached
    {
        if cached_id != id
            || owner != claim.account_id
            || cached_uid != uid
            || etag != remote.etag
            || raw != remote.raw_ics
            || lifecycle != "active"
            || parsed != "parsed"
        {
            return Err(PersistenceError::IdentityConflict);
        }
        series
    } else {
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM event_series WHERE account_id=?1 AND uid=?2)",
            params![claim.account_id, uid],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(PersistenceError::IdentityConflict);
        }
        Uuid::new_v4().to_string()
    };
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM identity_aliases WHERE calendar_id=?1 AND exact_href=?2 AND series_id<>?3)",
        params![claim.calendar_id,claim.exact_href,series],|r|r.get::<_,bool>(0))? {
        return Err(PersistenceError::IdentityConflict);
    }
    let normalized = normalize_vevent(&remote.raw_ics, &uid);
    if normalized.status != ProjectionStatus::Ready || normalized.unsupported_reason.is_some() {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE resource_id=?1)",
        [&id],
        |r| r.get::<_, bool>(0),
    )?;
    let previous_planned = remote_update_fields::event_planned_fires(&tx, &series)?;
    if !exists {
        tx.execute("INSERT INTO event_series(series_id,account_id,uid,current_calendar_id,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5)",params![series,claim.account_id,uid,claim.calendar_id,now])?;
        tx.execute("INSERT INTO remote_resources(resource_id,calendar_id,series_id,exact_href,etag,content_type,raw_ics,parse_status,lifecycle,last_seen_at_ms,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,'text/calendar',?6,'parsed','active',?7,?7,?7)",params![id,claim.calendar_id,series,claim.exact_href,remote.etag,remote.raw_ics,now])?;
        tx.execute("INSERT INTO identity_aliases(series_id,calendar_id,exact_href,first_observed_at_ms,last_observed_at_ms) VALUES(?1,?2,?3,?4,?4)",params![series,claim.calendar_id,claim.exact_href,now])?;
        upsert_calendar_event_projection(
            &tx,
            ProjectionSource {
                series_id: &series,
                resource_id: &id,
                calendar_id: &claim.calendar_id,
                etag: &remote.etag,
                raw_ics: &remote.raw_ics,
                uid: &uid,
            },
            now,
            standard_horizon_window(now)?,
        )?;
    }
    // A create is a schedule mutation: a past start was never a reminder
    // opportunity, while a future one must be scheduled before this transaction
    // commits so the new reminder is armed without another refresh.
    reconcile_calendar_notification_jobs(
        &tx,
        &series,
        now,
        NotificationReconcileContext::ScheduleMutation,
    )?;
    let jobs_changed = previous_planned != remote_update_fields::event_planned_fires(&tx, &series)?;
    tx.execute(
        "UPDATE remote_create_operations SET state='applied',claim_token=NULL,claimed_at_ms=NULL,
        applied_etag=?2,applied_raw_ics=?3,applied_at_ms=?4,updated_at_ms=?4 WHERE operation_id=?1",
        params![claim.operation_id, remote.etag, remote.raw_ics, now],
    )?;
    tx.commit()?;
    Ok(jobs_changed)
}

/// Releasing uncertain or expired work always preserves GET-first recovery.
/// There is deliberately no absence-to-ready or send-authorization API.
pub(super) fn defer(
    connection: &mut Connection,
    claim: RemoteCreationClaim,
    uncertain_delivery: bool,
    now: i64,
) -> Result<(), PersistenceError> {
    admitted_now_ms(now)?;
    let tx = connection.transaction()?;
    let row = tx
        .query_row(
            "SELECT claimed_at_ms,execution_phase,
            CASE WHEN (intent_version='utc-v1' AND codec_version='utc-v1' AND length(frozen_raw_ics)<=8192) OR (intent_version='utc-v2' AND codec_version='utc-v2' AND length(frozen_raw_ics)<=65536) THEN frozen_raw_ics END,CASE WHEN frozen_raw_ics IS NOT NULL AND codec_version IS NULL THEN 'invalid' ELSE codec_version END,
            send_state,send_attempt_id,send_attempted_at_ms,COALESCE(CASE WHEN (intent_version='utc-v1' AND length(CAST(intent_json AS BLOB))<=8192) OR (intent_version='utc-v2' AND length(CAST(intent_json AS BLOB))<=32768) THEN intent_json END,''),intent_version,idempotency_key,calendar_id,uid,created_at_ms
         FROM remote_create_operations WHERE operation_id=?1 AND account_id=?2
           AND claim_token=?3 AND state='pending'",
            params![claim.operation_id, claim.account_id, claim.claim_token],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, String>(7)?,r.get::<_, String>(8)?,r.get::<_, String>(9)?,r.get::<_, String>(10)?,r.get::<_, String>(11)?,r.get::<_, i64>(12)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    let semantics_valid = regenerate(&row.7, &row.8, &row.9, &row.10, &row.11, row.12)
        .is_ok_and(|content| row.3 == content.codec() && row.2 == content.bytes());
    if !valid_send_record(&row.4, row.5.as_deref(), row.6)
        || row.2 != claim.frozen_raw_ics
        || row.3 != claim.codec_version
        || row.8 != row.3
        || !matches!(row.3.as_str(), "utc-v1" | "utc-v2")
        || RemoteMutationExecutionPhase::from_storage(&row.1)? != claim.phase
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let verify = !semantics_valid
        || uncertain_delivery
        || row.4 != "not_started"
        || row.1 == "verify_first"
        || row.0 > now
        || now.saturating_sub(row.0) >= REMOTE_MUTATION_CLAIM_LEASE_MS;
    tx.execute(
        "UPDATE remote_create_operations SET claim_token=NULL,claimed_at_ms=NULL,
         execution_phase=?2,updated_at_ms=?3 WHERE operation_id=?1",
        params![
            claim.operation_id,
            if verify { "verify_first" } else { "ready" },
            now
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Pure reservation eligibility, never claim/freeze/record or rotate an epoch.
pub(super) fn reservation_target(
    connection: &mut Connection,
    account: &str,
    operation: &str,
) -> Result<(String, String), PersistenceError> {
    for id in [account, operation] {
        if !Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id && !u.is_nil()) {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
    }
    let tx = connection.transaction()?;
    let row=tx.query_row("SELECT o.calendar_id,o.resource_id,o.uid,o.collection_href,c.collection_url,o.exact_href,o.idempotency_key,COALESCE(CASE WHEN (o.intent_version='utc-v1' AND length(CAST(o.intent_json AS BLOB))<=8192) OR (o.intent_version='utc-v2' AND length(CAST(o.intent_json AS BLOB))<=32768) THEN o.intent_json END,''),o.created_at_ms,o.attempts,
        CASE WHEN (o.intent_version='utc-v1' AND o.codec_version='utc-v1' AND length(o.frozen_raw_ics)<=8192) OR (o.intent_version='utc-v2' AND o.codec_version='utc-v2' AND length(o.frozen_raw_ics)<=65536) THEN o.frozen_raw_ics END,CASE WHEN o.frozen_raw_ics IS NOT NULL AND o.codec_version IS NULL THEN 'invalid' ELSE o.codec_version END,c.privileges,o.intent_version
        FROM remote_create_operations o JOIN calendars c ON c.calendar_id=o.calendar_id AND c.account_id=o.account_id JOIN accounts a ON a.account_id=o.account_id
        WHERE o.operation_id=?1 AND o.account_id=?2 AND o.state='pending' AND o.execution_phase='ready'
        AND o.send_state='not_started' AND o.send_attempt_id IS NULL AND o.send_attempted_at_ms IS NULL
        AND o.claim_token IS NULL AND o.claimed_at_ms IS NULL AND a.enabled=1 AND a.removed_at_ms IS NULL
        AND a.credential_configured=1 AND a.remote_editing_enabled=1 AND c.available=1
        AND (o.intent_version='utc-v1' OR (a.provider_kind='caldav' AND a.region_profile='icloud-cn'))",params![operation,account],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?,r.get::<_,Option<Vec<u8>>>(10)?,r.get::<_,Option<String>>(11)?,r.get::<_,String>(12)?,r.get::<_,String>(13)?))).optional()?.ok_or(PersistenceError::RemoteMutationReadOnly)?;
    let (
        calendar,
        id,
        uid,
        saved,
        current,
        href,
        key,
        json,
        created,
        attempts,
        frozen,
        codec,
        privileges,
        version,
    ) = row;
    for value in [&calendar, &id, &uid] {
        if !Uuid::parse_str(value).is_ok_and(|u| u.to_string() == *value && !u.is_nil()) {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
    }
    if attempts < 0
        || !collection_content_writable(&privileges)
        || !collection_routes_match(&saved, &current)
        || !versioned_route_is_bounded(&version, &saved, &href)
        || !versioned_route_is_bounded(&version, &current, &href)
        || !resource_belongs_to_collection(&current, &href)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let path = if saved.starts_with('/') {
        saved.clone()
    } else {
        url::Url::parse(&saved)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
            .path()
            .to_owned()
    };
    if href != format!("{}/{}.ics", path.trim_end_matches('/'), id) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let content = regenerate(&json, &version, &key, &calendar, &uid, created)?;
    if !matches!((&frozen,codec.as_deref()),(None,None) if attempts==0)
        && !matches!((&frozen,codec.as_deref()),(Some(raw),Some(codec)) if codec==content.codec() && raw==content.bytes())
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM remote_resources WHERE resource_id=?1 OR (calendar_id=?2 AND exact_href=?3)
        UNION ALL SELECT 1 FROM identity_aliases WHERE calendar_id=?2 AND exact_href=?3
        UNION ALL SELECT 1 FROM event_series WHERE account_id=?4 AND uid=?5
        UNION ALL SELECT 1 FROM outbox_operations WHERE resource_id=?1
        UNION ALL SELECT 1 FROM remote_delete_operations WHERE resource_id=?1
        UNION ALL SELECT 1 FROM remote_create_operations WHERE operation_id<>?6 AND (resource_id=?1 OR uid=?5 OR (calendar_id=?2 AND exact_href=?3)))",params![id,calendar,href,account,uid,operation],|r|r.get::<_,bool>(0))? {return Err(PersistenceError::IdentityConflict);}
    tx.commit()?;
    Ok((calendar, id))
}

pub(super) fn queue(
    connection: &mut Connection,
    input: QueueRemoteEventCreate,
    now: i64,
) -> Result<RemoteCreationSummary, PersistenceError> {
    let input = input.validate()?;
    let intent = serde_json::json!({
        "title": input.title, "startAtUtcMs": input.start_at_utc_ms,
        "endAtUtcMs": input.end_at_utc_ms
    })
    .to_string();
    let tx = connection.transaction()?;
    let existing = tx
        .query_row(
            "SELECT operation_id,account_id,calendar_id,created_at_ms,intent_json
         FROM remote_create_operations WHERE idempotency_key=?1",
            [&input.idempotency_key],
            |row| {
                Ok((
                    RemoteCreationSummary {
                        operation_id: row.get(0)?,
                        account_id: row.get(1)?,
                        calendar_id: row.get(2)?,
                        created_at_ms: row.get(3)?,
                    },
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((summary, saved)) = existing {
        // Historical migration fixtures use the unversioned v1 schema.
        let versioned = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('remote_create_operations') WHERE name='intent_version')", [], |r| r.get::<_, bool>(0))?;
        if versioned
            && tx.query_row(
                "SELECT intent_version FROM remote_create_operations WHERE operation_id=?1",
                [&summary.operation_id],
                |r| r.get::<_, String>(0),
            )? != "utc-v1"
        {
            return Err(PersistenceError::IdempotencyConflict);
        }
        if summary.calendar_id != input.calendar_id || saved != intent {
            return Err(PersistenceError::IdempotencyConflict);
        }
        return Ok(summary);
    }
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE idempotency_key=?1
         UNION ALL SELECT 1 FROM remote_delete_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }
    let (account, route, privileges, enabled, archived, available, opted_in): (
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        bool,
    ) = tx
        .query_row(
            "SELECT a.account_id,c.collection_url,c.privileges,a.enabled,a.removed_at_ms,
                c.available,a.remote_editing_enabled
         FROM calendars c JOIN accounts a ON a.account_id=c.account_id
         WHERE c.calendar_id=?1",
            [&input.calendar_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    if !enabled || archived.is_some() || !available {
        return Err(PersistenceError::AccountDisabled);
    }
    if !opted_in || !collection_content_writable(&privileges) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let resource_id = Uuid::new_v4().to_string();
    if !collection_routes_match(&route, &route) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let collection_path = if route.starts_with('/') {
        route.clone()
    } else {
        url::Url::parse(&route)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
            .path()
            .to_owned()
    };
    let href = format!(
        "{}/{}.ics",
        collection_path.trim_end_matches('/'),
        resource_id
    );
    if !resource_belongs_to_collection(&route, &href) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2)",
        params![input.calendar_id, href],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::PendingMutationExists);
    }
    let operation_id = Uuid::new_v4().to_string();
    let uid = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO remote_create_operations(operation_id,idempotency_key,account_id,
         calendar_id,resource_id,uid,collection_href,exact_href,intent_json,created_at_ms,updated_at_ms,send_state)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,'not_started')",
        params![operation_id,input.idempotency_key,account,input.calendar_id,resource_id,uid,route,href,intent,now],
    )?;
    tx.commit()?;
    Ok(RemoteCreationSummary {
        operation_id,
        account_id: account,
        calendar_id: input.calendar_id,
        created_at_ms: now,
    })
}

pub(super) fn queue_v2(
    connection: &mut Connection,
    input: QueueRemoteEventCreateV2,
    now: i64,
) -> Result<RemoteCreationSummary, PersistenceError> {
    admitted_now_ms(now)?;
    let input = input.validate()?;
    let intent = serde_json::json!({
        "title": input.title, "startAtUtcMs": input.start_at_utc_ms,
        "endAtUtcMs": input.end_at_utc_ms,
        "version":"utc-v2", "description":input.description,"location":input.location
    })
    .to_string();
    if intent.len() > 32768 {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let tx = connection.transaction()?;
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE idempotency_key=?1
         UNION ALL SELECT 1 FROM remote_delete_operations WHERE idempotency_key=?1)",
        [&input.idempotency_key],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::IdempotencyConflict);
    }
    let existing = tx
        .query_row(
            "SELECT operation_id,account_id,calendar_id,created_at_ms,COALESCE(CASE WHEN length(CAST(intent_json AS BLOB))<=32768 THEN intent_json END,''),intent_version,uid,
             CASE WHEN length(frozen_raw_ics)<=65536 THEN frozen_raw_ics END,
             CASE WHEN frozen_raw_ics IS NOT NULL AND (codec_version IS NULL OR length(frozen_raw_ics)>65536) THEN 'invalid' ELSE codec_version END,attempts,send_state,send_attempt_id,send_attempted_at_ms
         FROM remote_create_operations WHERE idempotency_key=?1",
            [&input.idempotency_key],
            |row| {
                Ok((
                    RemoteCreationSummary {
                        operation_id: row.get(0)?,
                        account_id: row.get(1)?,
                        calendar_id: row.get(2)?,
                        created_at_ms: row.get(3)?,
                    },
                    row.get::<_, String>(4)?,row.get::<_, String>(5)?,row.get::<_, String>(6)?,row.get::<_, Option<Vec<u8>>>(7)?,row.get::<_, Option<String>>(8)?,row.get::<_, i64>(9)?,row.get::<_, String>(10)?,row.get::<_, Option<String>>(11)?,row.get::<_, Option<i64>>(12)?,
                ))
            },
        )
        .optional()?;
    if let Some((
        summary,
        saved,
        version,
        uid,
        frozen,
        codec,
        attempts,
        send_state,
        send_id,
        send_time,
    )) = existing
    {
        if version == "utc-v1" {
            return Err(PersistenceError::IdempotencyConflict);
        }
        let content = regenerate(
            &saved,
            &version,
            &input.idempotency_key,
            &summary.calendar_id,
            &uid,
            summary.created_at_ms,
        )?;
        if !valid_send_record(&send_state, send_id.as_deref(), send_time)
            || (!matches!((&frozen, codec.as_deref()), (None, None) if attempts == 0 && send_state == "not_started")
                && !matches!((&frozen,codec.as_deref()),(Some(raw),Some(codec)) if raw==content.bytes() && codec==content.codec()))
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        let saved_fields: StoredIntentV2 =
            serde_json::from_str(&saved).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
        let requested_fields: StoredIntentV2 =
            serde_json::from_str(&intent).map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
        if version != "utc-v2"
            || summary.calendar_id != input.calendar_id
            || saved_fields != requested_fields
        {
            return Err(PersistenceError::IdempotencyConflict);
        }
        return Ok(summary);
    }
    let (account, route, privileges, enabled, archived, available, opted_in): (
        String,
        String,
        String,
        bool,
        Option<i64>,
        bool,
        bool,
    ) = tx
        .query_row(
            "SELECT a.account_id,c.collection_url,c.privileges,a.enabled,a.removed_at_ms,
                c.available,a.remote_editing_enabled
         FROM calendars c JOIN accounts a ON a.account_id=c.account_id
         WHERE c.calendar_id=?1",
            [&input.calendar_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?
        .ok_or(PersistenceError::NotFound)?;
    if !enabled || archived.is_some() || !available {
        return Err(PersistenceError::AccountDisabled);
    }
    if !opted_in || !collection_content_writable(&privileges)
        || !tx.query_row("SELECT credential_configured=1 AND provider_kind='caldav' AND region_profile='icloud-cn' FROM accounts WHERE account_id=?1", [&account], |r|r.get::<_,bool>(0))? {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let resource_id = Uuid::new_v4().to_string();
    if !collection_routes_match(&route, &route) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let collection_path = if route.starts_with('/') {
        route.clone()
    } else {
        url::Url::parse(&route)
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?
            .path()
            .to_owned()
    };
    let href = format!(
        "{}/{}.ics",
        collection_path.trim_end_matches('/'),
        resource_id
    );
    if !versioned_route_is_bounded("utc-v2", &route, &href)
        || !resource_belongs_to_collection(&route, &href)
    {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_resources WHERE calendar_id=?1 AND exact_href=?2)",
        params![input.calendar_id, href],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(PersistenceError::PendingMutationExists);
    }
    let operation_id = Uuid::new_v4().to_string();
    let uid = Uuid::new_v4().to_string();
    regenerate(
        &intent,
        "utc-v2",
        &input.idempotency_key,
        &input.calendar_id,
        &uid,
        now,
    )?;
    tx.execute(
        "INSERT INTO remote_create_operations(operation_id,idempotency_key,account_id,
         calendar_id,resource_id,uid,collection_href,exact_href,intent_json,created_at_ms,updated_at_ms,send_state,intent_version)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,'not_started','utc-v2')",
        params![operation_id,input.idempotency_key,account,input.calendar_id,resource_id,uid,route,href,intent,now],
    )?;
    tx.commit()?;
    Ok(RemoteCreationSummary {
        operation_id,
        account_id: account,
        calendar_id: input.calendar_id,
        created_at_ms: now,
    })
}
