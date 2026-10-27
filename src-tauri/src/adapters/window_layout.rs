use crate::{
    adapters::{
        persistence::{Database, StoredWindowPlacement},
        window::{self, PlacementTarget},
    },
    domain::window_placement::WindowPlacement,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};
use thiserror::Error;

/// Windows whose geometry survives a restart. `tray_menu` is excluded on
/// purpose: it is anchored to the tray icon and sized from its own content, so a
/// stored rectangle would be both wrong and unclampable after a content change.
pub const TRACKED_WINDOWS: [&str; 2] = ["main", "desktop"];

/// A move or resize is written only after the user stops dragging, so one drag
/// cannot turn into hundreds of actor transactions.
const SETTLE_DELAY: Duration = Duration::from_millis(400);

enum Command {
    Wake,
    Shutdown,
}

/// Merges a fresh snapshot with the last stored placement.
///
/// A maximized or collapsed window reports a temporary rectangle that must not
/// become the next launch's restore target, so those snapshots keep the previous
/// bounds and only update the maximized flag.
pub fn merge_snapshot(
    previous: Option<&WindowPlacement>,
    snapshot: WindowPlacement,
    keep_bounds: bool,
) -> WindowPlacement {
    match (keep_bounds, previous) {
        (true, Some(previous)) => WindowPlacement {
            maximized: snapshot.maximized,
            ..previous.clone()
        },
        _ => snapshot,
    }
}

/// Records window geometry on a worker thread and persists the settled result
/// through the SQLite actor. Nothing here schedules or polls: a write happens
/// only after a real window event has stopped arriving.
pub struct WindowLayoutRecorder {
    current: Arc<Mutex<HashMap<&'static str, WindowPlacement>>>,
    pending: Arc<Mutex<Vec<&'static str>>>,
    sender: mpsc::Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl WindowLayoutRecorder {
    /// `stored` is whatever the previous session persisted; it seeds the cache
    /// so the first event after a startup restore does not rewrite the same row.
    pub fn start(
        database: Database,
        stored: Vec<StoredWindowPlacement>,
    ) -> Result<Self, WindowLayoutError> {
        let current = stored
            .into_iter()
            .filter_map(|item| {
                TRACKED_WINDOWS
                    .iter()
                    .copied()
                    .find(|label| *label == item.window_label)
                    .map(|label| (label, item.placement))
            })
            .collect::<HashMap<_, _>>();
        let current = Arc::new(Mutex::new(current));
        let pending = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("calendar-todo-window-layout".into())
            .spawn({
                let current = current.clone();
                let pending = pending.clone();
                move || run_recorder(database, receiver, current, pending)
            })
            .map_err(|_| WindowLayoutError::Unavailable)?;
        Ok(Self {
            current,
            pending,
            sender,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Captures one moved or resized window. `collapsed` marks a window that is
    /// deliberately too short to be a restore target (the collapsed desktop
    /// list), which keeps its last expanded bounds.
    pub fn observe(&self, window: &impl PlacementTarget, collapsed: bool) {
        let Some(label) = TRACKED_WINDOWS
            .iter()
            .copied()
            .find(|label| *label == window.label())
        else {
            return;
        };
        let Some(snapshot) = window::capture_placement(window) else {
            return;
        };
        let keep_bounds = collapsed || snapshot.maximized;
        let changed = {
            let Ok(mut current) = self.current.lock() else {
                return;
            };
            let merged = merge_snapshot(current.get(label), snapshot, keep_bounds);
            if current.get(label) == Some(&merged) {
                false
            } else {
                current.insert(label, merged);
                true
            }
        };
        if !changed {
            return;
        }
        if let Ok(mut pending) = self.pending.lock()
            && !pending.contains(&label)
        {
            pending.push(label);
        }
        let _ = self.sender.send(Command::Wake);
    }

    /// Writes any settled-but-unwritten geometry and stops the worker. Called on
    /// explicit quit so a move made within the settle window is not lost.
    pub fn shutdown(&self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn run_recorder(
    database: Database,
    receiver: mpsc::Receiver<Command>,
    current: Arc<Mutex<HashMap<&'static str, WindowPlacement>>>,
    pending: Arc<Mutex<Vec<&'static str>>>,
) {
    // The worker persists after every quiet period and once more when it is told
    // to stop, so an explicit quit cannot lose a move made inside that window.
    loop {
        let mut shutdown = match receiver.recv() {
            Ok(Command::Wake) => false,
            Ok(Command::Shutdown) | Err(_) => true,
        };
        if !shutdown {
            // Coalesce every event that arrives before the window settles.
            loop {
                match receiver.recv_timeout(SETTLE_DELAY) {
                    Ok(Command::Wake) => continue,
                    Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        shutdown = true;
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
        }
        persist(&database, &current, &pending);
        if shutdown {
            break;
        }
    }
}

fn persist(
    database: &Database,
    current: &Arc<Mutex<HashMap<&'static str, WindowPlacement>>>,
    pending: &Arc<Mutex<Vec<&'static str>>>,
) {
    let labels = match pending.lock() {
        Ok(mut pending) => std::mem::take(&mut *pending),
        Err(_) => return,
    };
    for label in labels {
        let placement = current
            .lock()
            .ok()
            .and_then(|current| current.get(label).cloned());
        if let Some(placement) = placement {
            // A failed write is not retried in a loop: the cached rectangle stays
            // current, so the next real move or resize records it again.
            let _ = database.save_window_placement(label, placement);
        }
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WindowLayoutError {
    #[error("window layout recorder is unavailable")]
    Unavailable,
}
