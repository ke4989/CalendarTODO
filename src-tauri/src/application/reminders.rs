use crate::{
    adapters::persistence::{Database, NotificationJob},
    ports::clock::Clock,
};
use std::{
    sync::{Arc, Mutex, Weak, mpsc},
    thread::{self, JoinHandle},
};
use thiserror::Error;

/// Bounded delivery outcome. A platform failure never carries text into the
/// scheduler; the job is recorded as `failed`, which is terminal, so a refused
/// reminder is not retried and cannot be delivered twice.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailure {
    #[error("reminder delivery is unavailable")]
    Unavailable,
}

pub trait ReminderDelivery: Send + Sync + 'static {
    fn deliver(&self, job: &NotificationJob) -> Result<(), DeliveryFailure>;
}

/// The production slice deliberately has no Windows toast implementation yet.
/// Claiming a job still records it durably; the fake port exercises delivery.
pub struct NoopDelivery;
impl ReminderDelivery for NoopDelivery {
    fn deliver(&self, _job: &NotificationJob) -> Result<(), DeliveryFailure> {
        Err(DeliveryFailure::Unavailable)
    }
}

enum Command {
    Reconcile,
    Shutdown,
}

#[derive(Debug, Error)]
pub enum ReminderError {
    #[error("reminder worker unavailable")]
    Unavailable,
}

pub struct ReminderRuntime {
    owner: Arc<ReminderOwner>,
    worker: Mutex<Option<JoinHandle<()>>>,
    scheduler: Option<super::reminder_deadlines::ProductionReminderDeadlineOwner>,
}

impl ReminderRuntime {
    pub(crate) fn matches_application_owner(
        &self,
        database: &Database,
        clock: &Arc<dyn Clock>,
    ) -> bool {
        self.matches_captured_owner(&database.actor_provenance(), clock)
    }
    pub(crate) fn matches_captured_owner(&self, actor: &Weak<()>, clock: &Arc<dyn Clock>) -> bool {
        Weak::ptr_eq(&self.owner.actor, actor)
            && Arc::ptr_eq(&self.owner.clock, clock)
            && self.owner.state.lock().is_ok_and(|state| !state.stopped)
    }
    pub fn new(
        database: Database,
        clock: Arc<dyn Clock>,
        delivery: Arc<dyn ReminderDelivery>,
    ) -> Result<Self, ReminderError> {
        Self::build(database, clock, delivery, true)
    }

    /// Retain the real scheduling owner for scoped CRUD composition without
    /// claiming or delivering any user's reminder jobs during a dev Probe run.

    fn build(
        database: Database,
        clock: Arc<dyn Clock>,
        delivery: Arc<dyn ReminderDelivery>,
        install: bool,
    ) -> Result<Self, ReminderError> {
        Self::build_inner(database, clock, delivery, install, true)
    }
    fn build_inner(
        database: Database,
        clock: Arc<dyn Clock>,
        delivery: Arc<dyn ReminderDelivery>,
        install: bool,
        reconcile: bool,
    ) -> Result<Self, ReminderError> {
        let (sender, receiver) = mpsc::channel();
        let owner = Arc::new(ReminderOwner {
            actor: database.actor_provenance(),
            clock: clock.clone(),
            generation: uuid::Uuid::new_v4(),
            sender,
            state: Mutex::new(ReminderOwnerState::default()),
        });
        let scheduler = if install {
            Some(
                super::reminder_deadlines::ProductionReminderDeadlineOwner::from_capability(
                    owner.mint()?,
                    database.clone(),
                )?,
            )
        } else {
            None
        };
        let scope = scheduler.as_ref().map(|s| s.scope()).transpose()?;
        let worker = thread::Builder::new()
            .name("calendar-todo-reminders".into())
            .spawn(move || {
                if reconcile {
                    run(database, clock, delivery, receiver, scope);
                } else {
                    while let Ok(command) = receiver.recv() {
                        if matches!(command, Command::Shutdown) {
                            break;
                        }
                    }
                }
            })
            .map_err(|_| ReminderError::Unavailable)?;
        let runtime = Self {
            owner,
            worker: Mutex::new(Some(worker)),
            scheduler,
        };
        runtime.refresh()?;
        Ok(runtime)
    }

    pub(crate) fn scheduling_composer(
        &self,
    ) -> Result<super::reminder_deadlines::ProductionReminderComposer, ReminderError> {
        self.scheduler
            .as_ref()
            .ok_or(ReminderError::Unavailable)?
            .composer()
    }

    pub fn refresh(&self) -> Result<(), ReminderError> {
        self.owner.refresh()?;
        let linked = self
            .owner
            .state
            .lock()
            .map_err(|_| ReminderError::Unavailable)?
            .deadline_owner
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(linked) = linked {
            linked.refresh_registered()?;
        }
        Ok(())
    }

    /// The switch is already durable in the actor before this wake. Delivery
    /// authority itself lives in the actor gate, so a failed wake only delays
    /// the idle deadline instead of re-enabling anything.
    pub fn settings_changed(&self) -> Result<(), ReminderError> {
        self.refresh()
    }

    pub fn shutdown(&self) {
        let linked = {
            // Poison forbids normal use/mint. Terminal cleanup only recovers
            // this owner's own weak link so it can stop keys before joining.
            let mut state = match self.owner.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.stopped = true;
            state
                .deadline_owner
                .take()
                .and_then(|owner| owner.upgrade())
        };
        if let Some(linked) = linked {
            linked.stop();
        }
        if let Some(scheduler) = &self.scheduler {
            scheduler.shutdown();
        }
        let _ = self.owner.sender.send(Command::Shutdown);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for ReminderRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct ReminderOwnerState {
    stopped: bool,
    minted: bool,
    deadline_owner: Option<Weak<super::reminder_deadlines::DeadlineInner>>,
}
struct ReminderOwner {
    clock: Arc<dyn Clock>,
    actor: Weak<()>,
    generation: uuid::Uuid,
    sender: mpsc::Sender<Command>,
    state: Mutex<ReminderOwnerState>,
}
impl ReminderOwner {
    fn refresh(&self) -> Result<(), ReminderError> {
        let state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
        if state.stopped {
            return Err(ReminderError::Unavailable);
        }
        self.sender
            .send(Command::Reconcile)
            .map_err(|_| ReminderError::Unavailable)
    }
    fn mint(self: &Arc<Self>) -> Result<ProductionReminderScheduleCapability, ReminderError> {
        let mut state = self.state.lock().map_err(|_| ReminderError::Unavailable)?;
        if state.stopped || state.minted || self.actor.upgrade().is_none() {
            return Err(ReminderError::Unavailable);
        }
        state.minted = true;
        Ok(ProductionReminderScheduleCapability {
            owner: Arc::downgrade(self),
            generation: self.generation,
        })
    }
}
/// Private real-runtime mint. No Clone, Serialize, or public constructor.
pub(super) struct ProductionReminderScheduleCapability {
    owner: Weak<ReminderOwner>,
    generation: uuid::Uuid,
}
impl ProductionReminderScheduleCapability {
    pub(super) fn live(&self) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            owner.generation == self.generation
                && owner.state.lock().is_ok_and(|state| !state.stopped)
        })
    }
    pub(super) fn matches_actor(&self, database: &Database) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            owner.actor.upgrade().is_some()
                && Weak::ptr_eq(&owner.actor, &database.actor_provenance())
        })
    }
    pub(super) fn clock(&self, database: &Database) -> Result<Arc<dyn Clock>, ReminderError> {
        let owner = self.owner.upgrade().ok_or(ReminderError::Unavailable)?;
        if !self.live() || !self.matches_actor(database) || !database.matches_clock(&owner.clock) {
            return Err(ReminderError::Unavailable);
        }
        Ok(owner.clock.clone())
    }
    pub(super) fn register(
        &self,
        linked: Weak<super::reminder_deadlines::DeadlineInner>,
    ) -> Result<(), ReminderError> {
        let owner = self.owner.upgrade().ok_or(ReminderError::Unavailable)?;
        let mut state = owner.state.lock().map_err(|_| ReminderError::Unavailable)?;
        if state.stopped || owner.generation != self.generation || state.deadline_owner.is_some() {
            return Err(ReminderError::Unavailable);
        }
        state.deadline_owner = Some(linked);
        Ok(())
    }
    pub(super) fn wake(&self) -> Result<(), ReminderError> {
        if !self.live() {
            return Err(ReminderError::Unavailable);
        }
        self.owner
            .upgrade()
            .ok_or(ReminderError::Unavailable)?
            .refresh()
    }
}
impl std::fmt::Debug for ProductionReminderScheduleCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProductionReminderScheduleCapability(<redacted>)")
    }
}

fn run(
    database: Database,
    clock: Arc<dyn Clock>,
    delivery: Arc<dyn ReminderDelivery>,
    receiver: mpsc::Receiver<Command>,
    scope: Option<super::reminder_deadlines::ProductionReminderScope>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Shutdown => break,
            Command::Reconcile => {
                let Ok(now) = clock.now_utc_ms() else {
                    if let Some(scope) = &scope {
                        scope.failed();
                    }
                    continue;
                };
                let Ok(_) = database.reconcile_notification_jobs(now) else {
                    if let Some(scope) = &scope {
                        scope.failed();
                    }
                    continue;
                };
                let Ok(jobs) = database.claim_due_notification_jobs(now) else {
                    if let Some(scope) = &scope {
                        scope.failed();
                    }
                    continue;
                };
                for job in jobs {
                    let delivered = delivery.deliver(&job).is_ok();
                    let _ = database.finish_notification_job(job.job_id, delivered, now);
                }
                if database.reconcile_notification_jobs(now).is_ok() {
                    if let Some(scope) = &scope {
                        let _ = scope.rearm();
                    }
                } else if let Some(scope) = &scope {
                    scope.failed();
                }
            }
        }
    }
}
