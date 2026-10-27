//! Consuming completion registry boundary for dedicated guarded Probe sources.

use super::{ClosedCompositionGate, ConflictAttachmentOwner};
use crate::{
    adapters::{
        persistence::{Database, probe_target::CommittedProbeTarget},
        production_mutation::ProductionMutationSessions,
    },
    application::production_creation_owner::CreationSessionEvidence,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

// A dedicated producer must capture these owners at successful completion,
// never retrospectively bind historical success to a replacement session.
pub(super) struct CompletedProbeSource {
    candidate: CommittedProbeTarget,
    runtime: ConflictAttachmentOwner,
    session: CreationSessionEvidence,
    sessions: Weak<ProductionMutationSessions>,
    source: Weak<()>,
    cancelled: Arc<AtomicBool>,
    ledger: Option<crate::adapters::persistence::probe_ledger::ProbeLedgerScope>,
    controller: Option<super::ProbeControllerScope>,
}

impl CompletedProbeSource {
    // Production mint accepts only the adapter-private guarded exchange bridge.
    // It cannot accept a prepared shape, parsed raw bytes or caller provenance.
    pub(super) fn from_guarded(
        proof: crate::adapters::production_mutation::probe_preflight::GuardedProbeMetadata,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
    ) -> Result<Self, ()> {
        let seal = proof.completion_seal(gate)?;
        let (candidate, runtime, session, captured_sessions, source, cancelled, ledger, controller) =
            seal.into_parts();
        Self {
            candidate,
            runtime,
            session,
            sessions: captured_sessions,
            source,
            cancelled,
            ledger,
            controller,
        }
        .revalidate(gate, sessions, database)
    }
    fn revalidate(
        mut self,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
    ) -> Result<Self, ()> {
        if !self.matches(gate, sessions, database) {
            return Err(());
        }
        self.candidate = database
            .revalidate_probe_target(self.candidate)
            .map_err(|_| ())?;
        if !self.matches(gate, sessions, database) {
            return Err(());
        }
        Ok(self)
    }

    fn matches(
        &self,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
    ) -> bool {
        let runtime = gate.attachments();
        let (registry, actor) = runtime.inner.registry.creation_owner_provenance();
        let (account, _, route, _) = self.candidate.checked_parts();
        self.source.upgrade().is_some()
            && self.controller.as_ref().is_none_or(|scope| scope.current())
            && self.ledger.as_ref().is_none_or(|scope| {
                scope.matches_target(&self.candidate)
                    && database.probe_source_active(scope).unwrap_or(false)
            })
            && !self.cancelled.load(Ordering::SeqCst)
            && Weak::ptr_eq(&self.sessions, &Arc::downgrade(sessions))
            && self.sessions.upgrade().is_some()
            && self.candidate.matches_actor(&database.actor_provenance())
            && self.candidate.matches_actor(&actor)
            && runtime.owns_activation_sessions(&runtime.inner.registry, sessions)
            && self.runtime.matches_gate(gate)
            && self.runtime.current(&registry, &actor)
            && sessions.probe_completion_session_matches(account, route, &self.session)
            && !self.cancelled.load(Ordering::SeqCst)
    }
}

struct Entry {
    completion: CompletedProbeSource,
    registration: Arc<()>,
}

pub(super) struct ProbeCompletionTicket {
    calendar: String,
    registration: Weak<()>,
}

pub(super) struct ProbeCompletionView {
    pub(super) completion_current: bool,
    pub(super) production_binding_available: bool,
    pub(super) network_authorized: bool,
}

#[derive(Default)]
pub(super) struct ProbeCompletionRegistry {
    entries: HashMap<String, Entry>,
}

impl ProbeCompletionRegistry {
    pub(super) fn register_guarded(
        &mut self,
        proof: crate::adapters::production_mutation::probe_preflight::GuardedProbeMetadata,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
    ) -> Result<ProbeCompletionTicket, ()> {
        let completion = CompletedProbeSource::from_guarded(proof, gate, sessions, database)?;
        self.register(gate, sessions, database, Some(completion))
    }
    pub(super) fn register(
        &mut self,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
        completion: Option<CompletedProbeSource>,
    ) -> Result<ProbeCompletionTicket, ()> {
        let completion = completion.ok_or(())?.revalidate(gate, sessions, database)?;
        let calendar = completion.candidate.checked_parts().1.to_owned();
        // Only two fixed selections exist; retain an explicit independent cap.
        // A duplicate never silently overwrites a current or unresolved entry.
        if self.entries.len() >= 2 || self.entries.contains_key(&calendar) {
            return Err(());
        }
        let registration = Arc::new(());
        let ticket = ProbeCompletionTicket {
            calendar: calendar.clone(),
            registration: Arc::downgrade(&registration),
        };
        self.entries.insert(
            calendar,
            Entry {
                completion,
                registration,
            },
        );
        Ok(ticket)
    }

    pub(super) fn retire(&mut self, ticket: &ProbeCompletionTicket) -> bool {
        if !self.matches(ticket) {
            return false;
        }
        self.entries.remove(&ticket.calendar);
        true
    }

    fn matches(&self, ticket: &ProbeCompletionTicket) -> bool {
        self.entries.get(&ticket.calendar).is_some_and(|entry| {
            Weak::ptr_eq(&ticket.registration, &Arc::downgrade(&entry.registration))
        })
    }

    pub(super) fn consume(
        &mut self,
        ticket: ProbeCompletionTicket,
        gate: &ClosedCompositionGate<'_>,
        sessions: &Arc<ProductionMutationSessions>,
        database: &Database,
    ) -> Result<ProbeCompletionView, ()> {
        if !self.matches(&ticket) {
            return Err(());
        }
        let entry = self.entries.remove(&ticket.calendar).ok_or(())?;
        let _validated = entry.completion.revalidate(gate, sessions, database)?;
        Ok(ProbeCompletionView {
            completion_current: true,
            production_binding_available: false,
            network_authorized: false,
        })
    }
}

// Synthetic contract fixture, deliberately separate from guarded production mint.
