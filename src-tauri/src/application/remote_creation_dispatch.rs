//! Internal consuming CREATE orchestration; AppState stays closed; shared worker receives real owner services.
#![allow(dead_code)]
use crate::{
    adapters::{persistence::Database, production_mutation::ProductionMutationSessions},
    application::{
        production_creation_owner::CreationReservationComposer,
        remote_creation::{
            RemoteCreationVerificationError, RemoteCreationVerificationOutcome,
            finish_creation_read_back,
        },
        remote_mutation_runtime::MutationRequestGate,
    },
    domain::remote_mutation::RemoteMutationExecutionPhase,
};
use std::sync::{Arc, Weak};

pub(crate) struct RemoteCreationDispatchService {
    database: Database,
    owners: CreationReservationComposer,
    sessions: Arc<ProductionMutationSessions>,
}
impl RemoteCreationDispatchService {
    pub(crate) fn new(
        database: Database,
        owners: CreationReservationComposer,
        sessions: Arc<ProductionMutationSessions>,
    ) -> Result<Self, ()> {
        if !Weak::ptr_eq(
            &database.actor_provenance(),
            &owners.registry().creation_owner_provenance().1,
        ) || !owners.current()
            || !sessions
                .creation_session_evidence(owners.account())
                .is_ok_and(|e| owners.accepts_session(&e))
        {
            return Err(());
        }
        Ok(Self {
            database,
            owners,
            sessions,
        })
    }
    pub(crate) fn account(&self) -> &str {
        self.owners.account()
    }
    pub(crate) fn matches_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.owners.matches_gate(gate)
    }
    pub(crate) fn current_under_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.owners.current_under_gate(gate)
    }
    pub(crate) fn belongs_to(
        &self,
        registry: &crate::application::account_registry::AccountSourceRegistry,
    ) -> bool {
        self.owners.current()
            && Weak::ptr_eq(
                &self.owners.registry().creation_owner_provenance().0,
                &registry.creation_owner_provenance().0,
            )
    }
    pub(crate) fn cancel_in_flight(&self, account: &str) -> bool {
        account == self.account()
            && crate::ports::remote_mutation::RemoteMutationTransport::cancel_in_flight(
                self.sessions.as_ref(),
                account,
            )
    }
    pub(crate) fn run_next(
        &self,
        request: &MutationRequestGate,
    ) -> Result<
        crate::application::remote_mutation::RemoteMutationRunOutcome,
        RemoteCreationVerificationError,
    > {
        use crate::application::remote_mutation::RemoteMutationRunOutcome as R;
        if !self.sessions.creation_current(&self.owners, request) {
            return Ok(R::Deferred);
        }
        let Some(operation) = self
            .database
            .select_account_remote_creation(self.account().into())?
        else {
            return Ok(R::Idle);
        };
        Ok(match self.run_operation(&operation, request)? {
            Some(RemoteCreationVerificationOutcome::Applied) => R::Applied,
            Some(RemoteCreationVerificationOutcome::Unresolved(_)) | None => R::Deferred,
        })
    }
    pub(crate) fn run_operation(
        &self,
        operation: &str,
        request: &MutationRequestGate,
    ) -> Result<Option<RemoteCreationVerificationOutcome>, RemoteCreationVerificationError> {
        let ownership = || self.sessions.creation_current(&self.owners, request);
        if !ownership() {
            return Ok(None);
        }
        // Eligibility is captured before claim; it never records an attempt.
        // Recovery needs no ready permit and cannot enter the consuming path.
        let permit = self.owners.eligibility(operation).ok();
        let Some(mut claim) = self
            .database
            .claim_account_remote_creation_v1(self.owners.account().into())?
        else {
            return Ok(None);
        };
        // This reviewed dispatcher remains utc-v1 only.
        if claim.codec_version != "utc-v1" || claim.operation_id != operation {
            self.database.defer_remote_creation(claim, false)?;
            return Ok(None);
        }
        if !ownership() {
            let _ = self.database.defer_remote_creation(claim, true);
            return Ok(None);
        }
        if claim.phase == RemoteMutationExecutionPhase::Ready {
            if !permit.is_some_and(|p| p.accepts_claim(&claim, self.owners.registry())) {
                self.database.defer_remote_creation(claim, true)?;
                return Ok(None);
            }
            let (refreshed, attempt) = self.database.record_remote_creation_first_send(claim)?;
            claim = refreshed;
            // Every response/error is uncertain. Status never supplies a verdict.
            // One consuming call spends the original evidence, even on failure.
            let _ = self
                .sessions
                .create_once_guarded(&self.owners, request, &claim, attempt);
        }
        let read_gate = || {
            ownership()
                && self
                    .database
                    .remote_creation_read_allowed(claim.clone())
                    .unwrap_or(false)
                && ownership()
        };
        if !read_gate() {
            let _ = self.database.defer_remote_creation(claim, true);
            return Ok(None);
        }
        let read = self
            .sessions
            .read_creation_guarded(&self.owners, request, &claim);
        if !read_gate() {
            let _ = self.database.defer_remote_creation(claim, true);
            return Ok(None);
        }
        match read {
            Ok(evidence) => finish_creation_read_back(
                &self.database,
                claim,
                evidence,
                &ownership,
                true,
                Some(request),
            ),
            Err(error) => {
                let _ = self.database.defer_remote_creation(claim, true);
                Err(error.into())
            }
        }
    }
}
impl std::fmt::Debug for RemoteCreationDispatchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteCreationDispatchService(<redacted>)")
    }
}
