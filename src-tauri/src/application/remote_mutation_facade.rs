use crate::{
    adapters::persistence::Database,
    application::{
        account_registry::AccountSourceRegistry,
        production_mutation_conflict::ReviewedMutationConflictSurface,
        remote_mutation_runtime::{
            MutationRuntimeError, MutationRuntimeView, RemoteMutationRuntime,
        },
    },
    domain::remote_mutation::{
        ConflictResolutionChoice, ConflictResolutionResult, ConflictResolutionState,
        QueueRemoteEventDelete, QueueRemoteTitleUpdate, RemoteConflictSummary, RemoteMutationState,
        RemoteMutationSummary,
    },
    ports::clock::Clock,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

const MAX_VISIBLE_ITEMS: usize = 128;

pub const MUTATION_APPLICATION_EVENT: &str = "calendar-todo://remote-mutation-application";

/// A deletion intent cannot be admitted from an omitted/defaulted confirmation.
/// The sole accepted wire value is the explicit `"confirmed"` literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteEventDeleteConfirmation {
    Confirmed,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmedRemoteEventDelete {
    pub idempotency_key: String,
    pub resource_id: String,
    pub confirmation: RemoteEventDeleteConfirmation,
}

impl fmt::Debug for ConfirmedRemoteEventDelete {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfirmedRemoteEventDelete(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationAuthorizationView {
    pub can_queue_intent: bool,
    pub can_keep_remote: bool,
    pub can_retry_local: bool,
    pub production_execution_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationOperationView {
    pub operation_id: String,
    pub resource_id: String,
    pub state: RemoteMutationState,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationConflictView {
    pub operation_id: String,
    pub resource_id: String,
    pub resolution_state: ConflictResolutionState,
    pub retry_operation_id: Option<String>,
    pub detected_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationApplicationView {
    pub account_id: String,
    pub authorization: MutationAuthorizationView,
    pub runtime: MutationRuntimeView,
    pub operation_count: u32,
    pub conflict_count: u32,
    pub operations: Vec<MutationOperationView>,
    pub conflicts: Vec<MutationConflictView>,
}

pub trait MutationApplicationEmitter: Send + Sync + 'static {
    fn emit(&self, view: MutationApplicationView);
}

/// Unforgeable outside this module: presence means one facade owns the bounded
/// view, redacted emitter, and both actor-backed conflict actions.
pub(crate) struct CompleteMutationConflictSurface(());

trait MutationStore: Send + Sync + 'static {
    fn queue(
        &self,
        input: QueueRemoteTitleUpdate,
    ) -> Result<RemoteMutationSummary, MutationApplicationError>;
    fn queue_delete(
        &self,
        input: QueueRemoteEventDelete,
    ) -> Result<RemoteMutationSummary, MutationApplicationError>;
    fn resolve(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
        now_ms: i64,
    ) -> Result<ConflictResolutionResult, MutationApplicationError>;
    fn operations(&self) -> Result<Vec<RemoteMutationSummary>, MutationApplicationError>;
    fn conflicts(&self) -> Result<Vec<RemoteConflictSummary>, MutationApplicationError>;
}

impl MutationStore for Database {
    fn queue(
        &self,
        input: QueueRemoteTitleUpdate,
    ) -> Result<RemoteMutationSummary, MutationApplicationError> {
        Ok(self.queue_remote_title_update(input)?)
    }

    fn queue_delete(
        &self,
        input: QueueRemoteEventDelete,
    ) -> Result<RemoteMutationSummary, MutationApplicationError> {
        Ok(self.queue_remote_event_delete(input)?)
    }

    fn resolve(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
        now_ms: i64,
    ) -> Result<ConflictResolutionResult, MutationApplicationError> {
        Ok(self.resolve_remote_conflict(operation_id, choice, now_ms)?)
    }

    fn operations(&self) -> Result<Vec<RemoteMutationSummary>, MutationApplicationError> {
        Ok(self.list_remote_mutations()?)
    }

    fn conflicts(&self) -> Result<Vec<RemoteConflictSummary>, MutationApplicationError> {
        Ok(self.list_remote_conflicts()?)
    }
}

trait MutationAccountStatus: Send + Sync + 'static {
    fn authorization(&self, account_id: &str) -> Result<(bool, bool), MutationApplicationError>;
}

impl MutationAccountStatus for AccountSourceRegistry {
    fn authorization(&self, account_id: &str) -> Result<(bool, bool), MutationApplicationError> {
        let profile = self.profile(account_id)?;
        Ok((profile.enabled, profile.remote_editing_enabled))
    }
}

trait MutationRuntimeStatus: Send + Sync + 'static {
    fn state(&self, account_id: &str) -> Result<MutationRuntimeView, MutationApplicationError>;
}

impl MutationRuntimeStatus for RemoteMutationRuntime {
    fn state(&self, account_id: &str) -> Result<MutationRuntimeView, MutationApplicationError> {
        Ok(RemoteMutationRuntime::state(self, account_id)?)
    }
}

#[derive(Clone)]
pub struct RemoteMutationApplication {
    store: Arc<dyn MutationStore>,
    accounts: Arc<dyn MutationAccountStatus>,
    runtime: Arc<dyn MutationRuntimeStatus>,
    clock: Arc<dyn Clock>,
    emitter: Arc<dyn MutationApplicationEmitter>,
    surface_owner: Arc<()>,
    surface_provenance: Option<(std::sync::Weak<()>, std::sync::Weak<()>)>,
    execution_closed: bool,
}

impl RemoteMutationApplication {
    pub(crate) fn same_admission_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.surface_owner, &other.surface_owner)
    }
    pub fn new(
        database: Database,
        accounts: Arc<AccountSourceRegistry>,
        runtime: Arc<RemoteMutationRuntime>,
        clock: Arc<dyn Clock>,
        emitter: Arc<dyn MutationApplicationEmitter>,
    ) -> Self {
        Self {
            execution_closed: false,
            surface_provenance: Some((
                database.actor_provenance(),
                accounts.creation_owner_provenance().0,
            )),
            store: Arc::new(database),
            accounts,
            runtime,
            clock,
            emitter,
            surface_owner: Arc::new(()),
        }
    }

    /// Builds the bounded application surface while production execution stays
    /// deliberately absent from AppState. Local intent/conflict actions still
    /// pass through the actor, and the public view always reports no executor.
    pub fn without_execution(
        database: Database,
        accounts: Arc<AccountSourceRegistry>,
        clock: Arc<dyn Clock>,
        emitter: Arc<dyn MutationApplicationEmitter>,
    ) -> Self {
        Self {
            execution_closed: true,
            surface_provenance: Some((
                database.actor_provenance(),
                accounts.creation_owner_provenance().0,
            )),
            store: Arc::new(database),
            accounts,
            runtime: Arc::new(UnavailableMutationRuntime),
            clock,
            emitter,
            surface_owner: Arc::new(()),
        }
    }

    pub(crate) fn scoped_conflict_surface(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
        account: &str,
    ) -> Result<ReviewedMutationConflictSurface, ()> {
        if !self.execution_closed {
            return Err(());
        }
        let (actor, registry) = self.surface_provenance.as_ref().ok_or(())?;
        let attachment = gate
            .attachments()
            .conflict_owner_under_gate(gate, account, registry, actor)?;
        let evidence = ReviewedMutationConflictSurface::scoped(
            &CompleteMutationConflictSurface(()),
            &self.surface_owner,
            actor.clone(),
            registry.clone(),
            attachment,
        );
        if !evidence.verify() {
            return Err(());
        }
        Ok(evidence)
    }

    pub fn queue_title(
        &self,
        input: QueueRemoteTitleUpdate,
    ) -> Result<RemoteMutationSummary, MutationApplicationError> {
        let summary = self.store.queue(input)?;
        self.emit_account(&summary.account_id)?;
        Ok(summary)
    }

    pub fn queue_confirmed_delete(
        &self,
        input: ConfirmedRemoteEventDelete,
    ) -> Result<MutationApplicationView, MutationApplicationError> {
        let ConfirmedRemoteEventDelete {
            idempotency_key,
            resource_id,
            confirmation: RemoteEventDeleteConfirmation::Confirmed,
        } = input;
        let summary = self.store.queue_delete(QueueRemoteEventDelete {
            idempotency_key,
            resource_id,
        })?;
        self.emit_account(&summary.account_id)?;
        self.view(&summary.account_id)
    }

    pub fn resolve_conflict(
        &self,
        operation_id: String,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, MutationApplicationError> {
        let now_ms = self.clock.now_utc_ms()?;
        let result = self.store.resolve(operation_id, choice, now_ms)?;
        self.emit_account(&result.account_id)?;
        Ok(result)
    }

    pub fn resolve_account_conflict(
        &self,
        account_id: &str,
        operation_id: String,
        choice: ConflictResolutionChoice,
    ) -> Result<ConflictResolutionResult, MutationApplicationError> {
        Uuid::parse_str(account_id).map_err(|_| MutationApplicationError::InvalidIdentity)?;
        Uuid::parse_str(&operation_id).map_err(|_| MutationApplicationError::InvalidIdentity)?;
        let belongs_to_account = self
            .store
            .conflicts()?
            .into_iter()
            .any(|item| item.account_id == account_id && item.operation_id == operation_id);
        if !belongs_to_account {
            return Err(MutationApplicationError::InvalidIdentity);
        }
        self.resolve_conflict(operation_id, choice)
    }

    pub fn view(
        &self,
        account_id: &str,
    ) -> Result<MutationApplicationView, MutationApplicationError> {
        Uuid::parse_str(account_id).map_err(|_| MutationApplicationError::InvalidIdentity)?;
        let (enabled, opted_in) = self.accounts.authorization(account_id)?;
        let runtime = self.runtime.state(account_id)?;
        let mut operations = self
            .store
            .operations()?
            .into_iter()
            .filter(|item| item.account_id == account_id)
            .collect::<Vec<_>>();
        let mut conflicts = self
            .store
            .conflicts()?
            .into_iter()
            .filter(|item| item.account_id == account_id)
            .collect::<Vec<_>>();
        let operation_count = bounded_count(operations.len());
        let conflict_count = bounded_count(conflicts.len());
        operations.reverse();
        conflicts.reverse();
        let unresolved = conflicts
            .iter()
            .any(|item| item.resolution_state == ConflictResolutionState::Unresolved);
        Ok(MutationApplicationView {
            account_id: account_id.to_owned(),
            authorization: MutationAuthorizationView {
                can_queue_intent: enabled && opted_in,
                can_keep_remote: unresolved,
                can_retry_local: enabled && opted_in && unresolved,
                production_execution_available: false,
            },
            runtime,
            operation_count,
            conflict_count,
            operations: operations
                .into_iter()
                .take(MAX_VISIBLE_ITEMS)
                .map(|item| MutationOperationView {
                    operation_id: item.operation_id,
                    resource_id: item.resource_id,
                    state: item.state,
                    updated_at_ms: item.updated_at_ms,
                })
                .collect(),
            conflicts: conflicts
                .into_iter()
                .take(MAX_VISIBLE_ITEMS)
                .map(|item| MutationConflictView {
                    operation_id: item.operation_id,
                    resource_id: item.resource_id,
                    resolution_state: item.resolution_state,
                    retry_operation_id: item.retry_operation_id,
                    detected_at_ms: item.detected_at_ms,
                    resolved_at_ms: item.resolved_at_ms,
                })
                .collect(),
        })
    }

    fn emit_account(&self, account_id: &str) -> Result<(), MutationApplicationError> {
        self.emitter.emit(self.view(account_id)?);
        Ok(())
    }
}

struct UnavailableMutationRuntime;

impl MutationRuntimeStatus for UnavailableMutationRuntime {
    fn state(&self, account_id: &str) -> Result<MutationRuntimeView, MutationApplicationError> {
        Ok(MutationRuntimeView {
            account_id: account_id.to_owned(),
            state: crate::application::remote_mutation_runtime::MutationRuntimeState::Idle,
            applied_count: 0,
            conflict_count: 0,
        })
    }
}

fn bounded_count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[derive(Debug, Error)]
pub enum MutationApplicationError {
    #[error("remote mutation identity is invalid")]
    InvalidIdentity,
    #[error("remote mutation application is unavailable")]
    Unavailable,
    #[error(transparent)]
    Persistence(#[from] crate::adapters::persistence::PersistenceError),
    #[error(transparent)]
    Account(#[from] crate::application::account_registry::AccountRegistryError),
    #[error(transparent)]
    Runtime(#[from] MutationRuntimeError),
    #[error(transparent)]
    Clock(#[from] crate::ports::clock::ClockError),
}
