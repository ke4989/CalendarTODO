//! Pure episode decisions for the Windows observer's keyed one-shot deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    DesktopForeground,
    Minimized,
    ShellCloaked,
    Uncloaked,
    OtherForeground,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Prepare,
    Restore,
    /// Opaque recovery after reveal preparation failed or timed out. The
    /// renderer token is tombstoned first so a late script cannot start a fade.
    Fallback,
    Verify,
    Cancel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Request {
    pub generation: u64,
    pub episode: u64,
    pub attempt: u8,
    pub action: Action,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    None,
    Arm,
    Execute(Request),
    Stop,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Pending,
    Executing(Action),
    Verifying,
    Settled(bool),
}

pub(super) struct Recovery {
    generation: u64,
    episode: u64,
    attempt: u8,
    phase: Phase,
}
impl Recovery {
    pub fn new() -> Self {
        Self {
            generation: 0,
            episode: 0,
            attempt: 0,
            phase: Phase::Idle,
        }
    }
    pub fn cancel(&mut self, generation: u64) -> Effect {
        self.generation = generation;
        self.phase = Phase::Idle;
        self.episode += 1;
        Effect::Stop
    }
    pub fn signal(&mut self, signal: Signal, generation: u64) -> Effect {
        if self.generation != generation {
            self.cancel(generation);
        }
        if signal == Signal::OtherForeground {
            return self.cancel(generation);
        }
        // Uncloak is evidence, never new execution authority. The existing
        // verification deadline reads it together with live visibility/Z-order.
        if signal == Signal::Uncloaked {
            return Effect::None;
        }
        match self.phase {
            Phase::Idle => {}
            // A verified success may be followed by the next real cloak
            // episode. A failed/exhausted episode ignores cloak echoes; a
            // foreground/minimize signal can still establish a new episode.
            Phase::Settled(true) if signal == Signal::ShellCloaked => {}
            Phase::Settled(false) if signal == Signal::ShellCloaked => return Effect::None,
            Phase::Settled(_) => {}
            _ => return Effect::None,
        }
        self.episode += 1;
        self.attempt = 0;
        self.phase = Phase::Pending;
        Effect::Arm
    }
    fn request(&self, action: Action) -> Request {
        Request {
            generation: self.generation,
            episode: self.episode,
            attempt: self.attempt,
            action,
        }
    }
    pub fn deadline(&mut self) -> Effect {
        let action = match self.phase {
            Phase::Pending => {
                self.attempt = 1;
                Action::Prepare
            }
            Phase::Executing(Action::Prepare) => Action::Fallback,
            Phase::Verifying => Action::Verify,
            _ => return Effect::None,
        };
        self.phase = Phase::Executing(action);
        Effect::Execute(self.request(action))
    }
    /// None means live intent/ownership/context denied the queued action.
    pub fn completed(&mut self, request: Request, healthy: Option<bool>) -> Effect {
        if request != self.request(request.action) || self.phase != Phase::Executing(request.action)
        {
            return Effect::None;
        }
        let Some(healthy) = healthy else {
            self.phase = Phase::Settled(false);
            return Effect::Stop;
        };
        if request.action == Action::Prepare {
            let next = if healthy {
                Action::Restore
            } else {
                Action::Fallback
            };
            self.phase = Phase::Executing(next);
            return Effect::Execute(self.request(next));
        }
        if matches!(request.action, Action::Restore | Action::Fallback) {
            // Even an immediate success gets one later observation: Shell can
            // complete its own transition after our synchronous native calls.
            self.phase = Phase::Verifying;
            return Effect::Arm;
        }
        if healthy || self.attempt == 2 {
            self.phase = Phase::Settled(healthy);
            Effect::Stop
        } else {
            self.attempt += 1;
            self.phase = Phase::Executing(Action::Restore);
            Effect::Execute(self.request(Action::Restore))
        }
    }
}

