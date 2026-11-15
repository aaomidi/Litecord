//! Linux: decoded pictures handed to the windows as GPU buffers instead of pixels.
//!
//! With the GPU renderer on an EGL context (Wayland), the decoder leaves each picture in a
//! DMABuf and the windows that show the stream import it as a texture (ui::gl_video): no
//! read-back, no copy, no upload. This file is the meeting point of the two threads and
//! holds no GL: the decode thread offers its newest picture, the UI thread takes it when
//! it draws.
//!
//! Whether this works is only known once a window has drawn its first frame, and it can
//! stop working (an import fails). `give_up` is final for the run: streams go back to
//! pixels from their next keyframe, still decoded on the GPU.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use gstreamer as gst;

const UNKNOWN: u8 = 0;
const READY: u8 = 1;
const OFF: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

/// A picture waiting to be shown: the decoder's buffer (it stays out of the decoder's
/// pool for as long as any clone of the sample lives).
#[derive(Clone)]
pub struct GpuFrame {
    pub peer: u64,
    /// Differs for every picture offered.
    pub seq: u64,
    pub sample: gst::Sample,
    /// The stream's own height (the picture may be smaller).
    pub stream_height: u32,
}

static LATEST: Mutex<Option<GpuFrame>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Pictures offered and pictures a window imported (read by tests).
pub static OFFERED: AtomicU64 = AtomicU64::new(0);
pub static IMPORTED: AtomicU64 = AtomicU64::new(0);

type Wake = Box<dyn Fn() + Send + Sync>;
static WAKE: OnceLock<Wake> = OnceLock::new();

/// `LITECORD_GL_PRESENT=0` keeps every stream on the pixel path.
fn turned_off() -> bool {
    std::env::var("LITECORD_GL_PRESENT").map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")).unwrap_or(false)
}

/// `LITECORD_GL_PRESENT=1` tries it on a driver it was not verified on (see `verified_on`).
fn forced() -> bool {
    std::env::var("LITECORD_GL_PRESENT").map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")).unwrap_or(false)
}

/// Whether pictures drawn this way were compared with the pixel path on this driver (its
/// GL_RENDERER string): the colours come out of the GPU's own conversion and this program
/// never looks at them, so an unknown driver stays on pixels unless asked. So far: Mesa
/// radeonsi (AMD).
pub fn verified_on(gl_renderer: &str) -> bool {
    forced() || gl_renderer.contains("radeonsi")
}

/// Test hook: `LITECORD_GL_PRESENT_TEST_FAIL=after:N` makes a window's Nth import fail.
pub fn test_fail_after() -> Option<u64> {
    static AFTER: OnceLock<Option<u64>> = OnceLock::new();
    *AFTER.get_or_init(|| std::env::var("LITECORD_GL_PRESENT_TEST_FAIL").ok()?.strip_prefix("after:")?.parse().ok())
}

/// Pictures can be handed over as GPU buffers right now.
pub fn ready() -> bool {
    STATE.load(Ordering::Acquire) == READY
}

/// Given up for this run (or never possible: software renderer, X11).
pub fn is_off() -> bool {
    STATE.load(Ordering::Acquire) == OFF
}

/// The main window can import the decoder's buffers (called once it has drawn a frame).
pub fn set_ready() {
    if turned_off() {
        give_up("turned off by LITECORD_GL_PRESENT=0");
    } else if STATE.compare_exchange(UNKNOWN, READY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        log::info!("🎞️ [GL VIDEO] GPU presentation available: received streams are drawn from the decoder's buffers (no read-back)");
    }
}

/// Streams go back to pixels, for the rest of the run. Logged once, with the reason.
pub fn give_up(reason: &str) {
    if STATE.swap(OFF, Ordering::AcqRel) != OFF {
        log::info!("🎞️ [GL VIDEO] GPU presentation off, received streams are shown through pixels: {}", reason);
    }
    clear_all();
}

/// Called (from the decode thread) whenever a new picture waits; it must get the windows
/// to draw.
pub fn set_wake(wake: Wake) {
    let _ = WAKE.set(wake);
}

/// The decoder's newest picture of `peer`. False when it cannot be shown this way (any
/// more): the caller asks for a keyframe and decodes to pixels from there.
pub fn offer(peer: u64, sample: gst::Sample, stream_height: u32) -> bool {
    if !ready() {
        return false;
    }
    let frame = GpuFrame { peer, seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1, sample, stream_height };
    // The picture it replaces (never taken, or already imported) is released here.
    let old = LATEST.lock().unwrap_or_else(|e| e.into_inner()).replace(frame);
    drop(old);
    OFFERED.fetch_add(1, Ordering::Relaxed);
    if let Some(wake) = WAKE.get() {
        wake();
    }
    true
}

/// The newest picture offered, if it is of `peer`.
pub fn latest(peer: u64) -> Option<GpuFrame> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|f| f.peer == peer).cloned()
}

/// The stream of `peer` ended: its last picture is not shown again.
pub fn clear(peer: u64) {
    let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    if latest.as_ref().is_some_and(|f| f.peer == peer) {
        *latest = None;
    }
}

pub fn clear_all() {
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
