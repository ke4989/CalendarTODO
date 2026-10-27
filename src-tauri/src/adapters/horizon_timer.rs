use crate::ports::clock::Clock;
use std::{
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};
use thiserror::Error;

use crate::application::recurrence_runtime::{DeadlineScheduler, SchedulerError};

enum Command {
    Arm(i64),
    Cancel,
    Shutdown,
}

pub struct SystemDeadlineScheduler {
    sender: mpsc::Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SystemDeadlineScheduler {
    pub fn new(
        clock: Arc<dyn Clock>,
        deadline_sender: mpsc::Sender<()>,
    ) -> Result<Self, TimerStartError> {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("calendar-todo-horizon-deadline".into())
            .spawn(move || run_timer(clock, deadline_sender, &receiver))
            .map_err(|_| TimerStartError::Unavailable)?;
        Ok(Self {
            sender,
            worker: Mutex::new(Some(worker)),
        })
    }
}

impl DeadlineScheduler for SystemDeadlineScheduler {
    fn arm(&self, deadline_at_utc_ms: i64) -> Result<(), SchedulerError> {
        self.sender
            .send(Command::Arm(deadline_at_utc_ms))
            .map_err(|_| SchedulerError::Unavailable)
    }

    fn cancel(&self) -> Result<(), SchedulerError> {
        self.sender
            .send(Command::Cancel)
            .map_err(|_| SchedulerError::Unavailable)
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
    deadline_sender: mpsc::Sender<()>,
    receiver: &mpsc::Receiver<Command>,
) {
    let mut deadline: Option<i64> = None;
    loop {
        let command = match deadline {
            Some(deadline_at) => match clock.now_utc_ms() {
                Ok(now) if deadline_at <= now => {
                    deadline = None;
                    let _ = deadline_sender.send(());
                    continue;
                }
                Ok(now) => {
                    let wait_ms =
                        u64::try_from(deadline_at.saturating_sub(now)).unwrap_or(u64::MAX);
                    match receiver.recv_timeout(Duration::from_millis(wait_ms)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            deadline = None;
                            let _ = deadline_sender.send(());
                            continue;
                        }
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
            Command::Arm(value) => deadline = Some(value),
            Command::Cancel => deadline = None,
            Command::Shutdown => break,
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum TimerStartError {
    #[error("deadline scheduler is unavailable")]
    Unavailable,
}
