//! Turns the GPU video layers of the main window and the popout on and off (UI thread).
//!
//! A window shows a received stream through its layers when the stream is decoded on the
//! GPU, the window can be seen, and it uses the software renderer (the layers take what
//! that renderer draws). Otherwise the stream reaches the window as CPU pictures, as
//! before. `refresh` re-evaluates this and is called whenever one of those inputs changes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::ComponentHandle;

use crate::screen_capture;
use crate::{AppWindow, PopoutStreamWindow};

#[cfg(windows)]
use crate::screen_capture::hw_decode::layers::WindowLayers;

#[cfg(windows)]
struct Windows {
    main: Option<Rc<WindowLayers>>,
    popout: Option<Rc<WindowLayers>>,
}

thread_local! {
    #[cfg(windows)]
    static LAYERS: RefCell<Windows> = const { RefCell::new(Windows { main: None, popout: None }) };
    /// The remote stream area in the main window, physical pixels (zero: not shown).
    static VIEWPORT: Cell<(i32, i32, u32, u32)> = const { Cell::new((0, 0, 0, 0)) };
    /// The popout's new size while its resize is being handled (the window itself still
    /// has the old one then); zero otherwise.
    static POPOUT_RESIZING: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    /// The sizes the decoder will be told next, and the timer that tells it.
    static VIEW_WANTED: Cell<Option<((u32, u32), (u32, u32))>> = const { Cell::new(None) };
    static VIEW_TIMER: slint::Timer = slint::Timer::default();
}

/// A size is told to the decoder once it has stayed the same this long: the card's height
/// animates for 200 ms and a window is resized in many steps, and every new size makes
/// the decoder set its conversion up again.
const VIEW_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// Tells the decoder how large a received stream is shown (physical pixels): its card in
/// the main window and the popout, each zero while it does not show one. Pictures are
/// converted to the larger of the two instead of the stream's own size.
fn publish_view_size(app: &AppWindow, popout: &PopoutStreamWindow) {
    let (_, _, vw, vh) = VIEWPORT.with(|v| v.get());
    let peer = app.get_active_stream_user_id();
    let popped_out = !peer.is_empty() && app.get_popped_out_stream_uid() == peer;
    let main = if screen_capture::main_window_watching() && !popped_out { (vw, vh) } else { (0, 0) };
    let pop = if screen_capture::popout_watching() && !popout.get_is_self() {
        match POPOUT_RESIZING.with(|p| p.get()) {
            (0, _) | (_, 0) => {
                let s = popout.window().size();
                (s.width, s.height)
            }
            size => size,
        }
    } else {
        (0, 0)
    };
    let sizes = (main, pop);
    if VIEW_WANTED.with(|w| w.replace(Some(sizes))) == Some(sizes) {
        return;
    }
    VIEW_TIMER.with(|t| t.start(slint::TimerMode::SingleShot, VIEW_SETTLE, move || screen_capture::set_stream_view_sizes(sizes.0, sizes.1)));
}

/// Hooks the layers into both windows (once the native windows exist).
pub fn install(app: &AppWindow, popout: &PopoutStreamWindow) {
    #[cfg(windows)]
    {
        let make = |w: &slint::Window| -> Option<Rc<WindowLayers>> {
            let layers = Rc::new(WindowLayers::new(hwnd_of(w)?));
            // Fails when the window uses a GPU renderer: then streams stay on the CPU path.
            i_slint_backend_winit::set_layer_sink(w, Some(layers.clone() as Rc<dyn i_slint_backend_winit::LayerSink>)).then_some(layers)
        };
        let (main, popout) = (make(app.window()), make(popout.window()));
        log::info!("🎞️ [GPU VIDEO] Camadas de vídeo pela GPU: janela principal {}, popout {}", main.is_some(), popout.is_some());
        LAYERS.with(|l| *l.borrow_mut() = Windows { main, popout });
    }
    #[cfg(not(windows))]
    let _ = (app, popout);
}

/// The remote stream area of the main window moved or changed size (logical pixels;
/// zero size when it is gone).
pub fn viewport_changed(app: &AppWindow, popout: &PopoutStreamWindow, x: f32, y: f32, w: f32, h: f32) {
    let k = app.window().scale_factor();
    let rect = ((x * k).round() as i32, (y * k).round() as i32, (w * k).round().max(0.0) as u32, (h * k).round().max(0.0) as u32);
    VIEWPORT.with(|v| v.set(rect));
    #[cfg(windows)]
    LAYERS.with(|l| {
        if let Some(m) = l.borrow().main.as_ref() {
            m.set_video_rect(rect.0, rect.1, rect.2, rect.3);
        }
    });
    refresh(app, popout);
}

/// The popout changed size (physical pixels): the video fills it.
pub fn popout_resized(app: &AppWindow, popout: &PopoutStreamWindow, w: u32, h: u32) {
    POPOUT_RESIZING.with(|p| p.set((w, h)));
    #[cfg(windows)]
    LAYERS.with(|l| {
        if let Some(p) = l.borrow().popout.as_ref() {
            p.set_video_rect(0, 0, w, h);
        }
    });
    refresh(app, popout);
    POPOUT_RESIZING.with(|p| p.set((0, 0)));
}

/// Re-evaluates which windows show their stream through the GPU, and at what size.
pub fn refresh(app: &AppWindow, popout: &PopoutStreamWindow) {
    publish_view_size(app, popout);
    #[cfg(windows)]
    {
        let parse = |s: slint::SharedString| s.parse::<u64>().unwrap_or(0);
        let (_, _, vw, vh) = VIEWPORT.with(|v| v.get());
        let main_peer = parse(app.get_active_stream_user_id());
        let popped = app.get_popped_out_stream_uid();
        // A hidden stream is no longer wanted: its window must not take the layers back.
        let main_wants = main_peer != 0
            && screen_capture::watch::is_wanted(main_peer)
            && app.get_has_active_stream()
            && popped.as_str() != main_peer.to_string()
            && vw > 0
            && vh > 0
            && screen_capture::main_window_watching()
            && screen_capture::hw_decode::peer_on_gpu(main_peer);
        let pop_peer = parse(popout.get_user_id());
        let pop_wants = pop_peer != 0
            && screen_capture::watch::is_wanted(pop_peer)
            && !popout.get_is_self()
            && screen_capture::popout_watching()
            && screen_capture::hw_decode::peer_on_gpu(pop_peer);
        LAYERS.with(|l| {
            let l = l.borrow();
            if let Some(m) = l.main.as_ref() {
                if !screen_capture::main_window_watching() && m.is_active() {
                    // Not drawn while hidden: free the GPU now rather than on the next frame.
                    m.stop_now();
                    app.set_gpu_video_main(false);
                }
                apply(m, main_wants, main_peer, |on| app.set_gpu_video_main(on));
            }
            if let Some(p) = l.popout.as_ref() {
                if !screen_capture::popout_watching() && p.is_active() {
                    p.stop_now();
                    popout.set_gpu_video(false);
                }
                if pop_wants && !p.is_active() {
                    let s = popout.window().size();
                    p.set_video_rect(0, 0, s.width, s.height);
                }
                apply(p, pop_wants, pop_peer, |on| popout.set_gpu_video(on));
            }
        });
    }
    #[cfg(not(windows))]
    let _ = (app, popout);
}

#[cfg(windows)]
fn apply(layers: &WindowLayers, wants: bool, peer: u64, set_hole: impl Fn(bool)) {
    if wants && layers.usable() && layers.peer() != Some(peer) {
        match layers.start(peer) {
            Ok(()) => {
                log::info!("🎞️ [GPU VIDEO] Transmissão do peer {} exibida direto pela GPU", peer);
                set_hole(true);
            }
            Err(e) => log::warn!("🎞️ [GPU VIDEO] Não foi possível ligar as camadas de vídeo: {:?}", e),
        }
    } else if !wants && layers.is_active() {
        log::info!("🎞️ [GPU VIDEO] Transmissão volta a ser exibida pela CPU");
        layers.stop();
        set_hole(false);
    }
}

#[cfg(windows)]
fn hwnd_of(w: &slint::Window) -> Option<isize> {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    w.with_winit_window(|ww| match ww.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    })
    .flatten()
}
