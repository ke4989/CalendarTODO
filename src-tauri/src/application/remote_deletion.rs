use crate::{
    adapters::persistence::{Database, PersistenceError, REMOTE_MUTATION_CLAIM_LEASE_MS},
    domain::remote_mutation::{
        RemoteDeletionClaim, RemoteDeletionReadBack, RemoteMutationExecutionPhase,
    },
    ports::{
        clock::{Clock, ClockError},
        remote_mutation::{
            MutationTransportOwnership, RemoteDeletionTransport, RemoteMutationTransportError,
        },
    },
};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDeletionRunOutcome {
    Idle,
    Applied,
    Conflict,
    Deferred,
}

/// Executes at most one deletion claim for one account. It is deliberately not
/// composed by AppState; the existing production session registry is the only
/// credentialed transport owner that can be injected.
#[derive(Clone)]
pub struct RemoteDeletionService {
    database: Database,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn RemoteDeletionTransport>,
}

impl RemoteDeletionService {
    pub fn new(
        database: Database,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn RemoteDeletionTransport>,
    ) -> Self {
        Self {
            database,
            clock,
            transport,
        }
    }

    pub fn run_one_for_account_guarded(
        &self,
        account_id: &str,
        accept: &dyn Fn() -> bool,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        let claimed_at = self.clock.now_utc_ms()?;
        let Some(claim) = self
            .database
            .claim_account_remote_deletion(account_id.to_owned(), claimed_at)?
        else {
            return Ok(RemoteDeletionRunOutcome::Idle);
        };
        self.execute(
            claim,
            claimed_at,
            &MutationTransportOwnership::from_callback(accept),
        )
    }

    pub(crate) fn run_one_for_account_with_request(
        &self,
        account_id: &str,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        let ownership = MutationTransportOwnership::from_request(request);
        let claimed_at = self.clock.now_utc_ms()?;
        let Some(claim) = (if let Some(scope) = request.event_scope() {
            self.database.claim_scoped_delete(scope, request)
        } else {
            self.database
                .claim_account_remote_deletion(account_id.to_owned(), claimed_at)
        })?
        else {
            return Ok(RemoteDeletionRunOutcome::Idle);
        };
        if request
            .event_scope()
            .is_some_and(|scope| scope.is_recovery())
            && claim.phase == RemoteMutationExecutionPhase::Ready
        {
            // Explicit recovery first verifies the original target even when no prior attempt was recorded.
            let evidence = match self
                .transport
                .read_back_deletion_guarded(&claim, &ownership)
            {
                Ok(evidence) => evidence,
                Err(error) => return self.defer_transport(claim, claimed_at, error),
            };
            let unchanged = matches!(&evidence, RemoteDeletionReadBack::Retained(remote) if remote.etag == claim.base_etag && remote.raw_ics == claim.base_raw_ics);
            if !unchanged {
                return self.finish_verified(claim, evidence, claimed_at, &ownership);
            }
        }
        self.execute(claim, claimed_at, &ownership)
    }

    pub fn cancel_in_flight(&self, account_id: &str) -> bool {
        self.transport.cancel_deletion_in_flight(account_id)
    }

    fn execute(
        &self,
        claim: RemoteDeletionClaim,
        claimed_at: i64,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        if !ownership.current() {
            return self.defer(claim, claimed_at, true, "ambiguous");
        }
        if claim.phase == RemoteMutationExecutionPhase::VerifyFirst {
            return match self.transport.read_back_deletion_guarded(&claim, ownership) {
                Ok(evidence) => self.finish_verified(claim, evidence, claimed_at, ownership),
                Err(error) => self.defer_transport(claim, claimed_at, error),
            };
        }

        if self.clock.now_utc_ms()? > claimed_at.saturating_add(REMOTE_MUTATION_CLAIM_LEASE_MS) {
            return Ok(RemoteDeletionRunOutcome::Deferred);
        }
        if !ownership.current()
            || !self
                .database
                .remote_deletion_send_allowed(claim.claim_token.clone(), self.clock.now_utc_ms()?)?
            || !ownership.current()
        {
            return self.defer(claim, claimed_at, true, "ambiguous");
        }

        // Once this call begins, every result may represent delivered DELETE.
        // Verification is therefore unconditional and DELETE is never retried.
        let delete_result = self.transport.conditional_delete_guarded(&claim, ownership);
        let verification = self.transport.read_back_deletion_guarded(&claim, ownership);
        match verification {
            Ok(evidence) => {
                let outcome = self.finish_verified(claim, evidence, claimed_at, ownership)?;
                match delete_result {
                    Ok(()) | Err(RemoteMutationTransportError::PreconditionFailed) => Ok(outcome),
                    Err(_error) if outcome != RemoteDeletionRunOutcome::Deferred => Ok(outcome),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => self.defer_transport(claim, claimed_at, error),
        }
    }

    fn finish_verified(
        &self,
        claim: RemoteDeletionClaim,
        evidence: RemoteDeletionReadBack,
        claimed_at: i64,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        if !ownership.current() {
            return self.defer(claim, claimed_at, true, "ambiguous");
        }
        let outcome = match &evidence {
            RemoteDeletionReadBack::Absent { .. } => RemoteDeletionRunOutcome::Applied,
            RemoteDeletionReadBack::Retained(remote)
                if remote.etag == claim.base_etag && remote.raw_ics == claim.base_raw_ics =>
            {
                RemoteDeletionRunOutcome::Deferred
            }
            RemoteDeletionReadBack::Retained(_) => RemoteDeletionRunOutcome::Conflict,
        };
        if let Some(request) = ownership.request() {
            let change = self.database.finish_remote_deletion_for_request(
                claim.claim_token,
                evidence,
                self.clock.now_utc_ms()?,
                request,
            )?;
            if ownership.current() {
                request.accept_event_schedule_change(change);
            }
        } else {
            self.database.finish_remote_deletion(
                claim.claim_token,
                evidence,
                self.clock.now_utc_ms()?,
            )?;
        }
        Ok(outcome)
    }

    fn defer_transport(
        &self,
        claim: RemoteDeletionClaim,
        claimed_at: i64,
        error: RemoteMutationTransportError,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        let outcome = match error {
            RemoteMutationTransportError::Unauthorized => "unauthorized",
            RemoteMutationTransportError::UnsafeData => "unsafe",
            RemoteMutationTransportError::Transient => "transient",
            RemoteMutationTransportError::Ambiguous
            | RemoteMutationTransportError::Cancelled
            | RemoteMutationTransportError::PreconditionFailed => "ambiguous",
        };
        self.defer(claim, claimed_at, true, outcome)?;
        if matches!(
            error,
            RemoteMutationTransportError::Transient
                | RemoteMutationTransportError::Ambiguous
                | RemoteMutationTransportError::Cancelled
        ) {
            Ok(RemoteDeletionRunOutcome::Deferred)
        } else {
            Err(error.into())
        }
    }

    fn defer(
        &self,
        claim: RemoteDeletionClaim,
        claimed_at: i64,
        ambiguous: bool,
        outcome: &'static str,
    ) -> Result<RemoteDeletionRunOutcome, RemoteDeletionServiceError> {
        self.database.defer_remote_deletion(
            claim.claim_token,
            ambiguous,
            outcome,
            self.clock.now_utc_ms().unwrap_or(claimed_at),
        )?;
        Ok(RemoteDeletionRunOutcome::Deferred)
    }
}

#[derive(Debug, Error)]
pub enum RemoteDeletionServiceError {
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Transport(#[from] RemoteMutationTransportError),
}
