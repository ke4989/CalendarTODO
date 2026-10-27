use crate::application::{
    account_profiles::{
        AccountProfileService, AccountProfileServiceError, RuntimeAccountView, SourceAttachmentMode,
    },
    account_registry::AccountSourceRegistry,
    discovery_runtime::{AccountCalendarDiscovery, DiscoveryError},
    production_mutation_lifecycle::ReviewedMutationLifecycleIntegration,
    production_mutation_transport::ReviewedMutationTransportRegistration,
    source_factory::AttachmentCancellation,
    sync_runtime::SyncExecutionRuntime,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

pub const ATTACHMENT_STATE_EVENT: &str = "calendar-todo://attachment-state";

// Lifecycle registration only. No ordinary discovery, reads, sync or deadlines.

#[allow(dead_code)]
mod probe_completion;
#[allow(dead_code)]
mod probe_controller;
#[allow(dead_code)]
mod probe_diagnostic;
pub(crate) use probe_controller::ProbeControllerScope;
pub(crate) use probe_diagnostic::ProbeLocalClosure;

/// Weak, execution-free metadata for one exact captured attachment/session.
#[derive(Clone)]
pub(crate) struct ConflictAttachmentOwner {
    runtime: std::sync::Weak<Inner>,
    account: String,
    generation: u64,
    session: crate::application::production_creation_owner::CreationSessionEvidence,
}
impl ConflictAttachmentOwner {
    pub(crate) fn exact_event_owner_current(
        &self,
        binding: &crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
    ) -> bool {
        let Some(inner) = self.runtime.upgrade() else {
            return false;
        };
        let runtime = AttachmentRuntime { inner };
        let Ok(requests) = runtime.inner.lock_requests() else {
            return false;
        };
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        self.matches_gate(&gate) && binding.event_owner_under_gate(&gate)
    }
    pub(crate) fn revalidate_exact_probe_binding(
        &self,
        binding: crate::adapters::production_mutation::exact_probe::ExactProbeBinding,
    ) -> Result<crate::adapters::production_mutation::exact_probe::ExactProbeBinding, ()> {
        let runtime = AttachmentRuntime {
            inner: self.runtime.upgrade().ok_or(())?,
        };
        let requests = runtime.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        binding.revalidate_under_gate(&gate)
    }
    pub(crate) fn probe_acquisition_current(
        &self,
        scope: &crate::adapters::production_mutation::probe_preflight::live_acquisition::AcquisitionScope,
    ) -> bool {
        let Some(inner) = self.runtime.upgrade() else {
            return false;
        };
        let runtime = AttachmentRuntime { inner };
        let Ok(requests) = runtime.inner.lock_requests() else {
            return false;
        };
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        self.matches_gate(&gate) && scope.under_gate(&gate)
    }
    /// Actor-thread check only; durable eligibility is rechecked by that actor.
    pub(crate) fn probe_seed_owner_alive(
        &self,
        registry: &std::sync::Weak<()>,
        actor: &std::sync::Weak<()>,
    ) -> bool {
        let Some(runtime) = self.runtime.upgrade() else {
            return false;
        };
        let (actual_registry, actual_actor) = runtime.registry.creation_owner_provenance();
        !runtime.stopped.load(Ordering::SeqCst)
            && !runtime.closing.load(Ordering::SeqCst)
            && !runtime.requests.is_poisoned()
            && !runtime.worker_gates.is_poisoned()
            && runtime.worker_gates.lock().is_ok_and(|gates| {
                gates
                    .get(&self.account)
                    .is_none_or(|gate| !gate.is_poisoned())
            })
            && std::sync::Weak::ptr_eq(registry, &actual_registry)
            && std::sync::Weak::ptr_eq(actor, &actual_actor)
            && runtime
                .registry
                .is_current_attachment(&self.account, self.generation)
    }
    /// Dedicated short gate check; never retain the attachment lock over I/O.
    pub(crate) fn revalidate_probe_metadata(
        &self,
        prepared: crate::adapters::production_mutation::probe_preflight::PreparedProbeMetadata,
    ) -> Result<crate::adapters::production_mutation::probe_preflight::PreparedProbeMetadata, ()>
    {
        let runtime = AttachmentRuntime {
            inner: self.runtime.upgrade().ok_or(())?,
        };
        let requests = runtime.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: &runtime,
            _requests: &requests,
        };
        if !self.matches_gate(&gate) {
            return Err(());
        }
        prepared.revalidate(&gate)
    }
    pub(crate) fn matches_gate(&self, gate: &ClosedCompositionGate<'_>) -> bool {
        std::sync::Weak::ptr_eq(&self.runtime, &Arc::downgrade(&gate.attachments().inner))
    }
    pub(crate) fn matches(&self, account: &str, generation: u64) -> bool {
        self.account == account && self.generation == generation
    }
    pub(crate) fn current(
        &self,
        registry: &std::sync::Weak<()>,
        actor: &std::sync::Weak<()>,
    ) -> bool {
        let Some(runtime) = self.runtime.upgrade() else {
            return false;
        };
        let (actual_registry, actual_actor) = runtime.registry.creation_owner_provenance();
        !runtime.stopped.load(Ordering::SeqCst)
            && !runtime.closing.load(Ordering::SeqCst)
            && !runtime.requests.is_poisoned()
            && !runtime.worker_gates.is_poisoned()
            && runtime.worker_gates.lock().is_ok_and(|gates| {
                gates
                    .get(&self.account)
                    .is_none_or(|gate| !gate.is_poisoned())
            })
            && std::sync::Weak::ptr_eq(registry, &actual_registry)
            && std::sync::Weak::ptr_eq(actor, &actual_actor)
            && runtime
                .registry
                .is_current_attachment(&self.account, self.generation)
            && runtime
                .registry
                .profile(&self.account)
                .is_ok_and(|profile| {
                    profile.enabled
                        && profile.credential_configured
                        && profile.remote_editing_enabled
                        && profile.provider_kind == crate::domain::account::ProviderKind::CalDav
                        && profile.region_profile == "icloud-cn"
                })
            && runtime.mutation_sessions.as_ref().is_some_and(|owner| {
                owner
                    .creation_session_evidence(&self.account)
                    .is_ok_and(|fresh| {
                        fresh.same_owner(&self.session)
                            && fresh.matches(&runtime.registry, &self.account)
                    })
            })
            && !runtime.stopped.load(Ordering::SeqCst)
            && !runtime.closing.load(Ordering::SeqCst)
            && !runtime.requests.is_poisoned()
    }
}
impl std::fmt::Debug for ConflictAttachmentOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConflictAttachmentOwner(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachmentState {
    Detached,
    Attaching,
    Ready,
    ActionRequired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachmentFailure {
    Credential,
    Disabled,
    UnsafeConfiguration,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentView {
    pub account_id: String,
    pub source_mode: Option<SourceAttachmentMode>,
    pub state: AttachmentState,
    pub failure: Option<AttachmentFailure>,
}

pub trait AttachmentStateEmitter: Send + Sync + 'static {
    fn emit(&self, view: AttachmentView);
}

/// Optional internal session owner. The normal AppState constructor supplies
/// no owner, so adding lifecycle hooks cannot activate production writes.
pub(crate) trait AttachmentMutationSessions: Send + Sync {
    fn register(&self, account_id: &str, attachment_generation: u64) -> Result<(), ()>;
    fn revoke(&self, account_id: &str);
    fn shutdown(&self);
    fn creation_session_evidence(
        &self,
        _account: &str,
    ) -> Result<crate::application::production_creation_owner::CreationSessionEvidence, ()> {
        Err(())
    }

    /// Positive evidence is deliberately unavailable to lifecycle test doubles.
    /// Only the installed production session owner overrides this method.
    #[allow(dead_code)]
    fn reviewed_title_registration(
        &self,
        _account_id: &str,
    ) -> Result<ReviewedMutationTransportRegistration, ()> {
        Err(())
    }
}

/// Unforgeable outside this module: presence means the one attachment runtime
/// owns all mutation-session teardown hooks.
pub(crate) struct CompleteAttachmentMutationLifecycle(());

struct Request {
    generation: u64,
    cancellation: AttachmentCancellation,
    /// Whether this attach should also run an immediate Startup sync. It comes
    /// from the stored "sync on startup" preference at request time.
    startup_sync: bool,
    production: bool,
    view: AttachmentView,
    completion: Arc<ConnectionCompletion>,
}

#[derive(Default)]
struct ConnectionCompletion {
    result: Mutex<Option<Result<u64, AccountProfileServiceError>>>,
    changed: std::sync::Condvar,
}
impl ConnectionCompletion {
    fn finish(&self, result: Result<u64, AccountProfileServiceError>) {
        if let Ok(mut stored) = self.result.lock() {
            if stored.is_none() {
                *stored = Some(result);
                self.changed.notify_all();
            }
        }
    }
    fn wait(&self) -> Result<u64, AccountProfileServiceError> {
        let stored = self
            .result
            .lock()
            .map_err(|_| AccountProfileServiceError::Unavailable)?;
        let (stored, _) = self
            .changed
            .wait_timeout_while(stored, std::time::Duration::from_secs(60), |value| {
                value.is_none()
            })
            .map_err(|_| AccountProfileServiceError::Unavailable)?;
        stored.unwrap_or(Err(AccountProfileServiceError::Unavailable))
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        self.completion
            .finish(Err(AccountProfileServiceError::Unavailable));
    }
}

/// Captured real connection ownership, not read-sync or mutation authority.
pub(crate) struct ConfirmedConnection {
    runtime: AttachmentRuntime,
    sessions: std::sync::Weak<crate::adapters::production_mutation::ProductionMutationSessions>,
    account: String,
    route: String,
    generation: u64,
    session: crate::application::production_creation_owner::CreationSessionEvidence,
}
impl ConfirmedConnection {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn current(&self) -> bool {
        self.session
            .matches(&self.runtime.inner.registry, &self.account)
            && self
                .runtime
                .inner
                .registry
                .is_current_attachment(&self.account, self.generation)
            && self.sessions.upgrade().is_some_and(|sessions| {
                self.runtime
                    .connected_owner_current(&sessions, &self.account, Some(&self.route))
            })
    }
}

struct Inner {
    accounts: AccountProfileService,
    registry: Arc<AccountSourceRegistry>,
    sync: Arc<SyncExecutionRuntime>,
    discovery: Arc<dyn AccountCalendarDiscovery>,
    emitter: Arc<dyn AttachmentStateEmitter>,
    requests: Mutex<AttachmentRequests>,
    worker_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    next_generation: AtomicU64,
    stopped: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    mutation_sessions: Option<Arc<dyn AttachmentMutationSessions>>,
}

#[derive(Default)]
struct AttachmentRequests {
    requests: HashMap<String, Request>,
    closed_links: HashMap<
        (String, u64),
        std::sync::Weak<crate::application::remote_mutation_runtime::ClosedMutationLifecycle>,
    >,
}
impl std::ops::Deref for AttachmentRequests {
    type Target = HashMap<String, Request>;
    fn deref(&self) -> &Self::Target {
        &self.requests
    }
}
impl std::ops::DerefMut for AttachmentRequests {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.requests
    }
}

/// Publications are owned by the held handoff guard and drained after unlock,
/// including on an error return. The registry retains only weak graph links.
struct AttachmentRequestGuard<'a> {
    guard: Option<std::sync::MutexGuard<'a, AttachmentRequests>>,
    publications: Vec<Arc<crate::application::remote_mutation_runtime::ClosedMutationLifecycle>>,
    publication_sink: Option<ClosedPublicationSink>,
}
type ClosedPublicationSink =
    Arc<Mutex<Vec<Arc<crate::application::remote_mutation_runtime::ClosedMutationLifecycle>>>>;
struct WorkerPublications(ClosedPublicationSink);
impl Drop for WorkerPublications {
    fn drop(&mut self) {
        let tickets = std::mem::take(&mut *self.0.lock().unwrap());
        for ticket in tickets {
            ticket.publish_pending();
        }
    }
}
impl std::ops::Deref for AttachmentRequestGuard<'_> {
    type Target = AttachmentRequests;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for AttachmentRequestGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}
impl Drop for AttachmentRequestGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some(sink) = &self.publication_sink {
            sink.lock().unwrap().append(&mut self.publications);
            return;
        }
        for lifecycle in self.publications.drain(..) {
            lifecycle.publish_pending();
        }
    }
}

#[derive(Clone)]
pub struct AttachmentRuntime {
    inner: Arc<Inner>,
}

/// Minted only while this exact attachment runtime owns its handoff gate.
pub(crate) struct ClosedCompositionGate<'a> {
    attachments: &'a AttachmentRuntime,
    _requests: &'a AttachmentRequestGuard<'a>,
}

/// Gate-private weak metadata for backend atomic checks; no normal authority.
pub(crate) struct NotificationAttachmentLiveness {
    owner: std::sync::Weak<Inner>,
}
impl NotificationAttachmentLiveness {
    pub(crate) fn fast_live(&self) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            !owner.stopped.load(Ordering::SeqCst) && !owner.closing.load(Ordering::SeqCst)
        })
    }
}
impl ClosedCompositionGate<'_> {
    pub(crate) fn notification_liveness(&self) -> NotificationAttachmentLiveness {
        NotificationAttachmentLiveness {
            owner: Arc::downgrade(&self.attachments.inner),
        }
    }
    pub(crate) fn matches_runtime(&self, runtime: &AttachmentRuntime) -> bool {
        Arc::ptr_eq(&self.attachments.inner, &runtime.inner)
    }
    pub(crate) fn attachments(&self) -> &AttachmentRuntime {
        self.attachments
    }
}

impl AttachmentRuntime {
    pub(crate) fn prepare_probe_harness(&self, sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>, account: &str) -> Result<(crate::adapters::production_mutation::probe_preflight::ProbeSourceOwner, crate::adapters::production_mutation::probe_preflight::live_acquisition::PreparedAcquisition), ()>{
        let requests = self.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: self,
            _requests: &requests,
        };
        sessions.prepare_probe_acquisition(&gate, account)
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    pub(crate) fn owns_activation_sessions(
        &self,
        accounts: &Arc<AccountSourceRegistry>,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
    ) -> bool {
        let concrete: Arc<dyn AttachmentMutationSessions> = sessions.clone();
        Arc::ptr_eq(&self.inner.registry, accounts)
            && self.inner.accounts.belongs_to_registry(accounts)
            && self
                .reviewed_session_owner()
                .is_some_and(|installed| Arc::ptr_eq(installed, &concrete))
    }
    pub fn new(
        accounts: AccountProfileService,
        registry: Arc<AccountSourceRegistry>,
        sync: Arc<SyncExecutionRuntime>,
        discovery: Arc<dyn AccountCalendarDiscovery>,
        emitter: Arc<dyn AttachmentStateEmitter>,
    ) -> Self {
        Self::with_mutation_sessions(accounts, registry, sync, discovery, emitter, None)
    }

    pub(crate) fn with_mutation_sessions(
        accounts: AccountProfileService,
        registry: Arc<AccountSourceRegistry>,
        sync: Arc<SyncExecutionRuntime>,
        discovery: Arc<dyn AccountCalendarDiscovery>,
        emitter: Arc<dyn AttachmentStateEmitter>,
        mutation_sessions: Option<Arc<dyn AttachmentMutationSessions>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                accounts,
                registry,
                sync,
                discovery,
                emitter,
                requests: Mutex::new(AttachmentRequests::default()),
                worker_gates: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(1),
                stopped: Arc::new(AtomicBool::new(false)),
                closing: Arc::new(AtomicBool::new(false)),
                mutation_sessions,
            }),
        }
    }

    pub(crate) fn reviewed_mutation_lifecycle(&self) -> ReviewedMutationLifecycleIntegration {
        if self.reviewed_session_owner().is_some() {
            ReviewedMutationLifecycleIntegration::reviewed(
                &CompleteAttachmentMutationLifecycle(()),
                self.inner.stopped.clone(),
                self.inner.closing.clone(),
            )
        } else {
            ReviewedMutationLifecycleIntegration::unavailable()
        }
    }

    /// Obtains transport and lifecycle evidence from the same installed owner.
    /// Neither evidence contains the owner or an executable transport handle.
    #[allow(dead_code)]
    pub(crate) fn reviewed_title_update_evidence(
        &self,
        account_id: &str,
    ) -> Result<
        (
            Arc<AccountSourceRegistry>,
            ReviewedMutationTransportRegistration,
            ReviewedMutationLifecycleIntegration,
        ),
        (),
    > {
        let sessions = self.reviewed_session_owner().ok_or(())?;
        let registration = sessions.reviewed_title_registration(account_id)?;
        Ok((
            self.inner.registry.clone(),
            registration,
            ReviewedMutationLifecycleIntegration::reviewed(
                &CompleteAttachmentMutationLifecycle(()),
                self.inner.stopped.clone(),
                self.inner.closing.clone(),
            ),
        ))
    }

    pub(crate) fn creation_owner_evidence(
        &self,
        account: &str,
    ) -> Result<
        (
            Arc<AccountSourceRegistry>,
            crate::application::production_creation_owner::CreationSessionEvidence,
            crate::application::production_creation_owner::CreationLifecycleEvidence,
        ),
        (),
    > {
        let sessions = self.reviewed_session_owner().ok_or(())?;
        let session = sessions.creation_session_evidence(account)?;
        if self.inner.stopped.load(Ordering::SeqCst)
            || !session.matches(&self.inner.registry, account)
        {
            return Err(());
        }
        let lifecycle =
            crate::application::production_creation_owner::CreationLifecycleEvidence::from_runtime(
                &CompleteAttachmentMutationLifecycle(()),
                self.inner.stopped.clone(),
                self.inner.closing.clone(),
                session.clone(),
            );
        Ok((self.inner.registry.clone(), session, lifecycle))
    }
    fn reviewed_session_owner(&self) -> Option<&Arc<dyn AttachmentMutationSessions>> {
        (!self.inner.stopped.load(Ordering::SeqCst) && !self.inner.closing.load(Ordering::SeqCst))
            .then_some(self.inner.mutation_sessions.as_ref())
            .flatten()
    }

    pub(crate) fn creation_owner_evidence_under_gate(
        &self,
        account: &str,
        gate: &ClosedCompositionGate<'_>,
    ) -> Result<
        (
            Arc<AccountSourceRegistry>,
            crate::application::production_creation_owner::CreationSessionEvidence,
            crate::application::production_creation_owner::CreationLifecycleEvidence,
        ),
        (),
    > {
        if !gate.matches_runtime(self) || self.inner.stopped.load(Ordering::SeqCst) {
            return Err(());
        }
        let sessions = self.inner.mutation_sessions.as_ref().ok_or(())?;
        let session = sessions.creation_session_evidence(account)?;
        if !session.matches(&self.inner.registry, account) {
            return Err(());
        }
        let lifecycle =
            crate::application::production_creation_owner::CreationLifecycleEvidence::from_runtime(
                &CompleteAttachmentMutationLifecycle(()),
                self.inner.stopped.clone(),
                self.inner.closing.clone(),
                session.clone(),
            );
        Ok((self.inner.registry.clone(), session, lifecycle))
    }

    pub(crate) fn conflict_owner_under_gate(
        &self,
        gate: &ClosedCompositionGate<'_>,
        account: &str,
        registry: &std::sync::Weak<()>,
        actor: &std::sync::Weak<()>,
    ) -> Result<ConflictAttachmentOwner, ()> {
        if !gate.matches_runtime(self)
            || self.inner.requests.is_poisoned()
            || self.inner.worker_gates.is_poisoned()
        {
            return Err(());
        }
        let (_, session, _) = self.creation_owner_evidence_under_gate(account, gate)?;
        let owner = ConflictAttachmentOwner {
            runtime: Arc::downgrade(&self.inner),
            account: account.into(),
            generation: self
                .inner
                .registry
                .attachment_generation(account)
                .ok_or(())?,
            session,
        };
        if !owner.current(registry, actor) {
            return Err(());
        }
        Ok(owner)
    }

    pub(crate) fn reviewed_conflict_evidence(
        &self,
        facade: &crate::application::remote_mutation_facade::RemoteMutationApplication,
        account: &str,
    ) -> Result<crate::application::production_mutation_conflict::ReviewedMutationConflictSurface, ()>
    {
        let requests = self.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: self,
            _requests: &requests,
        };
        facade.scoped_conflict_surface(&gate, account)
    }

    /// Serialize closed graph construction with the existing attachment owner
    /// handoff. This gate confers no mutation submission or transport authority.
    #[allow(dead_code)]
    pub(crate) fn compose_execution_closed(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflicts: &crate::application::remote_mutation_facade::RemoteMutationApplication,
        account: &str,
    ) -> crate::application::production_crud_composition::ClosedCompositionResult {
        self.compose_execution_closed_inner(sessions, surface, conflicts, account, None, || {})
    }

    /// Dependency-complete construction, serialized with owner handoff. Only the
    /// explicitly enabled application activation owner may submit its graph.
    pub(crate) fn compose_execution_closed_with_reminders(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflicts: &crate::application::remote_mutation_facade::RemoteMutationApplication,
        account: &str,
        composer: crate::application::reminder_deadlines::ProductionReminderComposer,
    ) -> crate::application::production_crud_composition::ClosedCompositionResult {
        self.compose_execution_closed_inner(
            sessions,
            surface,
            conflicts,
            account,
            Some(composer),
            || {},
        )
    }

    /// Retire only the activation owner's exact captured lifecycle. The guard drains
    /// publication and scheduling cleanup after releasing the attachment gate.
    pub(crate) fn retire_execution_closed(
        &self,
        graph: &crate::application::production_crud_composition::ExecutionClosedProductionMutationComposition,
    ) -> Result<(), AccountProfileServiceError> {
        let mut requests = self.inner.lock_requests()?;
        let (key, captured) = graph.lifecycle_registration();
        if requests
            .closed_links
            .get(&key)
            .is_some_and(|registered| std::sync::Weak::ptr_eq(registered, &captured))
        {
            self.inner.retire_closed_locked(&key.0, &mut requests)?;
        } else {
            // A same-generation successor may already own the attachment link.
            // Ungated local cleanup never interrupts that successor's transports.
            requests.publications.push(graph.shutdown_deferred());
        }
        Ok(())
    }

    pub(crate) fn prepare_activation_session(&self, account: &str) -> Result<(), ()> {
        let mut requests = self.inner.lock_requests().map_err(|_| ())?;
        self.inner.ensure_running().map_err(|_| ())?;
        let profile = self.inner.registry.profile(account).map_err(|_| ())?;
        if !profile.remote_editing_enabled || !profile.enabled || !profile.credential_configured {
            return Err(());
        }
        let owner = self.reviewed_session_owner().ok_or(())?;
        if owner.creation_session_evidence(account).is_ok() {
            return Ok(());
        }
        self.inner
            .retire_closed_locked(account, &mut requests)
            .map_err(|_| ())?;
        let generation = self
            .inner
            .registry
            .attachment_generation(account)
            .ok_or(())?;
        owner.register(account, generation)?;
        Ok(())
    }

    #[allow(dead_code)] // Internal collection binding; no product dispatch entry.
    pub(crate) fn exact_probe_binding(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
    ) -> Result<crate::adapters::production_mutation::exact_probe::ExactProbeBinding, ()> {
        let requests = self.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: self,
            _requests: &requests,
        };
        sessions.bind_exact_probe_collections(&gate)
    }

    pub(crate) fn connected_owner_current(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        account: &str,
        route: Option<&str>,
    ) -> bool {
        !self.inner.stopped.load(Ordering::SeqCst)
            && !self.inner.closing.load(Ordering::SeqCst)
            && !self.inner.requests.is_poisoned()
            && self
                .inner
                .worker_gates
                .lock()
                .is_ok_and(|gates| gates.get(account).is_none_or(|gate| !gate.is_poisoned()))
            && self.owns_activation_sessions(&self.inner.registry, sessions)
            && sessions.connected_owner_current(account, route)
    }
    /// Use the existing handoff gate through the actor admission round trip.
    /// The callback only checks in-memory owners on the actor thread.
    pub(crate) fn admit_connected_event<T>(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        admit: impl FnOnce(
            crate::adapters::persistence::scoped_event::ConnectedOwnerCheck,
        ) -> Result<T, crate::adapters::persistence::PersistenceError>,
    ) -> Result<T, crate::adapters::persistence::PersistenceError> {
        let _requests = self.inner.lock_requests().map_err(|_| {
            crate::adapters::persistence::PersistenceError::RemoteAccountConnectionRequired
        })?;
        self.inner.ensure_running().map_err(|_| {
            crate::adapters::persistence::PersistenceError::RemoteAccountConnectionRequired
        })?;
        let runtime = self.clone();
        let sessions = Arc::downgrade(sessions);
        admit(Arc::new(move |account, route| {
            sessions.upgrade().is_some_and(|sessions| {
                runtime.connected_owner_current(&sessions, account, Some(route))
            })
        }))
    }
    pub(crate) fn prepare_selected_calendar_owner(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        scope: crate::adapters::persistence::scoped_event::ScopedEventSnapshot,
    ) -> Result<crate::adapters::production_mutation::exact_probe::ExactProbeBinding, ()> {
        let requests = self.inner.lock_requests().map_err(|_| ())?;
        self.inner.ensure_running().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: self,
            _requests: &requests,
        };
        let account = sessions.selected_owner_account(&gate, &scope)?;
        if !self.connected_owner_current(sessions, &account, Some(scope.account_and_route().1)) {
            return Err(());
        }
        sessions.bind_selected_collection(&gate, scope)
    }
    #[allow(dead_code)]
    fn compose_execution_closed_inner(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        surface: &crate::application::remote_creation_facade::RemoteCreationApplication,
        conflicts: &crate::application::remote_mutation_facade::RemoteMutationApplication,
        account: &str,
        reminder: Option<crate::application::reminder_deadlines::ProductionReminderComposer>,
        after_build: impl FnOnce(),
    ) -> crate::application::production_crud_composition::ClosedCompositionResult {
        let mut requests = self.inner.lock_requests().map_err(|_| ())?;
        let gate = ClosedCompositionGate {
            attachments: self,
            _requests: &requests,
        };
        if self.inner.stopped.load(Ordering::SeqCst) || self.inner.closing.load(Ordering::SeqCst) {
            return Err(());
        }
        let composition =
            sessions.compose_execution_closed_under_gate(&gate, surface, conflicts, account)?;
        if let Some(composer) = reminder {
            composition.install_notification_scope(&gate, composer)?;
        }
        after_build();
        // Shutdown closes admission and issued lifecycle evidence before waiting
        // on this gate; reject a graph built while shutdown began.
        if self.inner.stopped.load(Ordering::SeqCst)
            || self.inner.closing.load(Ordering::SeqCst)
            || !composition.view().owner_current
        {
            requests.publications.push(composition.shutdown_deferred());
            return Err(());
        }
        let registration = composition.lifecycle_registration();
        requests
            .closed_links
            .retain(|_, link| link.strong_count() > 0);
        if requests.closed_links.keys().any(|(id, _)| id == account) {
            requests.publications.push(composition.shutdown_deferred());
            return Err(());
        }
        requests.closed_links.insert(registration.0, registration.1);
        Ok(composition)
    }

    pub fn submit(
        &self,
        account_id: &str,
        mode: SourceAttachmentMode,
        startup_sync: bool,
    ) -> Result<AttachmentView, AccountProfileServiceError> {
        self.submit_with_spawner(account_id, mode, startup_sync, |worker| {
            thread::Builder::new()
                .name("calendar-todo-attachment".into())
                .spawn(worker)
                .map(|_| ())
                .map_err(|_| ())
        })
    }

    /// Caller must first validate semantic input and actor-select the exact
    /// cached target. Run from a background continuation; never from an IPC or
    /// actor thread. Startup preference does not gate this confirmed action.
    pub(crate) fn ensure_confirmed_connection(
        &self,
        sessions: &Arc<crate::adapters::production_mutation::ProductionMutationSessions>,
        account_id: &str,
        route: &str,
    ) -> Result<ConfirmedConnection, AccountProfileServiceError> {
        if !self.owns_activation_sessions(&self.inner.registry, sessions) {
            return Err(AccountProfileServiceError::Unavailable);
        }
        let current = || self.connected_owner_current(sessions, account_id, Some(route));
        let (_, completion) = self.submit_connection_with_spawner(
            account_id,
            SourceAttachmentMode::IcloudChinaReadOnly,
            false,
            Some(&current),
            |worker| {
                thread::Builder::new()
                    .name("calendar-todo-confirmed-connection".into())
                    .spawn(worker)
                    .map(|_| ())
                    .map_err(|_| ())
            },
        )?;
        let connection = ConfirmedConnection {
            runtime: self.clone(),
            sessions: Arc::downgrade(sessions),
            account: account_id.into(),
            route: route.into(),
            generation: completion.wait()?,
            session: sessions
                .creation_session_evidence(account_id)
                .map_err(|_| AccountProfileServiceError::Unavailable)?,
        };
        if !connection.current() {
            return Err(AccountProfileServiceError::Unavailable);
        }
        Ok(connection)
    }

    fn submit_with_spawner(
        &self,
        account_id: &str,
        mode: SourceAttachmentMode,
        startup_sync: bool,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), ()>,
    ) -> Result<AttachmentView, AccountProfileServiceError> {
        self.submit_connection_with_spawner(account_id, mode, startup_sync, None, spawn)
            .map(|value| value.0)
    }

    fn submit_connection_with_spawner(
        &self,
        account_id: &str,
        mode: SourceAttachmentMode,
        startup_sync: bool,
        ensure_current: Option<&dyn Fn() -> bool>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), ()>,
    ) -> Result<(AttachmentView, Arc<ConnectionCompletion>), AccountProfileServiceError> {
        let mut requests = self.inner.lock_requests()?;
        self.inner.ensure_running()?;
        if self.inner.stopped.load(Ordering::SeqCst) || self.inner.closing.load(Ordering::SeqCst) {
            return Err(AccountProfileServiceError::Unavailable);
        }
        self.inner.accounts.validate_attachment(account_id)?;
        if let Some(current) = ensure_current {
            if current() {
                let completion = Arc::new(ConnectionCompletion::default());
                completion.finish(
                    self.inner
                        .registry
                        .attachment_generation(account_id)
                        .ok_or(AccountProfileServiceError::Unavailable),
                );
                return Ok((
                    AttachmentView {
                        account_id: account_id.into(),
                        source_mode: Some(SourceAttachmentMode::IcloudChinaReadOnly),
                        state: AttachmentState::Ready,
                        failure: None,
                    },
                    completion,
                ));
            }
            if let Some(request) = requests.get(account_id) {
                if request.production
                    && request.view.state == AttachmentState::Attaching
                    && !request.cancellation.is_cancelled()
                {
                    return Ok((request.view.clone(), request.completion.clone()));
                }
            }
            // A mismatching live source/session is not authority to replace it.
            if self
                .inner
                .registry
                .attachment_generation(account_id)
                .is_some()
            {
                return Err(AccountProfileServiceError::Unavailable);
            }
        }
        self.inner.retire_closed_locked(account_id, &mut requests)?;
        if let Some(sessions) = &self.inner.mutation_sessions {
            sessions.revoke(account_id);
        }
        if let Some(previous) = requests.remove(account_id) {
            previous.cancellation.cancel();
        }
        let cancellation = AttachmentCancellation::new();
        let completion = Arc::new(ConnectionCompletion::default());
        let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
        let view = AttachmentView {
            account_id: account_id.to_owned(),
            source_mode: Some(mode),
            state: AttachmentState::Attaching,
            failure: None,
        };
        requests.insert(
            account_id.to_owned(),
            Request {
                generation,
                cancellation: cancellation.clone(),
                startup_sync,
                production: mode == SourceAttachmentMode::IcloudChinaReadOnly,
                view: view.clone(),
                completion: completion.clone(),
            },
        );
        let gate = match self.inner.worker_gates.lock() {
            Ok(mut gates) => gates
                .entry(account_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone(),
            Err(_) => {
                self.inner
                    .remove_matching_request_locked(account_id, generation, &mut requests);
                return Err(AccountProfileServiceError::Unavailable);
            }
        };
        let inner = self.inner.clone();
        let id = account_id.to_owned();
        if spawn(Box::new(move || {
            let publications = Arc::new(Mutex::new(Vec::new()));
            let _publish_after_worker_gate = WorkerPublications(publications.clone());
            let _guard = match gate.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    inner.complete_with_sink(
                        &id,
                        generation,
                        Err(AccountProfileServiceError::Unavailable),
                        Some(publications.clone()),
                    );
                    return;
                }
            };
            if !inner.prepare_with_sink(&id, generation, Some(publications.clone())) {
                return;
            }
            let result = if mode == SourceAttachmentMode::IcloudChinaReadOnly {
                inner
                    .discovery
                    .discover_cancellable(&id, &cancellation)
                    .map_err(map_discovery_error)
                    .and_then(|_| {
                        inner
                            .accounts
                            .attach_source_cancellable(&id, mode, &cancellation)
                    })
            } else {
                inner
                    .accounts
                    .attach_source_cancellable(&id, mode, &cancellation)
            };
            inner.complete_with_sink(&id, generation, result, Some(publications));
        }))
        .is_err()
        {
            self.inner
                .remove_matching_request_locked(account_id, generation, &mut requests);
            return Err(AccountProfileServiceError::Unavailable);
        }
        drop(requests);
        self.inner.emitter.emit(view.clone());
        Ok((view, completion))
    }

    /// Reconnect every eligible production account after process startup.
    ///
    /// Profile listing itself is local and non-secret. Each submitted account
    /// then enters the same cancellable discovery -> snapshot -> attachment ->
    /// Startup-sync path as the explicit Connect action.
    pub fn submit_startup_accounts(
        &self,
    ) -> Result<Vec<AttachmentView>, AccountProfileServiceError> {
        self.inner
            .accounts
            .list()?
            .into_iter()
            .filter(|account| {
                account.enabled
                    && account.credential_configured
                    && account.provider_kind == "caldav"
                    && account.region_profile == "icloud-cn"
            })
            .map(|account| {
                self.submit(
                    &account.account_id,
                    SourceAttachmentMode::IcloudChinaReadOnly,
                    true,
                )
            })
            .collect()
    }

    pub fn set_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<RuntimeAccountView, AccountProfileServiceError> {
        let mut requests = self.inner.lock_requests()?;
        self.inner.ensure_running()?;
        if !enabled {
            self.inner.cancel_locked(account_id, &mut requests)?;
        }
        let account = self.inner.accounts.set_enabled(account_id, enabled)?;
        drop(requests);
        if !enabled {
            self.inner.emitter.emit(AttachmentView {
                account_id: account_id.to_owned(),
                source_mode: None,
                state: AttachmentState::Detached,
                failure: None,
            });
        }
        Ok(account)
    }

    pub fn detach(&self, account_id: &str) -> Result<(), AccountProfileServiceError> {
        let mut requests = self.inner.lock_requests()?;
        self.inner.ensure_running()?;
        self.inner.cancel_locked(account_id, &mut requests)?;
        drop(requests);
        self.inner.emitter.emit(AttachmentView {
            account_id: account_id.to_owned(),
            source_mode: None,
            state: AttachmentState::Detached,
            failure: None,
        });
        Ok(())
    }

    /// Return every in-process source to the detached local-only state.
    pub fn detach_all(&self) -> Result<(), AccountProfileServiceError> {
        let ids: Vec<String> = self
            .inner
            .accounts
            .list()?
            .into_iter()
            .map(|a| a.account_id)
            .collect();
        let mut requests = self.inner.lock_requests()?;
        self.inner.ensure_running()?;
        let mut views = Vec::with_capacity(ids.len());
        for id in &ids {
            self.inner.retire_closed_locked(id, &mut requests)?;
        }
        for id in ids {
            self.inner.cancel_locked(&id, &mut requests)?;
            views.push(AttachmentView {
                account_id: id,
                source_mode: None,
                state: AttachmentState::Detached,
                failure: None,
            });
        }
        drop(requests);
        for view in views {
            self.inner.emitter.emit(view);
        }
        Ok(())
    }

    pub fn remove_account(&self, account_id: &str) -> Result<(), AccountProfileServiceError> {
        let mut requests = self.inner.lock_requests()?;
        self.inner.ensure_running()?;
        self.inner.cancel_locked(account_id, &mut requests)?;
        self.inner.accounts.remove(account_id)?;
        drop(requests);
        self.inner.emitter.emit(AttachmentView {
            account_id: account_id.to_owned(),
            source_mode: None,
            state: AttachmentState::Detached,
            failure: None,
        });
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<AttachmentView>, AccountProfileServiceError> {
        let requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| AccountProfileServiceError::Unavailable)?;
        Ok(self
            .inner
            .accounts
            .list()?
            .into_iter()
            .map(|account| {
                requests.get(&account.account_id).map_or_else(
                    || AttachmentView {
                        account_id: account.account_id,
                        source_mode: None,
                        state: if account.source_attached {
                            AttachmentState::Ready
                        } else {
                            AttachmentState::Detached
                        },
                        failure: None,
                    },
                    |request| request.view.clone(),
                )
            })
            .collect())
    }

    pub fn shutdown(&self) {
        let _ = self.try_shutdown_closed();
    }

    /// The internal caller can retry only shutdown after retirement rejects.
    /// Closing blocks new work immediately without revoking the retained owner.
    pub(crate) fn try_shutdown_closed(&self) -> Result<(), AccountProfileServiceError> {
        self.inner.closing.store(true, Ordering::SeqCst);
        let mut requests = self.inner.lock_requests()?;
        if self.inner.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let ids: Vec<_> = requests
            .closed_links
            .keys()
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.inner.retire_closed_locked(&id, &mut requests)?;
        }
        if let Some(sessions) = &self.inner.mutation_sessions {
            sessions.shutdown();
        }
        self.inner.stopped.store(true, Ordering::SeqCst);
        for request in requests.values() {
            request.cancellation.cancel();
        }
        requests.clear();
        Ok(())
    }
}

fn map_discovery_error(error: DiscoveryError) -> AccountProfileServiceError {
    match error {
        DiscoveryError::CredentialActionRequired => {
            AccountProfileServiceError::CredentialActionRequired
        }
        DiscoveryError::Disabled => AccountProfileServiceError::Disabled,
        DiscoveryError::InvalidIdentity
        | DiscoveryError::UnsupportedProvider
        | DiscoveryError::UnsafeConfiguration => AccountProfileServiceError::Validation,
        DiscoveryError::Superseded | DiscoveryError::Unavailable => {
            AccountProfileServiceError::Unavailable
        }
    }
}

impl Inner {
    fn ensure_running(&self) -> Result<(), AccountProfileServiceError> {
        if self.closing.load(Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst) {
            Err(AccountProfileServiceError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn lock_requests(&self) -> Result<AttachmentRequestGuard<'_>, AccountProfileServiceError> {
        Ok(AttachmentRequestGuard {
            guard: Some(
                self.requests
                    .lock()
                    .map_err(|_| AccountProfileServiceError::Unavailable)?,
            ),
            publications: Vec::new(),
            publication_sink: None,
        })
    }

    fn retire_closed_locked(
        self: &Arc<Self>,
        account: &str,
        requests: &mut AttachmentRequestGuard<'_>,
    ) -> Result<(), AccountProfileServiceError> {
        let links: Vec<_> = requests
            .closed_links
            .iter()
            .filter(|((id, _), _)| id == account)
            .map(|(key, link)| (key.clone(), link.upgrade()))
            .collect();
        let runtime = AttachmentRuntime {
            inner: self.clone(),
        };
        for (key, link) in links {
            let Some(link) = link else {
                requests.closed_links.remove(&key);
                continue;
            };
            if !link.matches_registration(&key.0, key.1) {
                return Err(AccountProfileServiceError::Unavailable);
            }
            requests.publications.push(link.clone());
            let gate = ClosedCompositionGate {
                attachments: &runtime,
                _requests: requests,
            };
            link.retire_before_handoff(&gate)
                .map_err(|_| AccountProfileServiceError::Unavailable)?;
            requests.closed_links.remove(&key);
        }
        Ok(())
    }
    fn remove_matching_request_locked(
        &self,
        account_id: &str,
        generation: u64,
        requests: &mut AttachmentRequestGuard<'_>,
    ) {
        if requests
            .get(account_id)
            .is_some_and(|request| request.generation == generation)
            && let Some(request) = requests.remove(account_id)
        {
            request.cancellation.cancel();
        }
    }

    fn prepare_with_sink(
        self: &Arc<Self>,
        account_id: &str,
        generation: u64,
        sink: Option<ClosedPublicationSink>,
    ) -> bool {
        let Ok(mut requests) = self.lock_requests() else {
            return false;
        };
        requests.publication_sink = sink.clone();
        let current = requests.get(account_id).is_some_and(|request| {
            request.generation == generation && !request.cancellation.is_cancelled()
        });
        if self.stopped.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) || !current {
            return false;
        }
        if self
            .retire_closed_locked(account_id, &mut requests)
            .is_err()
        {
            return false;
        }
        if let Some(sessions) = &self.mutation_sessions {
            sessions.revoke(account_id);
        }
        if self.registry.detach_source(account_id).is_err() {
            drop(requests);
            self.complete_with_sink(
                account_id,
                generation,
                Err(AccountProfileServiceError::Unavailable),
                sink,
            );
            return false;
        }
        self.sync.account_detached(account_id);
        true
    }

    fn cancel_locked(
        self: &Arc<Self>,
        account_id: &str,
        requests: &mut AttachmentRequestGuard<'_>,
    ) -> Result<(), AccountProfileServiceError> {
        self.retire_closed_locked(account_id, requests)?;
        if let Some(sessions) = &self.mutation_sessions {
            sessions.revoke(account_id);
        }
        if let Some(previous) = requests.remove(account_id) {
            previous.cancellation.cancel();
        }
        self.registry.detach_source(account_id)?;
        self.sync.account_detached(account_id);
        Ok(())
    }

    fn complete_with_sink(
        self: &Arc<Self>,
        account_id: &str,
        generation: u64,
        result: Result<RuntimeAccountView, AccountProfileServiceError>,
        sink: Option<ClosedPublicationSink>,
    ) {
        let Ok(mut requests) = self.lock_requests() else {
            return;
        };
        requests.publication_sink = sink;
        let Some(request) = requests.get(account_id) else {
            return;
        };
        if request.generation != generation
            || self.stopped.load(Ordering::SeqCst)
            || self.closing.load(Ordering::SeqCst)
        {
            return;
        }
        if self
            .retire_closed_locked(account_id, &mut requests)
            .is_err()
        {
            return;
        }
        let request = requests.get_mut(account_id).unwrap();
        let startup_sync = request.startup_sync;
        let result = result.and_then(|account| {
            if request.production
                && let Some(sessions) = &self.mutation_sessions
            {
                let attached = self
                    .registry
                    .attachment_generation(account_id)
                    .ok_or(AccountProfileServiceError::Unavailable)?;
                sessions
                    .register(account_id, attached)
                    .map_err(|_| AccountProfileServiceError::Unavailable)?;
            }
            Ok(account)
        });
        let source_mode = Some(if request.production {
            SourceAttachmentMode::IcloudChinaReadOnly
        } else {
            SourceAttachmentMode::Synthetic
        });
        let view = match result {
            Ok(_) if self.registry.attachment_generation(account_id).is_some() => {
                // An accepted outcome is still "ready" when the stored
                // preference defers the first sync to the periodic deadline;
                // only a rejected registration or scheduling step fails.
                let scheduled = if startup_sync {
                    self.sync.trigger_startup(account_id).map(|_| ())
                } else {
                    self.sync.account_attached(account_id)
                };
                match scheduled {
                    Ok(_) => AttachmentView {
                        account_id: account_id.to_owned(),
                        source_mode,
                        state: AttachmentState::Ready,
                        failure: None,
                    },
                    Err(_) => AttachmentView {
                        account_id: account_id.to_owned(),
                        source_mode,
                        state: AttachmentState::Failed,
                        failure: Some(AttachmentFailure::Unavailable),
                    },
                }
            }
            Ok(_) => AttachmentView {
                account_id: account_id.to_owned(),
                source_mode,
                state: AttachmentState::Failed,
                failure: Some(AttachmentFailure::Unavailable),
            },
            Err(error) => {
                let failure = match error {
                    AccountProfileServiceError::CredentialSetupRequired
                    | AccountProfileServiceError::CredentialActionRequired => {
                        AttachmentFailure::Credential
                    }
                    AccountProfileServiceError::Disabled => AttachmentFailure::Disabled,
                    AccountProfileServiceError::Validation => {
                        AttachmentFailure::UnsafeConfiguration
                    }
                    AccountProfileServiceError::Unavailable => AttachmentFailure::Unavailable,
                };
                AttachmentView {
                    account_id: account_id.to_owned(),
                    source_mode,
                    state: if failure == AttachmentFailure::Credential {
                        AttachmentState::ActionRequired
                    } else {
                        AttachmentState::Failed
                    },
                    failure: Some(failure),
                }
            }
        };
        if view.state != AttachmentState::Ready
            && let Some(sessions) = &self.mutation_sessions
        {
            sessions.revoke(account_id);
        }
        request.view = view.clone();
        request
            .completion
            .finish(if view.state == AttachmentState::Ready {
                self.registry
                    .attachment_generation(account_id)
                    .ok_or(AccountProfileServiceError::Unavailable)
            } else {
                Err(AccountProfileServiceError::Unavailable)
            });
        drop(requests);
        self.emitter.emit(view);
    }
}
