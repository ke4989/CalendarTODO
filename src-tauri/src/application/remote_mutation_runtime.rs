use crate::application::{
    account_registry::AccountSourceRegistry,
    remote_deletion::{RemoteDeletionRunOutcome, RemoteDeletionService},
    remote_mutation::{RemoteMutationRunOutcome, RemoteMutationService},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use thiserror::Error;
use uuid::Uuid;

pub const MUTATION_STATE_EVENT: &str = "calendar-todo://remote-mutation-state";

pub trait MutationStateEmitter: Send + Sync + 'static {
    fn emit(&self, view: MutationRuntimeView);
    fn emit_scoped(&self, _: ScopedEventRuntimeView) {}
}

#[derive(Default)]
struct NoopMutationStateEmitter;

impl MutationStateEmitter for NoopMutationStateEmitter {
    fn emit(&self, _: MutationRuntimeView) {}
}

pub trait MutationAccountOwnership: Send + Sync + 'static {
    fn attachment_generation(&self, account_id: &str) -> Option<u64>;
    fn is_current_attachment(&self, account_id: &str, generation: u64) -> bool;
    fn actor_provenance(&self) -> Option<Weak<()>> {
        None
    }
    fn registry_provenance(&self) -> Option<Weak<()>> {
        None
    }
}

trait MutationRunner: Send + Sync + 'static {
    fn run_one(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()>;
    fn cancel_in_flight(&self, account_id: &str) -> bool;
}

impl MutationRunner for RemoteMutationService {
    fn run_one(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()> {
        if request.event_scope().is_some() {
            return Ok(RemoteMutationRunOutcome::Idle);
        }
        self.run_one_for_account_with_request(account_id, request)
            .map_err(|_| ())
    }

    fn cancel_in_flight(&self, account_id: &str) -> bool {
        RemoteMutationService::cancel_in_flight(self, account_id)
    }
}

impl MutationRunner for RemoteDeletionService {
    fn run_one(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()> {
        if request
            .event_scope()
            .is_some_and(|s| s.kind() != crate::domain::remote_crud::RemoteCrudKind::Delete)
        {
            return Ok(RemoteMutationRunOutcome::Idle);
        }
        self.run_one_for_account_with_request(account_id, request)
            .map(|outcome| match outcome {
                RemoteDeletionRunOutcome::Idle => RemoteMutationRunOutcome::Idle,
                RemoteDeletionRunOutcome::Applied => RemoteMutationRunOutcome::Applied,
                RemoteDeletionRunOutcome::Conflict => RemoteMutationRunOutcome::Conflict,
                RemoteDeletionRunOutcome::Deferred => RemoteMutationRunOutcome::Deferred,
            })
            .map_err(|_| ())
    }

    fn cancel_in_flight(&self, account_id: &str) -> bool {
        RemoteDeletionService::cancel_in_flight(self, account_id)
    }
}

/// Round-robin inspection is independent per account. Idle stores yield;
/// deferred or failed work stops the account without inspecting another store.
struct CombinedMutationRunner {
    runners: Vec<Arc<dyn MutationRunner>>,
    next_by_account: Mutex<HashMap<String, usize>>,
}
impl CombinedMutationRunner {
    fn new(title: Arc<dyn MutationRunner>, deletion: Arc<dyn MutationRunner>) -> Self {
        Self::from_runners(vec![title, deletion])
    }
    fn from_runners(runners: Vec<Arc<dyn MutationRunner>>) -> Self {
        Self {
            runners,
            next_by_account: Mutex::new(HashMap::new()),
        }
    }
}
impl MutationRunner for CombinedMutationRunner {
    fn run_one(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()> {
        let first = {
            let mut turns = self.next_by_account.lock().map_err(|_| ())?;
            *turns.entry(account_id.to_owned()).or_insert(0)
        };
        for offset in 0..self.runners.len() {
            let index = (first + offset) % self.runners.len();
            let result = self.runners[index].run_one(account_id, request);
            self.next_by_account
                .lock()
                .map_err(|_| ())?
                .insert(account_id.into(), (index + 1) % self.runners.len());
            match result? {
                RemoteMutationRunOutcome::Idle => {}
                outcome => return Ok(outcome),
            }
        }
        Ok(RemoteMutationRunOutcome::Idle)
    }
    fn cancel_in_flight(&self, account_id: &str) -> bool {
        if let Ok(mut turns) = self.next_by_account.lock() {
            turns.remove(account_id);
        }
        let mut cancelled = false;
        for runner in &self.runners {
            cancelled |= runner.cancel_in_flight(account_id);
        }
        cancelled
    }
}

/// Owners are supplied by the real attachment/session composition, never a
/// facade status view or reconstructed send evidence.
#[allow(dead_code)]
struct CreationMutationRunner {
    services: HashMap<
        String,
        Arc<crate::application::remote_creation_dispatch::RemoteCreationDispatchService>,
    >,
}
impl MutationRunner for CreationMutationRunner {
    fn run_one(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()> {
        if request.event_scope().is_some() {
            return Ok(RemoteMutationRunOutcome::Idle);
        }
        if !request.current() {
            return Ok(RemoteMutationRunOutcome::Deferred);
        }
        let Some(service) = self.services.get(account_id) else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        service.run_next(request).map_err(|_| ())
    }
    fn cancel_in_flight(&self, account_id: &str) -> bool {
        self.services
            .get(account_id)
            .is_some_and(|service| service.cancel_in_flight(account_id))
    }
}

#[derive(Clone, Copy)]
enum VersionedMutationStore {
    Fields,
    CreationV2,
    Event,
}
struct VersionedMutationRunner {
    services: HashMap<String, Arc<crate::adapters::production_mutation::VersionedMutationServices>>,
    store: VersionedMutationStore,
}
impl MutationRunner for VersionedMutationRunner {
    fn run_one(
        &self,
        account: &str,
        request: &MutationRequestGate,
    ) -> Result<RemoteMutationRunOutcome, ()> {
        if let Some(scope) = request.event_scope() {
            let matches = matches!(
                (scope.kind(), self.store),
                (
                    crate::domain::remote_crud::RemoteCrudKind::CreateUtcV2,
                    VersionedMutationStore::CreationV2
                ) | (
                    crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3,
                    VersionedMutationStore::Event
                )
            );
            if !matches {
                return Ok(RemoteMutationRunOutcome::Idle);
            }
        }
        if !request.current() {
            return Ok(RemoteMutationRunOutcome::Deferred);
        }
        let Some(service) = self.services.get(account) else {
            return Ok(RemoteMutationRunOutcome::Idle);
        };
        match self.store {
            VersionedMutationStore::Fields => service
                .fields
                .run_with_request(account, request)
                .map_err(|_| ()),
            VersionedMutationStore::CreationV2 => {
                service.creation_v2.run_next(request).map_err(|_| ())
            }
            VersionedMutationStore::Event => service
                .event
                .run_with_request(account, request)
                .map_err(|_| ()),
        }
    }
    fn cancel_in_flight(&self, account: &str) -> bool {
        self.services
            .get(account)
            .is_some_and(|service| match self.store {
                VersionedMutationStore::Fields => service.cancel_fields(account),
                VersionedMutationStore::CreationV2 => service.creation_v2.cancel_in_flight(account),
                VersionedMutationStore::Event => service.event.cancel_in_flight(account),
            })
    }
}

impl MutationAccountOwnership for AccountSourceRegistry {
    fn actor_provenance(&self) -> Option<Weak<()>> {
        Some(self.creation_owner_provenance().1)
    }
    fn registry_provenance(&self) -> Option<Weak<()>> {
        Some(self.creation_owner_provenance().0)
    }
    fn attachment_generation(&self, account_id: &str) -> Option<u64> {
        AccountSourceRegistry::attachment_generation(self, account_id)
    }

    fn is_current_attachment(&self, account_id: &str, generation: u64) -> bool {
        AccountSourceRegistry::is_current_attachment(self, account_id, generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MutationRuntimeState {
    Running,
    Queued,
    Idle,
    Deferred,
    Conflict,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationRuntimeView {
    pub account_id: String,
    pub state: MutationRuntimeState,
    pub applied_count: u32,
    pub conflict_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedEventRuntimeView {
    pub kind: crate::domain::remote_crud::RemoteCrudKind,
    pub runtime: MutationRuntimeView,
    pub operation: crate::domain::remote_crud::RemoteCrudOperation,
}
type EventScope = Arc<(
    crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
    crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
)>;

#[derive(Clone, Default)]
struct Cancellation(Arc<AtomicBool>, Arc<AtomicBool>, Arc<()>);

impl Cancellation {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct Request {
    request_generation: u64,
    attachment_generation: u64,
    cancellation: Cancellation,
    view: MutationRuntimeView,
    event: Option<EventScope>,
    successor: Option<(EventScope, i64)>,
    followups: VecDeque<(EventScope, u64, Option<i64>)>,
}

#[derive(Default)]
struct PendingEvents {
    views: VecDeque<MutationRuntimeView>,
    draining: bool,
    handoff_paused: bool,
    schedules: VecDeque<MutationRequestGate>,
    scoped: VecDeque<(MutationRequestGate, MutationRuntimeView)>,
}

struct DrainerGuard<'a> {
    events: &'a Mutex<PendingEvents>,
    armed: bool,
}

impl Drop for DrainerGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut events) = self.events.lock()
        {
            events.draining = false;
        }
    }
}

struct EventScheduleBinding {
    account: String,
    generation: u64,
    port: Weak<dyn ClosedEventSchedulePort>,
    production: Option<Weak<super::reminder_deadlines::ProductionGraphReminderScope>>,
}

struct Inner {
    ownership: Arc<dyn MutationAccountOwnership>,
    service: Arc<dyn MutationRunner>,
    versioned_services: Option<
        HashMap<String, Weak<crate::adapters::production_mutation::VersionedMutationServices>>,
    >,
    emitter: Arc<dyn MutationStateEmitter>,
    scoped_emitter: Mutex<Option<Arc<dyn MutationStateEmitter>>>,
    requests: Mutex<HashMap<String, Request>>,
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    events: Mutex<PendingEvents>,
    next_generation: AtomicU64,
    stopped: AtomicBool,
    schedule_port: Mutex<Option<EventScheduleBinding>>,
    schedule_failed: AtomicBool,
}

#[derive(Clone)]
pub struct RemoteMutationRuntime {
    inner: Arc<Inner>,
}

/// Lifecycle-only weak link for one closed graph. It cannot submit work or
/// expose the runtime, runner, transport or request gate.
#[allow(dead_code)]
pub(crate) struct ClosedMutationLifecycle {
    owner: Weak<Inner>,
    account: String,
    generation: u64,
    session_current: Arc<dyn Fn() -> bool + Send + Sync>,
    gate_current: Arc<
        dyn Fn(&crate::application::attachment_runtime::ClosedCompositionGate<'_>) -> bool
            + Send
            + Sync,
    >,
    handoff_session_current: Arc<
        dyn Fn(&crate::application::attachment_runtime::ClosedCompositionGate<'_>) -> bool
            + Send
            + Sync,
    >,
}

#[allow(dead_code)]
impl ClosedMutationLifecycle {
    pub(crate) fn scheduling_alive(&self) -> bool {
        self.owner
            .upgrade()
            .is_some_and(|o| !o.stopped.load(Ordering::SeqCst))
    }
    pub(crate) fn schedule_scope(
        &self,
    ) -> Option<Arc<super::reminder_deadlines::ProductionGraphReminderScope>> {
        self.owner
            .upgrade()?
            .schedule_port
            .lock()
            .ok()?
            .as_ref()?
            .production
            .as_ref()?
            .upgrade()
    }
    pub(crate) fn matches_registration(&self, account: &str, generation: u64) -> bool {
        self.account == account && self.generation == generation
    }
    pub(crate) fn is_live(&self) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            !owner.stopped.load(Ordering::SeqCst)
                && (self.session_current)()
                && owner
                    .ownership
                    .is_current_attachment(&self.account, self.generation)
        })
    }
    /// Call under the attachment handoff gate, before source/session changes.
    /// Events remain queued so synchronous emitters cannot reenter that gate.
    pub(crate) fn retire_before_handoff(
        &self,
        gate: &crate::application::attachment_runtime::ClosedCompositionGate<'_>,
    ) -> Result<(), ()> {
        if !(self.gate_current)(gate) {
            return Err(());
        }
        self.retire_with_session(true, Some((self.handoff_session_current)(gate)))
    }

    /// Ungated shutdown/Drop can revoke only this graph's local request state.
    pub(crate) fn retire_local(&self) -> Result<(), ()> {
        self.retire(false)
    }

    fn retire(&self, interrupt_transport: bool) -> Result<(), ()> {
        self.retire_with_session(interrupt_transport, None)
    }
    fn retire_with_session(
        &self,
        interrupt_transport: bool,
        current_under_gate: Option<bool>,
    ) -> Result<(), ()> {
        let owner = self.owner.upgrade().ok_or(())?;
        owner.stopped.store(true, Ordering::SeqCst);
        let publication_ready = match owner.events.lock() {
            Ok(mut events) => {
                events.handoff_paused = true;
                !events.draining
            }
            Err(_) => false,
        };
        let mut requests = match owner.requests.lock() {
            Ok(requests) => requests,
            Err(_) => {
                // Poison cannot prove matching request metadata. Stop all
                // callbacks and interrupt only while the exact attachment is
                // still current; never target a successor's session.
                if interrupt_transport
                    && current_under_gate.unwrap_or_else(|| (self.session_current)())
                    && owner
                        .ownership
                        .is_current_attachment(&self.account, self.generation)
                {
                    owner.service.cancel_in_flight(&self.account);
                }
                return Err(());
            }
        };
        if let Some(request) = requests.get_mut(&self.account) {
            if request.attachment_generation != self.generation {
                return Err(());
            }
            request.cancellation.cancel();
            request.view.state = MutationRuntimeState::Cancelled;
            owner.enqueue(request.view.clone());
            if interrupt_transport
                && current_under_gate.unwrap_or_else(|| (self.session_current)())
                && owner
                    .ownership
                    .is_current_attachment(&self.account, self.generation)
            {
                owner.service.cancel_in_flight(&self.account);
            }
        }
        if publication_ready { Ok(()) } else { Err(()) }
    }

    /// Publish only after releasing every attachment/lifecycle gate.
    pub(crate) fn publish_pending(&self) {
        if let Some(owner) = self.owner.upgrade() {
            if owner.stopped.load(Ordering::SeqCst) {
                let port = owner
                    .schedule_port
                    .lock()
                    .ok()
                    .and_then(|binding| binding.as_ref().and_then(|b| b.port.upgrade()));
                if let Some(port) = port {
                    port.retire();
                }
            }
            if let Ok(mut events) = owner.events.lock() {
                events.handoff_paused = false;
            }
            owner.drain_events();
        }
    }
}

impl std::fmt::Debug for ClosedMutationLifecycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClosedMutationLifecycle(<redacted>)")
    }
}

/// Current real request metadata only; never a transport or sender capability.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct MutationRequestGate {
    owner: Weak<Inner>,
    registry: Weak<()>,
    account: String,
    attachment_generation: u64,
    request_generation: u64,
    cancellation: Cancellation,
    event: Option<EventScope>,
}
#[allow(dead_code)]
impl MutationRequestGate {
    pub(crate) fn event_scope(
        &self,
    ) -> Option<&crate::adapters::persistence::scoped_event::ScopedEventSnapshot> {
        self.event.as_ref().map(|event| &event.1)
    }
    pub(crate) fn event_actor_current(
        &self,
        claim: &crate::domain::remote_event_update::RemoteEventMutationClaimV3,
    ) -> bool {
        let Some(event) = &self.event else {
            return true;
        };
        event.1.matches_claim(claim) && self.event_selection_current(&event.1)
    }
    pub(crate) fn event_selection_current(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> bool {
        let Some(event) = &self.event else {
            return false;
        };
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        event.1.ids() == scope.ids()
            && event.0.event_actor_owner_alive()
            && !self.cancellation.is_cancelled()
            && !owner.stopped.load(Ordering::SeqCst)
            && owner.requests.lock().is_ok_and(|requests| {
                requests.get(&self.account).is_some_and(|r| {
                    r.request_generation == self.request_generation
                        && r.attachment_generation == self.attachment_generation
                        && Arc::ptr_eq(&r.cancellation.0, &self.cancellation.0)
                        && r.view.state == MutationRuntimeState::Running
                })
            })
    }
    fn scoped_view(&self, view: MutationRuntimeView) -> Option<ScopedEventRuntimeView> {
        let event = self.event.as_ref()?;
        if !self.current_impl(false) || !event.0.event_owner_current() {
            return None;
        }
        let owner = self.owner.upgrade()?;
        if !owner
            .requests
            .lock()
            .ok()?
            .get(&self.account)
            .is_some_and(|r| r.view == view)
        {
            return None;
        }
        let services = owner
            .versioned_services
            .as_ref()?
            .get(&self.account)?
            .upgrade()?;
        let operation = services.event.scoped_status(&event.1).ok()?;
        if !self.current_impl(false) || !event.0.event_owner_current() {
            return None;
        }
        Some(ScopedEventRuntimeView {
            kind: event.1.kind(),
            runtime: view,
            operation,
        })
    }
    pub(crate) fn accept_event_schedule_change(
        &self,
        evidence: crate::adapters::persistence::CommittedEventScheduleChange,
    ) {
        if !evidence.changed_for(&self.account)
            || !evidence.matches_request(&self.event_schedule_provenance())
            || !self.current()
        {
            return;
        }
        if let Some(owner) = self.owner.upgrade()
            && owner
                .ownership
                .actor_provenance()
                .is_some_and(|actor| evidence.matches_actor(&actor))
            && let Ok(mut events) = owner.events.lock()
            && !events.handoff_paused
        {
            events.schedules.retain(|r| r.account != self.account);
            events.schedules.push_back(self.clone());
        }
    }

    pub(crate) fn event_schedule_provenance(&self) -> Weak<()> {
        Arc::downgrade(&self.cancellation.2)
    }

    /// Consumed committed scheduling tickets retain ownership after worker
    /// completion. This never authorizes transport or a new mutation claim.
    fn schedule_current(&self) -> bool {
        self.cancellation.1.load(Ordering::SeqCst) && self.current_impl(false)
    }

    pub(crate) fn current_for(
        &self,
        registry: &AccountSourceRegistry,
        account: &str,
        generation: u64,
    ) -> bool {
        self.account == account
            && self.attachment_generation == generation
            && self.registry.upgrade().is_some()
            && Weak::ptr_eq(&self.registry, &registry.creation_owner_provenance().0)
            && self.current()
            && registry.is_current_attachment(account, generation)
    }
    pub(crate) fn current(&self) -> bool {
        self.current_impl(true)
    }
    fn current_impl(&self, require_running: bool) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        if self.cancellation.is_cancelled() || owner.stopped.load(Ordering::SeqCst) {
            return false;
        }
        if let Some(provenance) = owner.ownership.registry_provenance()
            && (provenance.upgrade().is_none() || !Weak::ptr_eq(&provenance, &self.registry))
        {
            return false;
        }
        let Ok(gates) = owner.gates.lock() else {
            return false;
        };
        if !gates
            .get(&self.account)
            .is_some_and(|gate| !gate.is_poisoned())
        {
            return false;
        }
        drop(gates);
        let Ok(requests) = owner.requests.lock() else {
            return false;
        };
        let current = requests.get(&self.account).is_some_and(|r| {
            r.request_generation == self.request_generation
                && r.attachment_generation == self.attachment_generation
                && Arc::ptr_eq(&r.cancellation.0, &self.cancellation.0)
                && !r.cancellation.is_cancelled()
                && (!require_running || r.view.state == MutationRuntimeState::Running)
                && r.view.state != MutationRuntimeState::Cancelled
        });
        drop(requests);
        current
            && self
                .event
                .as_ref()
                .is_none_or(|event| event.0.event_owner_current())
            && owner.versioned_services.as_ref().is_none_or(|services| {
                services
                    .get(&self.account)
                    .and_then(Weak::upgrade)
                    .is_some_and(|services| services.current())
            })
            && !owner.stopped.load(Ordering::SeqCst)
            && !self.cancellation.is_cancelled()
            && owner
                .ownership
                .is_current_attachment(&self.account, self.attachment_generation)
    }
}
impl std::fmt::Debug for MutationRequestGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutationRequestGate(<redacted>)")
    }
}

/// C3c1 internal scheduler boundary. The owner must recheck the supplied guard
/// immediately before and after its scheduling action. Production scope only
/// accepts the drainer-private actor-committed ticket and merges the global key.
pub(crate) struct AcceptedEventScheduleTicket<'a> {
    request: &'a MutationRequestGate,
}
impl AcceptedEventScheduleTicket<'_> {
    pub(crate) fn account(&self) -> &str {
        &self.request.account
    }
    pub(crate) fn generation(&self) -> u64 {
        self.request.attachment_generation
    }
    pub(crate) fn current(&self) -> bool {
        self.request.schedule_current()
    }
    pub(crate) fn current_for(&self, lifecycle: &ClosedMutationLifecycle) -> bool {
        Weak::ptr_eq(&self.request.owner, &lifecycle.owner)
            && lifecycle.matches_registration(self.account(), self.generation())
            && self.current()
    }
}

pub(crate) trait ClosedEventSchedulePort: Send + Sync {
    fn rearm(&self, account: &str, generation: u64, current: &dyn Fn() -> bool) -> Result<(), ()>;
    fn rearm_accepted(&self, ticket: &AcceptedEventScheduleTicket<'_>) -> Result<(), ()> {
        self.rearm(ticket.account(), ticket.generation(), &|| ticket.current())
    }
    fn notification_current(&self) -> bool {
        true
    }
    fn retire(&self) {}
}
impl RemoteMutationRuntime {
    pub(crate) fn install_scoped_emitter(
        &self,
        emitter: Arc<dyn MutationStateEmitter>,
    ) -> Result<(), ()> {
        let mut current = self.inner.scoped_emitter.lock().map_err(|_| ())?;
        if current.is_some() || self.inner.stopped.load(Ordering::SeqCst) {
            return Err(());
        }
        *current = Some(emitter);
        Ok(())
    }

    pub(crate) fn bind_production_schedule_port(
        &self,
        lifecycle: &ClosedMutationLifecycle,
        scope: &Arc<super::reminder_deadlines::ProductionGraphReminderScope>,
    ) -> Result<(), ()> {
        let port: Arc<dyn ClosedEventSchedulePort> = scope.clone();
        self.bind_schedule_inner(lifecycle, &port, Some(Arc::downgrade(scope)))
    }

    fn bind_schedule_inner(
        &self,
        lifecycle: &ClosedMutationLifecycle,
        port: &Arc<dyn ClosedEventSchedulePort>,
        production: Option<Weak<super::reminder_deadlines::ProductionGraphReminderScope>>,
    ) -> Result<(), ()> {
        if !Weak::ptr_eq(&lifecycle.owner, &Arc::downgrade(&self.inner))
            || !lifecycle.is_live()
            || self.inner.versioned_services.is_none()
        {
            return Err(());
        }
        let mut binding = self.inner.schedule_port.lock().map_err(|_| ())?;
        if binding.is_some() {
            return Err(());
        }
        *binding = Some(EventScheduleBinding {
            account: lifecycle.account.clone(),
            generation: lifecycle.generation,
            port: Arc::downgrade(port),
            production,
        });
        if !lifecycle.is_live() {
            *binding = None;
            return Err(());
        }
        Ok(())
    }
    #[allow(dead_code)]
    pub(crate) fn event_schedule_failed(&self) -> bool {
        self.inner.schedule_failed.load(Ordering::SeqCst)
    }

    pub(crate) fn closed_versioned_lifecycle(
        &self,
        account: &str,
        generation: u64,
        services: &Arc<crate::adapters::production_mutation::VersionedMutationServices>,
        accounts: &Arc<AccountSourceRegistry>,
    ) -> Result<ClosedMutationLifecycle, ()> {
        if services.account() != account || !services.belongs_to(accounts) {
            return Err(());
        }
        if !self
            .inner
            .ownership
            .registry_provenance()
            .is_some_and(|registry| {
                Weak::ptr_eq(&registry, &accounts.creation_owner_provenance().0)
            })
        {
            return Err(());
        }
        let same_collection = self
            .inner
            .versioned_services
            .as_ref()
            .is_some_and(|issued| {
                issued
                    .get(account)
                    .is_some_and(|issued| Weak::ptr_eq(issued, &Arc::downgrade(services)))
            });
        if !same_collection {
            #[cfg(not(test))]
            return Err(());
            // Synthetic handoff fixtures exercise lifecycle ordering only.
        }
        let session_services = Arc::downgrade(services);
        let gate_services = session_services.clone();
        let handoff_services = session_services.clone();
        let accounts = Arc::downgrade(accounts);
        let mut lifecycle = self.closed_lifecycle_with_session(
            account,
            generation,
            Arc::new(move || {
                session_services
                    .upgrade()
                    .zip(accounts.upgrade())
                    .is_some_and(|(services, accounts)| services.belongs_to(&accounts))
            }),
        )?;
        lifecycle.gate_current = Arc::new(move |gate| {
            gate_services
                .upgrade()
                .is_some_and(|services| services.matches_gate(gate))
        });
        lifecycle.handoff_session_current = Arc::new(move |gate| {
            handoff_services
                .upgrade()
                .is_some_and(|services| services.current_under_gate(gate))
        });
        Ok(lifecycle)
    }

    fn closed_lifecycle_with_session(
        &self,
        account: &str,
        generation: u64,
        session_current: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<ClosedMutationLifecycle, ()> {
        if self.inner.stopped.load(Ordering::SeqCst)
            || !self
                .inner
                .ownership
                .is_current_attachment(account, generation)
        {
            return Err(());
        }
        let requests = self.inner.requests.lock().map_err(|_| ())?;
        if requests
            .iter()
            .any(|(id, request)| id != account || request.attachment_generation != generation)
        {
            return Err(());
        }
        let _events = self.inner.events.lock().map_err(|_| ())?;
        Ok(ClosedMutationLifecycle {
            owner: Arc::downgrade(&self.inner),
            account: account.into(),
            generation,
            session_current,
            gate_current: Arc::new(|_| false),
            handoff_session_current: Arc::new(|_| false),
        })
    }
    #[allow(dead_code)]
    pub(crate) fn current_request_gate(
        &self,
        registry: &AccountSourceRegistry,
        account: &str,
    ) -> Result<MutationRequestGate, MutationRuntimeError> {
        let registry_owner = self
            .inner
            .ownership
            .registry_provenance()
            .ok_or(MutationRuntimeError::Unavailable)?;
        if !Weak::ptr_eq(&registry_owner, &registry.creation_owner_provenance().0) {
            return Err(MutationRuntimeError::Unavailable);
        }
        let requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| MutationRuntimeError::Unavailable)?;
        let request = requests
            .get(account)
            .ok_or(MutationRuntimeError::Unavailable)?;
        let gate = MutationRequestGate {
            owner: Arc::downgrade(&self.inner),
            registry: registry_owner,
            account: account.to_owned(),
            attachment_generation: request.attachment_generation,
            request_generation: request.request_generation,
            cancellation: request.cancellation.clone(),
            event: request.event.clone(),
        };
        drop(requests);
        if gate.current_for(registry, account, gate.attachment_generation) {
            Ok(gate)
        } else {
            Err(MutationRuntimeError::Unavailable)
        }
    }
    pub fn new(
        ownership: Arc<dyn MutationAccountOwnership>,
        service: RemoteMutationService,
    ) -> Self {
        Self::with_runner_and_emitter(
            ownership,
            Arc::new(service),
            Arc::new(NoopMutationStateEmitter),
        )
    }

    pub fn with_deletion(
        ownership: Arc<dyn MutationAccountOwnership>,
        title_service: RemoteMutationService,
        deletion_service: RemoteDeletionService,
    ) -> Self {
        Self::with_runner_and_emitter(
            ownership,
            Arc::new(CombinedMutationRunner::new(
                Arc::new(title_service),
                Arc::new(deletion_service),
            )),
            Arc::new(NoopMutationStateEmitter),
        )
    }

    #[allow(dead_code)]
    pub(crate) fn with_creation_services(
        ownership: Arc<AccountSourceRegistry>,
        title: RemoteMutationService,
        deletion: RemoteDeletionService,
        services: Vec<
            Arc<crate::application::remote_creation_dispatch::RemoteCreationDispatchService>,
        >,
        emitter: Arc<dyn MutationStateEmitter>,
    ) -> Result<Self, MutationRuntimeError> {
        let mut by_account = HashMap::new();
        for service in services {
            if !service.belongs_to(&ownership)
                || by_account
                    .insert(service.account().to_owned(), service)
                    .is_some()
            {
                return Err(MutationRuntimeError::Unavailable);
            }
        }
        Ok(Self::with_runner_and_emitter(
            ownership,
            Arc::new(CombinedMutationRunner::from_runners(vec![
                Arc::new(title),
                Arc::new(deletion),
                Arc::new(CreationMutationRunner {
                    services: by_account,
                }),
            ])),
            emitter,
        ))
    }

    pub(crate) fn with_versioned_services_closed(
        ownership: Arc<AccountSourceRegistry>,
        title: RemoteMutationService,
        deletion: RemoteDeletionService,
        services: Vec<Arc<crate::adapters::production_mutation::VersionedMutationServices>>,
    ) -> Result<Self, MutationRuntimeError> {
        if services.is_empty() {
            return Err(MutationRuntimeError::Unavailable);
        }
        let mut by_account = HashMap::new();
        let mut creations = HashMap::new();
        for service in services {
            if !service.belongs_to(&ownership) || by_account.contains_key(service.account()) {
                return Err(MutationRuntimeError::Unavailable);
            }
            creations.insert(service.account().to_owned(), service.creation.clone());
            by_account.insert(service.account().to_owned(), service);
        }
        let weak_services = by_account
            .iter()
            .map(|(account, services)| (account.clone(), Arc::downgrade(services)))
            .collect();
        let mut runtime = Self::with_runner_and_emitter(
            ownership,
            Arc::new(CombinedMutationRunner::from_runners(vec![
                Arc::new(title),
                Arc::new(deletion),
                Arc::new(CreationMutationRunner {
                    services: creations,
                }),
                Arc::new(VersionedMutationRunner {
                    services: by_account.clone(),
                    store: VersionedMutationStore::Fields,
                }),
                Arc::new(VersionedMutationRunner {
                    services: by_account.clone(),
                    store: VersionedMutationStore::CreationV2,
                }),
                Arc::new(VersionedMutationRunner {
                    services: by_account,
                    store: VersionedMutationStore::Event,
                }),
            ])),
            Arc::new(NoopMutationStateEmitter),
        );
        Arc::get_mut(&mut runtime.inner)
            .expect("new unshared runtime")
            .versioned_services = Some(weak_services);
        Ok(runtime)
    }

    pub fn with_emitter(
        ownership: Arc<dyn MutationAccountOwnership>,
        service: RemoteMutationService,
        emitter: Arc<dyn MutationStateEmitter>,
    ) -> Self {
        Self::with_runner_and_emitter(ownership, Arc::new(service), emitter)
    }

    pub fn with_deletion_and_emitter(
        ownership: Arc<dyn MutationAccountOwnership>,
        title_service: RemoteMutationService,
        deletion_service: RemoteDeletionService,
        emitter: Arc<dyn MutationStateEmitter>,
    ) -> Self {
        Self::with_runner_and_emitter(
            ownership,
            Arc::new(CombinedMutationRunner::new(
                Arc::new(title_service),
                Arc::new(deletion_service),
            )),
            emitter,
        )
    }

    fn with_runner_and_emitter(
        ownership: Arc<dyn MutationAccountOwnership>,
        service: Arc<dyn MutationRunner>,
        emitter: Arc<dyn MutationStateEmitter>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                ownership,
                service,
                versioned_services: None,
                schedule_port: Mutex::new(None),
                schedule_failed: AtomicBool::new(false),
                emitter,
                scoped_emitter: Mutex::new(None),
                requests: Mutex::new(HashMap::new()),
                gates: Mutex::new(HashMap::new()),
                events: Mutex::new(PendingEvents::default()),
                next_generation: AtomicU64::new(1),
                stopped: AtomicBool::new(false),
            }),
        }
    }

    pub fn submit(&self, account_id: &str) -> Result<MutationRuntimeView, MutationRuntimeError> {
        Uuid::parse_str(account_id).map_err(|_| MutationRuntimeError::InvalidIdentity)?;
        if self.inner.stopped.load(Ordering::SeqCst) {
            return Err(MutationRuntimeError::Unavailable);
        }
        let attachment_generation = self
            .inner
            .ownership
            .attachment_generation(account_id)
            .ok_or(MutationRuntimeError::Detached)?;
        self.submit_for_attachment(account_id, attachment_generation)
    }

    pub(crate) fn submit_for_attachment(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<MutationRuntimeView, MutationRuntimeError> {
        self.submit_with_spawner(account_id, attachment_generation, |worker| {
            thread::Builder::new()
                .name("calendar-todo-remote-mutation".into())
                .spawn(worker)
                .map(|_| ())
        })
    }

    fn submit_with_spawner(
        &self,
        account_id: &str,
        attachment_generation: u64,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) -> Result<MutationRuntimeView, MutationRuntimeError> {
        self.submit_inner(account_id, attachment_generation, None, spawn)
    }
    pub(crate) fn submit_scoped_event(
        &self,
        generation: u64,
        binding: crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
        scope: crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<MutationRuntimeView, MutationRuntimeError> {
        let binding = binding
            .revalidate()
            .map_err(|_| MutationRuntimeError::Detached)?;
        if !binding.matches_event(&scope) {
            return Err(MutationRuntimeError::Detached);
        }
        let account = scope.ids().1.to_owned();
        let services = self
            .inner
            .versioned_services
            .as_ref()
            .and_then(|services| services.get(&account))
            .and_then(Weak::upgrade)
            .ok_or(MutationRuntimeError::Unavailable)?;
        let status = services
            .event
            .scoped_status(&scope)
            .map_err(|_| MutationRuntimeError::Detached)?;
        if let Some(predecessor) = &status.predecessor_operation_id {
            let mut requests = self
                .inner
                .requests
                .lock()
                .map_err(|_| MutationRuntimeError::Unavailable)?;
            if let Some(previous) = requests.get_mut(&account) {
                if previous.view.state == MutationRuntimeState::Running {
                    let Some(head) = &previous.event else {
                        return Err(MutationRuntimeError::Unavailable);
                    };
                    if head.1.ids().0 != predecessor {
                        if previous.attachment_generation != generation {
                            return Err(MutationRuntimeError::Detached);
                        }
                        if let Some(position) =
                            previous.followups.iter().position(|(queued, _, _)| {
                                queued.1.ids().0 == predecessor && queued.1.ids().3 == scope.ids().3
                            })
                        {
                            if let Some(existing) = previous
                                .followups
                                .iter_mut()
                                .find(|(queued, _, _)| queued.1.ids() == scope.ids())
                            {
                                if existing
                                    .2
                                    .is_none_or(|revision| revision <= status.revision)
                                {
                                    *existing = (
                                        Arc::new((binding, scope)),
                                        generation,
                                        Some(status.revision),
                                    );
                                }
                            } else {
                                if previous.followups.len() >= 128 {
                                    return Err(MutationRuntimeError::Unavailable);
                                }
                                previous.followups.insert(
                                    position + 1,
                                    (
                                        Arc::new((binding, scope)),
                                        generation,
                                        Some(status.revision),
                                    ),
                                );
                            }
                            return Ok(MutationRuntimeView {
                                state: MutationRuntimeState::Queued,
                                ..previous.view.clone()
                            });
                        }
                        return Err(MutationRuntimeError::Unavailable);
                    }
                    if head.1.ids().1 != scope.ids().1
                        || head.1.ids().2 != scope.ids().2
                        || head.1.ids().3 != scope.ids().3
                        || previous.attachment_generation != generation
                    {
                        return Err(MutationRuntimeError::Unavailable);
                    }
                    if previous
                        .successor
                        .as_ref()
                        .is_none_or(|(_, revision)| *revision <= status.revision)
                    {
                        previous.successor = Some((Arc::new((binding, scope)), status.revision));
                    }
                    return Ok(previous.view.clone());
                }
            }
        }
        if status.state != crate::domain::remote_mutation::RemoteMutationState::Pending {
            return Ok(MutationRuntimeView {
                account_id: account,
                state: MutationRuntimeState::Idle,
                applied_count: 0,
                conflict_count: 0,
            });
        }
        self.submit_inner(
            &account,
            generation,
            Some(Arc::new((binding, scope))),
            |worker| {
                thread::Builder::new()
                    .name("calendar-todo-scoped-event".into())
                    .spawn(worker)
                    .map(|_| ())
            },
        )
    }
    pub(crate) fn scoped_event_view(
        &self,
        scope: &crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<ScopedEventRuntimeView, ()> {
        let account = scope.ids().1;
        let request = {
            let requests = self.inner.requests.lock().map_err(|_| ())?;
            let r = requests.get(account).ok_or(())?;
            if let Some((event, generation, _)) = r
                .followups
                .iter()
                .find(|(event, _, _)| event.1.ids() == scope.ids())
            {
                let event = event.clone();
                let generation = *generation;
                drop(requests);
                if self.inner.stopped.load(Ordering::SeqCst)
                    || !self
                        .inner
                        .ownership
                        .is_current_attachment(account, generation)
                    || !event.0.event_owner_current()
                {
                    return Err(());
                }
                let services = self
                    .inner
                    .versioned_services
                    .as_ref()
                    .and_then(|s| s.get(account))
                    .and_then(Weak::upgrade)
                    .ok_or(())?;
                let operation = services.event.scoped_status(&event.1).map_err(|_| ())?;
                return Ok(ScopedEventRuntimeView {
                    kind: event.1.kind(),
                    runtime: MutationRuntimeView {
                        account_id: account.into(),
                        state: MutationRuntimeState::Queued,
                        applied_count: 0,
                        conflict_count: 0,
                    },
                    operation,
                });
            }
            if !r
                .event
                .as_ref()
                .is_some_and(|event| event.1.ids() == scope.ids())
            {
                return Err(());
            }
            (
                MutationRequestGate {
                    owner: Arc::downgrade(&self.inner),
                    registry: self
                        .inner
                        .ownership
                        .registry_provenance()
                        .unwrap_or_default(),
                    account: account.to_owned(),
                    attachment_generation: r.attachment_generation,
                    request_generation: r.request_generation,
                    cancellation: r.cancellation.clone(),
                    event: r.event.clone(),
                },
                r.view.clone(),
            )
        };
        request.0.scoped_view(request.1).ok_or(())
    }
    fn submit_inner(
        &self,
        account_id: &str,
        attachment_generation: u64,
        event: Option<EventScope>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) -> Result<MutationRuntimeView, MutationRuntimeError> {
        Uuid::parse_str(account_id).map_err(|_| MutationRuntimeError::InvalidIdentity)?;
        let request_generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
        let cancellation = Cancellation::default();
        let view = MutationRuntimeView {
            account_id: account_id.to_owned(),
            state: MutationRuntimeState::Running,
            applied_count: 0,
            conflict_count: 0,
        };
        let gate = self
            .inner
            .gates
            .lock()
            .map_err(|_| MutationRuntimeError::Unavailable)?
            .entry(account_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let mut requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| MutationRuntimeError::Unavailable)?;
        if self.inner.stopped.load(Ordering::SeqCst) {
            return Err(MutationRuntimeError::Unavailable);
        }
        if !self
            .inner
            .ownership
            .is_current_attachment(account_id, attachment_generation)
        {
            return Err(MutationRuntimeError::Detached);
        }
        if let Some(event) = &event
            && let Some(previous) = requests.get_mut(account_id)
            && previous.view.state == MutationRuntimeState::Running
        {
            if previous
                .event
                .as_ref()
                .is_some_and(|old| old.1.ids() == event.1.ids())
            {
                return Ok(previous.view.clone());
            }
            if previous.attachment_generation != attachment_generation
                || previous
                    .event
                    .as_ref()
                    .is_none_or(|old| old.1.ids().3 == event.1.ids().3)
            {
                return Err(MutationRuntimeError::Unavailable);
            }
            if !previous
                .followups
                .iter()
                .any(|(old, _, _)| old.1.ids() == event.1.ids())
            {
                if previous.followups.len() >= 128 {
                    return Err(MutationRuntimeError::Unavailable);
                }
                previous
                    .followups
                    .push_back((event.clone(), attachment_generation, None));
            }
            return Ok(MutationRuntimeView {
                state: MutationRuntimeState::Queued,
                ..view
            });
        }
        let mut followups = VecDeque::new();
        if let Some(mut previous) = requests.remove(account_id) {
            if previous.attachment_generation == attachment_generation {
                followups = std::mem::take(&mut previous.followups);
            }
            previous.cancellation.cancel();
            // A terminal worker has left transport work and the account gate.
            // Re-submitting recovery must preserve its live attachment session.
            if previous.view.state == MutationRuntimeState::Running {
                self.inner.service.cancel_in_flight(account_id);
            }
        }
        requests.insert(
            account_id.to_owned(),
            Request {
                request_generation,
                attachment_generation,
                cancellation: cancellation.clone(),
                view: view.clone(),
                event: event.clone(),
                successor: None,
                followups,
            },
        );
        if let Some(event) = &event {
            let request = MutationRequestGate {
                owner: Arc::downgrade(&self.inner),
                registry: self
                    .inner
                    .ownership
                    .registry_provenance()
                    .unwrap_or_default(),
                account: account_id.into(),
                attachment_generation,
                request_generation,
                cancellation: cancellation.clone(),
                event: Some(event.clone()),
            };
            self.inner
                .events
                .lock()
                .map_err(|_| MutationRuntimeError::Unavailable)?
                .scoped
                .push_back((request, view.clone()));
        } else {
            self.inner.enqueue(view.clone());
        }
        drop(requests);
        self.inner.drain_events();
        let inner = self.inner.clone();
        let account_id = account_id.to_owned();
        let worker_cancellation = cancellation.clone();
        let worker_account = account_id.clone();
        if spawn(Box::new(move || {
            let guard = match gate.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    // Do not recover a gate whose worker unwound while holding it.
                    drop(poisoned);
                    inner.finish(
                        &worker_account,
                        request_generation,
                        attachment_generation,
                        &worker_cancellation,
                        (MutationRuntimeState::Failed, 0, 0),
                    );
                    return;
                }
            };
            let request = MutationRequestGate {
                owner: Arc::downgrade(&inner),
                registry: inner.ownership.registry_provenance().unwrap_or_default(),
                account: worker_account.clone(),
                attachment_generation,
                request_generation,
                cancellation: worker_cancellation.clone(),
                event,
            };
            let outcome = inner.run(&worker_account, &request);
            drop(guard);
            worker_cancellation.1.store(true, Ordering::SeqCst);
            inner.drain_events();
            inner.finish(
                &worker_account,
                request_generation,
                attachment_generation,
                &worker_cancellation,
                outcome,
            );
        }))
        .is_err()
        {
            self.inner.finish(
                &account_id,
                request_generation,
                attachment_generation,
                &cancellation,
                (MutationRuntimeState::Failed, 0, 0),
            );
            return Err(MutationRuntimeError::Unavailable);
        }
        Ok(view)
    }

    pub fn state(&self, account_id: &str) -> Result<MutationRuntimeView, MutationRuntimeError> {
        let requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| MutationRuntimeError::Unavailable)?;
        Ok(requests.get(account_id).map_or_else(
            || MutationRuntimeView {
                account_id: account_id.to_owned(),
                state: MutationRuntimeState::Idle,
                applied_count: 0,
                conflict_count: 0,
            },
            |request| request.view.clone(),
        ))
    }

    pub fn account_detached(&self, account_id: &str) {
        if let Ok(mut requests) = self.inner.requests.lock() {
            if let Some(request) = requests.get_mut(account_id) {
                request.cancellation.cancel();
                request.view.state = MutationRuntimeState::Cancelled;
                self.inner.enqueue(request.view.clone());
            }
            // Serialize interruption with submission before invoking user callbacks.
            self.inner.service.cancel_in_flight(account_id);
        }
        self.inner.drain_events();
    }

    pub fn shutdown(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut requests) = self.inner.requests.lock() {
            for (account_id, request) in requests.iter_mut() {
                request.cancellation.cancel();
                request.view.state = MutationRuntimeState::Cancelled;
                self.inner.service.cancel_in_flight(account_id);
                self.inner.enqueue(request.view.clone());
            }
        }
        self.inner.drain_events();
    }
}

impl Inner {
    // Called while holding requests: pending snapshots coalesce per account.
    fn enqueue(&self, view: MutationRuntimeView) {
        if let Ok(mut events) = self.events.lock() {
            events
                .views
                .retain(|pending| pending.account_id != view.account_id);
            events.views.push_back(view);
        }
    }

    fn drain_events(&self) {
        {
            let Ok(mut events) = self.events.lock() else {
                return;
            };
            if events.draining || events.handoff_paused {
                return;
            }
            events.draining = true;
        }
        let mut guard = DrainerGuard {
            events: &self.events,
            armed: true,
        };
        loop {
            let view = {
                let Ok(mut events) = self.events.lock() else {
                    return;
                };
                if events.handoff_paused {
                    events.draining = false;
                    guard.armed = false;
                    return;
                }
                if let Some(index) = events.schedules.iter().position(|request| {
                    request.cancellation.1.load(Ordering::SeqCst)
                        || request.cancellation.is_cancelled()
                }) && let Some(request) = events.schedules.remove(index)
                {
                    drop(events);
                    self.rearm_event_schedule(request);
                    continue;
                }
                let Some(view) = events.views.pop_front() else {
                    if let Some((request, view)) = events.scoped.pop_front() {
                        drop(events);
                        let emitter = self
                            .scoped_emitter
                            .lock()
                            .ok()
                            .and_then(|emitter| emitter.clone());
                        if let Some(view) = request.scoped_view(view) {
                            emitter
                                .unwrap_or_else(|| self.emitter.clone())
                                .emit_scoped(view);
                        }
                        continue;
                    }
                    events.draining = false;
                    guard.armed = false;
                    return;
                };
                view
            };
            // Reentrant callbacks only enqueue; this drainer retains publication order.
            self.emitter.emit(view);
        }
    }

    fn rearm_event_schedule(&self, request: MutationRequestGate) {
        let port = self.schedule_port.lock().ok().and_then(|binding| {
            binding
                .as_ref()
                .filter(|binding| {
                    binding.account == request.account
                        && binding.generation == request.attachment_generation
                })
                .and_then(|binding| binding.port.upgrade())
        });
        if !request.schedule_current() {
            return;
        }
        if let Some(port) = port {
            let ticket = AcceptedEventScheduleTicket { request: &request };
            let result = port.rearm_accepted(&ticket);
            if request.schedule_current() {
                self.schedule_failed
                    .store(result.is_err(), Ordering::SeqCst);
            }
        }
    }
    fn run(
        &self,
        account_id: &str,
        request: &MutationRequestGate,
    ) -> (MutationRuntimeState, u32, u32) {
        let mut applied = 0_u32;
        let mut conflicts = 0_u32;
        let terminal = loop {
            if !request.current() {
                break MutationRuntimeState::Cancelled;
            }
            let outcome = if request.event_scope().is_some_and(|s| {
                s.kind() == crate::domain::remote_crud::RemoteCrudKind::UpdateEventV3
            }) {
                self.versioned_services
                    .as_ref()
                    .and_then(|services| services.get(account_id))
                    .and_then(Weak::upgrade)
                    .ok_or(())
                    .and_then(|services| {
                        services
                            .event
                            .run_with_request(account_id, request)
                            .map_err(|_| ())
                    })
            } else {
                self.service.run_one(account_id, request)
            };
            if !request.current() {
                break MutationRuntimeState::Cancelled;
            }
            match outcome {
                Ok(RemoteMutationRunOutcome::Applied) => {
                    applied = applied.saturating_add(1);
                    if request.event.is_some() {
                        break MutationRuntimeState::Idle;
                    }
                }
                Ok(RemoteMutationRunOutcome::Conflict) => {
                    conflicts = conflicts.saturating_add(1);
                    if request.event.is_some() {
                        break MutationRuntimeState::Conflict;
                    }
                }
                Ok(RemoteMutationRunOutcome::Idle) => break MutationRuntimeState::Idle,
                Ok(RemoteMutationRunOutcome::Deferred) => break MutationRuntimeState::Deferred,
                Err(_) => break MutationRuntimeState::Failed,
            }
        };
        (terminal, applied, conflicts)
    }

    fn finish(
        self: &Arc<Self>,
        account_id: &str,
        request_generation: u64,
        attachment_generation: u64,
        cancellation: &Cancellation,
        (terminal, applied, conflicts): (MutationRuntimeState, u32, u32),
    ) {
        if let Ok(mut requests) = self.requests.lock()
            && let Some(request) = requests.get_mut(account_id)
            && request.request_generation == request_generation
            && request.attachment_generation == attachment_generation
            && request.view.state == MutationRuntimeState::Running
        {
            let current = !cancellation.is_cancelled()
                && !self.stopped.load(Ordering::SeqCst)
                && self
                    .ownership
                    .is_current_attachment(account_id, attachment_generation);
            request.view.state = if current {
                terminal
            } else {
                MutationRuntimeState::Cancelled
            };
            request.view.applied_count = applied;
            request.view.conflict_count = conflicts;
            if request.event.is_some() {
                let gate = MutationRequestGate {
                    owner: Arc::downgrade(self),
                    registry: self.ownership.registry_provenance().unwrap_or_default(),
                    account: account_id.to_owned(),
                    attachment_generation,
                    request_generation,
                    cancellation: cancellation.clone(),
                    event: request.event.clone(),
                };
                if let Ok(mut events) = self.events.lock() {
                    events.scoped.retain(|(gate, _)| gate.account != account_id);
                    events.scoped.push_back((gate, request.view.clone()));
                }
            } else {
                self.enqueue(request.view.clone());
            }
        }
        self.drain_events();
        self.continue_successor(account_id, request_generation);
        self.continue_exact_queue(account_id, request_generation);
    }

    fn continue_exact_queue(self: &Arc<Self>, account: &str, generation: u64) {
        let ticket = self.requests.lock().ok().and_then(|mut requests| {
            let request = requests.get_mut(account)?;
            if request.request_generation != generation
                || request.view.state == MutationRuntimeState::Running
                || request.cancellation.is_cancelled()
            {
                return None;
            }
            request.followups.pop_front()
        });
        let Some((ticket, attachment, revision)) = ticket else {
            return;
        };
        // Only a live, independently confirmed exact ticket can continue. No persisted scan.
        if !ticket.0.event_owner_current()
            || !self.ownership.is_current_attachment(account, attachment)
            || self.stopped.load(Ordering::SeqCst)
        {
            return;
        }
        let Some(services) = self
            .versioned_services
            .as_ref()
            .and_then(|s| s.get(account))
            .and_then(Weak::upgrade)
        else {
            return;
        };
        let Ok(status) = services.event.scoped_status(&ticket.1) else {
            return;
        };
        let previous = self
            .requests
            .lock()
            .ok()
            .and_then(|requests| requests.get(account).and_then(|r| r.event.clone()));
        let predecessor_applied =
            status
                .predecessor_operation_id
                .as_ref()
                .is_none_or(|predecessor| {
                    previous.as_ref().is_some_and(|head| {
                        head.1.ids().0 == predecessor
                        && services.event.scoped_status(&head.1).is_ok_and(|v| {
                            v.state == crate::domain::remote_mutation::RemoteMutationState::Applied
                        })
                    })
                });
        if status.state != crate::domain::remote_mutation::RemoteMutationState::Pending
            || revision.is_some_and(|revision| revision != status.revision)
            || !predecessor_applied
        {
            self.continue_exact_queue(account, generation);
            return;
        }
        let _ = RemoteMutationRuntime {
            inner: self.clone(),
        }
        .submit_inner(account, attachment, Some(ticket), |worker| {
            thread::Builder::new()
                .name("calendar-todo-scoped-event".into())
                .spawn(worker)
                .map(|_| ())
        });
    }

    fn continue_successor(self: &Arc<Self>, account: &str, generation: u64) {
        let ticket = self.requests.lock().ok().and_then(|mut requests| {
            let request = requests.get_mut(account)?;
            if request.request_generation != generation
                || request.view.state != MutationRuntimeState::Idle
                || request.cancellation.is_cancelled()
            {
                return None;
            }
            request
                .successor
                .take()
                .map(|ticket| (ticket, request.event.clone(), request.attachment_generation))
        });
        let Some(((ticket, revision), Some(head), attachment)) = ticket else {
            return;
        };
        let Some(services) = self
            .versioned_services
            .as_ref()
            .and_then(|s| s.get(account))
            .and_then(Weak::upgrade)
        else {
            return;
        };
        let Ok(prior) = services.event.scoped_status(&head.1) else {
            return;
        };
        let Ok(next) = services.event.scoped_status(&ticket.1) else {
            return;
        };
        if prior.state != crate::domain::remote_mutation::RemoteMutationState::Applied
            || next.predecessor_operation_id.as_deref() != Some(head.1.ids().0)
            || next.revision != revision
            || next.state != crate::domain::remote_mutation::RemoteMutationState::Pending
            || !ticket.0.event_owner_current()
        {
            return;
        }
        // Consume only this live exact ticket, never scan persisted pending work.
        if let Ok((binding, scope)) = Arc::try_unwrap(ticket) {
            let _ = RemoteMutationRuntime {
                inner: self.clone(),
            }
            .submit_scoped_event(attachment, binding, scope);
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MutationRuntimeError {
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account mutation source is detached")]
    Detached,
    #[error("remote mutation runtime is unavailable")]
    Unavailable,
}
