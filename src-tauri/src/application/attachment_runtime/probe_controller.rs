//! Private installation, explicit one-shot activation. No AppState/IPC entry.
use super::{
    AttachmentRuntime, ClosedCompositionGate, Inner,
    probe_diagnostic::{ProbeDiagnosticChoice, ProbeDiagnosticRegistry, ProbeDiagnosticRun},
};
use crate::{
    adapters::{
        persistence::{Database, probe_target::ProbeTargetSelection},
        production_mutation::ProductionMutationSessions,
    },
    application::account_registry::AccountSourceRegistry,
    ports::clock::Clock,
};
use serde::Deserialize;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

pub(super) struct ProbeInstallationOwner {
    identity: Arc<()>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for ProbeInstallationOwner {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

// Only this controller mints these weak exact identities. No callback or owner
// substitution is accepted by the adapter. Owner loss closes escaped proof.
pub(crate) struct ProbeControllerScope {
    installation: Weak<()>,
    installation_cancelled: Arc<AtomicBool>,
    generation: Weak<()>,
    generation_cancelled: Arc<AtomicBool>,
}
impl ProbeControllerScope {
    pub(crate) fn current(&self) -> bool {
        self.installation.upgrade().is_some()
            && self.generation.upgrade().is_some()
            && !self.installation_cancelled.load(Ordering::SeqCst)
            && !self.generation_cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnableInput {
    enable: String,
    calendar: String,
    expected_generation: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationInput {
    action: String,
    generation: String,
}
fn parse<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, ()> {
    if raw.is_empty() || raw.len() > 1024 {
        return Err(());
    }
    serde_json::from_str(raw).map_err(|_| ())
}
fn generation_input(raw: &str, action: &str) -> Result<Uuid, ()> {
    let input: GenerationInput = parse(raw)?;
    let generation = Uuid::parse_str(&input.generation).map_err(|_| ())?;
    if input.action != action || generation.is_nil() || generation.to_string() != input.generation {
        return Err(());
    }
    Ok(generation)
}

pub(super) struct ProbeActivationView {
    pub(super) generation: Option<String>,
    pub(super) enabled: bool,
    pub(super) triggered: bool,
    pub(super) execution_available: bool,
}
struct Active {
    id: Uuid,
    identity: Arc<()>,
    cancelled: Arc<AtomicBool>,
    run: ProbeDiagnosticRun,
    triggered: bool,
}
pub(super) struct ProbeDiagnosticController {
    database: Database,
    clock: Arc<dyn Clock>,
    accounts: Arc<AccountSourceRegistry>,
    sessions: Arc<ProductionMutationSessions>,
    runtime: Weak<Inner>,
    installation: Weak<()>,
    cancelled: Arc<AtomicBool>,
    registry: Arc<ProbeDiagnosticRegistry>,
    active: Option<Active>,
    closing: bool,
    stopped: bool,
}
impl ProbeDiagnosticController {
    pub(super) fn install(
        gate: &ClosedCompositionGate<'_>,
        database: &Database,
        clock: Arc<dyn Clock>,
        sessions: &Arc<ProductionMutationSessions>,
    ) -> Result<(ProbeInstallationOwner, Self), ()> {
        let owner = ProbeInstallationOwner {
            identity: Arc::new(()),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let controller = Self {
            database: database.clone(),
            clock,
            accounts: gate.attachments().inner.registry.clone(),
            sessions: sessions.clone(),
            runtime: Arc::downgrade(&gate.attachments().inner),
            installation: Arc::downgrade(&owner.identity),
            cancelled: owner.cancelled.clone(),
            registry: Arc::new(ProbeDiagnosticRegistry::default()),
            active: None,
            closing: false,
            stopped: false,
        };
        controller.check(gate)?;
        // Installation does not select a row, claim a slot, mint authentication,
        // prepare a request, or run anything.
        Ok((owner, controller))
    }
    fn check(&self, gate: &ClosedCompositionGate<'_>) -> Result<(), ()> {
        let runtime = gate.attachments();
        let (_, actor) = self.accounts.creation_owner_provenance();
        if self.stopped
            || self.closing
            || self.installation.upgrade().is_none()
            || self.cancelled.load(Ordering::SeqCst)
            || !Weak::ptr_eq(&self.runtime, &Arc::downgrade(&runtime.inner))
            || runtime.inner.stopped.load(Ordering::SeqCst)
            || runtime.inner.closing.load(Ordering::SeqCst)
            || !Weak::ptr_eq(&actor, &self.database.actor_provenance())
            || !self.database.matches_clock(&self.clock)
            || !self
                .sessions
                .matches_captured_owners(&self.accounts, &actor, &self.clock)
            || !runtime.owns_activation_sessions(&self.accounts, &self.sessions)
        {
            return Err(());
        }
        Ok(())
    }
    fn runtime(&self) -> Result<AttachmentRuntime, ()> {
        Ok(AttachmentRuntime {
            inner: self.runtime.upgrade().ok_or(())?,
        })
    }
    pub(super) fn view(&self) -> ProbeActivationView {
        ProbeActivationView {
            generation: self.active.as_ref().map(|a| a.id.to_string()),
            enabled: !self.stopped
                && !self.closing
                && !self.cancelled.load(Ordering::SeqCst)
                && self.installation.upgrade().is_some()
                && self.active.is_some(),
            triggered: self.active.as_ref().is_some_and(|a| a.triggered),
            execution_available: false,
        }
    }
    pub(super) fn enable(
        &mut self,
        raw: &str,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<ProbeActivationView, ()> {
        let input: EnableInput = parse(raw)?;
        if input.enable != "explicit_probe_diagnostic" {
            return Err(());
        }
        let selection = match input.calendar.as_str() {
            "Calendar TODO Probe" => ProbeTargetSelection::First,
            "Calendar TODO Probe 2" => ProbeTargetSelection::Second,
            _ => return Err(()),
        };
        self.check(gate)?;
        // Both initial enable and replacement compare the caller's explicit
        // observed state before touching any current generation or ledger.
        match &self.active {
            None if input.expected_generation == "disabled" => {}
            Some(active) if input.expected_generation == active.id.to_string() => {}
            _ => return Err(()),
        }
        // Selection is only a restriction: candidate/account/route come from
        // the actor and E3b11b composer, never these strings or caller routes.
        self.retire()?;
        self.check(gate)?;
        let identity = Arc::new(());
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut run = ProbeDiagnosticRun::prepare(
            ProbeDiagnosticChoice::RunOnce,
            selection,
            gate,
            &self.database,
            &self.sessions,
            &self.registry,
        )?;
        run.bind_controller(
            ProbeControllerScope {
                installation: self.installation.clone(),
                installation_cancelled: self.cancelled.clone(),
                generation: Arc::downgrade(&identity),
                generation_cancelled: cancelled.clone(),
            },
            gate,
        )?;
        self.active = Some(Active {
            id: Uuid::new_v4(),
            identity,
            cancelled,
            run,
            triggered: false,
        });
        if self.check(gate).is_err() {
            let _ = self.retire();
            return Err(());
        }
        Ok(self.view())
    }
    fn begin_trigger(&mut self, raw: &str) -> Result<Uuid, ()> {
        let id = generation_input(raw, "run_once")?;
        let runtime = self.runtime()?;
        let requests = runtime.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        self.check(&gate)?;
        let active = self.active.as_mut().ok_or(())?;
        if active.id != id || active.triggered || active.cancelled.load(Ordering::SeqCst) {
            return Err(());
        }
        active.run.revalidate_prepared(&gate)?;
        // A failed attempt remains consumed. There is no retry/automatic loop.
        active.triggered = true;
        Ok(id)
    }
    fn finish_trigger(
        &mut self,
        id: Uuid,
        proof: crate::adapters::production_mutation::probe_preflight::GuardedProbeMetadata,
    ) -> Result<ProbeActivationView, ()> {
        let runtime = self.runtime()?;
        let requests = runtime.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        self.check(&gate)?;
        let active = self.active.as_mut().ok_or(())?;
        if active.id != id || active.cancelled.load(Ordering::SeqCst) {
            return Err(());
        }
        active.run.register_completion(proof, &gate)?;
        self.check(&gate)?;
        Ok(self.view())
    }
    pub(super) fn trigger(&mut self, raw: &str) -> Result<ProbeActivationView, ()> {
        let id = self.begin_trigger(raw)?;
        let proof = self.active.as_mut().ok_or(())?.run.execute_metadata()?;
        self.finish_trigger(id, proof)
    }
    fn retire(&mut self) -> Result<(), ()> {
        self.closing = true;
        if let Some(active) = &mut self.active {
            active.cancelled.store(true, Ordering::SeqCst);
            active.run.shutdown()?;
        }
        self.active.take();
        self.closing = false;
        Ok(())
    }
    pub(super) fn disable(&mut self, raw: &str) -> Result<ProbeActivationView, ()> {
        let id = generation_input(raw, "disable")?;
        if self.active.as_ref().is_none_or(|active| active.id != id) {
            return Err(());
        }
        // Cleanup accepts only original generation, including closing retry.
        self.retire()?;
        Ok(self.view())
    }
    pub(super) fn shutdown(&mut self) -> Result<(), ()> {
        self.stopped = true;
        self.retire()
    }
}
impl Drop for ProbeDiagnosticController {
    fn drop(&mut self) {
        self.stopped = true;
        if let Some(active) = &self.active {
            active.cancelled.store(true, Ordering::SeqCst);
        }
        // Run Drop preserves unresolved; no successful shutdown is fabricated.
    }
}
