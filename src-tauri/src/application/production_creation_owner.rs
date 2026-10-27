//! Inert CREATE owner metadata and reservation eligibility. No dispatch path.
#![allow(dead_code)]
use crate::application::{
    account_registry::AccountSourceRegistry,
    attachment_runtime::AttachmentRuntime,
    remote_creation_facade::{CreationAdmissionStatusEvidence, RemoteCreationApplication},
};
use std::{
    fmt,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CreationReservationSnapshot {
    account: String,
    operation: String,
    calendar: String,
    resource: String,
    epoch: Uuid,
}
impl CreationReservationSnapshot {
    pub(crate) fn actor_issued(
        account: String,
        operation: String,
        calendar: String,
        resource: String,
        epoch: Uuid,
    ) -> Self {
        Self {
            account,
            operation,
            calendar,
            resource,
            epoch,
        }
    }
}
impl fmt::Debug for CreationReservationSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationReservationSnapshot(<redacted>)")
    }
}

/// Distinct from title registration; attests an active CREATE session foundation.
#[derive(Clone)]
pub(crate) struct CreationSessionEvidence {
    account: String,
    generation: u64,
    registry: Weak<()>,
    actor: Weak<()>,
    revoked: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    owner: Weak<()>,
}
impl CreationSessionEvidence {
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        self.account == other.account
            && self.generation == other.generation
            && Weak::ptr_eq(&self.registry, &other.registry)
            && Weak::ptr_eq(&self.actor, &other.actor)
            && Weak::ptr_eq(&self.owner, &other.owner)
            && Arc::ptr_eq(&self.revoked, &other.revoked)
            && Arc::ptr_eq(&self.closed, &other.closed)
    }
    pub(crate) fn from_session(
        account: String,
        generation: u64,
        registry: Weak<()>,
        actor: Weak<()>,
        marker: &crate::adapters::production_mutation::CompleteCreationSessionRegistration,
    ) -> Self {
        Self {
            account,
            generation,
            registry,
            actor,
            revoked: marker.revocation_state(),
            closed: marker.closure_state(),
            owner: marker.owner_liveness(),
        }
    }
    pub(crate) fn matches(&self, accounts: &AccountSourceRegistry, account: &str) -> bool {
        let (registry, actor) = accounts.creation_owner_provenance();
        !self.revoked.load(Ordering::SeqCst)
            && !self.closed.load(Ordering::SeqCst)
            && self.owner.upgrade().is_some()
            && self.account == account
            && self.generation > 0
            && self.registry.upgrade().is_some()
            && self.actor.upgrade().is_some()
            && Weak::ptr_eq(&self.registry, &registry)
            && Weak::ptr_eq(&self.actor, &actor)
            && accounts.is_current_attachment(account, self.generation)
    }
}
impl fmt::Debug for CreationSessionEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationSessionEvidence(<redacted>)")
    }
}

#[derive(Clone)]
pub(crate) struct CreationLifecycleEvidence {
    stopped: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    session: CreationSessionEvidence,
}
impl CreationLifecycleEvidence {
    pub(crate) fn from_runtime(
        _: &crate::application::attachment_runtime::CompleteAttachmentMutationLifecycle,
        stopped: Arc<AtomicBool>,
        closing: Arc<AtomicBool>,
        session: CreationSessionEvidence,
    ) -> Self {
        Self {
            stopped,
            closing,
            session,
        }
    }
    fn matches(&self, session: &CreationSessionEvidence, accounts: &AccountSourceRegistry) -> bool {
        !self.stopped.load(Ordering::SeqCst)
            && !self.closing.load(Ordering::SeqCst)
            && Arc::ptr_eq(&self.session.revoked, &session.revoked)
            && self.session.matches(accounts, &session.account)
    }
}
impl fmt::Debug for CreationLifecycleEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationLifecycleEvidence(<redacted>)")
    }
}

pub(crate) struct CreationReservationComposer {
    runtime: AttachmentRuntime,
    accounts: Arc<AccountSourceRegistry>,
    session: CreationSessionEvidence,
    lifecycle: CreationLifecycleEvidence,
    surface: CreationAdmissionStatusEvidence,
    stopped: Arc<AtomicBool>,
}
impl CreationReservationComposer {
    pub(crate) fn matches_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        gate.matches_runtime(&self.runtime)
    }
    pub(crate) fn account(&self) -> &str {
        &self.session.account
    }
    pub(crate) fn generation(&self) -> u64 {
        self.session.generation
    }
    pub(crate) fn registry(&self) -> &AccountSourceRegistry {
        &self.accounts
    }
    pub(crate) fn accepts_session(&self, fresh: &CreationSessionEvidence) -> bool {
        self.current()
            && Arc::ptr_eq(&fresh.revoked, &self.session.revoked)
            && fresh.matches(&self.accounts, &self.session.account)
    }
    pub(crate) fn from_owners(
        runtime: &AttachmentRuntime,
        surface: &RemoteCreationApplication,
        account: &str,
    ) -> Result<Self, ()> {
        let (accounts, session, lifecycle) = runtime.creation_owner_evidence(account)?;
        let evidence = surface.admission_status_evidence();
        if !evidence.matches_actor(&accounts.creation_owner_provenance().1) {
            return Err(());
        }
        Ok(Self {
            runtime: runtime.clone(),
            accounts,
            session,
            lifecycle,
            surface: evidence,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }
    pub(crate) fn eligibility(&self, operation: &str) -> Result<CreationReservationPermit, ()> {
        if !self.current() {
            return Err(());
        }
        let snapshot = self
            .accounts
            .creation_reservation_snapshot(&self.session.account, operation)
            .map_err(|_| ())?;
        if !self.current() {
            return Err(());
        }
        Ok(CreationReservationPermit {
            snapshot,
            session: self.session.clone(),
            lifecycle: self.lifecycle.clone(),
            surface: self.surface.clone(),
            stopped: self.stopped.clone(),
        })
    }
    pub(crate) fn current(&self) -> bool {
        let fresh = self.runtime.creation_owner_evidence(&self.session.account);
        self.matches_fresh(fresh)
    }
    pub(crate) fn current_under_gate(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> bool {
        if !self.matches_gate(gate) {
            return false;
        }
        let fresh = self
            .runtime
            .creation_owner_evidence_under_gate(&self.session.account, gate);
        fresh.is_ok_and(|(accounts, session, lifecycle)| {
            Arc::ptr_eq(&accounts, &self.accounts)
                && Arc::ptr_eq(&session.revoked, &self.session.revoked)
                && Arc::ptr_eq(&session.closed, &self.session.closed)
                && Weak::ptr_eq(&session.owner, &self.session.owner)
                && session.account == self.session.account
                && session.generation == self.session.generation
                && Arc::ptr_eq(&lifecycle.stopped, &self.lifecycle.stopped)
                && Arc::ptr_eq(&lifecycle.closing, &self.lifecycle.closing)
                && !lifecycle.stopped.load(Ordering::SeqCst)
        }) && !self.stopped.load(Ordering::SeqCst)
            && self.session.matches(&self.accounts, &self.session.account)
            && self
                .surface
                .matches_actor(&self.accounts.creation_owner_provenance().1)
            && eligible_profile(&self.accounts, &self.session.account)
    }
    fn matches_fresh(
        &self,
        fresh: Result<
            (
                Arc<AccountSourceRegistry>,
                CreationSessionEvidence,
                CreationLifecycleEvidence,
            ),
            (),
        >,
    ) -> bool {
        fresh.is_ok_and(|(accounts, session, lifecycle)| {
            Arc::ptr_eq(&accounts, &self.accounts)
                && Arc::ptr_eq(&session.revoked, &self.session.revoked)
                && Arc::ptr_eq(&lifecycle.stopped, &self.lifecycle.stopped)
                && Arc::ptr_eq(&lifecycle.closing, &self.lifecycle.closing)
        }) && !self.stopped.load(Ordering::SeqCst)
            && self.session.matches(&self.accounts, &self.session.account)
            && self.lifecycle.matches(&self.session, &self.accounts)
            && self
                .surface
                .matches_actor(&self.accounts.creation_owner_provenance().1)
            && eligible_profile(&self.accounts, &self.session.account)
    }
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
impl Drop for CreationReservationComposer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl fmt::Debug for CreationReservationComposer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationReservationComposer(<redacted>)")
    }
}

/// Snapshot only; contains no sender, actor handle, route, bytes or claim.
pub(crate) struct CreationReservationPermit {
    snapshot: CreationReservationSnapshot,
    session: CreationSessionEvidence,
    lifecycle: CreationLifecycleEvidence,
    surface: CreationAdmissionStatusEvidence,
    stopped: Arc<AtomicBool>,
}
impl CreationReservationPermit {
    pub(crate) fn accepts_claim(
        &self,
        claim: &crate::domain::remote_creation::RemoteCreationClaim,
        accounts: &AccountSourceRegistry,
    ) -> bool {
        claim.phase == crate::domain::remote_mutation::RemoteMutationExecutionPhase::Ready
            && claim.account_id == self.snapshot.account
            && claim.operation_id == self.snapshot.operation
            && claim.calendar_id == self.snapshot.calendar
            && claim.sync_epoch == Some(self.snapshot.epoch)
            && claim
                .exact_href
                .ends_with(&format!("/{}.ics", self.snapshot.resource))
            && !self.stopped.load(Ordering::SeqCst)
            && self.session.matches(accounts, &claim.account_id)
            && self.lifecycle.matches(&self.session, accounts)
            && self
                .surface
                .matches_actor(&accounts.creation_owner_provenance().1)
            && eligible_profile(accounts, &claim.account_id)
    }
    pub(crate) fn matches_current(
        &self,
        accounts: &AccountSourceRegistry,
        account: &str,
        calendar: &str,
        resource: &str,
        operation: &str,
    ) -> bool {
        let gate = || {
            !self.stopped.load(Ordering::SeqCst)
                && self.snapshot.account == account
                && self.snapshot.calendar == calendar
                && self.snapshot.resource == resource
                && self.snapshot.operation == operation
                && self.session.matches(accounts, account)
                && self.lifecycle.matches(&self.session, accounts)
                && self
                    .surface
                    .matches_actor(&accounts.creation_owner_provenance().1)
                && eligible_profile(accounts, account)
        };
        gate()
            && accounts
                .creation_reservation_snapshot(account, operation)
                .is_ok_and(|current| current == self.snapshot)
            && gate()
    }
}
impl fmt::Debug for CreationReservationPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreationReservationPermit(<redacted>)")
    }
}
fn eligible_profile(accounts: &AccountSourceRegistry, account: &str) -> bool {
    accounts.profile(account).is_ok_and(|p| {
        p.enabled
            && p.credential_configured
            && p.remote_editing_enabled
            && p.region_profile == "icloud-cn"
            && p.provider_kind == crate::domain::account::ProviderKind::CalDav
    })
}
