use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::remote_creation::{RemoteCreationObservation, RemoteCreationReadBack},
    ports::remote_mutation::{RemoteCreationReader, RemoteMutationTransportError},
};
use std::sync::Arc;
use thiserror::Error;

/// Fake/internal GET-only orchestration; intentionally not composed by AppState
/// or the account mutation worker. It cannot send a creation request.
pub struct RemoteCreationVerificationService {
    database: Database,
    reader: Arc<dyn RemoteCreationReader>,
}

impl RemoteCreationVerificationService {
    pub fn new(database: Database, reader: Arc<dyn RemoteCreationReader>) -> Self {
        Self { database, reader }
    }

    /// Caller owns attachment/request cancellation. The final transport callback
    /// repeats that gate and actor-clock lease/current committed admission.
    pub fn run_one_for_account_guarded(
        &self,
        account_id: &str,
        accept: &dyn Fn() -> bool,
    ) -> Result<Option<RemoteCreationObservation>, RemoteCreationVerificationError> {
        Ok(self
            .run_guarded(account_id, accept, false)?
            .map(|outcome| match outcome {
                RemoteCreationVerificationOutcome::Unresolved(observation) => observation,
                RemoteCreationVerificationOutcome::Applied => unreachable!("observation-only mode"),
            }))
    }

    /// Internal opt-in to actor completion, not creation dispatch authority.
    /// Absence and unsafe evidence retain the observation-only recovery path.
    pub fn complete_one_for_account_guarded(
        &self,
        account_id: &str,
        accept: &dyn Fn() -> bool,
    ) -> Result<Option<RemoteCreationVerificationOutcome>, RemoteCreationVerificationError> {
        self.run_guarded(account_id, accept, true)
    }

    fn run_guarded(
        &self,
        account_id: &str,
        accept: &dyn Fn() -> bool,
        complete: bool,
    ) -> Result<Option<RemoteCreationVerificationOutcome>, RemoteCreationVerificationError> {
        if !accept() {
            return Ok(None);
        }
        let Some(claim) = self
            .database
            .claim_account_remote_creation_v1(account_id.into())?
        else {
            return Ok(None);
        };
        let gate = || {
            accept()
                && self
                    .database
                    .remote_creation_read_allowed(claim.clone())
                    .unwrap_or(false)
        };
        if !gate() {
            self.database.defer_remote_creation(claim, true)?;
            return Ok(None);
        }
        let evidence = self.reader.read_creation(&claim, &gate);
        if !gate() {
            self.database.defer_remote_creation(claim, true)?;
            return Ok(None);
        }
        let evidence = match evidence {
            Ok(evidence) => evidence,
            Err(error) => {
                self.database.defer_remote_creation(claim, true)?;
                return Err(error.into());
            }
        };
        finish_creation_read_back(&self.database, claim, evidence, accept, complete, None)
    }

    pub fn cancel_in_flight(&self, account_id: &str) -> bool {
        self.reader.cancel_creation_in_flight(account_id)
    }
}

pub(crate) fn finish_creation_read_back(
    database: &Database,
    claim: crate::domain::remote_creation::RemoteCreationClaim,
    evidence: RemoteCreationReadBack,
    accept: &dyn Fn() -> bool,
    complete: bool,
    request: Option<&crate::application::remote_mutation_runtime::MutationRequestGate>,
) -> Result<Option<RemoteCreationVerificationOutcome>, RemoteCreationVerificationError> {
    let result = match evidence {
        RemoteCreationReadBack::Retained(remote)
            if complete
                && remote.has_bounded_strong_revision()
                && remote.raw_ics == claim.frozen_content() =>
        {
            // A request-scoped create publishes committed schedule evidence so the
            // new reminder is armed now, instead of on some later refresh.
            match request {
                Some(request) => database
                    .complete_remote_creation_for_request(claim.clone(), remote, request)
                    .map(|schedule| {
                        if accept() {
                            request.accept_event_schedule_change(schedule);
                        }
                        RemoteCreationVerificationOutcome::Applied
                    }),
                None => database
                    .complete_remote_creation(claim.clone(), remote)
                    .map(|()| RemoteCreationVerificationOutcome::Applied),
            }
        }
        evidence => database
            .observe_remote_creation(claim.clone(), evidence)
            .map(RemoteCreationVerificationOutcome::Unresolved),
    };
    match result {
        // The terminal actor transition consumes the lease and rotates the
        // epoch: do not use the pre-terminal lease gate to publish success.
        Ok(outcome) => Ok(accept().then_some(outcome)),
        Err(error) => {
            // A newer lease must never be cleared by cleanup of this token.
            // Preserve the original failure if token-scoped cleanup fails.
            let _ = database.defer_remote_creation(claim, true);
            Err(error.into())
        }
    }
}

/// Bounded internal result. Applied can only originate from actor completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCreationVerificationOutcome {
    Applied,
    Unresolved(RemoteCreationObservation),
}

#[derive(Debug, Error)]
pub enum RemoteCreationVerificationError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Transport(#[from] RemoteMutationTransportError),
}
