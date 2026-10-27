//! Concrete notification owner and the single installed one-shot backend.
//! Production graph scopes consume accepted tickets; ordinary graph installation stays closed.
use super::reminders::{ProductionReminderScheduleCapability, ReminderError};
use crate::{adapters::persistence::Database, ports::clock::Clock};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use uuid::Uuid;

enum Command {
    Changed,
    Shutdown,
}

// The future production scope requires a separately reviewed closed graph mint.
// Only tests can currently construct one, and it is never exported from here.
struct Scope {
    account: String,
    attachment_generation: u64,
    identity: Uuid,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
}
struct Deadline {
    scope: Arc<Scope>,
    schedule_generation: u64,
    at_ms: i64,
}
#[derive(Default)]
struct State {
    stopped: bool,
    next_generation: u64,
    keys: HashMap<String, Deadline>,
    scopes: HashMap<String, Weak<Scope>>,
    global: Option<(u64, i64)>,
    global_recompute: u64,
    graph_scopes: HashMap<String, Weak<ProductionGraphReminderScope>>,
}

pub(super) struct DeadlineInner {
    capability: ProductionReminderScheduleCapability,
    database: Database,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    sender: mpsc::Sender<Command>,
    failed: AtomicBool,
}
impl DeadlineInner {
    /// Only terminal cleanup; never operates on a successor's independent owner.
    pub(super) fn stop(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.stopped = true;
        state.keys.clear();
        state.global = None;
        state.scopes.clear();
        state.graph_scopes.clear();
        drop(state);
        let _ = self.sender.send(Command::Shutdown);
    }

    // The installed runtime is the sole production producer. Global scope has
    // no attachment callback and cannot acquire mutation/attachment gates.
    pub(super) fn global_scope(self: &Arc<Self>) -> Result<ProductionReminderScope, ReminderError> {
        if !self.capability.live()
            || self
                .state
                .lock()
                .map_err(|_| ReminderError::Unavailable)?
                .stopped
        {
            return Err(ReminderError::Unavailable);
        }
        Ok(ProductionReminderScope {
            owner: Arc::downgrade(self),
        })
    }

    fn rearm_global(&self) -> Result<(), ReminderError> {
        self.rearm_global_guarded(None)
    }

    fn rearm_global_guarded(
        &self,
        scope: Option<(&ProductionGraphReminderScope, &dyn Fn() -> bool)>,
    ) -> Result<(), ReminderError> {
        let result = (|| {
            if scope.is_some_and(|(s, current)| !s.current() || !current()) {
                return Err(ReminderError::Unavailable);
            }
            let mut state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
            if state.stopped
                || !self.capability.live()
                || scope.is_some_and(|(s, _)| {
                    !s.fast_current()
                        || !state
                            .graph_scopes
                            .get(&s.account)
                            .and_then(Weak::upgrade)
                            .is_some_and(|registered| std::ptr::eq(registered.as_ref(), s))
                })
            {
                return Err(ReminderError::Unavailable);
            }
            state.global_recompute = state
                .global_recompute
                .checked_add(1)
                .ok_or(ReminderError::Unavailable)?;
            let recompute = state.global_recompute;
            drop(state);
            let next = self
                .database
                .next_notification_deadline()
                .map_err(|_| ReminderError::Unavailable)?;
            // Full request/session checks may acquire attachment gates. They
            // must run after the actor query and outside the backend mutex.
            if scope.is_some_and(|(s, current)| !s.current() || !current()) {
                return Err(ReminderError::Unavailable);
            }
            let mut state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
            if state.stopped
                || !self.capability.live()
                || scope.is_some_and(|(s, _)| {
                    !s.fast_current()
                        || !state
                            .graph_scopes
                            .get(&s.account)
                            .and_then(Weak::upgrade)
                            .is_some_and(|registered| std::ptr::eq(registered.as_ref(), s))
                })
            {
                return Err(ReminderError::Unavailable);
            }
            if state.global_recompute != recompute {
                // A newer recompute owns the result; overlap coalesces without
                // restoring this stale snapshot or reporting a healthy backend
                // as unavailable.
                return self
                    .sender
                    .send(Command::Changed)
                    .map_err(|_| ReminderError::Unavailable);
            }
            if state.global.map(|(_, at)| at) != next {
                state.next_generation = state
                    .next_generation
                    .checked_add(1)
                    .ok_or(ReminderError::Unavailable)?;
                state.global = next.map(|at| (state.next_generation, at));
            }
            self.sender
                .send(Command::Changed)
                .map_err(|_| ReminderError::Unavailable)
        })();
        self.failed.store(result.is_err(), Ordering::SeqCst);
        result
    }

    fn fire_global(&self, generation: u64, now: i64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.stopped
            || !state
                .global
                .is_some_and(|(g, at)| g == generation && at <= now)
        {
            return false;
        }
        if !self.capability.live() {
            state.global = None;
            self.failed.store(true, Ordering::SeqCst);
            return false;
        }
        state.global = None;
        // Invalidate snapshots taken before this one-shot was consumed.
        state.global_recompute = match state.global_recompute.checked_add(1) {
            Some(next) => next,
            None => {
                state.stopped = true;
                self.failed.store(true, Ordering::SeqCst);
                return false;
            }
        };
        let result = self.capability.wake();
        self.failed.store(result.is_err(), Ordering::SeqCst);
        result.is_ok()
    }

    pub(super) fn refresh_registered(&self) -> Result<(), ReminderError> {
        let scopes = {
            let state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
            if state.stopped || !self.capability.live() {
                return Err(ReminderError::Unavailable);
            }
            state
                .scopes
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for scope in scopes {
            self.rearm(&scope)?;
        }
        let result = self
            .sender
            .send(Command::Changed)
            .map_err(|_| ReminderError::Unavailable);
        self.failed.store(result.is_err(), Ordering::SeqCst);
        result
    }

    fn rearm(&self, scope: &Arc<Scope>) -> Result<(), ReminderError> {
        let result = (|| {
            let mut state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
            if state.stopped
                || !self.capability.live()
                || !(scope.current)()
                || !state
                    .scopes
                    .get(&scope.account)
                    .and_then(Weak::upgrade)
                    .is_some_and(|registered| Arc::ptr_eq(&registered, scope))
            {
                return Err(ReminderError::Unavailable);
            }
            // Serializing the read under this local scheduling gate coalesces
            // overlap and prevents an older queued query replacing a newer plan.
            let next = self
                .database
                .next_account_notification_deadline(scope.account.clone())
                .map_err(|_| ReminderError::Unavailable)?;
            if state.stopped
                || !self.capability.live()
                || !(scope.current)()
                || !state
                    .scopes
                    .get(&scope.account)
                    .and_then(Weak::upgrade)
                    .is_some_and(|registered| Arc::ptr_eq(&registered, scope))
            {
                return Err(ReminderError::Unavailable);
            }
            let unchanged = state.keys.get(&scope.account).is_some_and(|old| {
                old.scope.identity == scope.identity
                    && old.scope.attachment_generation == scope.attachment_generation
                    && Some(old.at_ms) == next
            });
            if unchanged || (next.is_none() && !state.keys.contains_key(&scope.account)) {
                // A coalesced plan still needs a live backend. Do not report
                // recovery merely because the retained key has not changed.
                return self
                    .sender
                    .send(Command::Changed)
                    .map_err(|_| ReminderError::Unavailable);
            }
            if let Some(at_ms) = next {
                if state.keys.len() >= 128 && !state.keys.contains_key(&scope.account) {
                    return Err(ReminderError::Unavailable);
                }
                state.next_generation = state
                    .next_generation
                    .checked_add(1)
                    .ok_or(ReminderError::Unavailable)?;
                let schedule_generation = state.next_generation;
                state.keys.insert(
                    scope.account.clone(),
                    Deadline {
                        scope: scope.clone(),
                        schedule_generation,
                        at_ms,
                    },
                );
            } else {
                // Only a still-current scope may remove this account's key.
                state.keys.remove(&scope.account);
            }
            self.sender
                .send(Command::Changed)
                .map_err(|_| ReminderError::Unavailable)
        })();
        if let Ok(state) = self.state.lock()
            && !state.stopped
            && self.capability.live()
            && (scope.current)()
            && state
                .scopes
                .get(&scope.account)
                .and_then(Weak::upgrade)
                .is_some_and(|registered| Arc::ptr_eq(&registered, scope))
        {
            self.failed.store(result.is_err(), Ordering::SeqCst);
        }
        result
    }

    fn cancel(&self, scope: &Scope) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state
            .scopes
            .get(&scope.account)
            .and_then(Weak::upgrade)
            .is_some_and(|registered| registered.identity == scope.identity)
        {
            state.scopes.remove(&scope.account);
        }
        if state.keys.get(&scope.account).is_some_and(|deadline| {
            deadline.scope.identity == scope.identity
                && deadline.scope.attachment_generation == scope.attachment_generation
        }) {
            state.keys.remove(&scope.account);
            let _ = self.sender.send(Command::Changed);
        }
    }

    /// The timer knows only opaque key/generation/instant. It never obtains or
    /// claims notification content. Sending Reconcile merely wakes the original
    /// reminder worker, whose existing actor claim/delivery boundaries remain.
    fn fire(&self, account: &str, generation: u64, now: i64) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.stopped {
            return false;
        }
        let Some(deadline) = state.keys.get(account) else {
            return false;
        };
        if deadline.schedule_generation != generation || deadline.at_ms > now {
            return false;
        }
        let registered = state
            .scopes
            .get(account)
            .and_then(Weak::upgrade)
            .is_some_and(|scope| Arc::ptr_eq(&scope, &deadline.scope));
        if !registered || !self.capability.live() || !(deadline.scope.current)() {
            let scope = deadline.scope.clone();
            drop(state);
            self.cancel(&scope);
            return false;
        }
        // This gate serializes replacement/cancellation with the final bounded
        // channel send. Neither an emitter nor delivery callback runs here.
        let result = self.capability.wake();
        state.keys.remove(account);
        self.failed.store(result.is_err(), Ordering::SeqCst);
        result.is_ok()
    }
}

/// Consuming notification lifecycle composer; only the real ReminderRuntime
/// can obtain it. Weak ownership never keeps the notification lifecycle alive.
pub(crate) struct ProductionReminderComposer {
    owner: Weak<DeadlineInner>,
}
impl ProductionReminderComposer {
    pub(crate) fn mint(
        self,
        gate: &super::attachment_runtime::ClosedCompositionGate<'_>,
        accounts: &super::account_registry::AccountSourceRegistry,
        account: &str,
        generation: u64,
        lifecycle: &Arc<super::remote_mutation_runtime::ClosedMutationLifecycle>,
        services: &Arc<crate::adapters::production_mutation::VersionedMutationServices>,
    ) -> Result<Arc<ProductionGraphReminderScope>, ()> {
        let owner = self.owner.upgrade().ok_or(())?;
        if !owner.capability.live()
            || !gate.notification_liveness().fast_live()
            || !lifecycle.scheduling_alive()
            || !Weak::ptr_eq(
                &owner.database.actor_provenance(),
                &accounts.creation_owner_provenance().1,
            )
            || !owner.database.matches_clock(&owner.clock)
            || !services.current_under_gate(gate)
            || !lifecycle.matches_registration(account, generation)
        {
            return Err(());
        }
        // No backend lock, query, registration or timer arm under the attachment gate.
        Ok(Arc::new(ProductionGraphReminderScope {
            owner: self.owner,
            attachment: gate.notification_liveness(),
            account: account.into(),
            generation,
            lifecycle: Arc::downgrade(lifecycle),
            services: Arc::downgrade(services),
            retired: AtomicBool::new(false),
            registered: AtomicBool::new(false),
        }))
    }
}

pub(crate) struct ProductionGraphReminderScope {
    owner: Weak<DeadlineInner>,
    attachment: super::attachment_runtime::NotificationAttachmentLiveness,
    account: String,
    generation: u64,
    lifecycle: Weak<super::remote_mutation_runtime::ClosedMutationLifecycle>,
    services: Weak<crate::adapters::production_mutation::VersionedMutationServices>,
    retired: AtomicBool,
    registered: AtomicBool,
}
impl ProductionGraphReminderScope {
    fn fast_current(&self) -> bool {
        !self.retired.load(Ordering::SeqCst)
            && self.attachment.fast_live()
            && self
                .lifecycle
                .upgrade()
                .is_some_and(|l| l.scheduling_alive())
    }
    pub(crate) fn notification_live(&self) -> bool {
        self.owner.upgrade().is_some_and(|o| o.capability.live())
    }
    fn current(&self) -> bool {
        self.fast_current()
            && self.lifecycle.upgrade().is_some_and(|l| l.is_live())
            && self.services.upgrade().is_some_and(|s| s.current())
            && self.owner.upgrade().is_some_and(|o| o.capability.live())
    }
    // Called only outside attachment/mutation/request gates. No scope callback
    // runs while the backend scheduling mutex is held.
    fn apply(&self, account: &str, generation: u64, current: &dyn Fn() -> bool) -> Result<(), ()> {
        if account != self.account || generation != self.generation || !self.current() || !current()
        {
            return Err(());
        }
        let owner = self.owner.upgrade().ok_or(())?;
        let scope = owner
            .state
            .lock()
            .map_err(|_| ())?
            .graph_scopes
            .get(account)
            .and_then(Weak::upgrade);
        if scope
            .as_ref()
            .is_some_and(|s| !std::ptr::eq(s.as_ref(), self) && s.fast_current())
        {
            return Err(());
        }
        // The binding retains this concrete scope, so the exact weak identity
        // can be obtained without exposing a public Arc/trait constructor.
        let own = self
            .lifecycle
            .upgrade()
            .ok_or(())?
            .schedule_scope()
            .ok_or(())?;
        if !std::ptr::eq(own.as_ref(), self) {
            return Err(());
        }
        {
            let mut state = owner.state.lock().map_err(|_| ())?;
            if state.stopped || !self.fast_current() || !owner.capability.live() {
                return Err(());
            }
            if state
                .graph_scopes
                .get(account)
                .and_then(Weak::upgrade)
                .is_some_and(|s| !std::ptr::eq(s.as_ref(), self) && s.fast_current())
            {
                return Err(());
            }
            if state.graph_scopes.len() >= 128 && !state.graph_scopes.contains_key(account) {
                return Err(());
            }
            state
                .graph_scopes
                .insert(account.into(), Arc::downgrade(&own));
            self.registered.store(true, Ordering::SeqCst);
        }
        if !self.current() || !current() {
            return Err(());
        }
        let result = owner.rearm_global_guarded(Some((self, current)));
        if !self.current() || !current() {
            return Err(());
        }
        result.map_err(|_| ())
    }
    fn retire_scope(&self) {
        self.retired.store(true, Ordering::SeqCst);
        if !self.registered.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(owner) = self.owner.upgrade()
            && let Ok(mut state) = owner.state.lock()
            && state
                .graph_scopes
                .get(&self.account)
                .and_then(Weak::upgrade)
                .is_some_and(|s| std::ptr::eq(s.as_ref(), self))
        {
            state.graph_scopes.remove(&self.account);
        }
        // Retiring a graph must never cancel the shared global key or successor.
    }
}
impl super::remote_mutation_runtime::ClosedEventSchedulePort for ProductionGraphReminderScope {
    fn rearm(&self, _: &str, _: u64, _: &dyn Fn() -> bool) -> Result<(), ()> {
        Err(())
    }
    fn rearm_accepted(
        &self,
        ticket: &super::remote_mutation_runtime::AcceptedEventScheduleTicket<'_>,
    ) -> Result<(), ()> {
        let lifecycle = self.lifecycle.upgrade().ok_or(())?;
        if !ticket.current_for(&lifecycle) {
            return Err(());
        }
        self.apply(ticket.account(), ticket.generation(), &|| {
            ticket.current_for(&lifecycle)
        })
    }
    fn notification_current(&self) -> bool {
        self.fast_current() && self.notification_live()
    }
    fn retire(&self) {
        self.retire_scope();
    }
}

// Opaque, private-minted same-runtime global scheduling scope. No Clone or Serialize.
pub(super) struct ProductionReminderScope {
    owner: Weak<DeadlineInner>,
}
impl ProductionReminderScope {
    pub(super) fn failed(&self) {
        if let Some(owner) = self.owner.upgrade() {
            if let Ok(mut state) = owner.state.lock() {
                state.global = None;
            }
            owner.failed.store(true, Ordering::SeqCst);
            let _ = owner.sender.send(Command::Changed);
        }
    }

    pub(super) fn rearm(&self) -> Result<(), ReminderError> {
        self.owner
            .upgrade()
            .ok_or(ReminderError::Unavailable)?
            .rearm_global()
    }
}

pub(crate) struct ProductionReminderDeadlineOwner {
    inner: Arc<DeadlineInner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl ProductionReminderDeadlineOwner {
    // The argument cannot be minted by arbitrary Arc trait implementations.
    // Construction consumes it once and independently checks exact actor/clock.
    pub(super) fn from_capability(
        capability: ProductionReminderScheduleCapability,
        database: Database,
    ) -> Result<Self, ReminderError> {
        let clock = capability.clock(&database)?;
        let (sender, receiver) = mpsc::channel();
        let inner = Arc::new(DeadlineInner {
            capability,
            database,
            clock,
            state: Mutex::new(State::default()),
            sender,
            failed: AtomicBool::new(false),
        });
        inner.capability.register(Arc::downgrade(&inner))?;
        let weak = Arc::downgrade(&inner);
        let worker = thread::Builder::new()
            .name("calendar-todo-notification-deadline".into())
            .spawn(move || run(weak, receiver))
            .map_err(|_| ReminderError::Unavailable)?;
        Ok(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub(super) fn composer(&self) -> Result<ProductionReminderComposer, ReminderError> {
        if !self.inner.capability.live() {
            return Err(ReminderError::Unavailable);
        }
        Ok(ProductionReminderComposer {
            owner: Arc::downgrade(&self.inner),
        })
    }

    pub(super) fn scope(&self) -> Result<ProductionReminderScope, ReminderError> {
        self.inner.global_scope()
    }

    pub(super) fn shutdown(&self) {
        self.inner.stop();
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}
impl Drop for ProductionReminderDeadlineOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl std::fmt::Debug for ProductionReminderDeadlineOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProductionReminderDeadlineOwner(<redacted>)")
    }
}

fn run(owner: Weak<DeadlineInner>, receiver: mpsc::Receiver<Command>) {
    loop {
        let Some(inner) = owner.upgrade() else {
            break;
        };
        let nearest = {
            let Ok(state) = inner.state.lock() else {
                break;
            };
            if state.stopped {
                break;
            }
            state
                .keys
                .iter()
                .min_by_key(|(_, deadline)| deadline.at_ms)
                .map(|(key, deadline)| {
                    (
                        Some(key.clone()),
                        deadline.schedule_generation,
                        deadline.at_ms,
                    )
                })
                .into_iter()
                .chain(state.global.map(|(generation, at)| (None, generation, at)))
                .min_by_key(|(_, _, at)| *at)
        };
        let command = if let Some((key, generation, deadline)) = nearest {
            match inner.clock.now_utc_ms() {
                Ok(now) if now >= deadline => {
                    if let Some(key) = key {
                        inner.fire(&key, generation, now);
                    } else {
                        inner.fire_global(generation, now);
                    }
                    continue;
                }
                Ok(now) => receiver.recv_timeout(Duration::from_millis(
                    u64::try_from(deadline.saturating_sub(now)).unwrap_or(u64::MAX),
                )),
                Err(_) => {
                    inner.failed.store(true, Ordering::SeqCst);
                    // Clock recovery is event-driven, never interval polling.
                    receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                }
            }
        } else {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match command {
            Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Changed) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
