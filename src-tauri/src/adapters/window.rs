use crate::domain::settings::WindowMode;
use crate::domain::window_placement::WindowPlacement;
use tauri::{Monitor, PhysicalPosition, PhysicalSize, Runtime, WebviewWindow, Window};

#[cfg(windows)]
use windows_sys::Win32::{
    Graphics::Gdi::{RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
    },
};


/// Smallest and largest popover height a tray menu page may request. The value
/// is a bound on untrusted content measurement, not a layout preference.
pub const TRAY_MENU_MIN_HEIGHT_DIP: f64 = 96.0;
pub const TRAY_MENU_MAX_HEIGHT_DIP: f64 = 760.0;
/// Configured popover width, used when a window reports no usable width.
pub const TRAY_MENU_WIDTH_DIP: f64 = 216.0;
const TRAY_POPOVER_GAP_X: i32 = 12;
const TRAY_POPOVER_GAP_Y: i32 = 8;

/// A physical-pixel rectangle. Used for both monitor work areas and the bounds
/// computed for a stored placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Work area and scale factor of one monitor, in physical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorRect {
    pub name: Option<String>,
    pub work_area: ScreenRect,
    pub scale_factor: f64,
}

impl From<&Monitor> for MonitorRect {
    fn from(monitor: &Monitor) -> Self {
        let work_area = monitor.work_area();
        Self {
            name: monitor.name().cloned(),
            work_area: ScreenRect {
                x: work_area.position.x,
                y: work_area.position.y,
                width: work_area.size.width as i32,
                height: work_area.size.height as i32,
            },
            scale_factor: monitor.scale_factor(),
        }
    }
}

impl MonitorRect {
    /// Monitors are matched by name first and by origin second, because a
    /// renamed or re-enumerated display keeps its physical position.
    fn matches(&self, other: &MonitorRect) -> bool {
        match (self.name.as_deref(), other.name.as_deref()) {
            (Some(left), Some(right)) => left == right,
            _ => self.work_area == other.work_area,
        }
    }
}

/// The window surface the placement code needs.
///
/// Tauri exposes the same geometry methods on `Window` and `WebviewWindow`
/// without a shared trait: the startup path holds a `WebviewWindow` and the
/// window-event path holds a `Window`. This trait is that shared surface, so one
/// implementation of capture, restore, and clamping serves both.
pub trait PlacementTarget {
    fn label(&self) -> &str;
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>>;
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>>;
    fn scale_factor(&self) -> tauri::Result<f64>;
    fn is_maximized(&self) -> tauri::Result<bool>;
    fn is_minimized(&self) -> tauri::Result<bool>;
    fn current_monitor(&self) -> tauri::Result<Option<Monitor>>;
    fn monitor_from_point(&self, x: f64, y: f64) -> tauri::Result<Option<Monitor>>;
    fn available_monitors(&self) -> tauri::Result<Vec<Monitor>>;
    fn primary_monitor(&self) -> tauri::Result<Option<Monitor>>;
    fn apply_bounds(&self, bounds: ScreenRect) -> tauri::Result<()>;
    fn maximize_window(&self) -> tauri::Result<()>;
}

fn apply_physical_bounds(
    bounds: ScreenRect,
    mut position: impl FnMut(PhysicalPosition<i32>) -> tauri::Result<()>,
    mut size: impl FnMut(PhysicalSize<u32>) -> tauri::Result<()>,
) -> tauri::Result<()> {
    // Windows can rescale a window when it crosses a monitor DPI boundary.
    // Apply its final physical size after moving it to the destination.
    position(PhysicalPosition::new(bounds.x, bounds.y))?;
    size(PhysicalSize::new(bounds.width as u32, bounds.height as u32))?;
    // A large old rectangle may have been clamped while crossing screens.
    position(PhysicalPosition::new(bounds.x, bounds.y))
}

impl<R: Runtime> PlacementTarget for Window<R> {
    fn label(&self) -> &str {
        Window::label(self)
    }
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>> {
        Window::outer_position(self)
    }
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>> {
        Window::inner_size(self)
    }
    fn scale_factor(&self) -> tauri::Result<f64> {
        Window::scale_factor(self)
    }
    fn is_maximized(&self) -> tauri::Result<bool> {
        Window::is_maximized(self)
    }
    fn is_minimized(&self) -> tauri::Result<bool> {
        Window::is_minimized(self)
    }
    fn current_monitor(&self) -> tauri::Result<Option<Monitor>> {
        Window::current_monitor(self)
    }
    fn monitor_from_point(&self, x: f64, y: f64) -> tauri::Result<Option<Monitor>> {
        Window::monitor_from_point(self, x, y)
    }
    fn available_monitors(&self) -> tauri::Result<Vec<Monitor>> {
        Window::available_monitors(self)
    }
    fn primary_monitor(&self) -> tauri::Result<Option<Monitor>> {
        Window::primary_monitor(self)
    }
    fn apply_bounds(&self, bounds: ScreenRect) -> tauri::Result<()> {
        apply_physical_bounds(
            bounds,
            |position| Window::set_position(self, position),
            |size| Window::set_size(self, size),
        )
    }
    fn maximize_window(&self) -> tauri::Result<()> {
        Window::maximize(self)
    }
}

impl<R: Runtime> PlacementTarget for WebviewWindow<R> {
    fn label(&self) -> &str {
        WebviewWindow::label(self)
    }
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>> {
        WebviewWindow::outer_position(self)
    }
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>> {
        WebviewWindow::inner_size(self)
    }
    fn scale_factor(&self) -> tauri::Result<f64> {
        WebviewWindow::scale_factor(self)
    }
    fn is_maximized(&self) -> tauri::Result<bool> {
        WebviewWindow::is_maximized(self)
    }
    fn is_minimized(&self) -> tauri::Result<bool> {
        WebviewWindow::is_minimized(self)
    }
    fn current_monitor(&self) -> tauri::Result<Option<Monitor>> {
        WebviewWindow::current_monitor(self)
    }
    fn monitor_from_point(&self, x: f64, y: f64) -> tauri::Result<Option<Monitor>> {
        WebviewWindow::monitor_from_point(self, x, y)
    }
    fn available_monitors(&self) -> tauri::Result<Vec<Monitor>> {
        WebviewWindow::available_monitors(self)
    }
    fn primary_monitor(&self) -> tauri::Result<Option<Monitor>> {
        WebviewWindow::primary_monitor(self)
    }
    fn apply_bounds(&self, bounds: ScreenRect) -> tauri::Result<()> {
        apply_physical_bounds(
            bounds,
            |position| WebviewWindow::set_position(self, position),
            |size| WebviewWindow::set_size(self, size),
        )
    }
    fn maximize_window(&self) -> tauri::Result<()> {
        WebviewWindow::maximize(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPresentation {
    pub decorations: bool,
    pub skip_taskbar: bool,
    pub always_on_top: bool,
    pub resizable: bool,
}

impl From<WindowMode> for WindowPresentation {
    fn from(mode: WindowMode) -> Self {
        // The desktop list is owned by the tray and the main window: it must never
        // take a taskbar slot in any mode, so a mode selects Z-order only.
        Self {
            decorations: false,
            skip_taskbar: true,
            always_on_top: matches!(mode, WindowMode::AlwaysOnTop),
            resizable: true,
        }
    }
}

pub fn apply_mode(window: &WebviewWindow, mode: WindowMode) -> tauri::Result<()> {
    let presentation = WindowPresentation::from(mode);
    window.set_always_on_top(presentation.always_on_top)?;
    window.set_decorations(presentation.decorations)?;
    window.set_shadow(false)?;
    window.set_skip_taskbar(presentation.skip_taskbar)?;
    window.set_resizable(presentation.resizable)?;
    Ok(())
}

pub fn restore_from_tray(window: &WebviewWindow) -> tauri::Result<()> {
    window.unminimize()?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// One opaque no-activate restore, called only on the HWND owner thread.
/// No layered alpha is installed: that path previously left transparent
/// WebView content waiting for an unrelated foreground transition.
#[cfg(windows)]
pub fn restore_after_show_desktop(
    hwnd: windows_sys::Win32::Foundation::HWND,
    cloak_reason: i32,
    is_current: impl Fn() -> bool,
) -> Result<(), ()> {
    if !is_current() {
        return Ok(());
    }
    if unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) }
        != unsafe { GetCurrentThreadId() }
    {
        return Err(());
    }
    let state = super::windows_desktop::window_state(hwnd).ok_or(())?;
    let needs_visibility = !state.visible || state.minimized || cloak_reason != 0;
    if needs_visibility {
        // An application cloak is not ours to override.
        if cloak_reason & !0x2 != 0 {
            return Err(());
        }
        if cloak_reason & 0x2 != 0 {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
        unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    }
    if !is_current() {
        return Ok(());
    }
    let surface = super::windows_desktop::desktop_surface();
    if needs_visibility
        || state.topmost
        || surface.is_some_and(|desktop| {
            super::windows_desktop::desktop_order(hwnd, desktop)
                == super::windows_desktop::DesktopOrder::Below
        })
    {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
        let promoted = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags) };
        // Always demote our temporary promotion before returning.
        let demoted = unsafe { SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags) };
        if promoted == 0 || demoted == 0 {
            return Err(());
        }
    }
    if !is_current() {
        return Ok(());
    }
    // Native visibility flags alone do not request transparent WebView content
    // to repaint. Include children without activation or another Z-order move.
    if unsafe {
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
        )
    } == 0
    {
        return Err(());
    }
    Ok(())
}

fn usable_scale_factor(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

fn round_dip(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Lists every monitor work area plus the index that should receive a window
/// whose recorded monitor is gone.
pub fn monitor_rects(window: &impl PlacementTarget) -> Option<(Vec<MonitorRect>, usize)> {
    let monitors = window.available_monitors().ok()?;
    let rects = monitors.iter().map(MonitorRect::from).collect::<Vec<_>>();
    if rects.is_empty() {
        return None;
    }
    let primary = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| MonitorRect::from(&monitor));
    let primary_index = primary
        .and_then(|primary| rects.iter().position(|rect| rect.matches(&primary)))
        .unwrap_or(0);
    Some((rects, primary_index))
}

/// Reads the current rectangle of a window as monitor-relative DIP values.
///
/// The outer position is paired with the inner size so a decorated window keeps
/// the same frame origin and client size on the next launch, which is exactly
/// what `restore_placement` writes back.
pub fn capture_placement(window: &impl PlacementTarget) -> Option<WindowPlacement> {
    // A minimized or otherwise degenerate window reports a temporary rectangle
    // (Windows uses offset 0/-32000 and a zero size) that must never become the
    // next launch's restore target.
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let position = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    if size.width == 0 || size.height == 0 {
        return None;
    }
    // The monitor is derived from the window's own rectangle rather than from
    // the window handle alone: a window that is currently hidden still has a
    // valid rectangle, and attributing a rectangle to the wrong monitor would
    // make it jump to that monitor's work area on the next launch.
    let monitor = window
        .monitor_from_point(f64::from(position.x), f64::from(position.y))
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten());
    let scale = monitor
        .as_ref()
        .map(|monitor| monitor.scale_factor())
        .or_else(|| window.scale_factor().ok())
        .map(usable_scale_factor)
        .unwrap_or(1.0);
    let (origin, monitor_name) = match monitor.as_ref() {
        Some(monitor) => (monitor.position().to_owned(), monitor.name().cloned()),
        None => (PhysicalPosition::new(0, 0), None),
    };
    Some(WindowPlacement {
        monitor_name,
        x_dip: round_dip(f64::from(position.x - origin.x) / scale),
        y_dip: round_dip(f64::from(position.y - origin.y) / scale),
        width_dip: round_dip(f64::from(size.width) / scale),
        height_dip: round_dip(f64::from(size.height) / scale),
        maximized: window.is_maximized().unwrap_or(false),
    })
}

/// Resolves a stored placement into physical pixel bounds.
///
/// The recorded monitor wins; otherwise the primary monitor receives the
/// window. Size and origin are then clamped into that work area so a removed
/// display, a smaller resolution, a taller taskbar, or a scale-factor change
/// can never leave the window off-screen.
pub fn resolve_bounds(
    placement: &WindowPlacement,
    monitors: &[MonitorRect],
    primary_index: usize,
    min_width_dip: f64,
    min_height_dip: f64,
) -> Option<ScreenRect> {
    let monitor = placement
        .monitor_name
        .as_deref()
        .and_then(|name| {
            monitors
                .iter()
                .find(|rect| rect.name.as_deref() == Some(name))
        })
        .or_else(|| monitors.get(primary_index))
        .or_else(|| monitors.first())?;
    let scale = usable_scale_factor(monitor.scale_factor);
    let scale_dip = |value: f64| (value * scale).round();
    let work_area = monitor.work_area;
    let width = scale_dip(placement.width_dip.max(min_width_dip))
        .min(f64::from(work_area.width))
        .max(1.0) as i32;
    let height = scale_dip(placement.height_dip.max(min_height_dip))
        .min(f64::from(work_area.height))
        .max(1.0) as i32;
    let x = work_area.x + scale_dip(placement.x_dip) as i32;
    let y = work_area.y + scale_dip(placement.y_dip) as i32;
    let max_x = (work_area.x + work_area.width - width).max(work_area.x);
    let max_y = (work_area.y + work_area.height - height).max(work_area.y);
    Some(ScreenRect {
        x: x.clamp(work_area.x, max_x),
        y: y.clamp(work_area.y, max_y),
        width,
        height,
    })
}

/// Applies a stored placement. Returns `false` when no monitor is available, so
/// the caller can keep the window at its configured default.
pub fn restore_placement(
    window: &impl PlacementTarget,
    placement: &WindowPlacement,
    min_size_dip: (f64, f64),
) -> tauri::Result<bool> {
    let bounds = monitor_rects(window).and_then(|(monitors, primary_index)| {
        resolve_bounds(
            placement,
            &monitors,
            primary_index,
            min_size_dip.0,
            min_size_dip.1,
        )
    });
    let Some(bounds) = bounds else {
        return Ok(false);
    };
    window.apply_bounds(bounds)?;
    if placement.maximized {
        window.maximize_window()?;
    }
    Ok(true)
}

/// Fits the main window's first-run target into the primary work area. Stored
/// placements continue through `restore_placement`; this path only prevents a
/// fresh 1280 × 760 window from exceeding a small or heavily scaled display.
pub fn fit_initial_bounds(
    window: &impl PlacementTarget,
    target_size_dip: (f64, f64),
    min_size_dip: (f64, f64),
) -> tauri::Result<bool> {
    let Some((monitors, primary_index)) = monitor_rects(window) else {
        return Ok(false);
    };
    let Some(monitor) = monitors.get(primary_index).or_else(|| monitors.first()) else {
        return Ok(false);
    };
    let scale = usable_scale_factor(monitor.scale_factor);
    let work = monitor.work_area;
    let width_cap = (f64::from(work.width) * 0.92).round().max(1.0);
    let height_cap = (f64::from(work.height) * 0.92).round().max(1.0);
    let width_floor = (min_size_dip.0 * scale).round().min(f64::from(work.width));
    let height_floor = (min_size_dip.1 * scale).round().min(f64::from(work.height));
    let width = (target_size_dip.0 * scale)
        .round()
        .min(width_cap.max(width_floor))
        .max(width_floor) as i32;
    let height = (target_size_dip.1 * scale)
        .round()
        .min(height_cap.max(height_floor))
        .max(height_floor) as i32;
    let bounds = ScreenRect {
        x: work.x + (work.width - width) / 2,
        y: work.y + (work.height - height) / 2,
        width,
        height,
    };
    window.apply_bounds(bounds)?;
    Ok(true)
}

/// Bounds a content-driven popover height before it is applied to the window.
pub fn tray_menu_height_dip(requested: f64) -> f64 {
    if !requested.is_finite() {
        return TRAY_MENU_MIN_HEIGHT_DIP;
    }
    requested.clamp(TRAY_MENU_MIN_HEIGHT_DIP, TRAY_MENU_MAX_HEIGHT_DIP)
}

/// Additional bound for a popover on a short screen: it may never be taller than
/// the space between the screen edge and the tray icon it is anchored to.
pub fn tray_menu_height_for_screen(requested_dip: f64, screen_height_dip: f64) -> f64 {
    tray_menu_height_dip(requested_dip).min(screen_height_dip.max(TRAY_MENU_MIN_HEIGHT_DIP))
}

/// Height in DIP the screen under `anchor` can show without covering the tray.
pub fn tray_menu_screen_height_dip(
    window: &impl PlacementTarget,
    anchor: (f64, f64),
) -> Option<f64> {
    let monitor = window
        .monitor_from_point(anchor.0, anchor.1)
        .ok()
        .flatten()?;
    let scale = usable_scale_factor(monitor.scale_factor());
    let work_area = monitor.work_area();
    Some(f64::from(work_area.size.height) / scale - f64::from(TRAY_POPOVER_GAP_Y))
}

/// Prefer the right of the tray icon, flip to its left when the right side
/// cannot fit, then clamp to the owning screen as a last resort.
pub fn tray_popover_origin(
    anchor_x: i32,
    anchor_y: i32,
    width: i32,
    height: i32,
    screen: Option<ScreenRect>,
) -> (i32, i32) {
    let right_x = anchor_x + TRAY_POPOVER_GAP_X;
    let left_x = anchor_x - width - TRAY_POPOVER_GAP_X;
    let mut x = right_x;
    let mut y = anchor_y - height - TRAY_POPOVER_GAP_Y;
    if let Some(screen) = screen {
        let max_x = (screen.x + screen.width - width).max(screen.x);
        let max_y = (screen.y + screen.height - height).max(screen.y);
        if right_x > max_x {
            x = left_x;
        }
        x = x.clamp(screen.x, max_x);
        y = y.clamp(screen.y, max_y);
    }
    (x, y)
}

