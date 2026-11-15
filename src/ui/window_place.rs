//! Remembers where the main window was and puts it back there on the next start.
//!
//! Moving or resizing sends an event per mouse step, so nothing is written while that
//! happens: the geometry is read and written once, a second after the last change, or
//! right away when the app closes or goes to the tray.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use slint::ComponentHandle;

use i_slint_backend_winit::WinitWindowAccessor;

use crate::utils::video_settings::{self, WindowGeometry, WINDOW_MIN_HEIGHT, WINDOW_MIN_WIDTH};
use crate::AppWindow;

const DEFAULT_SIZE: (u32, u32) = (980, 620);
/// The window's minimum in ui/appwindow.slint.
const MIN_SIZE: (u32, u32) = (680, 480);
const DEFAULT_POSITION: (f64, f64) = (100.0, 100.0);
const SAVE_AFTER: Duration = Duration::from_secs(1);
/// How much of the title bar must lie on a monitor for a saved position to be used.
const MIN_VISIBLE: (i32, i32) = (120, 24);

thread_local! {
    // Only the UI thread sees window events, so the state lives there.
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    static WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    static LAST: Cell<Option<WindowGeometry>> = const { Cell::new(None) };
    static SAVE_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// Sizes and places the window as it was saved, or at the defaults. A saved position
/// that no longer lands on any monitor (one was unplugged) is dropped.
pub fn restore(window: &winit::window::Window, ui_scale: f32) -> bool {
    let saved = video_settings::get_window_geometry();
    LAST.with(|l| l.set(saved));
    // Events from before this (the window appearing where the system put it) are stale.
    DIRTY.with(|d| d.set(false));
    let scale = window.scale_factor();
    // The interface scale setting can differ from the system's scale: the default and the
    // minimum size are in interface pixels, a saved size is in the system's logical pixels.
    let k = ui_scale as f64 / scale;
    let logical = |(width, height): (u32, u32)| winit::dpi::LogicalSize::new(width as f64 * k, height as f64 * k);
    let size: winit::dpi::PhysicalSize<u32> = match saved {
        Some(g) => {
            let min = logical(MIN_SIZE);
            winit::dpi::LogicalSize::new((g.width as f64).max(min.width), (g.height as f64).max(min.height)).to_physical(scale)
        }
        None => fit_to_monitor(window, logical(DEFAULT_SIZE).to_physical(scale)),
    };
    let _ = window.request_inner_size(size);
    let position = saved.and_then(|g| Some((g.x?, g.y?))).filter(|&(x, y)| {
        let title_width = size.width as i32;
        window.available_monitors().any(|m| {
            let (mp, ms) = (m.position(), m.size());
            let visible_x = (x + title_width).min(mp.x + ms.width as i32) - x.max(mp.x);
            let visible_y = (y + MIN_VISIBLE.1).min(mp.y + ms.height as i32) - y.max(mp.y);
            visible_x >= MIN_VISIBLE.0 && visible_y >= MIN_VISIBLE.1
        })
    });
    match position {
        Some((x, y)) => window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y)),
        None => window.set_outer_position(winit::dpi::LogicalPosition::new(DEFAULT_POSITION.0, DEFAULT_POSITION.1)),
    }
    let maximized = saved.is_some_and(|g| g.maximized);
    if maximized {
        window.set_maximized(true);
    }
    maximized
}

/// `size` cut down to the monitor the window is on.
fn fit_to_monitor(window: &winit::window::Window, size: winit::dpi::PhysicalSize<u32>) -> winit::dpi::PhysicalSize<u32> {
    let monitor = window.current_monitor().or_else(|| window.primary_monitor()).or_else(|| window.available_monitors().next());
    match monitor.map(|m| m.size()).filter(|m| m.width > 0 && m.height > 0) {
        Some(m) => winit::dpi::PhysicalSize::new(size.width.min(m.width), size.height.min(m.height)),
        None => size,
    }
}

/// The size to give a window that came up larger than its monitor (a large interface
/// scale on a small screen), or None when it fits.
pub fn size_fitting_monitor(window: &winit::window::Window) -> Option<winit::dpi::PhysicalSize<u32>> {
    let size = window.inner_size();
    Some(fit_to_monitor(window, size)).filter(|fitted| *fitted != size)
}

/// A move or resize happened: the geometry is read when it is saved, a second later.
/// Read during the events it can be wrong: maximizing reports the maximized size first
/// and the maximized flag only after.
pub fn observe(app: &slint::Weak<AppWindow>) {
    DIRTY.with(|d| d.set(true));
    WINDOW.with(|w| *w.borrow_mut() = Some(app.clone()));
    SAVE_TIMER.with(|t| {
        t.borrow_mut()
            .get_or_insert_with(slint::Timer::default)
            .start(slint::TimerMode::SingleShot, SAVE_AFTER, save_now);
    });
}

/// Writes a pending change at once (closing, going to the tray).
pub fn save_now() {
    if !DIRTY.with(|d| d.replace(false)) {
        return;
    }
    let Some(app) = WINDOW.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade())) else { return };
    let previous = LAST.with(|l| l.get());
    let Some(geometry) = app.window().with_winit_window(|w| current(w, previous)).flatten() else { return };
    if app.get_is_maximized() != geometry.maximized {
        app.set_is_maximized(geometry.maximized);
    }
    if Some(geometry) != previous {
        LAST.with(|l| l.set(Some(geometry)));
        video_settings::set_window_geometry(geometry);
    }
}

fn current(window: &winit::window::Window, previous: Option<WindowGeometry>) -> Option<WindowGeometry> {
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let maximized = window.is_maximized();
    if maximized {
        // Keep the size and place the window goes back to when restored.
        let (width, height) = previous.map_or(DEFAULT_SIZE, |g| (g.width, g.height));
        return Some(WindowGeometry { x: previous.and_then(|g| g.x), y: previous.and_then(|g| g.y), width, height, maximized });
    }
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let (width, height) = (size.width.round() as u32, size.height.round() as u32);
    if width < WINDOW_MIN_WIDTH || height < WINDOW_MIN_HEIGHT {
        return None;
    }
    let position = window.outer_position().ok();
    Some(WindowGeometry {
        x: position.map(|p| p.x).or(previous.and_then(|g| g.x)),
        y: position.map(|p| p.y).or(previous.and_then(|g| g.y)),
        width,
        height,
        maximized,
    })
}
