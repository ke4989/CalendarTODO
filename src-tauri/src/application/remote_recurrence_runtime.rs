use crate::{
    adapters::persistence::{Database, PersistenceError, RemoteRecurrenceTarget},
    application::{
        recurrence_runtime::{DeadlineScheduler, SchedulerError},
        remote_execution_activation::RemoteExecutionActivation,
        sync_runtime::SyncExecutionRuntime,
    },
    ports::clock::Clock,
};
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread::{self, JoinHandle},
};
use thiserror::Error;

/// Process-local coordinator for advancing one remote singleton from durable
/// local recurrence intent. Every network-capable step is delegated to the
/// existing confirmed connection/read-sync and scoped event execution owners.
pub(crate) struct RemoteRecurrenceRuntime {
    inner: Arc<RuntimeInner>,
    deadline_worker: Mutex<Option<JoinHandle<()>>>,
}

struct RuntimeInner {
    database: Database,
    execution: Arc<RemoteExecutionActivation>,
    sync: Arc<SyncExecutionRuntime>,
    clock: Arc<dyn Clock>,
    scheduler: Arc<dyn DeadlineScheduler>,
    running: AtomicBool,
    queued: AtomicBool,
    force_due_check: AtomicBool,
    stopped: AtomicBool,
}

impl RemoteRecurrenceRuntime {
    pub(crate) fn new(
        database: Database,
        execution: Arc<RemoteExecutionActivation>,
        sync: Arc<SyncExecutionRuntime>,
        clock: Arc<dyn Clock>,
        scheduler: Arc<dyn DeadlineScheduler>,
        deadline_receiver: Receiver<()>,
    ) -> Result<Self, RemoteRecurrenceRuntimeError> {
        let inner = Arc::new(RuntimeInner {
            database,
            execution,
            sync,
            clock,
            scheduler,
            running: AtomicBool::new(false),
            queued: AtomicBool::new(false),
            force_due_check: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        let deadline_worker = thread::Builder::new()
            .name("calendar-todo-remote-recurrence-deadline".into())
            .spawn(move || dispatch_deadlines(weak, deadline_receiver))
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        Ok(Self {
            inner,
            deadline_worker: Mutex::new(Some(deadline_worker)),
        })
    }

    /// Rehydrate eligible pending intent and arm the nearest elapsed deadline.
    pub(crate) fn startup_or_resume(&self) -> Result<(), RemoteRecurrenceRuntimeError> {
        self.inner.request(true)
    }

    /// Re-evaluate completion state and the nearest deadline after a local
    /// occurrence completion or recurrence metadata commit.
    pub(crate) fn recurrence_changed(&self) -> Result<(), RemoteRecurrenceRuntimeError> {
        self.inner.request(true)
    }

    /// Re-arm after a scoped remote terminal event. Applied recurrence work has
    /// already advanced its cursor transactionally; conflicts are excluded from
    /// target selection and therefore do not create a retry loop.
    pub(crate) fn remote_terminal(&self) -> Result<(), RemoteRecurrenceRuntimeError> {
        self.inner.request(false)
    }

    pub(crate) fn shutdown(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.scheduler.shutdown();
        if let Ok(mut worker) = self.deadline_worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for RemoteRecurrenceRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl RuntimeInner {
    fn request(
        self: &Arc<Self>,
        force_due_check: bool,
    ) -> Result<(), RemoteRecurrenceRuntimeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RemoteRecurrenceRuntimeError::Unavailable);
        }
        self.queued.store(true, Ordering::SeqCst);
        if force_due_check {
            self.force_due_check.store(true, Ordering::SeqCst);
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("calendar-todo-remote-recurrence".into())
            .spawn(move || run_requests(weak))
            .map_err(|_| {
                self.running.store(false, Ordering::SeqCst);
                RemoteRecurrenceRuntimeError::Unavailable
            })?;
        Ok(())
    }

    fn run_once(&self, force_due_check: bool) -> Result<(), RemoteRecurrenceRuntimeError> {
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        let targets = self.database.remote_recurrence_targets()?;
        for target in &targets {
            if target.pending || target.deadline_at_utc_ms <= now || force_due_check {
                let _ = self.advance(target);
            }
        }
        self.arm_next(now)
    }

    fn advance(&self, target: &RemoteRecurrenceTarget) -> Result<(), RemoteRecurrenceRuntimeError> {
        let connection = self
            .execution
            .ensure_confirmed_connection(&target.account_id, &target.collection_route)
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        let read = self
            .sync
            .confirmed_read_sync(&target.account_id, connection.generation())
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        let scope = self
            .execution
            .admit_connected_event(&self.database, |owner| {
                let current = Arc::new(move |account: &str, route: &str| {
                    owner(account, route) && connection.current() && read.current()
                });
                self.database
                    .admit_remote_recurrence_advance(target.resource_id.clone(), current)
            })?;
        let binding = self
            .execution
            .prepare_confirmed_event_binding(scope.clone())
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        self.execution
            .submit_scoped_event(binding, scope)
            .map_err(|_| RemoteRecurrenceRuntimeError::Unavailable)?;
        Ok(())
    }

    fn arm_next(&self, now: i64) -> Result<(), RemoteRecurrenceRuntimeError> {
        let next = self
            .database
            .remote_recurrence_targets()?
            .into_iter()
            // A failed due attempt is retried by the next explicit lifecycle,
            // completion or metadata trigger. Re-arming an elapsed
            // deadline here would create a tight timer loop while offline.
            .filter(|target| !target.pending && target.deadline_at_utc_ms > now)
            .map(|target| target.deadline_at_utc_ms)
            .min();
        match next {
            Some(deadline) => self.scheduler.arm(deadline)?,
            None => self.scheduler.cancel()?,
        }
        Ok(())
    }
}

fn run_requests(inner: Weak<RuntimeInner>) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    loop {
        inner.queued.store(false, Ordering::SeqCst);
        let force = inner.force_due_check.swap(false, Ordering::SeqCst);
        if !inner.stopped.load(Ordering::SeqCst) {
            let _ = inner.run_once(force);
        }
        if !inner.queued.swap(false, Ordering::SeqCst) {
            inner.running.store(false, Ordering::SeqCst);
            if !inner.queued.load(Ordering::SeqCst) || inner.running.swap(true, Ordering::SeqCst) {
                break;
            }
        }
    }
}

fn dispatch_deadlines(inner: Weak<RuntimeInner>, receiver: Receiver<()>) {
    while receiver.recv().is_ok() {
        let Some(inner) = inner.upgrade() else {
            break;
        };
        if inner.stopped.load(Ordering::SeqCst) {
            break;
        }
        let _ = inner.request(true);
    }
}

#[derive(Debug, Error)]
pub(crate) enum RemoteRecurrenceRuntimeError {
    #[error("remote recurrence runtime is unavailable")]
    Unavailable,
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
}
