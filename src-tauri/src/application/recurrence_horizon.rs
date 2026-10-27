use crate::{
    adapters::persistence::{Database, PersistenceError},
    domain::calendar_recurrence::RecurrenceHorizon,
    ports::clock::Clock,
};
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};
use thiserror::Error;

/// Submissions accepted before a producer has to wait for the worker.
const HORIZON_QUEUE_CAPACITY: usize = 64;

/// Events that can leave the bounded projection horizon behind the clock.
///
/// Every trigger is an event with an owner: application start, the day
/// boundary, a resume notification, or an explicit refresh. There is no timer
/// polling inside this component; it reports the next day boundary so a runtime
/// can arm exactly one deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonTrigger {
    Startup,
    ClockRollover,
    Resume,
    Manual,
}

impl HorizonTrigger {
    fn priority(self) -> u8 {
        match self {
            Self::ClockRollover => 0,
            Self::Startup => 1,
            Self::Resume => 2,
            Self::Manual => 3,
        }
    }
}

/// Bounded failure class of one refresh. No platform, SQL, or provider detail
/// crosses this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonFailureClass {
    Clock,
    Persistence,
}

/// Bounded result of one refresh batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonRefreshOutcome {
    Refreshed {
        window_start_at_utc_ms: i64,
        window_end_at_utc_ms: i64,
        series_count: u32,
        occurrence_count: u32,
        unsupported_series_count: u32,
    },
    Failed {
        class: HorizonFailureClass,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HorizonRefreshReport {
    pub trigger: HorizonTrigger,
    pub coalesced_requests: u32,
    pub outcome: HorizonRefreshOutcome,
    /// Next UTC day boundary computed from the same instant as the refresh. A
    /// runtime arms this deadline and then submits `ClockRollover`; `None`
    /// means the instant or the horizon could not be established.
    pub next_boundary_at_utc_ms: Option<i64>,
}

/// Event-driven owner of the recurrence projection horizon.
///
/// The refresher serializes on one worker thread and shares the SQLite actor
/// with the rest of the application, so a rebuild can never race a cache
/// promotion or a local-state command. Requests that arrive while a rebuild is
/// running coalesce into one follow-up batch instead of queueing rebuilds.
pub struct RecurrenceHorizonRefresher {
    clock: Arc<dyn Clock>,
    horizon: RecurrenceHorizon,
    sender: Option<SyncSender<Message>>,
    worker: Option<JoinHandle<()>>,
}

impl RecurrenceHorizonRefresher {
    pub fn new(database: Database, clock: Arc<dyn Clock>) -> Result<Self, HorizonError> {
        Self::with_policy(database, clock, RecurrenceHorizon::standard())
    }

    pub fn with_policy(
        database: Database,
        clock: Arc<dyn Clock>,
        horizon: RecurrenceHorizon,
    ) -> Result<Self, HorizonError> {
        let (sender, receiver) = mpsc::sync_channel(HORIZON_QUEUE_CAPACITY);
        let worker_clock = clock.clone();
        let worker = thread::Builder::new()
            .name("calendar-todo-horizon-refresh".into())
            .spawn(move || run_worker(database, worker_clock, horizon, receiver))
            .map_err(|_| HorizonError::WorkerUnavailable)?;
        Ok(Self {
            clock,
            horizon,
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub fn horizon(&self) -> RecurrenceHorizon {
        self.horizon
    }

    /// Deadline for the next day-boundary refresh, or `None` when the clock or
    /// the horizon arithmetic cannot produce one.
    pub fn next_boundary_at_utc_ms(&self) -> Option<i64> {
        self.clock
            .now_utc_ms()
            .ok()
            .and_then(|now_ms| self.horizon.next_boundary_at_utc_ms(now_ms).ok())
    }

    pub fn request(
        &self,
        trigger: HorizonTrigger,
    ) -> Result<Receiver<HorizonRefreshReport>, HorizonError> {
        let (respond_to, response) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or(HorizonError::WorkerUnavailable)?
            .send(Message::Trigger {
                trigger,
                respond_to,
            })
            .map_err(|_| HorizonError::WorkerUnavailable)?;
        Ok(response)
    }

    pub fn shutdown(&mut self) -> Result<(), HorizonError> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<(), HorizonError> {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Message::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| HorizonError::WorkerUnavailable)?;
        }
        Ok(())
    }
}

impl Drop for RecurrenceHorizonRefresher {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

enum Message {
    Trigger {
        trigger: HorizonTrigger,
        respond_to: SyncSender<HorizonRefreshReport>,
    },
    Shutdown,
}

struct Batch {
    trigger: HorizonTrigger,
    responders: Vec<SyncSender<HorizonRefreshReport>>,
}

impl Batch {
    fn new(trigger: HorizonTrigger, respond_to: SyncSender<HorizonRefreshReport>) -> Self {
        Self {
            trigger,
            responders: vec![respond_to],
        }
    }

    fn push(&mut self, trigger: HorizonTrigger, respond_to: SyncSender<HorizonRefreshReport>) {
        if trigger.priority() > self.trigger.priority() {
            self.trigger = trigger;
        }
        self.responders.push(respond_to);
    }

    fn report(
        &self,
        outcome: HorizonRefreshOutcome,
        next_boundary: Option<i64>,
    ) -> HorizonRefreshReport {
        HorizonRefreshReport {
            trigger: self.trigger,
            coalesced_requests: u32::try_from(self.responders.len().saturating_sub(1))
                .unwrap_or(u32::MAX),
            outcome,
            next_boundary_at_utc_ms: next_boundary,
        }
    }
}

fn run_worker(
    database: Database,
    clock: Arc<dyn Clock>,
    horizon: RecurrenceHorizon,
    receiver: Receiver<Message>,
) {
    loop {
        let message = match receiver.recv() {
            Ok(message) => message,
            Err(_) => return,
        };
        let mut batch = match message {
            Message::Trigger {
                trigger,
                respond_to,
            } => Batch::new(trigger, respond_to),
            Message::Shutdown => return,
        };
        if drain_pending(&receiver, &mut batch) {
            return;
        }

        let now_ms = clock.now_utc_ms().ok();
        // Raw-cache retention rides the same clock-driven tick as the horizon
        // rebuild (startup, UTC day boundary, resume, explicit refresh), so a
        // confirmed-deleted cache drains without a second timer or any polling.
        // It is optional maintenance: one bounded batch per tick, and a bounded
        // failure is ignored because projection refresh and retention are
        // independent.
        if let Some(now_ms) = now_ms {
            let _ = database.purge_deleted_raw_cache(now_ms);
        }
        let outcome = match now_ms {
            Some(now_ms) => match database.refresh_calendar_projections(now_ms, horizon) {
                Ok(summary) => HorizonRefreshOutcome::Refreshed {
                    window_start_at_utc_ms: summary.window_start_at_utc_ms,
                    window_end_at_utc_ms: summary.window_end_at_utc_ms,
                    series_count: summary.series_count,
                    occurrence_count: summary.occurrence_count,
                    unsupported_series_count: summary.unsupported_series_count,
                },
                Err(error) => HorizonRefreshOutcome::Failed {
                    class: failure_class(error),
                },
            },
            None => HorizonRefreshOutcome::Failed {
                class: HorizonFailureClass::Clock,
            },
        };
        let next_boundary = now_ms.and_then(|now_ms| horizon.next_boundary_at_utc_ms(now_ms).ok());
        let report = batch.report(outcome, next_boundary);
        for responder in std::mem::take(&mut batch.responders) {
            let _ = responder.send(report.clone());
        }
    }
}

fn drain_pending(receiver: &Receiver<Message>, batch: &mut Batch) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(Message::Trigger {
                trigger,
                respond_to,
            }) => batch.push(trigger, respond_to),
            Ok(Message::Shutdown) => return true,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

fn failure_class(error: PersistenceError) -> HorizonFailureClass {
    match error {
        PersistenceError::Clock => HorizonFailureClass::Clock,
        _ => HorizonFailureClass::Persistence,
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum HorizonError {
    #[error("recurrence horizon refresh worker is unavailable")]
    WorkerUnavailable,
}
