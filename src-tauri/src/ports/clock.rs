use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Bounded failure of a wall-clock read.
///
/// The error carries no platform detail: callers only need to know that the
/// current instant could not be established.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    #[error("the system clock is unavailable")]
    Unavailable,
}

/// Supplies the current UTC instant.
///
/// Time-driven work takes this port instead of reading the platform clock so
/// horizon, rollover, and resume decisions stay deterministic in tests.
pub trait Clock: Send + Sync + 'static {
    fn now_utc_ms(&self) -> Result<i64, ClockError>;
}

/// Wall-clock implementation backed by the operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc_ms(&self) -> Result<i64, ClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ClockError::Unavailable)?
            .as_millis();
        i64::try_from(millis).map_err(|_| ClockError::Unavailable)
    }
}
