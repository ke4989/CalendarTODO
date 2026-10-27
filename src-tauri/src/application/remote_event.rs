//! Internal event-v3 service; sealed worker collection only, no product entry.
use crate::{
    adapters::persistence::{Database, PersistenceError},
    adapters::production_mutation::ProductionEventTransport,
    application::remote_mutation::RemoteMutationRunOutcome as Outcome,
    domain::{
        remote_event_update::RemoteEventMutationClaimV3,
        remote_mutation::{RemoteMutationExecutionPhase, single_vevent_uid_v2},
    },
    ports::remote_mutation::MutationTransportOwnership,
};
use std::sync::Arc;
#[allow(dead_code)]
pub(crate) struct RemoteEventService {
    database: Database,
    transport: Arc<ProductionEventTransport>,
}
#[allow(dead_code)]
impl RemoteEventService {
    pub(crate) fn scoped_status(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<crate::domain::remote_crud::RemoteCrudOperation, PersistenceError> {
        self.database.scoped_event_status(scope)
    }
    pub(crate) fn new(
        database: Database,
        transport: Arc<ProductionEventTransport>,
        _: &crate::adapters::production_mutation::CompleteEventTransportAuthorization,
    ) -> Self {
        Self {
            database,
            transport,
        }
    }
    pub(crate) fn run_with_request(
        &self,
        account: &str,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Outcome, PersistenceError> {
        self.run(
            account,
            &MutationTransportOwnership::from_request(request),
            request,
        )
    }
    pub(crate) fn belongs_to(
        &self,
        accounts: &crate::application::account_registry::AccountSourceRegistry,
    ) -> bool {
        self.transport.belongs_to(accounts)
    }
    pub(crate) fn matches_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.transport.matches_gate(gate)
    }
    pub(crate) fn current_under_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        self.transport.current_under_gate(gate)
    }
    pub(crate) fn shares_session(
        &self,
        fields: &crate::adapters::production_mutation::ProductionFieldsTransport,
    ) -> bool {
        self.transport.shares_session(fields)
    }
    pub(crate) fn cancel_in_flight(&self, account: &str) -> bool {
        self.transport.cancel_in_flight(account)
    }
    fn defer(&self, claim: RemoteEventMutationClaimV3) -> Result<Outcome, PersistenceError> {
        match self.database.defer_remote_event_mutation(claim, true) {
            Ok(()) | Err(PersistenceError::OutboxClaimLost) => Ok(Outcome::Deferred),
            Err(error) => Err(error),
        }
    }
    fn run(
        &self,
        account: &str,
        ownership: &MutationTransportOwnership<'_>,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Outcome, PersistenceError> {
        if account != self.transport.account_id() || !self.transport.current(ownership) {
            return Ok(Outcome::Deferred);
        }
        let Some(mut claim) = (if let Some(scope) = request.event_scope() {
            self.database.claim_scoped_event_for_request(scope, request)
        } else {
            self.database
                .claim_account_remote_event_mutation(account.into())
        })?
        else {
            return Ok(Outcome::Idle);
        };
        if !self.transport.current(ownership) {
            return self.defer(claim);
        }
        if claim.captured.phase == RemoteMutationExecutionPhase::VerifyFirst {
            let remote = match self.transport.get(&claim, ownership) {
                Ok(r) => r,
                Err(_) => return self.defer(claim),
            };
            if !self.transport.current(ownership) {
                return self.defer(claim);
            }
            if remote.has_bounded_strong_revision()
                && remote.etag == claim.captured.base_etag
                && remote.raw_ics == claim.captured.base_raw_ics
            {
                claim = match self
                    .database
                    .restore_remote_event_base_for_request(&claim, remote, request)
                {
                    Ok(restored) => restored,
                    Err(_) => return self.defer(claim),
                };
            } else {
                return self.finish(claim, remote, ownership, request);
            }
        }
        let snapshot = match if let Some(scope) = request.event_scope() {
            self.database.scoped_event_send_snapshot(scope, &claim)
        } else {
            self.database.remote_event_send_snapshot(&claim)
        } {
            Ok(s) => s,
            Err(_) => return self.defer(claim),
        };
        if !self.transport.current(ownership) {
            return self.defer(claim);
        }
        // Any attempted outcome requires one exact GET while ownership permits.
        let _ = self.transport.put(&claim, snapshot, ownership);
        if !self.transport.current(ownership) {
            return self.defer(claim);
        }
        let remote = match self.transport.get(&claim, ownership) {
            Ok(r) => r,
            Err(_) => return self.defer(claim),
        };
        self.finish(claim, remote, ownership, request)
    }
    fn finish(
        &self,
        claim: RemoteEventMutationClaimV3,
        remote: crate::domain::remote_mutation::RemoteReadBack,
        ownership: &MutationTransportOwnership<'_>,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<Outcome, PersistenceError> {
        if !self.transport.current(ownership)
            || !remote.has_bounded_strong_revision()
            || remote.etag == claim.captured.base_etag
            || !single_vevent_uid_v2(&remote.raw_ics).is_ok_and(|uid| uid == claim.captured.uid)
        {
            return self.defer(claim);
        }
        let applied = match crate::domain::remote_event_update::event_readback_matches(
            &claim.captured.expected_raw_ics,
            &remote.raw_ics,
        ) {
            Ok(applied) => applied,
            Err(_) => return self.defer(claim),
        };
        if applied {
            match self.database.apply_remote_event_mutation_for_request(
                claim.clone(),
                remote,
                request,
            ) {
                Ok(evidence) => {
                    if self.transport.current(ownership) {
                        request.accept_event_schedule_change(evidence);
                    }
                }
                Err(_) => return self.defer(claim),
            }
        } else if self
            .database
            .conflict_remote_event_mutation_for_request(claim.clone(), remote, request)
            .is_err()
        {
            return self.defer(claim);
        }
        let outcome = if applied {
            Outcome::Applied
        } else {
            Outcome::Conflict
        };
        Ok(if self.transport.current(ownership) {
            outcome
        } else {
            Outcome::Deferred
        })
    }
}
