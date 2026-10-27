use crate::application::recurrence_horizon::{
    HorizonError, HorizonRefreshOutcome, HorizonTrigger, RecurrenceHorizonRefresher,
};
use serde::Serialize;
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Receiver,
    },
    thread::{self, JoinHandle},
};
use thiserror::Error;

pub const AGENDA_REFRESH_EVENT: &str = "calendar-todo://agenda-refresh";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgendaRefreshNotice {
    pub reason: AgendaRefreshReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgendaRefreshReason {
    RecurrenceHorizon,
    CalendarSync,
}

pub trait AgendaRefreshEmitter: Send + Sync + 'static {
    fn emit(&self, notice: AgendaRefreshNotice);
}

pub trait DeadlineScheduler: Send + Sync + 'static {
    fn arm(&self, deadline_at_utc_ms: i64) -> Result<(), SchedulerError>;
    fn cancel(&self) -> Result<(), SchedulerError>;
    fn shutdown(&self);
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("deadline scheduler is unavailable")]
    Unavailable,
}

pub struct HorizonRuntime {
    inner: Arc<RuntimeInner>,
    deadline_worker: Mutex<Option<JoinHandle<()>>>,
}

struct RuntimeInner {
    refresher: Mutex<RecurrenceHorizonRefresher>,
    scheduler: Arc<dyn DeadlineScheduler>,
    emitter: Arc<dyn AgendaRefreshEmitter>,
    generation: AtomicU64,
    stopped: AtomicBool,
}

impl HorizonRuntime {
    pub fn new(
        refresher: RecurrenceHorizonRefresher,
        scheduler: Arc<dyn DeadlineScheduler>,
        emitter: Arc<dyn AgendaRefreshEmitter>,
        deadline_receiver: Receiver<()>,
    ) -> Result<Self, HorizonRuntimeError> {
        let inner = Arc::new(RuntimeInner {
            refresher: Mutex::new(refresher),
            scheduler,
            emitter,
            generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        let deadline_worker = thread::Builder::new()
            .name("calendar-todo-horizon-runtime".into())
            .spawn(move || dispatch_deadlines(weak, deadline_receiver))
            .map_err(|_| HorizonRuntimeError::Unavailable)?;
        Ok(Self {
            inner,
            deadline_worker: Mutex::new(Some(deadline_worker)),
        })
    }

    pub fn trigger(&self, trigger: HorizonTrigger) -> Result<(), HorizonRuntimeError> {
        self.inner.trigger(trigger)
    }

    pub fn shutdown(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.scheduler.shutdown();
        if let Ok(mut refresher) = self.inner.refresher.lock() {
            let _ = refresher.shutdown();
        }
        if let Ok(mut worker) = self.deadline_worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for HorizonRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl RuntimeInner {
    fn trigger(self: &Arc<Self>, trigger: HorizonTrigger) -> Result<(), HorizonRuntimeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(HorizonRuntimeError::Unavailable);
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let response = self
            .refresher
            .lock()
            .map_err(|_| HorizonRuntimeError::Unavailable)?
            .request(trigger)?;
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("calendar-todo-horizon-report".into())
            .spawn(move || {
                if let Ok(report) = response.recv()
                    && let Some(inner) = weak.upgrade()
                    && generation == inner.generation.load(Ordering::SeqCst)
                    && !inner.stopped.load(Ordering::SeqCst)
                {
                    inner.apply_report(report.outcome, report.next_boundary_at_utc_ms);
                }
            })
            .map_err(|_| HorizonRuntimeError::Unavailable)?;
        Ok(())
    }

    fn apply_report(&self, outcome: HorizonRefreshOutcome, next_boundary: Option<i64>) {
        if let Some(deadline) = next_boundary {
            let _ = self.scheduler.arm(deadline);
        } else {
            let _ = self.scheduler.cancel();
        }
        if matches!(outcome, HorizonRefreshOutcome::Refreshed { .. }) {
            self.emitter.emit(AgendaRefreshNotice {
                reason: AgendaRefreshReason::RecurrenceHorizon,
            });
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
        let _ = inner.trigger(HorizonTrigger::ClockRollover);
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum HorizonRuntimeError {
    #[error("recurrence horizon runtime is unavailable")]
    Unavailable,
    #[error(transparent)]
    Horizon(#[from] HorizonError),
}
