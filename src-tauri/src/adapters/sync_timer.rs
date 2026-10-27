use crate::{
    application::sync_runtime::{ScheduledSyncDeadline, SyncDeadlineScheduler, SyncSchedulerError},
    ports::clock::Clock,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};
use thiserror::Error;

enum Command {
    Arm(ScheduledSyncDeadline),
    Cancel(String),
    Shutdown,
}

/// Owns one replaceable deadline per account and sleeps only until the nearest
/// registered instant. All time is sourced through the injected UTC clock.
pub struct SystemSyncDeadlineScheduler {
    sender: mpsc::Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SystemSyncDeadlineScheduler {
    pub fn new(
        clock: Arc<dyn Clock>,
        fired_sender: mpsc::Sender<ScheduledSyncDeadline>,
    ) -> Result<Self, SyncTimerStartError> {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("calendar-todo-sync-deadline".into())
            .spawn(move || run_timer(clock, fired_sender, &receiver))
            .map_err(|_| SyncTimerStartError::Unavailable)?;
        Ok(Self {
            sender,
            worker: Mutex::new(Some(worker)),
        })
    }
}

impl SyncDeadlineScheduler for SystemSyncDeadlineScheduler {
    fn arm(&self, deadline: ScheduledSyncDeadline) -> Result<(), SyncSchedulerError> {
        self.sender
            .send(Command::Arm(deadline))
            .map_err(|_| SyncSchedulerError::Unavailable)
    }

    fn cancel(&self, account_id: &str) -> Result<(), SyncSchedulerError> {
        self.sender
            .send(Command::Cancel(account_id.to_owned()))
            .map_err(|_| SyncSchedulerError::Unavailable)
    }

    fn shutdown(&self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn run_timer(
    clock: Arc<dyn Clock>,
    fired_sender: mpsc::Sender<ScheduledSyncDeadline>,
    receiver: &mpsc::Receiver<Command>,
) {
    let mut deadlines = HashMap::<String, ScheduledSyncDeadline>::new();
    loop {
        let nearest = deadlines
            .values()
            .min_by_key(|item| item.deadline_at_utc_ms)
            .cloned();
        let command = match nearest {
            Some(deadline) => match clock.now_utc_ms() {
                Ok(now) if deadline.deadline_at_utc_ms <= now => {
                    let due = deadlines
                        .remove(&deadline.account_id)
                        .expect("deadline exists");
                    let _ = fired_sender.send(due);
                    continue;
                }
                Ok(now) => {
                    let wait = u64::try_from(deadline.deadline_at_utc_ms.saturating_sub(now))
                        .unwrap_or(u64::MAX);
                    match receiver.recv_timeout(Duration::from_millis(wait)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Err(_) => match receiver.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                },
            },
            None => match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        match command {
            Command::Arm(deadline) => {
                deadlines.insert(deadline.account_id.clone(), deadline);
            }
            Command::Cancel(account_id) => {
                deadlines.remove(&account_id);
            }
            Command::Shutdown => break,
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SyncTimerStartError {
    #[error("sync deadline scheduler is unavailable")]
    Unavailable,
}
