use super::*;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConfirmEventInput {
    pub idempotency_key: String,
    pub resource_id: String,
    pub fields: SelectedFields,
    #[serde(default)]
    pub base: Option<SelectedFields>,
    #[serde(default)]
    pub metadata: Option<crate::domain::calendar_event::UpdateCalendarLocalMetadata>,
    pub confirmation: EventConfirmation,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum EventConfirmation {
    Confirmed,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ExactEventKind {
    CreateUtcV2,
    UpdateEventV3,
    Delete,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExactEventInput {
    pub kind: ExactEventKind,
    pub operation_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub resource_id: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ScopedConflictInput {
    pub scope: ExactEventInput,
    pub choice: ConflictResolutionChoice,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum ConfirmedEventRequest {
    Confirm {
        #[serde(default = "update_kind")]
        kind: Kind,
        input: ConfirmInput,
    },
    Status {
        input: ExactEventInput,
    },
    Retry {
        input: ExactEventInput,
    },
    ResolveConflict {
        input: ScopedConflictInput,
    },
}
fn update_kind() -> Kind {
    Kind::UpdateEventV3
}
#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum ConfirmInput {
    Update(ConfirmEventInput),
    Create(ConfirmCreateInput),
    Delete(ConfirmedRemoteEventDelete),
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConfirmCreateInput {
    pub idempotency_key: String,
    pub calendar_id: String,
    pub title: String,
    #[serde(default, deserialize_with = "selected")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "selected")]
    pub location: Option<String>,
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
    pub confirmation: EventConfirmation,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConfirmedEventFailure {
    OwnerUnavailable,
    SubmitUnavailable,
    StatusUnavailable,
    RetryLocalBlocked,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfirmedEventView {
    pub receipt: RemoteCrudReceipt,
    pub operation: Option<crate::domain::remote_crud::RemoteCrudOperation>,
    pub runtime_state: Option<crate::application::remote_mutation_runtime::MutationRuntimeState>,
    pub failure: Option<ConfirmedEventFailure>,
}
pub(crate) fn parse_confirmed(input: &str) -> Result<ConfirmedEventRequest, RemoteCrudError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(RemoteCrudError::InvalidInput);
    }
    serde_json::from_str(input).map_err(|_| RemoteCrudError::InvalidInput)
}
impl ConfirmedEventView {
    pub(crate) fn from_runtime(
        view: crate::application::remote_mutation_runtime::ScopedEventRuntimeView,
    ) -> Self {
        let operation = view.operation;
        Self {
            receipt: RemoteCrudReceipt {
                kind: view.kind,
                operation_id: operation.operation_id.clone(),
                account_id: view.runtime.account_id,
                calendar_id: Some(operation.calendar_id.clone()),
                resource_id: Some(operation.resource_id.clone()),
                created_at_ms: operation.created_at_ms,
                production_execution_available: false,
            },
            operation: Some(operation),
            runtime_state: Some(view.runtime.state),
            failure: None,
        }
    }
}
impl RemoteCrudApplication {
    pub(crate) fn confirmed_target(
        &self,
        raw: &str,
    ) -> Result<Option<crate::adapters::persistence::scoped_event::ConfirmedTarget>, RemoteCrudError>
    {
        match parse_confirmed(raw)? {
            ConfirmedEventRequest::Confirm { kind, input } => Ok(Some(
                self.database
                    .select_confirmed_target(confirmed_target_input(kind, input)?)?,
            )),
            _ => Ok(None),
        }
    }
    fn scoped_view(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
        execution: Option<
            &crate::application::remote_execution_activation::RemoteExecutionActivation,
        >,
        failure: Option<ConfirmedEventFailure>,
    ) -> ConfirmedEventView {
        let operation = self.database.scoped_event_status(scope).ok();
        let runtime_state = execution
            .and_then(|execution| execution.scoped_event_view(scope).ok())
            .map(|view| view.runtime.state);
        let failure = failure.or_else(|| {
            if operation.is_none() {
                Some(ConfirmedEventFailure::StatusUnavailable)
            } else if runtime_state.is_none()
                && operation.as_ref().is_some_and(|operation| {
                    operation.state == crate::domain::remote_mutation::RemoteMutationState::Pending
                        && operation.predecessor_operation_id.is_none()
                })
            {
                Some(ConfirmedEventFailure::OwnerUnavailable)
            } else {
                None
            }
        });
        ConfirmedEventView {
            receipt: scope.receipt(),
            operation,
            runtime_state,
            failure,
        }
    }
    fn captured_scope(
        &self,
        input: &ExactEventInput,
    ) -> Result<crate::adapters::persistence::scoped_event::ScopedEventSnapshot, RemoteCrudError>
    {
        let kind = match input.kind {
            ExactEventKind::CreateUtcV2 => Kind::CreateUtcV2,
            ExactEventKind::UpdateEventV3 => Kind::UpdateEventV3,
            ExactEventKind::Delete => Kind::Delete,
        };
        self.scoped_events
            .lock()
            .map_err(|_| RemoteCrudError::InvalidInput)?
            .iter()
            .find(|scope| {
                scope.kind() == kind
                    && scope.ids()
                        == (
                            input.operation_id.as_str(),
                            input.account_id.as_str(),
                            input.calendar_id.as_str(),
                            input.resource_id.as_str(),
                        )
            })
            .cloned()
            .ok_or(RemoteCrudError::InvalidInput)
    }
    pub(crate) fn confirmed_json_guarded(
        &self,
        input: &str,
        execution: Option<
            &crate::application::remote_execution_activation::RemoteExecutionActivation,
        >,
        preflight: Option<(
            crate::adapters::persistence::scoped_event::ConfirmedTarget,
            std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
        )>,
    ) -> Result<ConfirmedEventView, RemoteCrudError> {
        match parse_confirmed(input)? {
            ConfirmedEventRequest::Confirm { kind, input } => {
                let execution = execution.ok_or(RemoteCrudError::Persistence(
                    PersistenceError::RemoteAccountConnectionRequired,
                ))?;
                let selected = confirmed_target_input(kind, input)?;
                let scope = execution.admit_connected_event(&self.database, |connected| {
                    if let Some((target, current)) = preflight {
                        let combined: crate::adapters::persistence::scoped_event::ConnectedOwnerCheck =
                            std::sync::Arc::new(move |account, route| current() && connected(account, route));
                        return self.database.admit_confirmed_target(selected, target, combined);
                    }
                    // Legacy injected service tests retain their connected-admission seam.
                    // Every production confirmation requires successful preflight.
                    if !cfg!(test) {
                        return Err(PersistenceError::RemoteAccountConnectionRequired);
                    }
                    use crate::adapters::persistence::scoped_event::{ConfirmedTargetInput as Target, ScopedCrudInput};
                    match selected {
                        Target::Update(input) => self.database.admit_connected_scoped_event(
                            input,
                            crate::adapters::persistence::scoped_event::EventConfirmation::Confirmed,
                            Some(connected),
                        ),
                        Target::Edit {update,..} => self.database.admit_connected_scoped_event(update,crate::adapters::persistence::scoped_event::EventConfirmation::Confirmed,Some(connected)),
                        Target::Create(input) => self.database.admit_connected_scoped_crud(ScopedCrudInput::Create(input), Some(connected)),
                        Target::Delete(input) => self.database.admit_connected_scoped_crud(ScopedCrudInput::Delete(input), Some(connected)),
                    }
                })?;
                // Bounded editor contexts retain only actor-issued opaque scopes, not transport authority.
                let remembered = if let Ok(mut scopes) = self.scoped_events.lock() {
                    scopes.retain(|old| old.ids().0 != scope.ids().0);
                    if scopes.len() >= 128 {
                        scopes.pop_front();
                    }
                    scopes.push_back(scope.clone());
                    true
                } else {
                    false
                };
                if !remembered {
                    return Ok(self.scoped_view(
                        &scope,
                        Some(execution),
                        Some(ConfirmedEventFailure::StatusUnavailable),
                    ));
                }
                let binding = match execution.prepare_confirmed_event_binding(scope.clone()) {
                    Ok(binding) => binding,
                    Err(_) => {
                        return Ok(self.scoped_view(
                            &scope,
                            Some(execution),
                            Some(ConfirmedEventFailure::OwnerUnavailable),
                        ));
                    }
                };
                let failure = execution
                    .submit_scoped_event(binding, scope.clone())
                    .err()
                    .map(|_| ConfirmedEventFailure::SubmitUnavailable);
                Ok(self.scoped_view(&scope, Some(execution), failure))
            }
            ConfirmedEventRequest::Status { input } => {
                let scope = self.captured_scope(&input)?;
                Ok(self.scoped_view(&scope, execution, None))
            }
            ConfirmedEventRequest::Retry { input } => {
                let execution = execution.ok_or(RemoteCrudError::Persistence(
                    PersistenceError::RemoteAccountConnectionRequired,
                ))?;
                let kind = exact_kind(&input);
                let initial = self.database.restore_scoped_event(
                    kind,
                    input.operation_id.clone(),
                    input.account_id.clone(),
                    input.calendar_id.clone(),
                    input.resource_id.clone(),
                    None,
                )?;
                let (account, route) = initial.account_and_route();
                execution
                    .ensure_confirmed_connection(account, route)
                    .map_err(|_| {
                        RemoteCrudError::Persistence(
                            PersistenceError::RemoteAccountConnectionRequired,
                        )
                    })?;
                let scope = execution.admit_connected_event(&self.database, |connected| {
                    self.database.restore_scoped_event(
                        kind,
                        input.operation_id,
                        input.account_id,
                        input.calendar_id,
                        input.resource_id,
                        Some(connected),
                    )
                })?;
                // An existing durable operation owns its ready/verify-first phase.
                // Retry only resubmits that exact operation; it never creates a
                // fresh intent or infers resend permission from remote absence.
                if let Ok(mut scopes) = self.scoped_events.lock() {
                    scopes.retain(|old| old.ids().0 != scope.ids().0);
                    if scopes.len() >= 128 {
                        scopes.pop_front();
                    }
                    scopes.push_back(scope.clone());
                } else {
                    return Ok(self.scoped_view(
                        &scope,
                        Some(execution),
                        Some(ConfirmedEventFailure::StatusUnavailable),
                    ));
                }
                let binding = match execution.prepare_confirmed_event_binding(scope.clone()) {
                    Ok(binding) => binding,
                    Err(_) => {
                        return Ok(self.scoped_view(
                            &scope,
                            Some(execution),
                            Some(ConfirmedEventFailure::OwnerUnavailable),
                        ));
                    }
                };
                let failure = execution
                    .submit_scoped_event(binding, scope.clone())
                    .err()
                    .map(|_| ConfirmedEventFailure::SubmitUnavailable);
                Ok(self.scoped_view(&scope, Some(execution), failure))
            }
            ConfirmedEventRequest::ResolveConflict { input } => {
                let scope = self.captured_scope(&input.scope)?;
                let result = if input.choice == ConflictResolutionChoice::RetryLocal {
                    let execution = execution.ok_or(RemoteCrudError::Persistence(
                        PersistenceError::RemoteAccountConnectionRequired,
                    ))?;
                    execution.admit_connected_event(&self.database, |connected| {
                        self.database.resolve_connected_scoped_event(
                            &scope,
                            input.choice,
                            Some(connected),
                        )
                    })
                } else {
                    self.database.resolve_scoped_event(&scope, input.choice)
                };
                let failure = match result {
                    Err(PersistenceError::RemoteAccountConnectionRequired) => {
                        return Err(RemoteCrudError::Persistence(
                            PersistenceError::RemoteAccountConnectionRequired,
                        ));
                    }
                    Ok(_) => None,
                    Err(PersistenceError::ScopedEventRetryBlocked) => {
                        Some(ConfirmedEventFailure::RetryLocalBlocked)
                    }
                    Err(_) => Some(ConfirmedEventFailure::StatusUnavailable),
                };
                Ok(self.scoped_view(&scope, execution, failure))
            }
        }
    }
}

fn exact_kind(input: &ExactEventInput) -> Kind {
    match input.kind {
        ExactEventKind::CreateUtcV2 => Kind::CreateUtcV2,
        ExactEventKind::UpdateEventV3 => Kind::UpdateEventV3,
        ExactEventKind::Delete => Kind::Delete,
    }
}
fn confirmed_target_input(
    kind: Kind,
    input: ConfirmInput,
) -> Result<crate::adapters::persistence::scoped_event::ConfirmedTargetInput, PersistenceError> {
    use crate::adapters::persistence::scoped_event::ConfirmedTargetInput as Target;
    match &input {
        ConfirmInput::Update(v) => {
            let EventConfirmation::Confirmed = &v.confirmation;
        }
        ConfirmInput::Create(v) => {
            let EventConfirmation::Confirmed = &v.confirmation;
        }
        ConfirmInput::Delete(v) => {
            let RemoteEventDeleteConfirmation::Confirmed = v.confirmation;
        }
    }
    let text = |value: SelectedText| match value {
        SelectedText::Set { value } => RemoteOptionalTextUpdateV2::Set(value),
        SelectedText::Clear {} => RemoteOptionalTextUpdateV2::Clear,
    };
    Ok(match (kind, input) {
        (Kind::UpdateEventV3, ConfirmInput::Update(v)) => Target::Edit {
            update: QueueRemoteEventUpdateV3 {
                idempotency_key: v.idempotency_key,
                resource_id: v.resource_id,
                fields: RemoteEventUpdateFieldsV3 {
                    title: v.fields.title,
                    description: v.fields.description.map(text),
                    location: v.fields.location.map(text),
                    time: v.fields.time,
                },
            }
            .validate()?,
            base: v
                .base
                .map(|base| RemoteEventUpdateFieldsV3 {
                    title: base.title,
                    description: base.description.map(&text),
                    location: base.location.map(&text),
                    time: base.time,
                })
                .map(|v| v.validate())
                .transpose()?,
            metadata: v
                .metadata
                .map(|v| v.validate())
                .transpose()
                .map_err(|_| PersistenceError::RemoteMutationReadOnly)?,
        },
        (Kind::CreateUtcV2, ConfirmInput::Create(v)) => Target::Create(
            QueueRemoteEventCreateV2 {
                idempotency_key: v.idempotency_key,
                calendar_id: v.calendar_id,
                title: v.title,
                description: v.description,
                location: v.location,
                start_at_utc_ms: v.start_at_utc_ms,
                end_at_utc_ms: v.end_at_utc_ms,
            }
            .validate()?,
        ),
        (Kind::Delete, ConfirmInput::Delete(v)) => Target::Delete(
            QueueRemoteEventDelete {
                idempotency_key: v.idempotency_key,
                resource_id: v.resource_id,
            }
            .validate()?,
        ),
        _ => {
            return Err(PersistenceError::RemoteMutation(
                crate::domain::remote_mutation::RemoteMutationError::InvalidIdentity,
            ));
        }
    })
}
