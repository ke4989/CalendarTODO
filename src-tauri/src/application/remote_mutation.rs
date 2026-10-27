use crate::{
    adapters::persistence::{Database, PersistenceError, REMOTE_MUTATION_CLAIM_LEASE_MS},
    domain::remote_mutation::{
        RemoteMutationClaim, RemoteMutationExecutionPhase, RemoteReadBack,
        patch_single_vevent_title, single_vevent_title, single_vevent_uid,
    },
    ports::{
        clock::{Clock, ClockError},
        remote_mutation::{
            MutationTransportOwnership, RemoteMutationTransport, RemoteMutationTransportError,
        },
    },
};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMutationRunOutcome {
    Idle,
    Applied,
    Conflict,
    Deferred,
}

/// Executes at most one durable operation. It is intentionally not registered
/// in the Tauri runtime. Fakes and the explicitly authorized standalone
/// diagnostic exercise it; general production composition still requires the
/// complete writer, lifecycle and bounded conflict surface.
#[derive(Clone)]
pub struct RemoteMutationService {
    database: Database,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn RemoteMutationTransport>,
}

impl RemoteMutationService {
    pub fn new(
        database: Database,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn RemoteMutationTransport>,
    ) -> Self {
        Self {
            database,
            clock,
            transport,
        }
    }

    pub fn run_one(&self) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        let now_ms = self.clock.now_utc_ms()?;
        let Some(claim) = self.database.claim_remote_mutation(now_ms)? else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        self.execute(
            claim,
            now_ms,
            &MutationTransportOwnership::from_callback(&|| true),
        )
    }

    pub fn run_one_for_account(
        &self,
        account_id: &str,
    ) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        let now_ms = self.clock.now_utc_ms()?;
        let Some(claim) = self
            .database
            .claim_account_remote_mutation(account_id.to_owned(), now_ms)?
        else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        self.execute(
            claim,
            now_ms,
            &MutationTransportOwnership::from_callback(&|| true),
        )
    }

    pub fn run_one_for_account_guarded(
        &self,
        account_id: &str,
        accept: &dyn Fn() -> bool,
    ) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        let now_ms = self.clock.now_utc_ms()?;
        let Some(claim) = self
            .database
            .claim_account_remote_mutation(account_id.to_owned(), now_ms)?
        else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        self.execute(
            claim,
            now_ms,
            &MutationTransportOwnership::from_callback(accept),
        )
    }

    pub(crate) fn run_one_for_account_with_request(
        &self,
        account_id: &str,
        request: &crate::application::remote_mutation_runtime::MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        let ownership = MutationTransportOwnership::from_request(request);
        let now_ms = self.clock.now_utc_ms()?;
        let Some(claim) = self
            .database
            .claim_account_remote_mutation(account_id.to_owned(), now_ms)?
        else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        self.execute(claim, now_ms, &ownership)
    }

    pub fn cancel_in_flight(&self, account_id: &str) -> bool {
        self.transport.cancel_in_flight(account_id)
    }

    fn execute(
        &self,
        claim: RemoteMutationClaim,
        now_ms: i64,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        if !ownership.current() {
            return self.defer(claim, RemoteMutationTransportError::Cancelled, now_ms, true);
        }
        if claim.phase == RemoteMutationExecutionPhase::VerifyFirst {
            match self.transport.read_back_guarded(&claim, ownership) {
                Ok(remote) => {
                    if !ownership.current() {
                        return self.defer(
                            claim,
                            RemoteMutationTransportError::Cancelled,
                            now_ms,
                            true,
                        );
                    }
                    match self.verify_or_defer(&claim, &remote, now_ms)? {
                        Verification::Applied => {
                            self.database.apply_remote_mutation(
                                claim.claim_token,
                                remote,
                                now_ms,
                            )?;
                            return Ok(RemoteMutationRunOutcome::Applied);
                        }
                        Verification::BaseUnchanged => {}
                        Verification::Conflict => {
                            self.database.conflict_remote_mutation(
                                claim.claim_token,
                                remote,
                                now_ms,
                            )?;
                            return Ok(RemoteMutationRunOutcome::Conflict);
                        }
                    }
                }
                Err(error) => return self.defer(claim, error, now_ms, true),
            }
        }

        let patched = match patch_single_vevent_title(&claim.base_raw_ics, &claim.title) {
            Ok(patched) => patched,
            Err(error) => {
                self.database
                    .defer_remote_mutation(claim.claim_token, false, "unsafe", now_ms)?;
                return Err(error.into());
            }
        };
        if self.clock.now_utc_ms()? > now_ms.saturating_add(REMOTE_MUTATION_CLAIM_LEASE_MS) {
            // The actor will release this expired lease into GET-first on the
            // next claim; no stale worker may send another conditional PUT.
            return Ok(RemoteMutationRunOutcome::Deferred);
        }
        if !ownership.current() {
            return self.defer(claim, RemoteMutationTransportError::Cancelled, now_ms, true);
        }
        if !self
            .database
            .remote_mutation_send_allowed(claim.claim_token.clone(), self.clock.now_utc_ms()?)?
        {
            return self.defer(claim, RemoteMutationTransportError::Cancelled, now_ms, true);
        }
        if !ownership.current() {
            return self.defer(claim, RemoteMutationTransportError::Cancelled, now_ms, true);
        }
        // The actor query and ownership check are snapshots. A credentialed
        // transport must repeat its cancellation/generation/lease checks at
        // its actual send boundary; state can change after these return.
        match self
            .transport
            .conditional_update_guarded(&claim, &patched, ownership)
        {
            Ok(()) => match self.transport.read_back_guarded(&claim, ownership) {
                Ok(remote) => {
                    if !ownership.current() {
                        return self.defer(
                            claim,
                            RemoteMutationTransportError::Cancelled,
                            now_ms,
                            true,
                        );
                    }
                    match self.verify_or_defer(&claim, &remote, now_ms)? {
                        Verification::Applied => {
                            self.database.apply_remote_mutation(
                                claim.claim_token,
                                remote,
                                now_ms,
                            )?;
                            Ok(RemoteMutationRunOutcome::Applied)
                        }
                        Verification::BaseUnchanged => {
                            self.defer(claim, RemoteMutationTransportError::Ambiguous, now_ms, true)
                        }
                        Verification::Conflict => {
                            self.database.conflict_remote_mutation(
                                claim.claim_token,
                                remote,
                                now_ms,
                            )?;
                            Ok(RemoteMutationRunOutcome::Conflict)
                        }
                    }
                }
                Err(error) => self.defer(claim, error, now_ms, true),
            },
            Err(RemoteMutationTransportError::PreconditionFailed) => {
                match self.transport.read_back_guarded(&claim, ownership) {
                    Ok(remote) => {
                        if !ownership.current() {
                            return self.defer(
                                claim,
                                RemoteMutationTransportError::Cancelled,
                                now_ms,
                                true,
                            );
                        }
                        match self.verify_or_defer(&claim, &remote, now_ms)? {
                            Verification::Applied => {
                                self.database.apply_remote_mutation(
                                    claim.claim_token,
                                    remote,
                                    now_ms,
                                )?;
                                Ok(RemoteMutationRunOutcome::Applied)
                            }
                            Verification::BaseUnchanged => self.defer(
                                claim,
                                RemoteMutationTransportError::Ambiguous,
                                now_ms,
                                true,
                            ),
                            Verification::Conflict => {
                                self.database.conflict_remote_mutation(
                                    claim.claim_token,
                                    remote,
                                    now_ms,
                                )?;
                                Ok(RemoteMutationRunOutcome::Conflict)
                            }
                        }
                    }
                    Err(error) => self.defer(claim, error, now_ms, true),
                }
            }
            Err(error) => self.defer(claim, error, now_ms, true),
        }
    }

    fn defer(
        &self,
        claim: RemoteMutationClaim,
        error: RemoteMutationTransportError,
        now_ms: i64,
        ambiguous: bool,
    ) -> Result<RemoteMutationRunOutcome, RemoteMutationServiceError> {
        let outcome = match error {
            RemoteMutationTransportError::Transient => "transient",
            RemoteMutationTransportError::Ambiguous
            | RemoteMutationTransportError::Cancelled
            | RemoteMutationTransportError::PreconditionFailed => "ambiguous",
            RemoteMutationTransportError::Unauthorized => "unauthorized",
            RemoteMutationTransportError::UnsafeData => "unsafe",
        };
        self.database
            .defer_remote_mutation(claim.claim_token, ambiguous, outcome, now_ms)?;
        if matches!(
            error,
            RemoteMutationTransportError::Transient
                | RemoteMutationTransportError::Ambiguous
                | RemoteMutationTransportError::Cancelled
        ) {
            Ok(RemoteMutationRunOutcome::Deferred)
        } else {
            Err(error.into())
        }
    }

    fn verify_or_defer(
        &self,
        claim: &RemoteMutationClaim,
        remote: &RemoteReadBack,
        now_ms: i64,
    ) -> Result<Verification, RemoteMutationServiceError> {
        match verification(claim, remote) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.database.defer_remote_mutation(
                    claim.claim_token.clone(),
                    true,
                    "unsafe",
                    now_ms,
                )?;
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verification {
    Applied,
    BaseUnchanged,
    Conflict,
}

fn verification(
    claim: &RemoteMutationClaim,
    remote: &RemoteReadBack,
) -> Result<Verification, RemoteMutationServiceError> {
    if !remote.has_bounded_strong_revision() {
        return Err(crate::domain::remote_mutation::RemoteMutationError::UnsafeCalendarData.into());
    }
    if remote.etag == claim.base_etag && remote.raw_ics != claim.base_raw_ics {
        return Err(crate::domain::remote_mutation::RemoteMutationError::UnsafeCalendarData.into());
    }
    if single_vevent_uid(&remote.raw_ics)? != single_vevent_uid(&claim.base_raw_ics)? {
        return Err(crate::domain::remote_mutation::RemoteMutationError::UnsafeCalendarData.into());
    }
    if single_vevent_title(&remote.raw_ics)? == claim.title
        && remote.raw_ics == patch_single_vevent_title(&claim.base_raw_ics, &claim.title)?
    {
        Ok(Verification::Applied)
    } else if remote.etag == claim.base_etag && remote.raw_ics == claim.base_raw_ics {
        Ok(Verification::BaseUnchanged)
    } else {
        Ok(Verification::Conflict)
    }
}

#[derive(Debug, Error)]
pub enum RemoteMutationServiceError {
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Domain(#[from] crate::domain::remote_mutation::RemoteMutationError),
    #[error(transparent)]
    Transport(#[from] RemoteMutationTransportError),
}
