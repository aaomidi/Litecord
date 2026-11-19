#![allow(dead_code)]

#[allow(unused_imports)]
use slint::{Rgba8Pixel, SharedPixelBuffer};
#[allow(unused_imports)]
use super::super::types::{CapturableWindowItem, MonitorItemInfo};

#[cfg(not(windows))]
pub static PORTAL_FRAME: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);
/// Frames put in `PORTAL_FRAME` so far, and the count `capture_screen_rgb` last handed on: it
/// hands on new frames only, so a capture that stopped delivering is not taken for one that
/// delivers the same picture.
static PORTAL_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PORTAL_FRAMES_TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A shared window that sent no picture for this long gets a new capture (a still window
/// sends none either: the wait doubles each time, up to `WINDOW_SILENCE_MAX`).
#[cfg(feature = "vaapi")]
const WINDOW_SILENCE: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(feature = "vaapi")]
const WINDOW_SILENCE_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// A GPU capture started again after its source renegotiated: tries, and the wait between them.
#[cfg(feature = "vaapi")]
const GPU_RESTART_TRIES: u32 = 3;
#[cfg(feature = "vaapi")]
const GPU_RESTART_WAIT: std::time::Duration = std::time::Duration::from_millis(250);
/// How long a restart waits for the capture before it to be taken down.
#[cfg(feature = "vaapi")]
const GPU_RESTART_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(not(windows))]
pub static PORTAL_INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(not(windows))]
pub static PORTAL_CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "linux")]
static PORTAL_LAST_ATTEMPT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
#[cfg(target_os = "linux")]
pub static PORTAL_LOCAL_CB: std::sync::Mutex<Option<std::sync::Arc<dyn Fn(SharedPixelBuffer<Rgba8Pixel>) + Send + Sync + 'static>>> = std::sync::Mutex::new(None);
#[cfg(target_os = "linux")]
pub static PORTAL_CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
/// Goes up when a portal thread is started, and when a share starts or is stopped: a portal
/// thread whose number is no longer this one belongs to a share that is over (its source
/// dialog may still be open), and nothing it does or reports may touch the share of now.
#[cfg(target_os = "linux")]
static PORTAL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Held for every change of PORTAL_INITIALIZED, PORTAL_CANCELLED and SHARE_END_PENDING that
/// depends on PORTAL_GENERATION or changes it, so "is this still my share? then ..." is one
/// step. (Reading the flags alone needs no lock.)
#[cfg(target_os = "linux")]
static PORTAL_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// How long the portal may take over a step that asks the user nothing (connecting, creating
/// the session, naming the kinds of source) before the share is given up as failed.
#[cfg(target_os = "linux")]
const PORTAL_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Why a share's capture ended without the share being stopped.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareEnd {
    /// The source dialog was closed without choosing anything.
    Cancelled,
    /// The capture could not start.
    Failed,
    /// The shared window or screen went away (the window was closed, or the desktop's own
    /// "stop sharing" was used).
    SourceLost,
}
#[cfg(target_os = "linux")]
type ShareEndCallback = std::sync::Arc<dyn Fn(ShareEnd) + Send + Sync>;
#[cfg(target_os = "linux")]
static SHARE_END_CB: std::sync::Mutex<Option<ShareEndCallback>> = std::sync::Mutex::new(None);
/// Set while a `ShareEnd` told to the callback has not been acted on; a share that starts
/// takes it back, so a late callback cannot end the next share.
#[cfg(target_os = "linux")]
static SHARE_END_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called, on the capture's thread, when a share has lost its capture for good: the share
/// is over and whoever shows it has to stop it (`take_share_end` says whether that still
/// holds once on the UI thread). Nothing asks the portal again by itself: its dialog opening
/// alone after a window was closed is not something anyone asked for.
#[cfg(target_os = "linux")]
pub fn on_share_end(callback: impl Fn(ShareEnd) + Send + Sync + 'static) {
    *SHARE_END_CB.lock().unwrap() = Some(std::sync::Arc::new(callback));
}

/// True once per `ShareEnd`, and only while no other share has started since.
#[cfg(target_os = "linux")]
pub fn take_share_end() -> bool {
    SHARE_END_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// The capture of portal thread `generation` is over though nobody stopped the share.
#[cfg(target_os = "linux")]
fn share_ended(generation: u64, why: ShareEnd) {
    use std::sync::atomic::Ordering;
    {
        let _state = PORTAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        // (A thread of a share that was stopped, or replaced by another, has nothing to end.)
        if PORTAL_GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        // (The capture poll must not ask the portal again.)
        PORTAL_CANCELLED.store(true, Ordering::SeqCst);
        PORTAL_INITIALIZED.store(false, Ordering::SeqCst);
        SHARE_END_PENDING.store(true, Ordering::SeqCst);
    }
    let callback = SHARE_END_CB.lock().unwrap().clone();
    if let Some(callback) = callback {
        callback(why);
    }
}

/// A share is starting (of any kind: a camera share too): whatever portal thread is left
/// from an earlier one is no longer anyone's, and the portal may be asked once.
#[cfg(target_os = "linux")]
pub fn reset_wayland_portal_cancelled() {
    use std::sync::atomic::Ordering;
    let _state = PORTAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    PORTAL_GENERATION.fetch_add(1, Ordering::SeqCst);
    PORTAL_CANCELLED.store(false, Ordering::SeqCst);
    PORTAL_INITIALIZED.store(false, Ordering::SeqCst);
    SHARE_END_PENDING.store(false, Ordering::SeqCst);
}

/// The share was stopped, or there is none (leaving the call, quitting): the portal thread
/// of now, if any, is nobody's from here on, and the portal is not asked until a share starts
/// (`reset_wayland_portal_cancelled`). Doing it again changes nothing.
#[cfg(target_os = "linux")]
pub fn share_stopped() {
    use std::sync::atomic::Ordering;
    let _state = PORTAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    PORTAL_GENERATION.fetch_add(1, Ordering::SeqCst);
    // (Before PORTAL_INITIALIZED falls: the capture poll runs a little longer than the share
    // and would otherwise take the lowered flag for "ask the portal".)
    PORTAL_CANCELLED.store(true, Ordering::SeqCst);
    PORTAL_INITIALIZED.store(false, Ordering::SeqCst);
    SHARE_END_PENDING.store(false, Ordering::SeqCst);
}

/// Portal thread `generation` is done without anything to report: the flag is lowered if it
/// is still that thread's.
#[cfg(target_os = "linux")]
fn release(generation: u64) {
    use std::sync::atomic::Ordering;
    let _state = PORTAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if PORTAL_GENERATION.load(Ordering::SeqCst) == generation {
        PORTAL_INITIALIZED.store(false, Ordering::SeqCst);
    }
}

/// How a source dialog that gave no source ended: closed by the user, or anything else (the
/// portal's backend failed, the session was ended under it), which the user is told about.
#[cfg(target_os = "linux")]
fn dialog_end(error: &ashpd::Error) -> ShareEnd {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => ShareEnd::Cancelled,
        _ => ShareEnd::Failed,
    }
}

#[cfg(target_os = "linux")]
pub fn kill_portal_child() {
    share_stopped();
    if let Ok(mut lock) = PORTAL_CHILD.lock() {
        if let Some(mut child) = lock.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Where a picture of `source` size lies in a `frame` of another shape when its own shape is
/// kept: (x, y, width, height), centred, the rest being bars. The arithmetic is the one
/// `vapostproc add-borders=true` and `videoscale add-borders=true` share (the GPU capture and
/// the gst-launch capture: measured identical for every size tried), with square pixels on
/// both sides: the frame's width is filled when the height that goes with it fits (rounded
/// down), its height otherwise. A fit that is less than a pixel short fills the frame, which
/// is why 16:9 pictures fill 854x480 (0.08% wider than 16:9).
pub fn letterbox(source: (u32, u32), frame: (u32, u32)) -> (u32, u32, u32, u32) {
    let (sw, sh, w, h) = (source.0 as u64, source.1 as u64, frame.0 as u64, frame.1 as u64);
    if sw == 0 || sh == 0 || sw * h == sh * w {
        return (0, 0, frame.0, frame.1);
    }
    let fitted_h = w * sh / sw;
    if fitted_h <= h {
        (0, ((h - fitted_h) / 2) as u32, frame.0, fitted_h as u32)
    } else {
        let fitted_w = h * sw / sh;
        (((w - fitted_w) / 2) as u32, 0, fitted_w as u32, frame.1)
    }
}

/// Where the captures put a picture of `source` size in the stream's `frame`: `letterbox`,
/// except that a fit one pixel short of the frame fills it. Such a bar is rounding (1366x768
/// is 1079.4 rows high in 1920x1080), and in NV12 it would cost the picture's last row its
/// colour too (they share their chroma samples). The GPU capture follows this for every
/// picture; the gst-launch capture for a monitor whose size it is told
/// (`gst_launch_capture_args`).
pub fn picture_rect(source: (u32, u32), frame: (u32, u32)) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = letterbox(source, frame);
    if frame.0 - w <= 1 && frame.1 - h <= 1 {
        return (0, 0, frame.0, frame.1);
    }
    (x, y, w, h)
}

/// The arguments of the gst-launch capture: pictures of PipeWire node `source` (pipewiresrc
/// properties: `fd=`, `path=`) as BGRA frames of `dst_w` x `dst_h` on its standard output.
/// The picture keeps its shape: it is scaled to fit the frame and centred, the rest of the
/// frame being black bars (`videoscale add-borders`, which needs square pixels named on
/// both sides: asked for a pixel shape, pipewiresrc otherwise settles on 1/2147483647, and
/// with none named behind it videoscale stretches the picture and changes the pixel shape
/// instead, which the pipe does not carry). The in-process GPU capture puts the picture at
/// the same place (`letterbox`).
///
/// `monitor`: the size of the monitor shared, when the source is one and its size is known.
/// The pipeline cannot be told a size once it runs, and videoscale leaves a bar one pixel
/// thin where the fit is a pixel short (1366x768 in 1920x1080 or 1280x720: one black row
/// at the bottom), which the GPU capture does not (`picture_rect`). For just such a monitor
/// the picture is scaled to fill the frame, as it was before the bars existed. A window's
/// size changes while it is shared, so a window always gets the bars.
#[cfg(target_os = "linux")]
pub fn gst_launch_capture_args(source: &[String], dst_w: u32, dst_h: u32, target_fps: u64, monitor: Option<(u32, u32)>) -> Vec<String> {
    capture_pipeline(source, dst_w, dst_h, target_fps, monitor, false)
}

/// `gst_launch_capture_args`, or with `window` the same pipeline as `WindowCapture` runs it
/// inside this process: a videocrop in front of the conversion (`win_crop`) and an appsink
/// in place of the pipe (`win_sink`).
#[cfg(target_os = "linux")]
fn capture_pipeline(source: &[String], dst_w: u32, dst_h: u32, target_fps: u64, monitor: Option<(u32, u32)>, window: bool) -> Vec<String> {
    let whole = (0, 0, dst_w, dst_h);
    let fill = monitor.is_some_and(|size| letterbox(size, (dst_w, dst_h)) != whole && picture_rect(size, (dst_w, dst_h)) == whole);
    let (square, scale): (&str, &[&str]) = if fill { ("", &["videoscale"]) } else { (",pixel-aspect-ratio=1/1", &["videoscale", "add-borders=true"]) };
    let mut args = if window { vec!["pipewiresrc".to_string(), "name=win_src".to_string()] } else { vec!["-q".to_string(), "pipewiresrc".to_string()] };
    args.extend(source.iter().cloned());
    args.extend(["do-timestamp=true", "!"].map(String::from));
    if !fill {
        args.extend(["video/x-raw,pixel-aspect-ratio=1/1", "!"].map(String::from));
    }
    if window {
        args.extend(["videocrop", "name=win_crop", "!"].map(String::from));
    }
    args.extend(["videoconvert", "n-threads=4", "!"].map(String::from));
    args.extend(scale.iter().map(|s| s.to_string()));
    args.push("!".to_string());
    let frame = format!("video/x-raw,format=BGRA,width={},height={}{}", dst_w, dst_h, square);
    if target_fps > 0 && target_fps < 120 {
        args.extend(["videorate".to_string(), "!".to_string(), format!("{},framerate={}/1", frame, target_fps)]);
    } else {
        args.push(frame);
    }
    if window {
        args.extend(["!", "appsink", "name=win_sink", "sync=false", "max-buffers=1", "drop=true"].map(String::from));
    } else {
        args.extend(["!", "queue", "max-size-buffers=1", "max-size-bytes=0", "max-size-time=0", "leaky=downstream", "!", "fdsink", "fd=1", "sync=false"].map(String::from));
    }
    args
}

/// The capture that is not on the GPU, for a shared window: the gst-launch capture's
/// pipeline inside this process. Mutter gives a window's stream the size of the window's
/// monitor and says in each buffer which part of it is the window (a GstVideoCropMeta; see
/// the notes of `linux_gpu_tx`). No element that gst-launch could run in system memory
/// takes that part out, so gst-launch shows the whole buffer: the window small in the top
/// left corner of a black, transparent frame. Here a probe reads each buffer's rectangle
/// and sets it on a videocrop in front of the conversion, which renegotiates behind itself
/// for that very buffer; videoscale then puts the window where the GPU capture puts it
/// (`letterbox`). A source without the rectangle passes whole.
///
/// xdg-desktop-portal-hyprland gives a window's stream the window's own size and renegotiates
/// it at every resize; pipewiresrc gets no picture any more after a renegotiation by its
/// source (the source runs out of buffers, nothing reports an error). `read` therefore says
/// when the format changed or no picture came for a while, and the caller starts a new
/// capture on a new connection (`init_wayland_portal_screencast`).
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub struct WindowCapture {
    pipeline: gstreamer::Pipeline,
    sink: gstreamer_app::AppSink,
    /// The source negotiated another format after its first one.
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What `WindowCapture::read` found.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
#[derive(Debug, PartialEq)]
pub enum WindowRead {
    /// A picture is in the frame.
    Frame,
    /// The capture has ended (the window was closed, the pipeline failed) or the share has.
    Ended,
    /// The source changed its format (a window resized on Hyprland): no more pictures come.
    Changed,
    /// No picture for this long. A still window sends none either.
    Silent(std::time::Duration),
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
impl WindowCapture {
    /// `source`: pipewiresrc properties, as for `gst_launch_capture_args`.
    pub fn start(source: &[String], dst_w: u32, dst_h: u32, target_fps: u64) -> Result<Self, String> {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        use gstreamer_video as gst_video;
        gst::init().map_err(|e| format!("GStreamer: {e}"))?;
        let args = capture_pipeline(source, dst_w, dst_h, target_fps, None, true);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let pipeline = gst::parse::launchv(&args).map_err(|e| format!("pipeline: {e}"))?.downcast::<gst::Pipeline>().map_err(|_| "não é um pipeline")?;
        let crop = pipeline.by_name("win_crop").ok_or("sem videocrop")?;
        let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let format = std::sync::Mutex::new(None::<gst::Structure>);
        let on_format = changed.clone();
        pipeline.by_name("win_src").and_then(|src| src.static_pad("src")).ok_or("sem pipewiresrc")?.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if let gst::EventView::Caps(caps) = event.view() {
                    let now = caps.caps().structure(0).map(|s| s.to_owned());
                    let mut format = format.lock().unwrap_or_else(|e| e.into_inner());
                    if format.is_some() && *format != now {
                        on_format.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    *format = now;
                }
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or("probe não instalado")?;
        let sink = pipeline.by_name("win_sink").and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok()).ok_or("sem appsink")?;
        let pad = crop.static_pad("sink").ok_or("sem pad")?;
        // (left, top, right, bottom) as videocrop has them.
        let set = std::sync::Mutex::new((0i32, 0i32, 0i32, 0i32));
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
            let size = pad.current_caps().and_then(|c| c.structure(0).map(|s| (s.get::<i32>("width").unwrap_or(0) as i64, s.get::<i32>("height").unwrap_or(0) as i64)));
            let Some((w, h)) = size.filter(|s| s.0 > 0 && s.1 > 0) else { return gst::PadProbeReturn::Ok };
            let rect = buffer.meta::<gst_video::VideoCropMeta>().map(|meta| meta.rect()).filter(|r| r.2 > 0 && r.3 > 0);
            let wanted = match rect {
                Some((x, y, cw, ch)) => {
                    // (Kept inside the buffer whatever the compositor says.)
                    let (x, y) = ((x as i64).min(w - 1), (y as i64).min(h - 1));
                    let (cw, ch) = ((cw as i64).min(w - x), (ch as i64).min(h - y));
                    (x as i32, y as i32, (w - x - cw) as i32, (h - y - ch) as i32)
                }
                None => (0, 0, 0, 0),
            };
            let mut set = set.lock().unwrap_or_else(|e| e.into_inner());
            if *set != wanted {
                // On the thread that carries this buffer into videocrop: its new format is
                // negotiated before the buffer is cropped.
                *set = wanted;
                crop.set_property("left", wanted.0);
                crop.set_property("top", wanted.1);
                crop.set_property("right", wanted.2);
                crop.set_property("bottom", wanted.3);
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or("probe não instalado")?;
        pipeline.set_state(gst::State::Playing).map_err(|e| format!("o pipeline não iniciou: {e}"))?;
        Ok(Self { pipeline, sink, changed })
    }

    /// The next frame, BGRA and opaque, into `frame`, waiting at most `silence` for it.
    pub fn read(&self, frame: &mut [u8], wanted: &dyn Fn() -> bool, silence: std::time::Duration) -> WindowRead {
        use gstreamer as gst;
        use gstreamer::prelude::*;
        let since = std::time::Instant::now();
        loop {
            if !wanted() {
                return WindowRead::Ended;
            }
            if self.changed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return WindowRead::Changed;
            }
            let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
                if self.sink.is_eos() {
                    return WindowRead::Ended;
                }
                if let Some(error) = self.pipeline.bus().and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error])) {
                    if let gst::MessageView::Error(e) = error.view() {
                        log::warn!("🎬 [GStreamer] captura da janela encerrada: {}", e.error());
                    }
                    return WindowRead::Ended;
                }
                if since.elapsed() >= silence {
                    return WindowRead::Silent(since.elapsed());
                }
                continue;
            };
            let Some(Ok(map)) = sample.buffer().map(|b| b.map_readable()) else { return WindowRead::Ended };
            if map.len() != frame.len() {
                return WindowRead::Ended;
            }
            frame.copy_from_slice(&map);
            // (Around the window Mutter leaves the buffer transparent, and a window has
            // transparent corners: the UI would blend its preview with what is behind.)
            for pixel in frame.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            return WindowRead::Frame;
        }
    }
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
impl Drop for WindowCapture {
    fn drop(&mut self) {
        use gstreamer::prelude::*;
        // (pipewiresrc may wait inside its state change: not on the caller's thread.)
        let pipeline = self.pipeline.clone();
        let _ = std::thread::Builder::new().name("window-capture-stop".into()).spawn(move || {
            let _ = pipeline.set_state(gstreamer::State::Null);
        });
    }
}

#[cfg(target_os = "linux")]
pub fn init_wayland_portal_screencast(target_w: u32, target_h: u32, target_fps: u64) {
    use std::sync::atomic::Ordering;
    // (Every capture poll comes through here: the usual answers without the lock.)
    if PORTAL_CANCELLED.load(Ordering::SeqCst) || PORTAL_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    if let Ok(guard) = PORTAL_LAST_ATTEMPT.lock() {
        if let Some(last) = *guard {
            if last.elapsed() < std::time::Duration::from_secs(3) {
                return;
            }
        }
    }

    // Under the lock: either the share is still wanted here and its thread gets a number
    // that a later stop outdates, or the stop came first and nothing is started.
    let generation = {
        let _state = PORTAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if PORTAL_CANCELLED.load(Ordering::SeqCst) || PORTAL_INITIALIZED.swap(true, Ordering::SeqCst) {
            return;
        }
        PORTAL_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
    };
    if let Ok(mut guard) = PORTAL_LAST_ATTEMPT.lock() {
        *guard = Some(std::time::Instant::now());
    }
    // The share this thread was started for is still running, and this is its thread.
    let mine = move || PORTAL_INITIALIZED.load(Ordering::Relaxed) && PORTAL_GENERATION.load(Ordering::Relaxed) == generation;

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                log::error!("Falha ao criar tokio runtime para Portal ScreenCast: {:?}", e);
                share_ended(generation, ShareEnd::Failed);
                return;
            }
        };

        let _guard = rt.enter();

        rt.block_on(async move {
            // (Stopped before this thread got here: the portal is not asked at all.)
            if !mine() {
                return;
            }
            use ashpd::desktop::{
                PersistMode,
                screencast::{
                    CursorMode, OpenPipeWireRemoteOptions, Screencast,
                    SelectSourcesOptions, SourceType, StartCastOptions,
                },
                CreateSessionOptions,
            };
            use std::os::fd::AsRawFd;
            use std::process::{Command, Stdio};
            use std::io::Read;

            // Test builds can name the source themselves (LITECORD_TEST_SHARE_NODE): a PipeWire
            // node of the default remote stands in for everything the portal answers below,
            // and the portal is not asked (its dialog needs a person).
            #[cfg(feature = "automation")]
            let test_source = crate::utils::test_hooks::share_node();
            #[cfg(not(feature = "automation"))]
            let test_source: Option<Option<(u32, bool)>> = None;
            if test_source == Some(None) {
                log::error!("🧪 [TESTE] LITECORD_TEST_SHARE_NODE não deu uma fonte: tratado como um diálogo do portal cancelado");
                share_ended(generation, ShareEnd::Cancelled);
                return;
            }
            let test_source = test_source.flatten();

            let proxy = if test_source.is_some() {
                None
            } else {
            log::info!("📡 Solicitando sessão nativa do XDG Desktop Portal ScreenCast via conexão D-Bus dedicada...");
            // A portal that cannot be reached, refuses or does not answer: the share has no
            // capture and ends as failed (`share_ended`), here and in the two steps below.
            // Nothing asks again by itself.
            let connecting = async {
                let conn = ashpd::zbus::connection::Builder::session()
                    .map_err(|e| format!("builder D-Bus: {:?}", e))?
                    .build()
                    .await
                    .map_err(|e| format!("conexão D-Bus privada: {:?}", e))?;
                Screencast::with_connection(conn).await.map_err(|e| format!("Screencast Portal: {:?}", e))
            };
            match tokio::time::timeout(PORTAL_ANSWER_TIMEOUT, connecting).await {
                Ok(Ok(p)) => Some(p),
                Ok(Err(e)) => {
                    log::error!("Falha ao conectar no Portal ScreenCast ({})", e);
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
                Err(_) => {
                    log::error!("O Portal ScreenCast não respondeu à conexão em {} s", PORTAL_ANSWER_TIMEOUT.as_secs());
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
            }
            };

            let session = match &proxy {
                Some(proxy) => match tokio::time::timeout(PORTAL_ANSWER_TIMEOUT, proxy.create_session(CreateSessionOptions::default())).await {
                    Ok(Ok(s)) => Some(s),
                    Ok(Err(e)) => {
                        log::error!("Falha ao criar sessão do ScreenCast: {:?}", e);
                        share_ended(generation, ShareEnd::Failed);
                        return;
                    }
                    Err(_) => {
                        log::error!("O Portal ScreenCast não criou a sessão em {} s", PORTAL_ANSWER_TIMEOUT.as_secs());
                        share_ended(generation, ShareEnd::Failed);
                        return;
                    }
                },
                None => None,
            };
            // Closes the portal's session (a test source has none).
            macro_rules! close_session {
                () => {
                    if let Some(session) = &session {
                        let _ = session.close().await;
                    }
                };
            }
            // A new PipeWire connection of the session for a capture that takes over, and the
            // connection `$old` shut: two clients on one connection break it. `None` when
            // there is no portal (a test source: each capture connects to the default remote)
            // or it refused (the old connection stays).
            #[cfg(feature = "vaapi")]
            macro_rules! reopen_remote {
                ($old:expr) => {
                    match (&proxy, &session, $old) {
                        (Some(proxy), Some(session), Some(old)) => match proxy.open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default()).await {
                            Ok(fd) => {
                                let new_fd = fd.as_raw_fd();
                                unsafe {
                                    libc::shutdown(old, libc::SHUT_RDWR);
                                    let flags = libc::fcntl(new_fd, libc::F_GETFD);
                                    if flags >= 0 {
                                        libc::fcntl(new_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                                    }
                                }
                                Some(fd)
                            }
                            Err(e) => {
                                log::warn!("⚠️ Falha ao abrir um segundo PipeWire Remote ({:?}); a captura seguinte usa a conexão anterior", e);
                                None
                            }
                        },
                        _ => None,
                    }
                };
            }

            let response = if let (Some(proxy), Some(session)) = (&proxy, &session) {
            let select_opts = SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_restore_token(None)
                .set_persist_mode(PersistMode::DoNot);

            match tokio::time::timeout(PORTAL_ANSWER_TIMEOUT, proxy.select_sources(session, select_opts)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    log::error!("Falha ao selecionar fontes de captura no Portal: {:?}", e);
                    close_session!();
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
                Err(_) => {
                    // (Not closed: a portal that does not answer would not answer that
                    // either; the session goes with this thread's D-Bus connection.)
                    log::error!("O Portal ScreenCast não respondeu à escolha dos tipos de fonte em {} s", PORTAL_ANSWER_TIMEOUT.as_secs());
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
            }

            // The source dialog: open for as long as the user leaves it. Only its being
            // closed by the user ends the share without a word (`dialog_end`).
            match proxy.start(session, None, StartCastOptions::default()).await {
                Ok(r) => match r.response() {
                    Ok(resp) => Some(resp),
                    Err(e) => {
                        log::error!("Resposta de erro do Portal ScreenCast: {:?}", e);
                        close_session!();
                        share_ended(generation, dialog_end(&e));
                        return;
                    }
                },
                Err(e) => {
                    log::error!("Falha ao iniciar captura no Portal ScreenCast: {:?}", e);
                    close_session!();
                    share_ended(generation, dialog_end(&e));
                    return;
                }
            }
            } else {
                None
            };
            // The dialog was answered for a share that was stopped meanwhile (it stays open
            // when the share is stopped): nothing is captured, and the session is given back.
            if !mine() {
                log::info!("🛑 A fonte escolhida era de um compartilhamento já encerrado: sessão do portal fechada sem capturar");
                close_session!();
                return;
            }

            // The source: its node, whether it is a window, and a monitor's size (the portal
            // gives that in the compositor's coordinates: its shape).
            let (node_id, is_window, monitor_size) = match (&response, test_source) {
                (Some(response), _) => match response.streams().first() {
                    Some(stream) => (
                        stream.pipe_wire_node_id(),
                        stream.source_type() == Some(SourceType::Window),
                        stream.size().filter(|_| stream.source_type() == Some(SourceType::Monitor)).map(|(w, h)| (w.max(0) as u32, h.max(0) as u32)),
                    ),
                    None => {
                        log::error!("Nenhum stream retornado pelo Portal ScreenCast");
                        close_session!();
                        share_ended(generation, ShareEnd::Failed);
                        return;
                    }
                },
                (None, Some((node, window))) => {
                    log::warn!("🧪 [TESTE] Fonte dada por LITECORD_TEST_SHARE_NODE: nó PipeWire {} ({}), sem portal", node, if window { "janela" } else { "monitor" });
                    (node, window, None)
                }
                (None, None) => return,
            };

            let pw_fd = if let (Some(proxy), Some(session)) = (&proxy, &session) {
                match proxy.open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default()).await {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        log::error!("Falha ao abrir PipeWire Remote: {:?}", e);
                        close_session!();
                        share_ended(generation, ShareEnd::Failed);
                        return;
                    }
                }
            } else {
                None
            };

            // (`None`: the default PipeWire remote, for a test source.)
            let raw_fd: Option<i32> = pw_fd.as_ref().map(|fd| fd.as_raw_fd());
            if let Some(raw_fd) = raw_fd {
                unsafe {
                    let flags = libc::fcntl(raw_fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                }
            }
            // pipewiresrc properties naming the source. On the default remote `path=` alone is
            // only a wish (with the node gone, the session manager links any video source it
            // likes, a camera included), so there the stream is told not to fall back.
            let source_props = |fd: Option<i32>| -> Vec<String> {
                match fd {
                    Some(fd) => vec![format!("fd={}", fd), format!("path={}", node_id)],
                    None => vec![format!("path={}", node_id), "stream-properties=props,node.dont-fallback=(string)true".to_string()],
                }
            };

            log::info!("🎉 Conectando GStreamer ao PipeWire Node ID={}, FD={} ({}x{} @ {} FPS)...", node_id, raw_fd.map_or("remoto padrão".to_string(), |fd| fd.to_string()), target_w, target_h, target_fps);

            // Capture and encode on the GPU inside this process when that works. If it cannot
            // start, or fails later, the gst-launch capture below runs on the same session.
            #[cfg(feature = "vaapi")]
            let (raw_fd, _gst_fd, gpu_ran) = {
                use crate::screen_capture::linux_gpu_tx as gpu;
                let mut fallback = (raw_fd, None, false);
                if gpu::allowed(is_window) {
                    // The connection the GPU capture uses, and the one opened for it.
                    let (mut gpu_fd, mut gpu_remote) = (raw_fd, None);
                    let mut gpu_ran = false;
                    let mut restarting = false;
                    let ended = loop {
                        let mut begun = gpu::begin(gpu_fd, node_id, target_w, target_h, target_fps as u32, &mine);
                        // Right after a renegotiation the source may not take a new consumer yet
                        // ("no more input formats"): a few more tries before the fallback.
                        let mut tries = 1;
                        while restarting && begun.is_none() && tries < GPU_RESTART_TRIES && mine() {
                            tokio::time::sleep(GPU_RESTART_WAIT).await;
                            begun = gpu::begin(gpu_fd, node_id, target_w, target_h, target_fps as u32, &mine);
                            tries += 1;
                        }
                        gpu_ran |= begun.is_some();
                        let ended = match begun {
                            Some(id) => gpu::run_until_end(id, &mine),
                            None => gpu::Ended::Failed,
                        };
                        if ended != gpu::Ended::SourceChanged || !mine() {
                            break ended;
                        }
                        // A window resized on Hyprland: a new GPU capture on a new connection
                        // takes the new format (the old connection gets no more pictures).
                        // (Not while the old capture is still being taken down: two consumers
                        // of the node at once and the new one is refused.)
                        let deadline = std::time::Instant::now() + GPU_RESTART_STOP_WAIT;
                        while gpu::live_pipelines() > 0 && std::time::Instant::now() < deadline {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        if let Some(fd) = reopen_remote!(gpu_fd) {
                            gpu_fd = Some(fd.as_raw_fd());
                            gpu_remote = Some(fd);
                        }
                        restarting = true;
                    };
                    let ended = if ended == gpu::Ended::SourceChanged { gpu::Ended::Stopped } else { ended };
                    if ended != gpu::Ended::Failed || !mine() {
                        // The share was stopped, or the compositor ended the session (the
                        // window was closed): in the second case the share is over too.
                        close_session!();
                        if session.is_some() {
                            log::info!("🛑 Sessão do XDG Desktop Portal ScreenCast fechada com sucesso via D-Bus!");
                        }
                        if ended == gpu::Ended::SourceGone {
                            share_ended(generation, ShareEnd::SourceLost);
                        }
                        return;
                    }
                    // gst-launch gets a PipeWire connection of its own. The pipeline that
                    // failed stops in the background and may never finish (a driver that
                    // hangs); two clients on one connection break it, so the old one is shut.
                    fallback = match reopen_remote!(gpu_fd) {
                        Some(fd) => (Some(fd.as_raw_fd()), Some(fd), gpu_ran),
                        None => (gpu_fd, gpu_remote, gpu_ran),
                    };
                }
                fallback
            };
            #[cfg(not(feature = "vaapi"))]
            let gpu_ran = false;

            let (dst_w, dst_h) = (target_w, target_h);
            let frame_size = (dst_w * dst_h * 4) as usize;

            let gst_args = gst_launch_capture_args(&source_props(raw_fd), dst_w, dst_h, target_fps, monitor_size);

            // A shared window: the same pipeline inside this process, which takes the
            // window out of the monitor-sized buffer (`WindowCapture`); gst-launch when
            // that does not start.
            #[cfg(feature = "vaapi")]
            let mut window_capture = if is_window {
                // (pipewiresrc closes the connection it is given: a copy of it.)
                let own_fd = raw_fd.map(|fd| unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) });
                match WindowCapture::start(&source_props(own_fd), dst_w, dst_h, target_fps) {
                    Ok(capture) => {
                        log::info!("🔧 Captura da janela no processo (videocrop pelo retângulo de cada quadro), sem gst-launch");
                        Some(capture)
                    }
                    Err(e) => {
                        log::warn!("⚠️ A captura da janela no processo não iniciou ({}); usando gst-launch", e);
                        None
                    }
                }
            } else {
                None
            };
            #[cfg(feature = "vaapi")]
            let in_process = window_capture.is_some();
            #[cfg(not(feature = "vaapi"))]
            let in_process = false;

            let mut stdout = None;
            // (The gst-launch of this thread, to leave a newer share's alone at the end.)
            let mut child_pid = None;
            if !in_process {
            log::info!("🔧 GStreamer pipeline args: {:?}", gst_args);

            let mut child = match Command::new("gst-launch-1.0")
                .env_remove("PIPEWIRE_NODE")
                .args(&gst_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Falha ao iniciar pipeline GStreamer PipeWire: {:?}", e);
                    close_session!();
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
            };

            // Spawn a thread to log GStreamer stderr for debugging
            if let Some(stderr) = child.stderr.take() {
                std::thread::Builder::new()
                    .name("gst-stderr-logger".to_string())
                    .spawn(move || {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(stderr);
                        for line in reader.lines() {
                            match line {
                                Ok(l) => log::warn!("🎬 [GStreamer STDERR]: {}", l),
                                Err(_) => break,
                            }
                        }
                    })
                    .ok();
            }

            stdout = match child.stdout.take() {
                Some(s) => Some(s),
                None => {
                    close_session!();
                    share_ended(generation, ShareEnd::Failed);
                    return;
                }
            };

            // Handed over only while the share is this thread's: `stop` lowers the flags and
            // then kills what is in the slot, so a child put there after this check is killed
            // by it, and one of a share already over never takes a newer share's place.
            child_pid = Some(child.id());
            let handed_over = {
                let mut lock = PORTAL_CHILD.lock().unwrap_or_else(|e| e.into_inner());
                if mine() {
                    if let Some(mut old) = lock.take() {
                        let _ = old.kill();
                    }
                    *lock = Some(child);
                    true
                } else {
                    let _ = child.kill();
                    false
                }
            };
            if !handed_over {
                close_session!();
                return;
            }
            }
            // The window capture's connection, the one opened for it, and how long it may stay
            // without a picture before it gets a new capture.
            #[cfg(feature = "vaapi")]
            let (mut window_fd, mut _window_remote, mut window_silence) = (raw_fd, None, WINDOW_SILENCE);
            // The window capture now running was started again and has not delivered yet: the
            // compositor sends a new consumer a picture at once, so silence then is a source
            // that is gone (the capture ends, and the share with it).
            #[cfg(feature = "vaapi")]
            let mut window_restarted_silent = false;

            let mut raw_buf = vec![0u8; frame_size];
            let mut pw_frame_count = 0u64;
            let mut last_pw_stats = std::time::Instant::now();
            let mut last_local_preview_ts = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut got_first_frame = false;

            // (No time limit: a source may have nothing to show for a while.)
            log::info!("⏳ Aguardando primeiro quadro do GStreamer PipeWire...");

            // (Left false by a share that was stopped; true when the capture ended by itself.)
            let mut capture_ended = false;
            loop {
                if !mine() {
                    break;
                }
                #[cfg(feature = "vaapi")]
                let read = window_capture.as_ref().map(|capture| capture.read(&mut raw_buf, &mine, window_silence));
                #[cfg(not(feature = "vaapi"))]
                let read: Option<()> = None;
                let got = match read {
                    None => stdout.as_mut().is_some_and(|s| s.read_exact(&mut raw_buf).is_ok()),
                    #[cfg(feature = "vaapi")]
                    Some(WindowRead::Frame) => {
                        window_silence = WINDOW_SILENCE;
                        window_restarted_silent = false;
                        true
                    }
                    #[cfg(feature = "vaapi")]
                    Some(WindowRead::Silent(waited)) if window_restarted_silent => {
                        log::warn!("⚠️ A captura da janela iniciada de novo não recebeu nenhum quadro em {:.1} s: a fonte deixou de existir", waited.as_secs_f64());
                        false
                    }
                    #[cfg(feature = "vaapi")]
                    Some(WindowRead::Ended) => false,
                    #[cfg(feature = "vaapi")]
                    Some(why @ (WindowRead::Changed | WindowRead::Silent(_))) => {
                        // Not papered over with the last picture: a new capture on a new
                        // connection (see `WindowCapture`). A window that is only still gets
                        // the same, ever less often.
                        if let WindowRead::Silent(waited) = why {
                            log::warn!("⚠️ Captura da janela sem quadros há {:.1} s; uma captura nova é iniciada", waited.as_secs_f64());
                            window_silence = (window_silence * 2).min(WINDOW_SILENCE_MAX);
                        } else {
                            log::warn!("⚠️ A janela mudou de formato (redimensionada); uma captura nova é iniciada");
                        }
                        window_capture = None;
                        if let Some(fd) = reopen_remote!(window_fd) {
                            window_fd = Some(fd.as_raw_fd());
                            _window_remote = Some(fd);
                        }
                        let own_fd = window_fd.map(|fd| unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) });
                        match WindowCapture::start(&source_props(own_fd), dst_w, dst_h, target_fps) {
                            Ok(capture) => window_capture = Some(capture),
                            Err(e) => log::warn!("⚠️ A captura nova da janela não iniciou ({})", e),
                        }
                        window_restarted_silent = true;
                        if window_capture.is_some() {
                            continue;
                        }
                        false
                    }
                };
                if !got {
                    capture_ended = mine();
                    break;
                }
                if !got_first_frame {
                    got_first_frame = true;
                    log::info!("✅ Primeiro quadro do GStreamer PipeWire recebido com sucesso!");
                }
                pw_frame_count += 1;
                let now = std::time::Instant::now();
                if now.duration_since(last_pw_stats) >= std::time::Duration::from_secs(1) {
                    let elapsed_s = now.duration_since(last_pw_stats).as_secs_f64();
                    let fps = (pw_frame_count as f64) / elapsed_s;
                    log::info!("📊 [TELEMETRIA] PipeWire GStreamer Source: {:.1} FPS ({} frames em {:.2}s)", fps, pw_frame_count, elapsed_s);
                    pw_frame_count = 0;
                    last_pw_stats = now;
                }

                // Despacha miniatura da interface local apenas periodicamente (~7 FPS) para eliminar 500 MB/s de alocação
                if now.duration_since(last_local_preview_ts) >= std::time::Duration::from_millis(150) {
                    last_local_preview_ts = now;
                    if let Ok(cb_guard) = PORTAL_LOCAL_CB.lock() {
                        if let Some(ref local_cb) = *cb_guard {
                            let mut pixel_buffer = SharedPixelBuffer::<Rgba8Pixel>::new(dst_w, dst_h);
                            let bytes = pixel_buffer.make_mut_bytes();
                            bytes.copy_from_slice(&raw_buf);
                            for chunk in bytes.chunks_exact_mut(4) {
                                chunk.swap(0, 2); // BGRA -> RGBA para exibição na UI Slint
                            }
                            if let Some(digit) = get_test_watermark_digit() {
                                draw_test_watermark(pixel_buffer.make_mut_slice(), &mut raw_buf, dst_w, dst_h, digit);
                            }
                            local_cb(pixel_buffer);
                        }
                    }
                }

                if let Ok(mut slot) = PORTAL_FRAME.lock() {
                    if let Some(ref mut dest) = *slot {
                        if dest.len() != raw_buf.len() {
                            dest.resize(raw_buf.len(), 0);
                        }
                        dest.copy_from_slice(&raw_buf);
                    } else {
                        *slot = Some(raw_buf.clone());
                    }
                    PORTAL_FRAMES.fetch_add(1, Ordering::SeqCst);
                }
            }

            // (After a GPU capture that had been running, gst-launch delivering nothing is a
            // capture that ended mid-share, not one that never started.)
            let never_started = capture_ended && !got_first_frame && !gpu_ran;
            if never_started {
                log::error!("❌ GStreamer PipeWire encerrou sem produzir nenhum quadro! Pipeline falhou.");
                // Check if child exited with an error
                if let Ok(mut lock) = PORTAL_CHILD.lock() {
                    if let Some(ref mut child) = *lock {
                        match child.try_wait() {
                            Ok(Some(status)) => log::error!("❌ GStreamer exit status: {}", status),
                            Ok(None) => log::warn!("⚠️ GStreamer ainda está rodando mas stdout fechou"),
                            Err(e) => log::error!("❌ Erro ao verificar status do GStreamer: {:?}", e),
                        }
                    }
                }
            }

            if let Ok(mut lock) = PORTAL_CHILD.lock() {
                // (Only this thread's own: a newer share's gst-launch may be there by now.)
                if child_pid.is_some() && lock.as_ref().map(|child| child.id()) == child_pid {
                    if let Some(mut child) = lock.take() {
                        let _ = child.kill();
                    }
                }
            }
            close_session!();
            if session.is_some() {
                log::info!("🛑 Sessão do XDG Desktop Portal ScreenCast fechada com sucesso via D-Bus!");
            }
            // (Each of these does nothing when a newer share has the flags by now.)
            if never_started {
                share_ended(generation, ShareEnd::Failed);
            } else if capture_ended {
                // The source went away while the share was running: the share is over. (The
                // portal is not asked again; see `on_share_end`.)
                log::warn!("⚠️ A captura terminou sozinha (a fonte deixou de existir); o compartilhamento terminou");
                share_ended(generation, ShareEnd::SourceLost);
            } else {
                release(generation);
            }
        });
    });
}

#[cfg(not(windows))]
pub fn list_screens() -> Vec<MonitorItemInfo> {
    let mut screens = Vec::new();
    #[cfg(target_os = "linux")]
    {
        use x11rb::connection::Connection;
        if let Ok((conn, screen_num)) = x11rb::connect(None) {
            let root = &conn.setup().roots[screen_num];
            screens.push(MonitorItemInfo {
                id: 0,
                name: crate::utils::i18n_ui::pick("Main Screen", "Tela Principal").to_string(),
                resolution: format!("{} × {}", root.width_in_pixels, root.height_in_pixels),
                is_primary: true,
                hwnd: root.root as isize,
            });
        }
    }

    if screens.is_empty() {
        screens.push(MonitorItemInfo {
            id: 0,
            name: crate::utils::i18n_ui::pick("Screen 1 (Primary)", "Tela 1 (Principal)").to_string(),
            resolution: "1920 × 1080".to_string(),
            is_primary: true,
            hwnd: 0,
        });
    }
    screens
}

#[cfg(not(windows))]
pub fn list_capturable_windows() -> Vec<CapturableWindowItem> {
    let mut windows = Vec::new();
    #[cfg(target_os = "linux")]
    {
        windows.push(CapturableWindowItem {
            id: "0".to_string(),
            title: "Janela Ativa / Desktop".to_string(),
            app_name: "X11 / Wayland".to_string(),
            icon_rgba: None,
        });
    }
    windows
}

#[cfg(not(windows))]
pub fn capture_screen_rgb(_target_hwnd: isize, target_w: u32, target_h: u32, target_fps: u64, out_bgra: &mut Vec<u8>) -> Option<(u128, u128)> {
    #[cfg(target_os = "linux")]
    {
        let capture_backend = crate::video_settings::get_video_capture_backend();
        if capture_backend != "x11" {
            init_wayland_portal_screencast(target_w, target_h, target_fps);

            if let Ok(slot) = PORTAL_FRAME.lock() {
                // (Only a frame the reader put there since the last call: see `PORTAL_FRAMES`.)
                let frames = PORTAL_FRAMES.load(std::sync::atomic::Ordering::SeqCst);
                if PORTAL_FRAMES_TAKEN.swap(frames, std::sync::atomic::Ordering::SeqCst) == frames {
                    return None;
                }
                if let Some(ref frame) = *slot {
                    let total = (target_w * target_h * 4) as usize;
                    if frame.len() == total {
                        if out_bgra.len() != total {
                            out_bgra.resize(total, 0);
                        }
                        out_bgra.copy_from_slice(frame);
                        return Some((0, 0));
                    }
                }
            }
        }
        return None;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let total = (target_w * target_h * 4) as usize;
        if out_bgra.len() != total {
            out_bgra.resize(total, 24);
        }
        Some((0, 0))
    }
}

const GLYPH_1: [u8; 12] = [
    0b00011000,
    0b00111000,
    0b01111000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b00011000,
    0b01111110,
    0b01111110,
];

const GLYPH_2: [u8; 12] = [
    0b00111100,
    0b01100110,
    0b01100110,
    0b00000110,
    0b00001100,
    0b00011000,
    0b00110000,
    0b01100000,
    0b01100000,
    0b01100000,
    0b01111110,
    0b01111110,
];

pub fn get_test_watermark_digit() -> Option<char> {
    if let Ok(val) = std::env::var("LITECORD_INSTANCE_ID") {
        let v = val.trim();
        if v == "1" { return Some('1'); }
        if v == "2" { return Some('2'); }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.to_string_lossy().to_lowercase();
        if cwd_str.contains("instance2") || cwd_str.contains("profile2") || cwd_str.ends_with("_2") {
            return Some('2');
        }
        if cwd_str.contains("instance1") || cwd_str.contains("profile1") || cwd_str.ends_with("_1") {
            return Some('1');
        }
    }
    None
}

pub fn draw_test_watermark(
    slice: &mut [Rgba8Pixel],
    bgra_bytes: &mut [u8],
    width: u32,
    height: u32,
    digit: char,
) {
    let glyph = match digit {
        '1' => &GLYPH_1,
        '2' => &GLYPH_2,
        _ => return,
    };

    let cx = (width / 2) as i32;
    let cy = (height / 2) as i32;

    // Badge Dimensions
    let half_w = 90i32;
    let half_h = 90i32;
    let border_thick = 4i32;

    // Accent color: Instance 1 = Blurple (#5865F2), Instance 2 = Pink/Magenta (#EB459E)
    let (border_r, border_g, border_b) = if digit == '1' {
        (88u8, 101u8, 242u8)
    } else {
        (235u8, 69u8, 158u8)
    };

    // Draw background badge (dark semi-transparent box with solid border)
    for dy in -half_h..=half_h {
        let y = cy + dy;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for dx in -half_w..=half_w {
            let x = cx + dx;
            if x < 0 || x >= width as i32 {
                continue;
            }

            let idx = (y as usize) * (width as usize) + (x as usize);
            let offset_bgra = idx * 4;

            let is_border = dx.abs() >= half_w - border_thick || dy.abs() >= half_h - border_thick;

            let (r, g, b) = if is_border {
                (border_r, border_g, border_b)
            } else {
                let cur_b = bgra_bytes[offset_bgra];
                let cur_g = bgra_bytes[offset_bgra + 1];
                let cur_r = bgra_bytes[offset_bgra + 2];
                let bg_r = 17u8;
                let bg_g = 18u8;
                let bg_b = 20u8;
                let br = ((cur_r as u16 * 2 + bg_r as u16 * 8) / 10) as u8;
                let bg = ((cur_g as u16 * 2 + bg_g as u16 * 8) / 10) as u8;
                let bb = ((cur_b as u16 * 2 + bg_b as u16 * 8) / 10) as u8;
                (br, bg, bb)
            };

            slice[idx] = Rgba8Pixel::new(r, g, b, 255);
            bgra_bytes[offset_bgra] = b;
            bgra_bytes[offset_bgra + 1] = g;
            bgra_bytes[offset_bgra + 2] = r;
            bgra_bytes[offset_bgra + 3] = 255;
        }
    }

    // Draw the big scaled digit in the center of the badge
    let scale = 9i32; // 8 cols * 9 = 72px width, 12 rows * 9 = 108px height
    let glyph_w = 8 * scale;
    let glyph_h = 12 * scale;
    let start_x = cx - glyph_w / 2;
    let start_y = cy - glyph_h / 2;

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        for col_idx in 0..8 {
            if (row_bits & (1 << (7 - col_idx))) != 0 {
                for sy in 0..scale {
                    let y = start_y + (row_idx as i32) * scale + sy;
                    if y < 0 || y >= height as i32 {
                        continue;
                    }
                    for sx in 0..scale {
                        let x = start_x + (col_idx as i32) * scale + sx;
                        if x < 0 || x >= width as i32 {
                            continue;
                        }

                        let idx = (y as usize) * (width as usize) + (x as usize);
                        let offset_bgra = idx * 4;

                        let r = 255u8;
                        let g = 255u8;
                        let b = 255u8;

                        slice[idx] = Rgba8Pixel::new(r, g, b, 255);
                        bgra_bytes[offset_bgra] = b;
                        bgra_bytes[offset_bgra + 1] = g;
                        bgra_bytes[offset_bgra + 2] = r;
                        bgra_bytes[offset_bgra + 3] = 255;
                    }
                }
            }
        }
    }
}
