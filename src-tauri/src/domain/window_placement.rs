use thiserror::Error;

/// Largest logical offset or size accepted for a stored placement. The value is
/// far above any real desktop layout and exists only so corrupt or hostile input
/// cannot drive an unbounded window move.
pub const MAX_SUPPORTED_DIP: f64 = 32_768.0;
const MAX_MONITOR_NAME_LEN: usize = 128;

/// A persisted window rectangle expressed as logical (DIP) offsets from the
/// origin of the monitor that owned the window.
///
/// Storing monitor-relative DIP values instead of raw desktop pixels keeps the
/// same perceived position and size across resolution, taskbar, multi-monitor,
/// and DPI changes. Raw pixels would drift on every scale-factor change and
/// could not be clamped meaningfully after a monitor disappears.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowPlacement {
    pub monitor_name: Option<String>,
    pub x_dip: f64,
    pub y_dip: f64,
    pub width_dip: f64,
    pub height_dip: f64,
    pub maximized: bool,
}

impl WindowPlacement {
    pub fn validate(self) -> Result<Self, WindowPlacementError> {
        if !self.x_dip.is_finite()
            || !self.y_dip.is_finite()
            || !self.width_dip.is_finite()
            || !self.height_dip.is_finite()
        {
            return Err(WindowPlacementError::NotFinite);
        }
        if self
            .monitor_name
            .as_ref()
            .is_some_and(|name| name.len() > MAX_MONITOR_NAME_LEN)
        {
            return Err(WindowPlacementError::InvalidMonitorName);
        }
        let offset_in_range = |value: f64| value.abs() <= MAX_SUPPORTED_DIP;
        let size_in_range = |value: f64| (1.0..=MAX_SUPPORTED_DIP).contains(&value);
        if !offset_in_range(self.x_dip)
            || !offset_in_range(self.y_dip)
            || !size_in_range(self.width_dip)
            || !size_in_range(self.height_dip)
        {
            return Err(WindowPlacementError::OutOfRange);
        }
        Ok(self)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WindowPlacementError {
    #[error("window placement contains a non-finite value")]
    NotFinite,
    #[error("window placement monitor name is invalid")]
    InvalidMonitorName,
    #[error("window placement is outside the supported range")]
    OutOfRange,
}
