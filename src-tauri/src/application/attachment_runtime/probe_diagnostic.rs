//! Default-off provenance composer and local diagnostic lifecycle. No product entry or CRUD.

use super::{
    ClosedCompositionGate,
    probe_completion::{ProbeCompletionRegistry, ProbeCompletionTicket},
};
use crate::adapters::{
    persistence::{Database, probe_ledger::ProbeLedgerToken, probe_target::ProbeTargetSelection},
    production_mutation::{
        ProductionMutationSessions,
        probe_preflight::{GuardedProbeMetadata, PreparedProbeMetadata, ProbeSourceOwner},
    },
};
use std::sync::{Arc, Mutex};

// No Default or serialized caller choice. This internal opt-in grants no target
// or session authority: preparation still checks all installed captured owners.
pub(super) enum ProbeDiagnosticChoice {
    RunOnce,
}

// A private consuming composer of existing exact-owner evidence. This cannot
// discover/install an account or promote ordinary attachment into Probe origin.
pub(super) struct ProbeBootstrapProvenance {
    run: ProbeDiagnosticRun,
}

impl ProbeBootstrapProvenance {
    pub(super) fn compose(
        choice: ProbeDiagnosticChoice,
        selection: ProbeTargetSelection,
        gate: &ClosedCompositionGate<'_>,
        database: &Database,
        sessions: &Arc<ProductionMutationSessions>,
        registry: &Arc<ProbeDiagnosticRegistry>,
    ) -> Result<Self, ()> {
        let mut run = ProbeDiagnosticRun::prepare_core(
            choice, selection, gate, database, sessions, registry,
        )?;
        run.revalidate_prepared(gate)?;
        Ok(Self { run })
    }

    pub(super) fn prepare(
        mut self,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<ProbeDiagnosticRun, ()> {
        self.run.revalidate_prepared(gate)?;
        // Consumption cannot substitute new database/session/registry owners.
        Ok(self.run)
    }
}

// Only this module can mint a proof, after cancelling/dropping the exact source
// and retiring the exact ticket. It borrows the original token for retry; no
// token or capability is reconstructed from persisted unresolved state.
pub(crate) struct ProbeLocalClosure<'a> {
    token: &'a ProbeLedgerToken,
}
impl ProbeLocalClosure<'_> {
    pub(crate) fn token(&self) -> &ProbeLedgerToken {
        self.token
    }
}

#[derive(Default)]
pub(super) struct ProbeDiagnosticRegistry {
    completions: Mutex<ProbeCompletionRegistry>,
}

pub(super) struct ProbeLocalReconciliationView {
    pub(super) local_registration_retired: bool,
    pub(super) remote_cleanup_available: bool,
    pub(super) execution_available: bool,
}

pub(super) fn reconcile_closed_probe(
    database: &Database,
    evidence: crate::adapters::persistence::probe_ledger::ProbeReconciliationEvidence,
) -> Result<ProbeLocalReconciliationView, ()> {
    database.reconcile_probe_source(evidence).map_err(|_| ())?;
    Ok(ProbeLocalReconciliationView {
        local_registration_retired: true,
        remote_cleanup_available: false,
        execution_available: false,
    })
}

pub(super) struct ProbeDiagnosticRun {
    database: Database,
    sessions: Arc<ProductionMutationSessions>,
    registry: Arc<ProbeDiagnosticRegistry>,
    source: Option<ProbeSourceOwner>,
    prepared: Option<PreparedProbeMetadata>,
    ticket: Option<ProbeCompletionTicket>,
    ledger: Option<ProbeLedgerToken>,
    closing: bool,
    reconciliation_pending: bool,
}

impl ProbeDiagnosticRun {
    pub(super) fn prepare(
        choice: ProbeDiagnosticChoice,
        selection: ProbeTargetSelection,
        gate: &ClosedCompositionGate<'_>,
        database: &Database,
        sessions: &Arc<ProductionMutationSessions>,
        registry: &Arc<ProbeDiagnosticRegistry>,
    ) -> Result<Self, ()> {
        ProbeBootstrapProvenance::compose(choice, selection, gate, database, sessions, registry)?
            .prepare(gate)
    }

    fn prepare_core(
        _choice: ProbeDiagnosticChoice,
        selection: ProbeTargetSelection,
        gate: &ClosedCompositionGate<'_>,
        database: &Database,
        sessions: &Arc<ProductionMutationSessions>,
        registry: &Arc<ProbeDiagnosticRegistry>,
    ) -> Result<Self, ()> {
        // A poisoned local completion owner cannot start new diagnostics.
        drop(registry.completions.lock().map_err(|_| ())?);
        let candidate = database.probe_target_snapshot(selection).map_err(|_| ())?;
        // Commit exclusion before any permit/authentication/exchange exists.
        let (candidate, ledger) = database.begin_probe_source(candidate).map_err(|_| ())?;
        let prepared = sessions.prepare_probe_source_with_ledger(gate, candidate, &ledger);
        let (source, prepared) = match prepared {
            Ok(prepared) => prepared,
            Err(()) => {
                // No source escaped and no request was made. Failure to commit
                // retirement leaves durable exclusion, never a fabricated ack.
                if database.finish_probe_source(&ledger, true).is_err() {
                    let _ = database.finish_probe_source(&ledger, false);
                }
                return Err(());
            }
        };
        Ok(Self {
            database: database.clone(),
            sessions: sessions.clone(),
            registry: registry.clone(),
            source: Some(source),
            prepared: Some(prepared),
            ticket: None,
            ledger: Some(ledger),
            closing: false,
            reconciliation_pending: false,
        })
    }

    pub(super) fn revalidate_prepared(
        &mut self,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<(), ()> {
        if self.closing || self.registry.completions.is_poisoned() {
            return Err(());
        }
        self.database
            .probe_source_scope(self.ledger.as_ref().ok_or(())?)
            .map_err(|_| ())?;
        let prepared = self.prepared.take().ok_or(())?.revalidate(gate)?;
        self.database
            .probe_source_scope(self.ledger.as_ref().ok_or(())?)
            .map_err(|_| ())?;
        self.prepared = Some(prepared.revalidate(gate)?);
        Ok(())
    }

    pub(super) fn execute_metadata(&mut self) -> Result<GuardedProbeMetadata, ()> {
        if self.closing || self.registry.completions.is_poisoned() {
            return Err(());
        }
        let prepared = self.prepared.take().ok_or(())?;
        let permit = self.sessions.mint_probe_metadata_permit(prepared)?;
        let proof = self
            .sessions
            .execute_probe_metadata_permit(permit)
            .map_err(|_| ())?;
        if self.registry.completions.is_poisoned() {
            return Err(());
        }
        Ok(proof)
    }

    pub(super) fn register_completion(
        &mut self,
        proof: GuardedProbeMetadata,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<(), ()> {
        if self.closing
            || self.ticket.is_some()
            || !self
                .source
                .as_ref()
                .is_some_and(|owner| proof.belongs_to_source(owner))
        {
            return Err(());
        }
        let ticket = self
            .registry
            .completions
            .lock()
            .map_err(|_| ())?
            .register_guarded(proof, gate, &self.sessions, &self.database)?;
        self.ticket = Some(ticket);
        Ok(())
    }

    // Cancel first, retire only the exact ticket, then commit local retirement.
    // No session interrupt/revoke, network, owner gate or successor cleanup.
    pub(super) fn shutdown(&mut self) -> Result<(), ()> {
        // This runner transferred its durable closure into a consuming local
        // reconciliation receipt; it cannot independently claim retirement.
        if self.reconciliation_pending {
            return Err(());
        }
        self.close_local()?;
        if let Some(token) = &self.ledger {
            self.database
                .finish_probe_source(token, true)
                .map_err(|_| ())?;
            self.ledger.take();
        }
        self.source.take();
        Ok(())
    }

    pub(super) fn close_for_reconciliation(
        &mut self,
    ) -> Result<crate::adapters::persistence::probe_ledger::ProbeReconciliationEvidence, ()> {
        self.close_local()?;
        self.source.take();
        let token = self.ledger.as_ref().ok_or(())?;
        let evidence = self
            .database
            .record_probe_source_closure(ProbeLocalClosure { token })
            .map_err(|_| ())?;
        self.ledger.take();
        self.reconciliation_pending = true;
        Ok(evidence)
    }

    fn close_local(&mut self) -> Result<(), ()> {
        self.closing = true;
        if let Some(source) = &self.source {
            source.cancel();
        }
        self.prepared.take();
        if let Some(ticket) = &self.ticket {
            if !self
                .registry
                .completions
                .lock()
                .map_err(|_| ())?
                .retire(ticket)
            {
                return Err(());
            }
            self.ticket.take();
        }
        Ok(())
    }
}

impl Drop for ProbeDiagnosticRun {
    fn drop(&mut self) {
        if let Some(source) = &self.source {
            source.cancel();
        }
        self.prepared.take();
        if let Some(ticket) = &self.ticket
            && let Ok(mut registry) = self.registry.completions.lock()
        {
            registry.retire(ticket);
        }
        // Drop never claims successful durable shutdown. Preserve unresolved
        // evidence even when best-effort local ticket retirement succeeded.
        if let Some(token) = &self.ledger {
            let _ = self.database.finish_probe_source(token, false);
        }
    }
}

impl ProbeDiagnosticRun {
    pub(super) fn bind_controller(
        &mut self,
        scope: super::ProbeControllerScope,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<(), ()> {
        self.revalidate_prepared(gate)?;
        self.prepared = Some(
            self.prepared
                .take()
                .ok_or(())?
                .bind_controller(scope, gate)?,
        );
        self.revalidate_prepared(gate)
    }
}
