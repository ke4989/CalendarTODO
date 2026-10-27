//! Execution-free supported CRUD admission/status preparation. Actor is authoritative.
use crate::{
    adapters::persistence::{Database, PersistenceError},
    application::remote_mutation_facade::{
        ConfirmedRemoteEventDelete, RemoteEventDeleteConfirmation,
    },
    domain::{
        remote_creation::QueueRemoteEventCreateV2,
        remote_crud::{
            RemoteCrudConflictReceipt, RemoteCrudKind as Kind, RemoteCrudReceipt, RemoteCrudStatus,
        },
        remote_event_update::{
            QueueRemoteEventUpdateV3, RemoteEventUpdateFieldsV3, UtcEventTimePair,
        },
        remote_mutation::{
            ConflictResolutionChoice, QueueRemoteEventDelete, RemoteOptionalTextUpdateV2,
        },
    },
};
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
mod confirmed_event;
pub(crate) use confirmed_event::*;
const MAX_INPUT_BYTES: usize = 32768;
fn selected<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInput {
    pub idempotency_key: String,
    pub calendar_id: String,
    pub title: String,
    #[serde(default, deserialize_with = "selected")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "selected")]
    pub location: Option<String>,
    pub start_at_utc_ms: i64,
    pub end_at_utc_ms: i64,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectedText {
    Set { value: String },
    Clear {},
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectedFields {
    #[serde(default, deserialize_with = "selected")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "selected")]
    pub description: Option<SelectedText>,
    #[serde(default, deserialize_with = "selected")]
    pub location: Option<SelectedText>,
    #[serde(default, deserialize_with = "selected")]
    pub time: Option<UtcEventTimePair>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateInput {
    pub idempotency_key: String,
    pub resource_id: String,
    pub fields: SelectedFields,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusInput {
    pub account_id: String,
    pub kind: Kind,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictInput {
    pub operation_id: String,
    pub kind: Kind,
    pub choice: ConflictResolutionChoice,
}
#[derive(Clone, Deserialize)]
#[serde(
    tag = "action",
    content = "input",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteCrudRequest {
    Create(CreateInput),
    Update(UpdateInput),
    Delete(ConfirmedRemoteEventDelete),
    Status(StatusInput),
    ResolveConflict(ConflictInput),
}
impl fmt::Debug for RemoteCrudRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCrudRequest(<redacted>)")
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum RemoteCrudResponse {
    Receipt(RemoteCrudReceipt),
    Status(RemoteCrudStatus),
    Conflict(RemoteCrudConflictReceipt),
}
#[derive(Debug, Error)]
pub enum RemoteCrudError {
    #[error("remote CRUD input is invalid")]
    InvalidInput,
    #[error("remote CRUD persistence is unavailable")]
    Persistence(#[from] PersistenceError),
}
#[derive(Clone)]
pub struct RemoteCrudApplication {
    database: Database,
    scoped_events: std::sync::Arc<
        std::sync::Mutex<
            std::collections::VecDeque<
                crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
            >,
        >,
    >,
}
impl RemoteCrudApplication {
    pub(crate) fn prepare_connected_json(
        &self,
        input: &str,
        execution: Option<
            &crate::application::remote_execution_activation::RemoteExecutionActivation,
        >,
    ) -> Result<RemoteCrudResponse, RemoteCrudError> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(RemoteCrudError::InvalidInput);
        }
        let request: RemoteCrudRequest =
            serde_json::from_str(input).map_err(|_| RemoteCrudError::InvalidInput)?;
        match request {
            RemoteCrudRequest::Status(_) => self.prepare(request),
            RemoteCrudRequest::ResolveConflict(ref input)
                if input.choice != ConflictResolutionChoice::RetryLocal =>
            {
                self.prepare(request)
            }
            RemoteCrudRequest::ResolveConflict(_) => Err(RemoteCrudError::Persistence(
                PersistenceError::RemoteMutationReadOnly,
            )),
            request => {
                let execution = execution.ok_or(RemoteCrudError::Persistence(
                    PersistenceError::RemoteAccountConnectionRequired,
                ))?;
                let scope = execution.admit_connected_event(&self.database, |connected| match request {
                    RemoteCrudRequest::Create(i) => self.database.admit_connected_scoped_crud(
                        crate::adapters::persistence::scoped_event::ScopedCrudInput::Create(QueueRemoteEventCreateV2 {
                            idempotency_key:i.idempotency_key,calendar_id:i.calendar_id,title:i.title,
                            description:i.description,location:i.location,start_at_utc_ms:i.start_at_utc_ms,end_at_utc_ms:i.end_at_utc_ms,
                        }),Some(connected)),
                    RemoteCrudRequest::Update(i) => {
                        let text=|value| match value {SelectedText::Set{value}=>RemoteOptionalTextUpdateV2::Set(value),SelectedText::Clear{}=>RemoteOptionalTextUpdateV2::Clear};
                        self.database.admit_connected_scoped_event(QueueRemoteEventUpdateV3 {
                            idempotency_key:i.idempotency_key,resource_id:i.resource_id,
                            fields:RemoteEventUpdateFieldsV3{title:i.fields.title,description:i.fields.description.map(text),location:i.fields.location.map(text),time:i.fields.time},
                        },crate::adapters::persistence::scoped_event::EventConfirmation::Confirmed,Some(connected))
                    }
                    RemoteCrudRequest::Delete(ConfirmedRemoteEventDelete{idempotency_key,resource_id,confirmation:RemoteEventDeleteConfirmation::Confirmed}) => self.database.admit_connected_scoped_crud(crate::adapters::persistence::scoped_event::ScopedCrudInput::Delete(QueueRemoteEventDelete{idempotency_key,resource_id}),Some(connected)),
                    _ => Err(PersistenceError::RemoteMutationReadOnly),
                })?;
                Ok(RemoteCrudResponse::Receipt(scope.receipt()))
            }
        }
    }
    pub(crate) fn without_execution(database: Database) -> Self {
        Self {
            database,
            scoped_events: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
        }
    }
    /// Raw bounded JSON preserves duplicate keys until strict DTO deserialization.
    /// No Value intermediate may erase them before this boundary.
    fn prepare(&self, input: RemoteCrudRequest) -> Result<RemoteCrudResponse, RemoteCrudError> {
        match input {
            RemoteCrudRequest::Create(i) => {
                let r = self
                    .database
                    .queue_remote_event_create_v2(QueueRemoteEventCreateV2 {
                        idempotency_key: i.idempotency_key,
                        calendar_id: i.calendar_id,
                        title: i.title,
                        description: i.description,
                        location: i.location,
                        start_at_utc_ms: i.start_at_utc_ms,
                        end_at_utc_ms: i.end_at_utc_ms,
                    })?;
                Ok(RemoteCrudResponse::Receipt(RemoteCrudReceipt {
                    kind: Kind::CreateUtcV2,
                    operation_id: r.operation_id,
                    account_id: r.account_id,
                    calendar_id: Some(r.calendar_id),
                    resource_id: None,
                    created_at_ms: r.created_at_ms,
                    production_execution_available: false,
                }))
            }
            RemoteCrudRequest::Update(i) => {
                let text = |t: SelectedText| match t {
                    SelectedText::Set { value } => RemoteOptionalTextUpdateV2::Set(value),
                    SelectedText::Clear {} => RemoteOptionalTextUpdateV2::Clear,
                };
                let r = self
                    .database
                    .queue_remote_event_update_v3(QueueRemoteEventUpdateV3 {
                        idempotency_key: i.idempotency_key,
                        resource_id: i.resource_id,
                        fields: RemoteEventUpdateFieldsV3 {
                            title: i.fields.title,
                            description: i.fields.description.map(text),
                            location: i.fields.location.map(text),
                            time: i.fields.time,
                        },
                    })?;
                Ok(RemoteCrudResponse::Receipt(RemoteCrudReceipt {
                    kind: Kind::UpdateEventV3,
                    operation_id: r.operation_id,
                    account_id: r.account_id,
                    calendar_id: None,
                    resource_id: Some(r.resource_id),
                    created_at_ms: r.created_at_ms,
                    production_execution_available: false,
                }))
            }
            RemoteCrudRequest::Delete(ConfirmedRemoteEventDelete {
                idempotency_key,
                resource_id,
                confirmation: RemoteEventDeleteConfirmation::Confirmed,
            }) => {
                let r = self
                    .database
                    .queue_remote_crud_delete(QueueRemoteEventDelete {
                        idempotency_key,
                        resource_id,
                    })?;
                Ok(RemoteCrudResponse::Receipt(RemoteCrudReceipt {
                    kind: Kind::Delete,
                    operation_id: r.operation_id,
                    account_id: r.account_id,
                    calendar_id: None,
                    resource_id: Some(r.resource_id),
                    created_at_ms: r.created_at_ms,
                    production_execution_available: false,
                }))
            }
            RemoteCrudRequest::Status(i) => Ok(RemoteCrudResponse::Status(
                self.database.remote_crud_status(i.account_id, i.kind)?,
            )),
            RemoteCrudRequest::ResolveConflict(i) => {
                let r =
                    self.database
                        .resolve_remote_crud_conflict(i.kind, i.operation_id, i.choice)?;
                Ok(RemoteCrudResponse::Conflict(RemoteCrudConflictReceipt {
                    kind: i.kind,
                    operation_id: r.operation_id,
                    account_id: r.account_id,
                    choice: r.choice,
                    retry_operation_id: r.retry_operation.map(|r| r.operation_id),
                    production_execution_available: false,
                }))
            }
        }
    }
}
impl fmt::Debug for RemoteCrudApplication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteCrudApplication(<redacted>)")
    }
}
