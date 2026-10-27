use crate::{
    application::sync::{IncrementalSyncPath, ReadOnlySyncService},
    domain::calendar_sync::{SyncFailureClass, SyncTrigger},
    ports::caldav::ReadOnlyCalendarSource,
};
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    pub transient_base: Duration,
    pub other_base: Duration,
    pub maximum: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            transient_base: Duration::from_secs(30),
            other_base: Duration::from_secs(60),
            maximum: Duration::from_secs(15 * 60),
        }
    }
}

impl BackoffPolicy {
    fn delay(self, failure: SyncFailureClass, consecutive_failures: u32) -> Duration {
        if failure == SyncFailureClass::None || consecutive_failures == 0 {
            return Duration::ZERO;
        }
        if failure == SyncFailureClass::Unauthorized {
            return self.maximum;
        }
        let base = if failure == SyncFailureClass::Transient {
            self.transient_base
        } else {
            self.other_base
        };
        let exponent = consecutive_failures.saturating_sub(1).min(20);
        base.saturating_mul(1_u32 << exponent).min(self.maximum)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorOutcome {
    Success { path: IncrementalSyncPath },
    Failure { class: SyncFailureClass },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorReport {
    pub account_id: String,
    pub trigger: SyncTrigger,
    pub coalesced_requests: u32,
    pub outcome: CoordinatorOutcome,
    pub next_retry_delay_ms: Option<u64>,
}

pub struct AccountSyncCoordinator {
    account_id: String,
    source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
    sender: Option<SyncSender<Message>>,
    worker: Option<JoinHandle<()>>,
}

impl AccountSyncCoordinator {
    pub fn new(
        service: ReadOnlySyncService,
        source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
    ) -> Result<Self, CoordinatorError> {
        Self::with_policy(service, source, BackoffPolicy::default())
    }

    pub fn with_policy(
        service: ReadOnlySyncService,
        source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
        policy: BackoffPolicy,
    ) -> Result<Self, CoordinatorError> {
        let account_id = source.account_id().to_owned();
        if Uuid::parse_str(&account_id).is_err()
            || policy.maximum.is_zero()
            || policy.transient_base.is_zero()
            || policy.other_base.is_zero()
        {
            return Err(CoordinatorError::InvalidConfiguration);
        }
        let (sender, receiver) = mpsc::sync_channel(64);
        let worker_account_id = account_id.clone();
        let worker_source = source.clone();
        let worker = thread::Builder::new()
            .name("calendar-todo-sync-coordinator".into())
            .spawn(move || run_worker(service, worker_source, receiver, worker_account_id, policy))
            .map_err(|_| CoordinatorError::WorkerUnavailable)?;
        Ok(Self {
            account_id,
            source,
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn request(
        &self,
        trigger: SyncTrigger,
    ) -> Result<Receiver<CoordinatorReport>, CoordinatorError> {
        let (respond_to, response) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or(CoordinatorError::WorkerUnavailable)?
            .send(Message::Trigger {
                trigger,
                respond_to,
            })
            .map_err(|_| CoordinatorError::WorkerUnavailable)?;
        Ok(response)
    }

    pub fn shutdown(mut self) -> Result<(), CoordinatorError> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<(), CoordinatorError> {
        // Ask an in-flight exchange to stop before waiting for the worker, so a
        // disable or shutdown does not have to wait for a network deadline.
        self.source.cancel_in_flight();
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Message::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| CoordinatorError::WorkerUnavailable)?;
        }
        Ok(())
    }
}

impl Drop for AccountSyncCoordinator {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

enum Message {
    Trigger {
        trigger: SyncTrigger,
        respond_to: SyncSender<CoordinatorReport>,
    },
    Shutdown,
}

struct Batch {
    trigger: SyncTrigger,
    responders: Vec<SyncSender<CoordinatorReport>>,
}

impl Batch {
    fn new(trigger: SyncTrigger, respond_to: SyncSender<CoordinatorReport>) -> Self {
        Self {
            trigger,
            responders: vec![respond_to],
        }
    }

    fn push(&mut self, trigger: SyncTrigger, respond_to: SyncSender<CoordinatorReport>) {
        if trigger_priority(trigger) > trigger_priority(self.trigger) {
            self.trigger = trigger;
        }
        self.responders.push(respond_to);
    }
}

fn trigger_priority(trigger: SyncTrigger) -> u8 {
    match trigger {
        SyncTrigger::Periodic => 0,
        SyncTrigger::Startup => 1,
        SyncTrigger::Retry => 2,
        SyncTrigger::Manual => 3,
    }
}

fn run_worker(
    service: ReadOnlySyncService,
    source: Arc<dyn ReadOnlyCalendarSource + Send + Sync>,
    receiver: Receiver<Message>,
    account_id: String,
    policy: BackoffPolicy,
) {
    let mut consecutive_failures = 0_u32;
    let mut next_allowed_at = Instant::now();
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
        while batch.trigger != SyncTrigger::Manual && Instant::now() < next_allowed_at {
            match receiver.recv_timeout(next_allowed_at.saturating_duration_since(Instant::now())) {
                Ok(Message::Trigger {
                    trigger,
                    respond_to,
                }) => {
                    batch.push(trigger, respond_to);
                    if drain_pending(&receiver, &mut batch) {
                        return;
                    }
                }
                Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }

        let result = service.run_incremental_or_full(source.as_ref(), batch.trigger);
        let (outcome, delay) = match result {
            Ok(result) => {
                consecutive_failures = 0;
                next_allowed_at = Instant::now();
                (
                    CoordinatorOutcome::Success { path: result.path },
                    Duration::ZERO,
                )
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let class = error.failure_class();
                let delay = policy.delay(class, consecutive_failures);
                next_allowed_at = Instant::now() + delay;
                (CoordinatorOutcome::Failure { class }, delay)
            }
        };
        let report = CoordinatorReport {
            account_id: account_id.clone(),
            trigger: batch.trigger,
            coalesced_requests: u32::try_from(batch.responders.len().saturating_sub(1))
                .unwrap_or(u32::MAX),
            outcome,
            next_retry_delay_ms: (!delay.is_zero())
                .then(|| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
        };
        for responder in batch.responders {
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

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorError {
    #[error("sync coordinator configuration is invalid")]
    InvalidConfiguration,
    #[error("sync coordinator worker is unavailable")]
    WorkerUnavailable,
}
