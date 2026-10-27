//! Actor-owned exact event-v3 operation data; no transport or dispatch.
use super::*;
#[derive(Clone)]
pub(crate) enum ConfirmedTargetInput {
    Create(QueueRemoteEventCreateV2),
    Update(QueueRemoteEventUpdateV3),
    Edit {
        update: QueueRemoteEventUpdateV3,
        base: Option<crate::domain::remote_event_update::RemoteEventUpdateFieldsV3>,
        metadata: Option<UpdateCalendarLocalMetadata>,
    },
    Delete(QueueRemoteEventDelete),
}
#[derive(Clone)]
pub(crate) struct ConfirmedTarget {
    actor: std::sync::Weak<()>,
    account: String,
    calendar: String,
    route: String,
    revision: Option<(String, String, String, String, Vec<u8>)>,
    predecessor: Option<String>,
}
impl std::fmt::Debug for ConfirmedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfirmedTarget(<redacted>)")
    }
}
impl ConfirmedTarget {
    fn same_target(&self, other: &Self, input: &ConfirmedTargetInput) -> bool {
        let same_owner = self.actor.upgrade().is_some()
            && std::sync::Weak::ptr_eq(&self.actor, &other.actor)
            && self.account == other.account
            && self.calendar == other.calendar
            && self.route == other.route;
        if !same_owner {
            return false;
        }
        if self.revision == other.revision {
            return true;
        }
        let (
            Some((before_resource, before_series, before_href, _, before_raw)),
            Some((after_resource, after_series, after_href, _, after_raw)),
            ConfirmedTargetInput::Update(update),
        ) = (&self.revision, &other.revision, input)
        else {
            return false;
        };
        before_resource == after_resource
            && before_series == after_series
            && before_href == after_href
            && crate::domain::remote_event_update::selected_fields_unchanged_v3(
                before_raw,
                after_raw,
                &update.fields,
            )
            .unwrap_or(false)
    }
    pub(crate) fn account_and_route(&self) -> (&str, &str) {
        (&self.account, &self.route)
    }
    pub(crate) fn is_successor(&self) -> bool {
        self.predecessor.is_some()
    }
}
pub(super) fn select_confirmed_target(
    c: &Connection,
    input: &ConfirmedTargetInput,
    actor: std::sync::Weak<()>,
) -> Result<ConfirmedTarget, PersistenceError> {
    if let ConfirmedTargetInput::Edit {
        update,
        base,
        metadata,
    } = input
    {
        let mut target =
            select_confirmed_target(c, &ConfirmedTargetInput::Update(update.clone()), actor)?;
        let raw = if let Some(head) = remote_update_successor::head(c, &update.resource_id)? {
            let raw = remote_update_successor::desired(c, &update.resource_id, &head)?;
            target.predecessor = Some(head.id);
            raw
        } else {
            target
                .revision
                .as_ref()
                .ok_or(PersistenceError::NotFound)?
                .4
                .clone()
        };
        if let Some(base) = base {
            if !crate::domain::remote_event_update::selected_base_matches_v3(&raw, base)? {
                return Err(PersistenceError::StaleSyncCapture);
            }
        }
        if let Some(metadata) = metadata {
            if metadata.resource_id != update.resource_id {
                return Err(PersistenceError::IdentityConflict);
            }
            metadata
                .clone()
                .validate()
                .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
        }
        return Ok(target);
    }
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let (calendar, resource) = match input {
        ConfirmedTargetInput::Create(v) => {
            v.clone().validate()?;
            (v.calendar_id.clone(), None)
        }
        ConfirmedTargetInput::Update(v) => {
            v.clone().validate()?;
            (String::new(), Some(v.resource_id.as_str()))
        }
        ConfirmedTargetInput::Edit { .. } => unreachable!(),
        ConfirmedTargetInput::Delete(v) => {
            v.clone().validate()?;
            (String::new(), Some(v.resource_id.as_str()))
        }
    };
    let (account, calendar, revision) = if let Some(resource) = resource {
        let (account, calendar, series, href, etag, uid, raw, parse_status): (String,String,String,String,String,String,Vec<u8>,String) = c.query_row(
            "SELECT c.account_id,r.calendar_id,r.series_id,r.exact_href,r.etag,s.uid,r.raw_ics,r.parse_status FROM remote_resources r JOIN calendars c ON c.calendar_id=r.calendar_id JOIN event_series s ON s.series_id=r.series_id AND s.account_id=c.account_id WHERE r.resource_id=?1 AND r.lifecycle='active' AND length(r.raw_ics) BETWEEN 1 AND 4194304 AND length(CAST(r.etag AS BLOB)) BETWEEN 1 AND 4096 AND length(CAST(r.exact_href AS BLOB)) BETWEEN 1 AND 2048",
            [resource], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))
            .optional()?.ok_or(PersistenceError::RemoteMutationReadOnly)?;
        let route = selected_calendar_route(c, &account, &calendar)?;
        if !strong_mutation_etag(&etag)
            || !remote_update_fields::bounded_fields_route(&route, &href)
        {
            return Err(PersistenceError::RemoteMutationReadOnly);
        }
        match input {
            ConfirmedTargetInput::Update(v) => {
                crate::domain::remote_event_update::validate_event_base_v3(
                    &raw,
                    &uid,
                    v.fields.time.is_some(),
                )?;
                crate::domain::remote_event_update::patch_single_utc_event_v3(&raw, &v.fields)?;
            }
            ConfirmedTargetInput::Delete(_) => {
                if parse_status != "parsed" {
                    crate::domain::remote_event_update::validate_event_base_v3(&raw, &uid, false)?;
                } else if contains_invitation_property(&raw)
                    || contains_recurrence_sensitive_property(&raw)
                    || !single_vevent_uid(&raw).is_ok_and(|candidate| candidate == uid)
                {
                    return Err(PersistenceError::RemoteMutationReadOnly);
                }
            }
            _ => unreachable!(),
        }
        (
            account,
            calendar,
            Some((resource.into(), series, href, etag, raw)),
        )
    } else {
        let account = c
            .query_row(
                "SELECT account_id FROM calendars WHERE calendar_id=?1",
                [&calendar],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(PersistenceError::NotFound)?;
        (account, calendar, None)
    };
    let route = selected_calendar_route(c, &account, &calendar)?;
    Ok(ConfirmedTarget {
        actor,
        account,
        calendar,
        route,
        revision,
        predecessor: None,
    })
}
pub(super) fn admit_confirmed_target(
    c: &mut Connection,
    input: ConfirmedTargetInput,
    target: ConfirmedTarget,
    actor: std::sync::Weak<()>,
    now: i64,
    connected: &ConnectedOwnerCheck,
) -> Result<ScopedEventSnapshot, PersistenceError> {
    let fresh = select_confirmed_target(c, &input, actor.clone())
        .map_err(|_| PersistenceError::StaleSyncCapture)?;
    let check_input = match &input {
        ConfirmedTargetInput::Edit { update, .. } => ConfirmedTargetInput::Update(update.clone()),
        other => other.clone(),
    };
    if fresh.predecessor != target.predecessor || !fresh.same_target(&target, &check_input) {
        return Err(PersistenceError::StaleSyncCapture);
    }
    if let ConfirmedTargetInput::Edit {
        update, metadata, ..
    } = input
    {
        if !connected_owner_current(Some(connected), &fresh.account, &fresh.route) {
            return Err(PersistenceError::RemoteAccountConnectionRequired);
        }
        let tx = c.transaction()?;
        let operation = if target.is_successor() {
            remote_update_successor::admit(&tx, update, now)?
        } else {
            let json = String::from_utf8(
                crate::domain::remote_event_update::encode_event_intent_v3(&update.fields)?,
            )
            .map_err(|_| PersistenceError::RemoteMutationReadOnly)?;
            remote_update_fields::queue_encoded_in(
                &tx,
                update.idempotency_key,
                update.resource_id,
                json,
                remote_update_fields::UpdateKind::EventV3,
                now,
            )?
        };
        if let Some(metadata) = metadata {
            update_calendar_local_metadata_in(&tx, metadata, now)?;
        }
        tx.commit()?;
        return Ok(ScopedEventSnapshot {
            kind: crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3,
            operation: operation.operation_id,
            account: fresh.account,
            calendar: fresh.calendar,
            resource: operation.resource_id,
            route: fresh.route,
            actor,
            created_at_ms: operation.created_at_ms,
            recovery: false,
        });
    }
    match input {
        ConfirmedTargetInput::Edit { .. } => unreachable!(),
        ConfirmedTargetInput::Create(v) => {
            admit_crud(c, ScopedCrudInput::Create(v), actor, now, Some(connected))
        }
        ConfirmedTargetInput::Update(v) => admit(
            c,
            v,
            EventConfirmation::Confirmed,
            actor,
            now,
            Some(connected),
        ),
        ConfirmedTargetInput::Delete(v) => {
            admit_crud(c, ScopedCrudInput::Delete(v), actor, now, Some(connected))
        }
    }
}
pub(crate) type ConnectedOwnerCheck = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
fn connected_owner_current(
    check: Option<&ConnectedOwnerCheck>,
    account: &str,
    route: &str,
) -> bool {
    check.map_or(cfg!(test), |check| check(account, route))
}
pub(crate) enum EventConfirmation {
    Confirmed,
}
#[derive(Clone)]
pub(crate) struct ScopedEventSnapshot {
    kind: crate::domain::remote_crud::RemoteCrudKind,
    operation: String,
    account: String,
    calendar: String,
    resource: String,
    route: String,
    actor: std::sync::Weak<()>,
    created_at_ms: i64,
    recovery: bool,
}

pub(super) fn restore(
    c: &Connection,
    identity: (
        crate::domain::remote_crud::RemoteCrudKind,
        &str,
        &str,
        &str,
        &str,
    ),
    actor: std::sync::Weak<()>,
    connected: Option<&ConnectedOwnerCheck>,
) -> Result<ScopedEventSnapshot, PersistenceError> {
    let (kind, operation, account, calendar, resource) = identity;
    // After restart a successor points explicitly to the head that still owns recovery.
    if kind == crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 {
        if let Some(head)=c.query_row("SELECT predecessor_operation_id FROM remote_update_successors WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4 AND state='pending'",params![operation,account,calendar,resource],|r|r.get::<_,String>(0)).optional()? {
            return restore(c,(kind,&head,account,calendar,resource),actor,connected);
        }
    }
    for id in [operation, account, calendar, resource] {
        validate_account_id(id)?;
    }
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let matches = match kind {
        crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2 => c.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_create_operations WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4 AND intent_version='utc-v2' AND state='pending')",
            params![operation, account, calendar, resource],
            |row| row.get::<_, bool>(0),
        )?,
        crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 => c.query_row(
            "SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4 AND operation_kind='update_event' AND state='pending')",
            params![operation, account, calendar, resource],
            |row| row.get::<_, bool>(0),
        )?,
        crate::domain::remote_crud::RemoteCrudKind::Delete => c.query_row("SELECT EXISTS(SELECT 1 FROM remote_delete_operations WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4 AND state='pending')",params![operation,account,calendar,resource],|r|r.get::<_,bool>(0))?,
    };
    if !matches {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let route = selected_calendar_route(c, account, calendar)?;
    if connected.is_some() && !connected_owner_current(connected, account, &route) {
        return Err(PersistenceError::RemoteAccountConnectionRequired);
    }
    let created_at_ms = match kind {
        crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2 => c.query_row(
            "SELECT created_at_ms FROM remote_create_operations WHERE operation_id=?1",
            [operation],
            |row| row.get(0),
        )?,
        crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 => c.query_row(
            "SELECT created_at_ms FROM outbox_operations WHERE operation_id=?1",
            [operation],
            |row| row.get(0),
        )?,
        crate::domain::remote_crud::RemoteCrudKind::Delete => c.query_row(
            "SELECT created_at_ms FROM remote_delete_operations WHERE operation_id=?1",
            [operation],
            |r| r.get(0),
        )?,
    };
    Ok(ScopedEventSnapshot {
        kind,
        operation: operation.into(),
        account: account.into(),
        calendar: calendar.into(),
        resource: resource.into(),
        route,
        actor,
        created_at_ms,
        recovery: true,
    })
}
impl ScopedEventSnapshot {
    pub(crate) fn is_recovery(&self) -> bool {
        self.recovery
    }
    pub(crate) fn matches_create(&self, c: &RemoteCreationClaim) -> bool {
        self.kind == crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2
            && self.operation == c.operation_id
            && self.account == c.account_id
            && self.calendar == c.calendar_id
            && self.route == c.collection_href
    }
    pub(crate) fn matches_delete(&self, c: &RemoteDeletionClaim) -> bool {
        self.kind == crate::domain::remote_crud::RemoteCrudKind::Delete
            && self.operation == c.operation_id
            && self.account == c.account_id
            && self.calendar == c.calendar_id
            && self.resource == c.resource_id
            && self.route == c.collection_href
    }
    pub(crate) fn kind(&self) -> crate::domain::remote_crud::RemoteCrudKind {
        self.kind
    }
    pub(crate) fn account_and_route(&self) -> (&str, &str) {
        (&self.account, &self.route)
    }
    pub(crate) fn receipt(&self) -> crate::domain::remote_crud::RemoteCrudReceipt {
        crate::domain::remote_crud::RemoteCrudReceipt {
            kind: self.kind,
            operation_id: self.operation.clone(),
            account_id: self.account.clone(),
            calendar_id: Some(self.calendar.clone()),
            resource_id: Some(self.resource.clone()),
            created_at_ms: self.created_at_ms,
            production_execution_available: false,
        }
    }
    pub(crate) fn ids(&self) -> (&str, &str, &str, &str) {
        (
            &self.operation,
            &self.account,
            &self.calendar,
            &self.resource,
        )
    }
    pub(crate) fn matches_collections(
        &self,
        account: &str,
        routes: [&str; 2],
        actor: &std::sync::Weak<()>,
    ) -> bool {
        self.account == account
            && routes.contains(&self.route.as_str())
            && self.matches_actor(actor)
    }
    pub(crate) fn matches_actor(&self, actor: &std::sync::Weak<()>) -> bool {
        self.actor.upgrade().is_some() && std::sync::Weak::ptr_eq(&self.actor, actor)
    }
    pub(crate) fn matches_claim(&self, claim: &RemoteEventMutationClaimV3) -> bool {
        self.kind == crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3
            && self.matches_fields(&claim.captured)
    }
    pub(super) fn matches_fields(&self, claim: &RemoteFieldsMutationClaim) -> bool {
        self.operation == claim.operation_id
            && self.account == claim.account_id
            && self.calendar == claim.calendar_id
            && self.resource == claim.resource_id
            && self.route == claim.collection_href
    }
}
impl std::fmt::Debug for ScopedEventSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScopedEventSnapshot(<redacted>)")
    }
}

pub(super) fn current_route(
    c: &Connection,
    scope: &ScopedEventSnapshot,
) -> Result<(), PersistenceError> {
    if scope.actor.upgrade().is_none()
        || selected_calendar_route(c, &scope.account, &scope.calendar)? != scope.route
    {
        return Err(PersistenceError::StaleSyncCapture);
    }
    Ok(())
}
pub(super) fn admit(
    c: &mut Connection,
    input: QueueRemoteEventUpdateV3,
    _: EventConfirmation,
    actor: std::sync::Weak<()>,
    now: i64,
    connected: Option<&ConnectedOwnerCheck>,
) -> Result<ScopedEventSnapshot, PersistenceError> {
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    validate_account_id(&input.resource_id)?;
    let (account,calendar): (String,String) = c.query_row("SELECT c.account_id,r.calendar_id FROM remote_resources r JOIN calendars c ON c.calendar_id=r.calendar_id WHERE r.resource_id=?1",[&input.resource_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(PersistenceError::NotFound)?;
    let route = selected_calendar_route(c, &account, &calendar)?;
    if !connected_owner_current(connected, &account, &route) {
        return Err(PersistenceError::RemoteAccountConnectionRequired);
    }
    let resource = input.resource_id.clone();
    let operation = remote_update_event::queue(c, input, now)?;
    if operation.account_id != account || operation.resource_id != resource {
        return Err(PersistenceError::IdentityConflict);
    }
    Ok(ScopedEventSnapshot {
        kind: crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3,
        operation: operation.operation_id,
        account,
        calendar,
        resource,
        route,
        actor,
        created_at_ms: operation.created_at_ms,
        recovery: false,
    })
}
pub(super) fn selected_calendar_route(
    c: &Connection,
    account: &str,
    calendar: &str,
) -> Result<String, PersistenceError> {
    let (route, privileges): (String,String) = c.query_row("SELECT CASE WHEN length(CAST(c.collection_url AS BLOB)) BETWEEN 2 AND 2048 THEN c.collection_url ELSE '' END, CASE WHEN length(CAST(c.privileges AS BLOB)) BETWEEN 1 AND 512 THEN c.privileges ELSE '' END FROM calendars c JOIN accounts a ON a.account_id=c.account_id WHERE c.calendar_id=?1 AND c.account_id=?2 AND c.selected=1 AND c.available=1 AND a.enabled=1 AND a.credential_configured=1 AND a.remote_editing_enabled=1 AND a.provider_kind='caldav' AND a.region_profile='icloud-cn' AND a.removed_at_ms IS NULL AND a.credential_ref='calendar-todo/account/' || a.account_id",params![calendar,account],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(PersistenceError::RemoteMutationReadOnly)?;
    if !probe_target::safe_route(&route) || !collection_content_writable(&privileges) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    Ok(route)
}
pub(super) fn status(
    c: &Connection,
    scope: &ScopedEventSnapshot,
) -> Result<crate::domain::remote_crud::RemoteCrudOperation, PersistenceError> {
    current_route(c, scope)?;
    if scope.kind == crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 {
        if let Some((state,created,updated,revision,predecessor))=c.query_row("SELECT state,created_at_ms,updated_at_ms,revision,predecessor_operation_id FROM remote_update_successors WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4",params![scope.operation,scope.account,scope.calendar,scope.resource],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?))).optional()? {
            return Ok(crate::domain::remote_crud::RemoteCrudOperation{operation_id:scope.operation.clone(),calendar_id:scope.calendar.clone(),resource_id:scope.resource.clone(),state:if state=="blocked_conflict" {RemoteMutationState::Conflict}else{RemoteMutationState::Pending},created_at_ms:created,updated_at_ms:updated,resolution_state:None,retry_operation_id:None,revision,predecessor_operation_id:Some(predecessor),stage:if state=="blocked_conflict" {"conflict"}else{"admittedPending"}.into()});
        }
    }
    let sql = match scope.kind {
        crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 => {
            "SELECT o.state,o.created_at_ms,o.updated_at_ms,c.resolution_state,c.retry_operation_id FROM outbox_operations o LEFT JOIN conflicts c ON c.operation_id=o.operation_id WHERE o.operation_kind='update_event' AND o.operation_id=?1 AND o.account_id=?2 AND o.calendar_id=?3 AND o.resource_id=?4"
        }
        crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2 => {
            "SELECT state,created_at_ms,updated_at_ms,NULL,NULL FROM remote_create_operations WHERE intent_version='utc-v2' AND operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4"
        }
        crate::domain::remote_crud::RemoteCrudKind::Delete => {
            "SELECT state,created_at_ms,updated_at_ms,resolution_state,retry_operation_id FROM remote_delete_operations WHERE operation_id=?1 AND account_id=?2 AND calendar_id=?3 AND resource_id=?4"
        }
    };
    let (state, created_at_ms, updated_at_ms, resolution, retry): (
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
    ) = c
        .query_row(
            sql,
            params![
                scope.operation,
                scope.account,
                scope.calendar,
                scope.resource
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .ok_or(PersistenceError::RemoteMutationReadOnly)?;
    if retry.as_deref().is_some_and(|id| {
        !Uuid::parse_str(id).is_ok_and(|value| !value.is_nil() && value.to_string() == id)
    }) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    let (revision, predecessor, phase, outcome) = if scope.kind
        == crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3
    {
        c.query_row("SELECT update_revision,predecessor_operation_id,execution_phase,last_outcome FROM outbox_operations WHERE operation_id=?1",[&scope.operation],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?)))?
    } else if scope.kind == crate::domain::remote_crud::RemoteCrudKind::Delete {
        c.query_row("SELECT execution_phase,last_outcome FROM remote_delete_operations WHERE operation_id=?1",[&scope.operation],|r|Ok((1,None,r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))?
    } else {
        (1, None, String::new(), None)
    };
    let stage = match state.as_str() {
        "applied" => "applied",
        "conflict" => "conflict",
        "cancelled" => "cancelled",
        _ if phase == "verify_first" || outcome.is_some() => "recoveryNeeded",
        _ => "admittedPending",
    }
    .into();
    Ok(crate::domain::remote_crud::RemoteCrudOperation {
        revision,
        predecessor_operation_id: predecessor,
        stage,
        operation_id: scope.operation.clone(),
        calendar_id: scope.calendar.clone(),
        resource_id: scope.resource.clone(),
        state: RemoteMutationState::from_storage(&state)?,
        created_at_ms,
        updated_at_ms,
        resolution_state: resolution
            .as_deref()
            .map(ConflictResolutionState::from_storage)
            .transpose()?,
        retry_operation_id: retry,
    })
}
pub(super) fn claim(
    c: &mut Connection,
    scope: &ScopedEventSnapshot,
    now: i64,
) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
    claim_current(c, scope, now, &|| true)
}
pub(super) fn claim_current(
    c: &mut Connection,
    scope: &ScopedEventSnapshot,
    now: i64,
    current: &dyn Fn() -> bool,
) -> Result<Option<RemoteEventMutationClaimV3>, PersistenceError> {
    if !current() {
        return Err(PersistenceError::StaleSyncCapture);
    }
    if status(c, scope)?.state != RemoteMutationState::Pending {
        return Ok(None);
    }
    Ok(remote_update_fields::claim_kind_scoped_guarded(
        c,
        &scope.account,
        now,
        remote_update_fields::UpdateKind::EventV3,
        Some(scope),
        current,
    )?
    .map(|captured| RemoteEventMutationClaimV3 { captured }))
}
pub(super) fn snapshot(
    c: &Connection,
    scope: &ScopedEventSnapshot,
    claim: &RemoteEventMutationClaimV3,
    now: i64,
) -> Result<RemoteEventUpdateSnapshotV3, PersistenceError> {
    current_route(c, scope)?;
    if !scope.matches_claim(claim) {
        return Err(PersistenceError::RemoteMutationReadOnly);
    }
    remote_update_event::snapshot(c, claim, now)
}
pub(super) fn read_allowed(
    c: &Connection,
    scope: &ScopedEventSnapshot,
    claim: &RemoteEventMutationClaimV3,
    now: i64,
) -> Result<bool, PersistenceError> {
    Ok(current_route(c, scope).is_ok()
        && scope.matches_claim(claim)
        && remote_update_event::read_allowed(c, claim, now)?)
}
pub(super) fn resolve(
    c: &mut Connection,
    scope: &ScopedEventSnapshot,
    choice: ConflictResolutionChoice,
    now: i64,
    connected: Option<&ConnectedOwnerCheck>,
) -> Result<ConflictResolutionResult, PersistenceError> {
    let state = status(c, scope)?;
    if choice == ConflictResolutionChoice::RetryLocal
        && !connected_owner_current(connected, &scope.account, &scope.route)
    {
        return Err(PersistenceError::RemoteAccountConnectionRequired);
    }
    if scope.kind != crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3 {
        return remote_crud::resolve(c, scope.kind, &scope.operation, choice, now);
    }
    if state.state == RemoteMutationState::Conflict
        && state.resolution_state == Some(ConflictResolutionState::Unresolved)
        && choice == ConflictResolutionChoice::RetryLocal
    {
        let blocked:bool=c.query_row("SELECT r.etag=o.base_etag FROM outbox_operations o JOIN remote_resources r ON r.resource_id=o.resource_id WHERE o.operation_kind='update_event' AND o.operation_id=?1 AND o.account_id=?2 AND o.calendar_id=?3 AND o.resource_id=?4",params![scope.operation,scope.account,scope.calendar,scope.resource],|r|r.get(0))?;
        if blocked {
            return Err(PersistenceError::ScopedEventRetryBlocked);
        }
    }
    remote_update_event::resolve(c, &scope.operation, choice, now)
}
pub(crate) enum ScopedCrudInput {
    Create(QueueRemoteEventCreateV2),
    Delete(QueueRemoteEventDelete),
}
pub(super) fn admit_crud(
    c: &mut Connection,
    input: ScopedCrudInput,
    actor: std::sync::Weak<()>,
    now: i64,
    connected: Option<&ConnectedOwnerCheck>,
) -> Result<ScopedEventSnapshot, PersistenceError> {
    if actor.upgrade().is_none() {
        return Err(PersistenceError::ActorUnavailable);
    }
    let (kind, account, calendar, resource, operation, created_at_ms, route) = match input {
        ScopedCrudInput::Create(input) => {
            let account: String = c.query_row(
                "SELECT account_id FROM calendars WHERE calendar_id=?1",
                [&input.calendar_id],
                |r| r.get(0),
            )?;
            let route = selected_calendar_route(c, &account, &input.calendar_id)?;
            if !connected_owner_current(connected, &account, &route) {
                return Err(PersistenceError::RemoteAccountConnectionRequired);
            }
            let op = remote_creation::queue_v2(c, input, now)?;
            let resource: String = c.query_row(
                "SELECT resource_id FROM remote_create_operations WHERE operation_id=?1",
                [&op.operation_id],
                |r| r.get(0),
            )?;
            (
                crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2,
                account,
                op.calendar_id,
                resource,
                op.operation_id,
                op.created_at_ms,
                route,
            )
        }
        ScopedCrudInput::Delete(input) => {
            let (account,calendar):(String,String)=c.query_row("SELECT c.account_id,r.calendar_id FROM remote_resources r JOIN calendars c ON c.calendar_id=r.calendar_id WHERE r.resource_id=?1",[&input.resource_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let route = selected_calendar_route(c, &account, &calendar)?;
            if !connected_owner_current(connected, &account, &route) {
                return Err(PersistenceError::RemoteAccountConnectionRequired);
            }
            let op = queue_remote_event_delete_scoped(c, input, now, true)?;
            (
                crate::domain::remote_crud::RemoteCrudKind::Delete,
                account,
                calendar,
                op.resource_id,
                op.operation_id,
                op.created_at_ms,
                route,
            )
        }
    };
    Ok(ScopedEventSnapshot {
        kind,
        account,
        calendar,
        resource,
        operation,
        route,
        actor,
        created_at_ms,
        recovery: false,
    })
}
pub(crate) fn scope_ids(scope: Option<&ScopedEventSnapshot>) -> (Option<&str>, Option<&str>) {
    scope.map_or((None, None), |s| (Some(s.ids().0), Some(s.ids().2)))
}
