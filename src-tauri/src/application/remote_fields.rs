//! Typed text-field service, registered only inside the execution-closed internal worker graph.
use crate::{
    adapters::persistence::{Database, PersistenceError},
    application::remote_mutation::RemoteMutationRunOutcome as Outcome,
    domain::remote_mutation::{
        RemoteFieldsMutationClaim, RemoteMutationExecutionPhase, single_vevent_uid_v2,
    },
    ports::remote_mutation::{MutationTransportOwnership, RemoteFieldsTransport},
};
use std::sync::Arc;
#[allow(dead_code)]
pub(crate) struct RemoteFieldsService {
    database: Database,
    transport: Arc<dyn RemoteFieldsTransport>,
}
#[allow(dead_code)]
impl RemoteFieldsService {
    pub(crate) fn new(database: Database, transport: Arc<dyn RemoteFieldsTransport>) -> Self {
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
        self.run(account, &MutationTransportOwnership::from_request(request))
    }
    fn defer(&self, claim: RemoteFieldsMutationClaim) -> Result<Outcome, PersistenceError> {
        match self.database.defer_remote_fields_mutation(claim, true) {
            Ok(()) | Err(PersistenceError::OutboxClaimLost) => Ok(Outcome::Deferred),
            Err(error) => Err(error),
        }
    }
    fn run(
        &self,
        account: &str,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<Outcome, PersistenceError> {
        if account != self.transport.account_id() || !self.transport.current(ownership) {
            return Ok(Outcome::Deferred);
        }
        let Some(mut claim) = self
            .database
            .claim_account_remote_fields_mutation(account.into())?
        else {
            return Ok(Outcome::Idle);
        };
        if !self.transport.current(ownership) {
            return self.defer(claim);
        }
        if claim.phase == RemoteMutationExecutionPhase::VerifyFirst {
            let remote = match self.transport.get(&claim, ownership) {
                Ok(r) => r,
                Err(_) => return self.defer(claim),
            };
            if !self.transport.current(ownership) {
                return self.defer(claim);
            }
            if remote.has_bounded_strong_revision()
                && remote.etag == claim.base_etag
                && remote.raw_ics == claim.base_raw_ics
            {
                claim = match self.database.restore_remote_fields_base(&claim, remote) {
                    Ok(restored) => restored,
                    Err(_) => return self.defer(claim),
                };
            } else {
                return self.finish(claim, remote, ownership);
            }
        }
        let snapshot = match self.database.remote_fields_send_snapshot(&claim) {
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
        self.finish(claim, remote, ownership)
    }
    fn finish(
        &self,
        claim: RemoteFieldsMutationClaim,
        remote: crate::domain::remote_mutation::RemoteReadBack,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<Outcome, PersistenceError> {
        if !self.transport.current(ownership)
            || !remote.has_bounded_strong_revision()
            || remote.etag == claim.base_etag
            || !single_vevent_uid_v2(&remote.raw_ics).is_ok_and(|uid| uid == claim.uid)
        {
            return self.defer(claim);
        }
        let applied = remote.raw_ics == claim.expected_raw_ics;
        let result = if applied {
            self.database
                .apply_remote_fields_mutation(claim.clone(), remote)
        } else {
            self.database
                .conflict_remote_fields_mutation(claim.clone(), remote)
        };
        if result.is_err() {
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
