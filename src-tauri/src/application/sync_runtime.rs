use crate::{
    application::{
        account_registry::{AccountRegistryError, AccountSourceRegistry},
        recurrence_runtime::{AgendaRefreshEmitter, AgendaRefreshNotice, AgendaRefreshReason},
        sync::IncrementalSyncPath,
        sync_coordinator::{CoordinatorOutcome, CoordinatorReport},
    },
    domain::{
        calendar_sync::{SyncFailureClass, SyncTrigger},
        settings::{MAX_SYNC_INTERVAL_MINUTES, MIN_SYNC_INTERVAL_MINUTES},
    },
    ports::clock::Clock,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Receiver,
    },
    thread::{self, JoinHandle},
};
use thiserror::Error;

pub const SYNC_STATE_EVENT: &str = "calendar-todo://sync-state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncExecutionState {
    Detached,
    Idle,
    Running,
    Succeeded,
    ActionRequired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncExecutionTrigger {
    Startup,
    Manual,
    Periodic,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncExecutionPath {
    Incremental,
    FullInventoryFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncExecutionFailure {
    Transient,
    Unauthorized,
    Incomplete,
    UnsafeData,
    Validation,
    Persistence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncExecutionView {
    pub account_id: String,
    pub state: SyncExecutionState,
    pub trigger: Option<SyncExecutionTrigger>,
    pub path: Option<SyncExecutionPath>,
    pub failure: Option<SyncExecutionFailure>,
    pub coalesced_requests: u32,
    pub next_retry_delay_ms: Option<u64>,
}

pub trait SyncStateEmitter: Send + Sync + 'static {
    fn emit(&self, view: SyncExecutionView);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledSyncCause {
    Periodic,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledSyncDeadline {
    pub account_id: String,
    pub attachment_generation: u64,
    pub schedule_generation: u64,
    pub deadline_at_utc_ms: i64,
    pub cause: ScheduledSyncCause,
}

pub trait SyncDeadlineScheduler: Send + Sync + 'static {
    fn arm(&self, deadline: ScheduledSyncDeadline) -> Result<(), SyncSchedulerError>;
    fn cancel(&self, account_id: &str) -> Result<(), SyncSchedulerError>;
    fn shutdown(&self);
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SyncSchedulerError {
    #[error("sync deadline scheduler is unavailable")]
    Unavailable,
}

pub struct SyncExecutionRuntime {
    inner: Arc<RuntimeInner>,
    deadline_worker: Mutex<Option<JoinHandle<()>>>,
}

/// Privately issued only after this request's successful read promotion.
/// Later UI sync state cannot erase that result; attachment replacement or
/// runtime shutdown revokes it. Actor target/revision checks still gate admission.
pub(crate) struct ConfirmedReadSync {
    runtime: Weak<RuntimeInner>,
    account: String,
    attachment_generation: u64,
}
impl ConfirmedReadSync {
    pub(crate) fn current(&self) -> bool {
        self.runtime.upgrade().is_some_and(|inner| {
            !inner.stopped.load(Ordering::SeqCst)
                && inner
                    .registry
                    .is_current_attachment(&self.account, self.attachment_generation)
        })
    }
}

struct RuntimeInner {
    registry: Arc<AccountSourceRegistry>,
    state_emitter: Arc<dyn SyncStateEmitter>,
    agenda_emitter: Arc<dyn AgendaRefreshEmitter>,
    clock: Arc<dyn Clock>,
    scheduler: Arc<dyn SyncDeadlineScheduler>,
    states: Mutex<HashMap<String, StoredState>>,
    schedules: Mutex<HashMap<String, ScheduledSyncDeadline>>,
    next_request_generation: AtomicU64,
    next_schedule_generation: AtomicU64,
    sync_interval_minutes: AtomicU64,
    stopped: AtomicBool,
}

#[derive(Clone)]
struct StoredState {
    attachment_generation: u64,
    request_generation: u64,
    view: SyncExecutionView,
}

impl SyncExecutionRuntime {
    pub(crate) fn belongs_to_registry(
        &self,
        registry: &Arc<AccountSourceRegistry>,
        clock: &Arc<dyn Clock>,
    ) -> bool {
        Arc::ptr_eq(&self.inner.registry, registry) && Arc::ptr_eq(&self.inner.clock, clock)
    }

    pub fn new(
        registry: Arc<AccountSourceRegistry>,
        state_emitter: Arc<dyn SyncStateEmitter>,
        agenda_emitter: Arc<dyn AgendaRefreshEmitter>,
        clock: Arc<dyn Clock>,
        scheduler: Arc<dyn SyncDeadlineScheduler>,
        deadline_receiver: Receiver<ScheduledSyncDeadline>,
        sync_interval_minutes: u16,
    ) -> Result<Self, SyncRuntimeError> {
        validate_interval(sync_interval_minutes)?;
        let inner = Arc::new(RuntimeInner {
            registry,
            state_emitter,
            agenda_emitter,
            clock,
            scheduler,
            states: Mutex::new(HashMap::new()),
            schedules: Mutex::new(HashMap::new()),
            next_request_generation: AtomicU64::new(1),
            next_schedule_generation: AtomicU64::new(1),
            sync_interval_minutes: AtomicU64::new(u64::from(sync_interval_minutes)),
            stopped: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        let deadline_worker = thread::Builder::new()
            .name("calendar-todo-sync-schedule-runtime".into())
            .spawn(move || dispatch_deadlines(weak, deadline_receiver))
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        Ok(Self {
            inner,
            deadline_worker: Mutex::new(Some(deadline_worker)),
        })
    }

    pub fn trigger_startup(&self, account_id: &str) -> Result<SyncExecutionView, SyncRuntimeError> {
        self.inner.cancel_schedule(account_id);
        self.inner.trigger(account_id, SyncTrigger::Startup)
    }

    pub fn trigger_manual(&self, account_id: &str) -> Result<SyncExecutionView, SyncRuntimeError> {
        self.inner.trigger(account_id, SyncTrigger::Manual)
    }

    /// Call only from a background command continuation, after actor target
    /// selection and real production connection qualification. No polling.
    pub(crate) fn confirmed_read_sync(
        &self,
        account_id: &str,
        attachment_generation: u64,
    ) -> Result<ConfirmedReadSync, SyncRuntimeError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.inner.trigger_with_completion(
            account_id,
            SyncTrigger::Manual,
            Some((attachment_generation, sender)),
        )?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .map_err(|_| SyncRuntimeError::Unavailable)?
    }

    /// Arm the first periodic deadline for a source that just attached without
    /// running a sync now. This is the path used when the stored
    /// "sync on startup" preference is off, so connecting a source honors that
    /// choice instead of always syncing immediately.
    pub fn account_attached(&self, account_id: &str) -> Result<(), SyncRuntimeError> {
        self.inner.arm_initial_schedule(account_id)
    }

    pub fn settings_changed(&self, sync_interval_minutes: u16) -> Result<(), SyncRuntimeError> {
        validate_interval(sync_interval_minutes)?;
        self.inner
            .sync_interval_minutes
            .store(u64::from(sync_interval_minutes), Ordering::SeqCst);
        self.inner.rearm_attached_accounts(false)
    }

    pub fn resume(&self) -> Result<(), SyncRuntimeError> {
        self.inner.rearm_attached_accounts(true)
    }

    pub fn account_detached(&self, account_id: &str) {
        self.inner.cancel_schedule(account_id);
    }

    pub fn state(&self, account_id: &str) -> Result<SyncExecutionView, SyncRuntimeError> {
        self.inner.state(account_id)
    }

    pub fn list(&self) -> Result<Vec<SyncExecutionView>, SyncRuntimeError> {
        self.inner
            .registry
            .list()?
            .into_iter()
            .map(|account| self.inner.state(&account.profile.account_id))
            .collect()
    }

    pub fn shutdown(&self) {
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

impl Drop for SyncExecutionRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl RuntimeInner {
    /// One periodic deadline per freshly attached account. Replacing an earlier
    /// schedule keeps the same "nearest deadline per account" rule the rest of
    /// the runtime follows.
    fn arm_initial_schedule(self: &Arc<Self>, account_id: &str) -> Result<(), SyncRuntimeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(SyncRuntimeError::Unavailable);
        }
        let attachment_generation = self
            .registry
            .attachment_generation(account_id)
            .ok_or(SyncRuntimeError::Detached)?;
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        let periodic_at = add_ms(now, self.periodic_delay_ms())?;
        self.cancel_schedule(account_id);
        self.arm_schedule(
            account_id,
            attachment_generation,
            periodic_at,
            ScheduledSyncCause::Periodic,
        )
    }

    fn trigger(
        self: &Arc<Self>,
        account_id: &str,
        trigger: SyncTrigger,
    ) -> Result<SyncExecutionView, SyncRuntimeError> {
        self.trigger_with_completion(account_id, trigger, None)
    }

    fn trigger_with_completion(
        self: &Arc<Self>,
        account_id: &str,
        trigger: SyncTrigger,
        completion: Option<(
            u64,
            std::sync::mpsc::Sender<Result<ConfirmedReadSync, SyncRuntimeError>>,
        )>,
    ) -> Result<SyncExecutionView, SyncRuntimeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(SyncRuntimeError::Unavailable);
        }
        if completion.as_ref().is_some_and(|(generation, _)| {
            !self.registry.is_current_attachment(account_id, *generation)
        }) {
            return Err(SyncRuntimeError::Detached);
        }
        let triggered = self.registry.trigger_versioned_expected(
            account_id,
            trigger,
            completion.as_ref().map(|(generation, _)| *generation),
        )?;
        if completion
            .as_ref()
            .is_some_and(|(generation, _)| *generation != triggered.attachment_generation)
        {
            return Err(SyncRuntimeError::Detached);
        }
        let request_generation = self.next_request_generation.fetch_add(1, Ordering::SeqCst);
        let view = running_view(account_id, trigger);
        self.states
            .lock()
            .map_err(|_| SyncRuntimeError::Unavailable)?
            .insert(
                account_id.to_owned(),
                StoredState {
                    attachment_generation: triggered.attachment_generation,
                    request_generation,
                    view: view.clone(),
                },
            );
        self.state_emitter.emit(view.clone());
        let weak = Arc::downgrade(self);
        let account_id = account_id.to_owned();
        thread::Builder::new()
            .name("calendar-todo-sync-result".into())
            .spawn(move || {
                if let Ok(report) = triggered.report.recv() {
                    let successful = matches!(report.outcome, CoordinatorOutcome::Success { .. });
                    apply_report(
                        weak.clone(),
                        account_id.clone(),
                        triggered.attachment_generation,
                        request_generation,
                        report,
                    );
                    if let Some((_, sender)) = completion {
                        let success = ConfirmedReadSync {
                            runtime: weak,
                            account: account_id,
                            attachment_generation: triggered.attachment_generation,
                        };
                        let result = if !successful {
                            Err(SyncRuntimeError::Unavailable)
                        } else if success.current() {
                            Ok(success)
                        } else {
                            Err(SyncRuntimeError::Detached)
                        };
                        let _ = sender.send(result);
                    }
                }
            })
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        Ok(view)
    }

    fn state(&self, account_id: &str) -> Result<SyncExecutionView, SyncRuntimeError> {
        self.registry.profile(account_id)?;
        let Some(attachment_generation) = self.registry.attachment_generation(account_id) else {
            return Ok(empty_view(account_id, SyncExecutionState::Detached));
        };
        let states = self
            .states
            .lock()
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        Ok(states
            .get(account_id)
            .filter(|state| state.attachment_generation == attachment_generation)
            .map(|state| state.view.clone())
            .unwrap_or_else(|| empty_view(account_id, SyncExecutionState::Idle)))
    }

    fn rearm_attached_accounts(self: &Arc<Self>, fire_due: bool) -> Result<(), SyncRuntimeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(SyncRuntimeError::Unavailable);
        }
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        for account in self.registry.list()? {
            let account_id = account.profile.account_id;
            let Some(attachment_generation) = self.registry.attachment_generation(&account_id)
            else {
                self.cancel_schedule(&account_id);
                continue;
            };
            let existing = self
                .schedules
                .lock()
                .map_err(|_| SyncRuntimeError::Unavailable)?
                .get(&account_id)
                .cloned();
            if fire_due
                && existing
                    .as_ref()
                    .is_some_and(|deadline| deadline.deadline_at_utc_ms <= now)
            {
                if let Some(deadline) = existing {
                    self.accept_deadline(deadline);
                }
                continue;
            }
            let periodic_at = add_ms(now, self.periodic_delay_ms())?;
            let selected = existing
                .filter(|deadline| {
                    deadline.attachment_generation == attachment_generation
                        && deadline.cause == ScheduledSyncCause::Retry
                        && deadline.deadline_at_utc_ms < periodic_at
                })
                .map(|deadline| (deadline.deadline_at_utc_ms, deadline.cause))
                .unwrap_or((periodic_at, ScheduledSyncCause::Periodic));
            self.arm_schedule(&account_id, attachment_generation, selected.0, selected.1)?;
        }
        Ok(())
    }

    fn schedule_after_report(
        &self,
        account_id: &str,
        attachment_generation: u64,
        report: &CoordinatorReport,
    ) -> Result<(), SyncRuntimeError> {
        let now = self
            .clock
            .now_utc_ms()
            .map_err(|_| SyncRuntimeError::Unavailable)?;
        let periodic_at = add_ms(now, self.periodic_delay_ms())?;
        let (deadline, cause) = report
            .next_retry_delay_ms
            .and_then(|delay| add_ms(now, delay).ok())
            .filter(|retry_at| *retry_at <= periodic_at)
            .map(|retry_at| (retry_at, ScheduledSyncCause::Retry))
            .unwrap_or((periodic_at, ScheduledSyncCause::Periodic));
        self.arm_schedule(account_id, attachment_generation, deadline, cause)
    }

    fn periodic_delay_ms(&self) -> u64 {
        self.sync_interval_minutes
            .load(Ordering::SeqCst)
            .saturating_mul(60_000)
    }

    fn arm_schedule(
        &self,
        account_id: &str,
        attachment_generation: u64,
        deadline_at_utc_ms: i64,
        cause: ScheduledSyncCause,
    ) -> Result<(), SyncRuntimeError> {
        let schedule_generation = self.next_schedule_generation.fetch_add(1, Ordering::SeqCst);
        let deadline = ScheduledSyncDeadline {
            account_id: account_id.to_owned(),
            attachment_generation,
            schedule_generation,
            deadline_at_utc_ms,
            cause,
        };
        self.schedules
            .lock()
            .map_err(|_| SyncRuntimeError::Unavailable)?
            .insert(account_id.to_owned(), deadline.clone());
        self.scheduler
            .arm(deadline)
            .map_err(|_| SyncRuntimeError::Unavailable)
    }

    fn cancel_schedule(&self, account_id: &str) {
        if let Ok(mut schedules) = self.schedules.lock() {
            schedules.remove(account_id);
        }
        let _ = self.scheduler.cancel(account_id);
    }

    fn accept_deadline(self: &Arc<Self>, deadline: ScheduledSyncDeadline) {
        if self.stopped.load(Ordering::SeqCst)
            || !self
                .registry
                .is_current_attachment(&deadline.account_id, deadline.attachment_generation)
        {
            return;
        }
        let accepted = self.schedules.lock().is_ok_and(|mut schedules| {
            let is_current = schedules.get(&deadline.account_id).is_some_and(|current| {
                current.attachment_generation == deadline.attachment_generation
                    && current.schedule_generation == deadline.schedule_generation
                    && current.deadline_at_utc_ms == deadline.deadline_at_utc_ms
                    && current.cause == deadline.cause
            });
            if is_current {
                schedules.remove(&deadline.account_id);
            }
            is_current
        });
        if accepted {
            let trigger = match deadline.cause {
                ScheduledSyncCause::Periodic => SyncTrigger::Periodic,
                ScheduledSyncCause::Retry => SyncTrigger::Retry,
            };
            let _ = self.trigger(&deadline.account_id, trigger);
        }
    }
}

fn apply_report(
    weak: Weak<RuntimeInner>,
    account_id: String,
    attachment_generation: u64,
    request_generation: u64,
    report: CoordinatorReport,
) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    if inner.stopped.load(Ordering::SeqCst)
        || !inner
            .registry
            .is_current_attachment(&account_id, attachment_generation)
    {
        return;
    }
    let view = completed_view(report.clone());
    let accepted = inner.states.lock().is_ok_and(|mut states| {
        let Some(current) = states.get_mut(&account_id) else {
            return false;
        };
        if current.attachment_generation != attachment_generation
            || current.request_generation != request_generation
        {
            return false;
        }
        current.view = view.clone();
        true
    });
    if !accepted {
        return;
    }
    let _ = inner.schedule_after_report(&account_id, attachment_generation, &report);
    inner.state_emitter.emit(view.clone());
    if view.state == SyncExecutionState::Succeeded {
        inner.agenda_emitter.emit(AgendaRefreshNotice {
            reason: AgendaRefreshReason::CalendarSync,
        });
    }
}

fn dispatch_deadlines(inner: Weak<RuntimeInner>, receiver: Receiver<ScheduledSyncDeadline>) {
    while let Ok(deadline) = receiver.recv() {
        let Some(inner) = inner.upgrade() else {
            break;
        };
        inner.accept_deadline(deadline);
    }
}

fn validate_interval(sync_interval_minutes: u16) -> Result<(), SyncRuntimeError> {
    if !(MIN_SYNC_INTERVAL_MINUTES..=MAX_SYNC_INTERVAL_MINUTES).contains(&sync_interval_minutes) {
        return Err(SyncRuntimeError::InvalidSchedule);
    }
    Ok(())
}

fn add_ms(now: i64, delay_ms: u64) -> Result<i64, SyncRuntimeError> {
    let delay = i64::try_from(delay_ms).map_err(|_| SyncRuntimeError::InvalidSchedule)?;
    now.checked_add(delay)
        .ok_or(SyncRuntimeError::InvalidSchedule)
}

fn empty_view(account_id: &str, state: SyncExecutionState) -> SyncExecutionView {
    SyncExecutionView {
        account_id: account_id.to_owned(),
        state,
        trigger: None,
        path: None,
        failure: None,
        coalesced_requests: 0,
        next_retry_delay_ms: None,
    }
}

fn running_view(account_id: &str, trigger: SyncTrigger) -> SyncExecutionView {
    SyncExecutionView {
        account_id: account_id.to_owned(),
        state: SyncExecutionState::Running,
        trigger: Some(map_trigger(trigger)),
        path: None,
        failure: None,
        coalesced_requests: 0,
        next_retry_delay_ms: None,
    }
}

fn completed_view(report: CoordinatorReport) -> SyncExecutionView {
    let (state, path, failure) = match report.outcome {
        CoordinatorOutcome::Success { path } => {
            (SyncExecutionState::Succeeded, Some(map_path(path)), None)
        }
        CoordinatorOutcome::Failure { class } => (
            if class == SyncFailureClass::Unauthorized {
                SyncExecutionState::ActionRequired
            } else {
                SyncExecutionState::Failed
            },
            None,
            map_failure(class),
        ),
    };
    SyncExecutionView {
        account_id: report.account_id,
        state,
        trigger: Some(map_trigger(report.trigger)),
        path,
        failure,
        coalesced_requests: report.coalesced_requests,
        next_retry_delay_ms: report.next_retry_delay_ms,
    }
}

fn map_trigger(trigger: SyncTrigger) -> SyncExecutionTrigger {
    match trigger {
        SyncTrigger::Startup => SyncExecutionTrigger::Startup,
        SyncTrigger::Manual => SyncExecutionTrigger::Manual,
        SyncTrigger::Periodic => SyncExecutionTrigger::Periodic,
        SyncTrigger::Retry => SyncExecutionTrigger::Retry,
    }
}

fn map_path(path: IncrementalSyncPath) -> SyncExecutionPath {
    match path {
        IncrementalSyncPath::Incremental => SyncExecutionPath::Incremental,
        IncrementalSyncPath::FullInventoryFallback => SyncExecutionPath::FullInventoryFallback,
    }
}

fn map_failure(class: SyncFailureClass) -> Option<SyncExecutionFailure> {
    match class {
        SyncFailureClass::None => None,
        SyncFailureClass::Transient => Some(SyncExecutionFailure::Transient),
        SyncFailureClass::Unauthorized => Some(SyncExecutionFailure::Unauthorized),
        SyncFailureClass::Incomplete => Some(SyncExecutionFailure::Incomplete),
        SyncFailureClass::UnsafeData => Some(SyncExecutionFailure::UnsafeData),
        SyncFailureClass::Validation => Some(SyncExecutionFailure::Validation),
        SyncFailureClass::Persistence => Some(SyncExecutionFailure::Persistence),
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SyncRuntimeError {
    #[error("account identity is invalid")]
    InvalidIdentity,
    #[error("account is disabled")]
    Disabled,
    #[error("account source is detached")]
    Detached,
    #[error("synchronization schedule is invalid")]
    InvalidSchedule,
    #[error("synchronization runtime is unavailable")]
    Unavailable,
}

impl From<AccountRegistryError> for SyncRuntimeError {
    fn from(error: AccountRegistryError) -> Self {
        match error {
            AccountRegistryError::InvalidIdentity | AccountRegistryError::IdentityMismatch => {
                Self::InvalidIdentity
            }
            AccountRegistryError::Disabled => Self::Disabled,
            AccountRegistryError::Cancelled => Self::Unavailable,
            AccountRegistryError::SourceUnavailable => Self::Detached,
            AccountRegistryError::Unavailable => Self::Unavailable,
        }
    }
}
