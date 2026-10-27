use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const DEFAULT_SYNC_INTERVAL_MINUTES: u16 = 15;
pub const MIN_SYNC_INTERVAL_MINUTES: u16 = 5;
pub const MAX_SYNC_INTERVAL_MINUTES: u16 = 1_440;
pub const DEFAULT_DESKTOP_OPACITY_PERCENT: u8 = 82;
/// First-run presentation defaults. These mirror `MIGRATION_057`, which is the
/// only writer for a profile whose settings row was never saved.
pub const DEFAULT_THEME_PALETTE: ThemePalette = ThemePalette::QuietDaybook;
pub const DEFAULT_COLOR_MODE: ColorMode = ColorMode::Light;
pub const MIN_DESKTOP_OPACITY_PERCENT: u8 = 20;
pub const DEFAULT_LOCAL_TODO_COLOR: &str = "#2F7D68";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum AppLanguage {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl AppLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }
    pub fn from_storage(value: &str) -> Result<Self, SettingsError> {
        match value {
            "system" => Ok(Self::System),
            "zh-CN" => Ok(Self::ZhCn),
            "en" => Ok(Self::En),
            _ => Err(SettingsError::CorruptLanguage),
        }
    }
}

/// Presentation-only palette choice. It never reaches calendar data,
/// synchronization, reminders, cache or mutation admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ThemePalette {
    #[serde(rename = "quietDaybook")]
    QuietDaybook,
    #[serde(rename = "dawnWheat")]
    DawnWheat,
}

impl ThemePalette {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuietDaybook => "quietDaybook",
            Self::DawnWheat => "dawnWheat",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, SettingsError> {
        match value {
            "quietDaybook" => Ok(Self::QuietDaybook),
            "dawnWheat" => Ok(Self::DawnWheat),
            _ => Err(SettingsError::CorruptThemePalette),
        }
    }
}

/// Presentation-only light/dark preference. `System` is resolved independently
/// in every webview from the current Windows color-scheme media query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ColorMode {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "dark")]
    Dark,
}

impl ColorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, SettingsError> {
        match value {
            "system" => Ok(Self::System),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err(SettingsError::CorruptColorMode),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMode {
    DesktopCompanion,
    AlwaysOnTop,
}

impl WindowMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DesktopCompanion => "desktop_companion",
            Self::AlwaysOnTop => "always_on_top",
        }
    }

    pub fn from_storage(value: &str) -> Result<Self, SettingsError> {
        match value {
            "desktop_companion" => Ok(Self::DesktopCompanion),
            "always_on_top" => Ok(Self::AlwaysOnTop),
            // `normal_window` was removed; migration 017 rewrites existing rows,
            // so seeing it here means the stored value is not ours.
            _ => Err(SettingsError::CorruptWindowMode),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub language: AppLanguage,
    pub theme_palette: ThemePalette,
    pub color_mode: ColorMode,
    pub window_mode: WindowMode,
    pub sync_interval_minutes: u16,
    pub sync_on_startup: bool,
    pub desktop_list_mode: bool,
    pub desktop_opacity_percent: u8,
    pub desktop_show_calendar_name: bool,
    pub desktop_show_time: bool,
    pub desktop_show_tags: bool,
    pub main_show_calendar_name: bool,
    pub main_show_time: bool,
    pub main_show_tags: bool,
    /// When set, a launch keeps `main` hidden in the tray instead of showing
    /// the framed Agenda window. Every window still restores its stored
    /// placement, so the later explicit show action opens it where it was.
    pub start_hidden_in_tray: bool,
    /// Registers the current executable in the per-user Windows `Run` key.
    pub launch_at_login: bool,
    pub local_todo_color: String,
    /// Optional local Agenda presentation bounds. `None` is unbounded and does
    /// not affect reminder scheduling or stored TODO data.
    pub local_display_past_days: Option<u16>,
    pub local_display_future_days: Option<u16>,
    /// Stable local or CalDAV calendar identity used to seed new-item forms.
    /// Availability is resolved by each surface at open time.
    pub default_calendar_id: Option<String>,
    /// Position lock of the desktop list, toggled from its own header. It is
    /// owned by the desktop-list commands, never by the Settings form.
    pub desktop_pinned: bool,
    /// Whether the desktop list was left folded. Only the presentation choice
    /// is remembered; the folded rectangle is never a restore target.
    pub desktop_collapsed: bool,
    /// Whether due reminders are delivered as Windows notifications. The
    /// durable projection keeps tracking edits while this is off; only delivery
    /// authority and the scheduled wake are withdrawn.
    pub reminder_notifications_enabled: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAppSettings {
    pub language: AppLanguage,
    pub theme_palette: ThemePalette,
    pub color_mode: ColorMode,
    pub window_mode: WindowMode,
    pub sync_interval_minutes: u16,
    pub sync_on_startup: bool,
    pub desktop_list_mode: bool,
    pub desktop_opacity_percent: u8,
    pub desktop_show_calendar_name: bool,
    pub desktop_show_time: bool,
    pub desktop_show_tags: bool,
    pub main_show_calendar_name: bool,
    pub main_show_time: bool,
    pub main_show_tags: bool,
    pub start_hidden_in_tray: bool,
    pub launch_at_login: bool,
    pub local_todo_color: String,
    pub local_display_past_days: Option<u16>,
    pub local_display_future_days: Option<u16>,
    pub default_calendar_id: Option<String>,
    pub reminder_notifications_enabled: bool,
}

impl UpdateAppSettings {
    pub fn validate(self) -> Result<Self, SettingsError> {
        if !(MIN_SYNC_INTERVAL_MINUTES..=MAX_SYNC_INTERVAL_MINUTES)
            .contains(&self.sync_interval_minutes)
        {
            return Err(SettingsError::InvalidSyncInterval);
        }
        if !(MIN_DESKTOP_OPACITY_PERCENT..=100).contains(&self.desktop_opacity_percent) {
            return Err(SettingsError::InvalidDesktopOpacity);
        }
        if !valid_color(&self.local_todo_color) {
            return Err(SettingsError::InvalidLocalTodoColor);
        }
        if self
            .local_display_past_days
            .is_some_and(|value| value > 3650)
            || self
                .local_display_future_days
                .is_some_and(|value| value > 3650)
        {
            return Err(SettingsError::InvalidLocalDisplayRange);
        }
        if self.default_calendar_id.as_deref().is_some_and(|value| {
            !Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
        }) {
            return Err(SettingsError::InvalidDefaultCalendar);
        }
        Ok(self)
    }
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SettingsError {
    #[error("sync interval is outside the supported range")]
    InvalidSyncInterval,
    #[error("desktop background opacity is outside the supported range")]
    InvalidDesktopOpacity,
    #[error("local TODO color is invalid")]
    InvalidLocalTodoColor,
    #[error("local Agenda display range is outside the supported range")]
    InvalidLocalDisplayRange,
    #[error("default calendar identifier is invalid")]
    InvalidDefaultCalendar,
    #[error("stored window mode is invalid")]
    CorruptWindowMode,
    #[error("stored application language is invalid")]
    CorruptLanguage,
    #[error("stored theme palette is invalid")]
    CorruptThemePalette,
    #[error("stored color mode is invalid")]
    CorruptColorMode,
}
