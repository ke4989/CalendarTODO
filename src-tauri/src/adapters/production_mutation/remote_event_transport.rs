//! Concrete pinned event-v3 transport; only sealed internal collection ownership.
use super::*;
use crate::{
    application::{
        attachment_runtime::ClosedCompositionGate,
        production_creation_owner::CreationReservationComposer,
        production_mutation_conflict::ReviewedMutationConflictSurface,
    },
    domain::remote_event_update::{RemoteEventMutationClaimV3, RemoteEventUpdateSnapshotV3},
};
pub(crate) struct CompleteEventTransportAuthorization(());
#[allow(dead_code)]
pub(crate) struct ProductionEventTransport {
    sessions: std::sync::Weak<ProductionMutationSessions>,
    session: Arc<ProductionTitleMutationTransport>,
    owners: CreationReservationComposer,
    conflicts: ReviewedMutationConflictSurface,
}
#[allow(dead_code)]
impl ProductionMutationSessions {
    pub(crate) fn event_service_under_gate(
        self: &Arc<Self>,
        gate: &ClosedCompositionGate<'_>,
        creation_surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflicts: ReviewedMutationConflictSurface,
        account: &str,
    ) -> Result<Arc<crate::application::remote_event::RemoteEventService>, ()> {
        if !conflicts.matches_gate(gate) {
            return Err(());
        }
        let owners = CreationReservationComposer::from_owners(
            gate.attachments(),
            creation_surface,
            account,
        )?;
        let session = self.session(account).map_err(|_| ())?;
        let registration = self.creation_session_evidence(account).map_err(|_| ())?;
        if !owners.accepts_session(&registration)
            || !owners.current_under_gate(gate)
            || !std::ptr::eq(owners.registry(), self.accounts.as_ref())
        {
            return Err(());
        }
        let transport = Arc::new(ProductionEventTransport {
            sessions: Arc::downgrade(self),
            session,
            owners,
            conflicts,
        });
        if !transport.owners.current_under_gate(gate)
            || !transport.conflicts.matches_gate(gate)
            || !transport.session_current()
        {
            return Err(());
        }
        Ok(Arc::new(
            crate::application::remote_event::RemoteEventService::new(
                self.database.clone(),
                transport,
                &CompleteEventTransportAuthorization(()),
            ),
        ))
    }
}
#[allow(dead_code)]
impl ProductionEventTransport {
    pub(crate) fn belongs_to(&self, accounts: &AccountSourceRegistry) -> bool {
        std::ptr::eq(self.owners.registry(), accounts)
            && self.owners.current()
            && self.session_current()
            && self
                .conflicts
                .verify_for(accounts, self.account_id(), self.owners.generation())
    }
    pub(crate) fn matches_gate(&self, gate: &ClosedCompositionGate<'_>) -> bool {
        // Captured-owner retirement is gate-only: normal conflict verification
        // deliberately rejects closing and must not prevent shutdown retry.
        self.owners.matches_gate(gate)
    }
    pub(crate) fn current_under_gate(&self, gate: &ClosedCompositionGate<'_>) -> bool {
        self.matches_gate(gate) && self.owners.current_under_gate(gate) && self.session_current()
    }
    pub(crate) fn shares_session(&self, fields: &ProductionFieldsTransport) -> bool {
        Arc::ptr_eq(&self.session, &fields.session)
            && std::sync::Weak::ptr_eq(&self.sessions, &fields.owners)
    }
    pub(crate) fn cancel_in_flight(&self, account: &str) -> bool {
        account == self.account_id() && self.session.cancel_in_flight(account)
    }
    pub(crate) fn account_id(&self) -> &str {
        &self.session.account_id
    }
    fn session_current(&self) -> bool {
        !self.session.cancelled.load(Ordering::SeqCst)
            && self.sessions.upgrade().is_some_and(|owners| {
                owners
                    .session(&self.session.account_id)
                    .is_ok_and(|current| Arc::ptr_eq(&current, &self.session))
            })
    }
    pub(crate) fn current(&self, ownership: &MutationTransportOwnership<'_>) -> bool {
        self.session_current()
            && self.owners.current()
            && self.conflicts.verify_for(
                &self.session.accounts,
                &self.session.account_id,
                self.session.attachment_generation,
            )
            && ownership.current_for(
                &self.session.accounts,
                &self.session.account_id,
                self.session.attachment_generation,
            )
    }
    pub(crate) fn put(
        &self,
        claim: &RemoteEventMutationClaimV3,
        snapshot: RemoteEventUpdateSnapshotV3,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<(), RemoteMutationTransportError> {
        let s = &self.session;
        let live = || {
            self.current(ownership)
                && claim.account_id() == s.account_id
                && (if let Some(scope) = ownership.event_scope() {
                    s.database.scoped_event_send_snapshot(scope, claim)
                } else {
                    s.database.remote_event_send_snapshot(claim)
                })
                .is_ok()
                && self.current(ownership)
        };
        if !live() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let current = (if let Some(scope) = ownership.event_scope() {
            s.database.scoped_event_send_snapshot(scope, claim)
        } else {
            s.database.remote_event_send_snapshot(claim)
        })
        .map_err(|_| RemoteMutationTransportError::Cancelled)?;
        let c = &current.captured;
        let sent = &snapshot.captured;
        if c.operation_id != sent.operation_id
            || c.account_id != sent.account_id
            || c.calendar_id != sent.calendar_id
            || c.resource_id != sent.resource_id
            || c.collection_href != sent.collection_href
            || c.exact_href != sent.exact_href
            || c.base_etag != sent.base_etag
            || c.expected_raw_ics != sent.expected_raw_ics
            || current.before_time() != snapshot.before_time()
            || current.after_time() != snapshot.after_time()
            || current.time_alarm_evidence() != snapshot.time_alarm_evidence()
        {
            return Err(RemoteMutationTransportError::UnsafeData);
        }
        let request =
            crate::adapters::caldav_transport::ConditionalEventUpdateRequest::from_snapshot(
                &s.policy,
                &s.entry_url,
                claim,
                snapshot,
                &CompleteEventTransportAuthorization(()),
            )
            .map_err(|_| RemoteMutationTransportError::UnsafeData)?;
        let auth = s.authentication(claim.account_id())?;
        classify_conditional_update_result(s.executor.execute_event_guarded(
            request,
            &auth,
            &CompleteEventTransportAuthorization(()),
            &live,
        ))
    }
    pub(crate) fn get(
        &self,
        claim: &RemoteEventMutationClaimV3,
        ownership: &MutationTransportOwnership<'_>,
    ) -> Result<RemoteReadBack, RemoteMutationTransportError> {
        let s = &self.session;
        let live = || {
            self.current(ownership)
                && claim.account_id() == s.account_id
                && (if let Some(scope) = ownership.event_scope() {
                    s.database.scoped_event_read_allowed(scope, claim)
                } else {
                    s.database.remote_event_read_allowed(claim)
                })
                .unwrap_or(false)
                && self.current(ownership)
        };
        if !live() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        let auth = s.authentication(claim.account_id())?;
        let result = crate::adapters::caldav_transport::read_event_exact_guarded(
            &s.policy,
            &s.entry_url,
            claim,
            &s.executor,
            &auth,
            &CompleteEventTransportAuthorization(()),
            &live,
        )?;
        if !live() {
            return Err(RemoteMutationTransportError::Cancelled);
        }
        Ok(result)
    }
}
