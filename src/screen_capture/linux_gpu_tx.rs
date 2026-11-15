//! Linux screen share captured and encoded on the GPU, inside this process:
//!
//!   pipewiresrc -> queue -> vapostproc (scale, NV12) -> vah264enc -> appsink   access units
//!                                                    \-> a few pictures a second, read back
//!                                                        for the local preview
//!
//! Everything from vapostproc on is `encoder::vaapi::EncodeChain`, the same text and the
//! same rules as the encoder that takes frames from memory; only what feeds it differs. No
//! raw frame is copied by the CPU except the preview's.
//!
//! Found by running it against Mutter (GNOME, radeonsi) and synthetic PipeWire sources:
//! - DMABuf from the compositor reaches vapostproc only when pipewiresrc can see
//!   vapostproc's `drm-format` list. `capssetter` hides everything downstream of it from
//!   the caps query, so the list is copied into the capsfilter in front of it.
//! - Mutter declares its pictures with a gamma 2.2 transfer function; converted to the
//!   chain's sRGB-labelled NV12, radeonsi re-curves them (dark greys come out 4 levels
//!   low). `capssetter` relabels the input as sRGB, which is what the frames-from-memory
//!   encoder feeds the same chain. It also sets the nominal frame rate the encoder's rate
//!   control divides the bitrate by (PipeWire video is `framerate=0/1`, which vah264enc
//!   reads as 30) and square pixels (pipewiresrc otherwise fixates 1/2147483647).
//! - A `max-framerate` asked of Mutter is quantised to whole refreshes after the minimum
//!   interval: 60 on a 144 Hz monitor gives ~45 pictures a second, and a range breaks the
//!   DMABuf negotiation. So nothing is asked and the pace is kept here, in a pad probe.
//! - PipeWire only delivers pictures on damage. `keepalive-time` makes pipewiresrc push
//!   its last buffer again when nothing new came; the probe lets one of those through
//!   every 100 ms (what the send loop does on the CPU path), or at once when a keyframe is
//!   wanted, so a still screen costs ten tiny P pictures a second.
//! - Anything that renegotiates behind pipewiresrc makes it reconnect its stream, so
//!   there is no valve or videorate: a leaky queue and probes only, sizes pinned by caps.
//! - One DMABuf buffer must never reach two VA elements at once (a tee with a second
//!   vapostproc for the preview): gst-va's importer checks for an imported surface
//!   outside its lock, the second import destroys the first one's surface, and radeonsi
//!   crashes in vaEndPicture. Hence a single vapostproc; the preview reads its output.
//! - pipewiresrc duplicates the fd it is given and shares one connection per fd *number*
//!   between all its instances: each stream here hands it its own duplicate, closed only
//!   after its pipeline has stopped.
//! - pipewiresrc waits up to 30 s inside its state change when the PipeWire stream does not
//!   start, so the state change runs on its own thread and `begin` keeps its own deadline.
//! - When the source renegotiates (a shared window is resized) PipeWire takes its buffers
//!   away, unmapping memory that pictures still on their way to vapostproc point into, and
//!   the keepalive goes on pushing the last one: a crash in vapostproc's upload.
//!   `on-disconnect=error` makes pipewiresrc send flush-start first (which waits for the
//!   picture being converted) and then report an error; from that flush on nothing more is
//!   taken from the source and the stream is given up. The gst-launch capture follows such
//!   changes, so the share goes on there.
//! - Mutter renegotiates the very same format right after the stream first starts, about
//!   one start in ten. No picture has been handed out then, so that one is let pass.
//! - `path=N` is only a wish: with the node gone, the session manager links the stream to
//!   whatever video source it likes best. `node.dont-fallback` makes that an error.
//!
//! Threads: the portal thread calls `begin`, then `run_until_end` (which watches the
//! pipeline's health) and takes the capture over with gst-launch when that reports a failure;
//! the "screen-capture-tx" thread calls `wait_ready` / `next_au` / `set_watched`.

use crate::encoder::vaapi::{self, EncodeChain};
use crate::screen_capture::hw_decode::h264_state::nal_ranges;
use gst::prelude::*;
use gst_video::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;
use log::{info, warn};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::VecDeque;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// pipewiresrc pushes its last buffer again after this long without a new one.
const KEEPALIVE_MS: u32 = 25;
/// On a still screen the last picture is encoded again this often (as the send loop does
/// on the CPU path: viewers drop a stream after 2.5 s without pictures).
const STILL_REPEAT_EVERY: Duration = Duration::from_millis(100);
/// The local preview's pace (the gst-launch capture sends one every 150 ms), and how often
/// it is refreshed while the screen does not change.
const PREVIEW_EVERY: Duration = Duration::from_millis(150);
const PREVIEW_STILL_EVERY: Duration = Duration::from_millis(1000);
/// Connecting to PipeWire, creating the VA context and encoding the first picture.
const FIRST_PICTURE_TIMEOUT: Duration = Duration::from_millis(2500);
/// With the keepalive a buffer arrives every 25 ms; this long without one, the capture is
/// dead (stream closed by the compositor, PipeWire gone).
const SOURCE_SILENCE: Duration = Duration::from_millis(400);
/// A picture went into the encoder this long ago and nothing came out.
const ENCODER_SILENCE: Duration = Duration::from_millis(400);
/// An asked-for keyframe that has not come out after this long is asked for again.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
/// The health checks run every 25 ms or less. A longer pause between two of them means this
/// process itself did not run (stopped, suspended, starved): silence is then counted anew.
const CHECK_GAP: Duration = Duration::from_millis(150);
/// The compositor ending the session takes the buffers away and reports the stream's error
/// a moment later (7 ms with Mutter); a source that only changed its format reports none.
/// How long "buffers taken away" waits to learn which it was.
const SOURCE_GONE_WAIT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Memory {
    /// DMABuf when the compositor and the driver agree on a format, system memory otherwise.
    Auto,
    DmaBuf,
    System,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The portal's PipeWire connection; `None` connects to the session's own daemon.
    pub fd: Option<RawFd>,
    pub node: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub memory: Memory,
    /// Experiments only: ask the compositor for at most this many pictures a second.
    pub max_fps: Option<u32>,
}

impl Config {
    /// `LITECORD_VAAPI_CAPTURE_MEMORY=dmabuf|system` and `LITECORD_VAAPI_CAPTURE_MAXFPS=N`
    /// are for experiments.
    pub fn new(fd: Option<RawFd>, node: u32, width: u32, height: u32, fps: u32) -> Self {
        let memory = match std::env::var("LITECORD_VAAPI_CAPTURE_MEMORY").as_deref() {
            Ok("dmabuf") => Memory::DmaBuf,
            Ok("system") => Memory::System,
            _ => Memory::Auto,
        };
        let max_fps = std::env::var("LITECORD_VAAPI_CAPTURE_MAXFPS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0);
        Self { fd, node, width, height, fps, memory, max_fps }
    }
}

/// Test hook `LITECORD_VAAPI_CAPTURE_TEST_FAIL`: `start` fails `begin`, `first` lets no
/// picture through at the start, and from the Nth access unit on `after:N` reports an
/// error, `stall:N` swallows the encoder's output, `nosrc:N` swallows the capture's and
/// `hang:N` blocks the streaming thread at the encoder for 20 s (a driver that hangs).
#[derive(Clone, Copy, PartialEq)]
enum TestFail {
    None,
    Start,
    First,
    After(u64),
    Stall(u64),
    NoSource(u64),
    Hang(u64),
}

fn test_fail() -> TestFail {
    let Ok(v) = std::env::var("LITECORD_VAAPI_CAPTURE_TEST_FAIL") else { return TestFail::None };
    let n = |s: &str| s.parse::<u64>().unwrap_or(0);
    match v.split_once(':') {
        Some(("after", s)) => TestFail::After(n(s)),
        Some(("stall", s)) => TestFail::Stall(n(s)),
        Some(("nosrc", s)) => TestFail::NoSource(n(s)),
        Some(("hang", s)) => TestFail::Hang(n(s)),
        _ if v == "start" => TestFail::Start,
        _ if v == "first" => TestFail::First,
        _ => TestFail::None,
    }
}

/// What the probes count (read by the test harness and the send loop's telemetry).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// What pipewiresrc negotiated with the compositor, and what vapostproc is told it gets.
    pub source_caps: String,
    pub converter_caps: String,
    pub dmabuf: bool,
    pub source_size: (u32, u32),
    /// Renegotiations by the source before its first picture (harmless).
    pub early_renegotiations: u64,
    /// Buffers from pipewiresrc, keepalive repeats included, and the distinct pictures.
    pub buffers: u64,
    pub pictures: u64,
    /// Into the encoder: new pictures, and repeats of the last one (still screen, keyframes).
    pub encoded_new: u64,
    pub encoded_repeats: u64,
    /// New pictures left out to keep the target frame rate.
    pub skipped_pace: u64,
    pub previews: u64,
    /// Microseconds the preview pictures took in all: reading back from the GPU, allocating
    /// the UI's pixel buffer, converting to RGBA and the UI's callback; and the preview
    /// thread's CPU time.
    pub preview_us: [u64; 4],
    /// New pictures that carried the producer's own timestamp on the pipeline's clock
    /// (Mutter stamps the moment it recorded the frame), and how many microseconds after
    /// it they left pipewiresrc: in all, and the longest.
    pub stamped_pictures: u64,
    pub source_delay_us: u64,
    pub source_delay_max_us: u64,
    /// Access units handed to the caller, and thrown away while a keyframe was awaited.
    pub delivered: u64,
    pub dropped_before_keyframe: u64,
}

/// One access unit with what is known about its picture.
pub struct Picture {
    pub data: Vec<u8>,
    pub idr: bool,
    /// The same screen content as the access unit before it.
    pub repeat: bool,
    /// From the buffer leaving pipewiresrc to the access unit being handed over.
    pub latency: Duration,
}

/// State the streaming threads' probes share with the callers.
struct Shared {
    started: Instant,
    watched: AtomicBool,
    /// The next picture goes into the encoder even if it is a repeat or early.
    pass_next: AtomicBool,
    /// Milliseconds after `started` at which the last buffer left pipewiresrc, and at which
    /// the stream's health was last looked at.
    last_buffer_ms: AtomicU64,
    last_check_ms: AtomicU64,
    /// Nothing more is taken from the source: it took its buffers back (see the module's
    /// notes), or the test hook says so.
    source_shut: AtomicBool,
    /// Test hook: the streaming thread blocks at the encoder.
    hang: AtomicBool,
    /// Times the source took its buffers back before it had delivered any.
    early_renegotiations: AtomicU64,
    buffers: AtomicU64,
    pictures: AtomicU64,
    /// Pictures the send loop has not counted yet.
    uncounted_pictures: AtomicU64,
    encoded_new: AtomicU64,
    encoded_repeats: AtomicU64,
    skipped_pace: AtomicU64,
    previews: AtomicU64,
    preview_us: [AtomicU64; 4],
    stamped_pictures: AtomicU64,
    source_delay_us: AtomicU64,
    source_delay_max_us: AtomicU64,
    /// Set when the stream is being dropped (the preview thread ends).
    closing: AtomicBool,
    /// What the gate decided for the pictures now inside vapostproc.
    decisions: Mutex<VecDeque<Decision>>,
    /// Pictures in the encoder: timestamp, whether a repeat, when it went in.
    in_flight: Mutex<VecDeque<(gst::ClockTime, bool, Instant)>>,
}

/// A picture let into vapostproc, and what for.
struct Decision {
    pts: gst::ClockTime,
    encode: bool,
    repeat: bool,
    preview: bool,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// The send thread's side: bitrate, keyframes, output.
struct Tx {
    requested_bps: u32,
    asked_at: Instant,
    applied_bps: u32,
    /// A keyframe was asked of the encoder and has not come out yet.
    keyframe_asked_at: Option<Instant>,
    /// Output is thrown away until a keyframe (a viewer just arrived).
    await_keyframe: bool,
    ready: VecDeque<gst::Sample>,
    delivered: u64,
    dropped_before_keyframe: u64,
}

struct Stream {
    id: u64,
    /// `None` only while being dropped.
    chain: Option<EncodeChain>,
    /// The duplicate of the PipeWire fd pipewiresrc was pointed at.
    fd: Option<OwnedFd>,
    shared: Arc<Shared>,
    tx: Mutex<Tx>,
    /// Why the stream was given up, once it was.
    failure: Mutex<Option<String>>,
    /// The source took its buffers away, and PipeWire reported the stream itself as failed
    /// (the node is gone).
    buffers_removed: AtomicBool,
    gone: AtomicBool,
    /// The first picture came out: the pipeline's start is over.
    start_over: bool,
    test_fail: TestFail,
    source_caps: String,
    converter_caps: String,
    dmabuf: bool,
    source_size: (u32, u32),
}

static CURRENT: Mutex<Option<Arc<Stream>>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Pipelines started and not yet fully stopped (a stopped share's pipeline ends in the
/// background).
static LIVE_PIPELINES: AtomicU64 = AtomicU64::new(0);

pub fn live_pipelines() -> u64 {
    LIVE_PIPELINES.load(Ordering::SeqCst)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn current() -> Option<Arc<Stream>> {
    lock(&CURRENT).clone()
}

/// Whether a screen share may use this path at all: `LITECORD_VAAPI_CAPTURE=0` and any
/// encoder setting other than "auto" or "vaapi" keep the gst-launch capture.
pub fn enabled() -> Result<(), String> {
    if std::env::var("LITECORD_VAAPI_CAPTURE").as_deref() == Ok("0") {
        return Err("LITECORD_VAAPI_CAPTURE=0".into());
    }
    let encoder = crate::video_settings::get_video_encoder();
    if !matches!(encoder.as_str(), "auto" | "vaapi") {
        return Err(format!("codificador '{encoder}' escolhido nas configurações"));
    }
    Ok(())
}

/// Whether this share is captured here, with the reason logged when it is not. A shared
/// window changes size whenever the user resizes it and each change ends the GPU capture
/// (see the module's notes); what a resize does to DMABuf pictures from a real compositor
/// is untried, so windows stay with the gst-launch capture unless
/// `LITECORD_VAAPI_CAPTURE_WINDOWS=1`.
pub fn allowed(window: bool) -> bool {
    let reason = match enabled() {
        Err(why) => why,
        Ok(()) if window && std::env::var("LITECORD_VAAPI_CAPTURE_WINDOWS").as_deref() != Ok("1") => "janela compartilhada; LITECORD_VAAPI_CAPTURE_WINDOWS=1 para experimentar".into(),
        Ok(()) => return true,
    };
    info!("🎯 [VAAPI] Captura na GPU desativada ({}); usando a captura por gst-launch", reason);
    false
}

/// The caps pipewiresrc may negotiate: what vapostproc takes as DMABuf (with its
/// `drm-format` list) and/or in system memory.
fn source_caps(memory: Memory, max_fps: Option<u32>) -> Result<gst::Caps, String> {
    let probe = gst::ElementFactory::make("vapostproc").build().map_err(|e| e.to_string())?;
    let accepted = probe.static_pad("sink").ok_or("vapostproc sem pad de entrada")?.query_caps(None);
    let mut caps = gst::Caps::new_empty();
    {
        let caps = caps.get_mut().ok_or("caps compartilhadas")?;
        // DMABuf first: pipewiresrc offers the formats in this order.
        for want_dmabuf in [true, false] {
            if (want_dmabuf && memory == Memory::System) || (!want_dmabuf && memory == Memory::DmaBuf) {
                continue;
            }
            for (structure, features) in accepted.iter_with_features() {
                let is_dmabuf = features.contains("memory:DMABuf");
                let is_system = !features.is_any() && features.contains("memory:SystemMemory");
                if (want_dmabuf && is_dmabuf) || (!want_dmabuf && is_system) {
                    let mut structure = structure.to_owned();
                    if let Some(max) = max_fps {
                        structure.set("max-framerate", gst::Fraction::new(max as i32, 1));
                    }
                    caps.append_structure_full(structure, Some(features.to_owned()));
                }
            }
        }
    }
    if caps.is_empty() {
        return Err("vapostproc não aceita DMABuf nem memória do sistema".into());
    }
    Ok(caps)
}

impl Stream {
    fn chain(&self) -> &EncodeChain {
        // Only `drop` takes it.
        self.chain.as_ref().expect("stream chain")
    }

    fn start(cfg: &Config, test_fail: TestFail, wanted: &dyn Fn() -> bool) -> Result<Self, String> {
        gst::init().map_err(|e| format!("GStreamer: {e}"))?;
        for element in ["pipewiresrc", "vapostproc", "capssetter"] {
            if gst::ElementFactory::find(element).is_none() {
                return Err(format!("elemento GStreamer ausente: {element}"));
            }
        }
        let element = vaapi::ENCODER_ELEMENTS.into_iter().find(|e| gst::ElementFactory::find(e).is_some()).ok_or("nenhum codificador H.264 VA-API registrado")?;
        let (width, height, fps) = (cfg.width & !1, cfg.height & !1, cfg.fps.max(1));
        if width == 0 || height == 0 {
            return Err("tamanho inválido".into());
        }

        // pipewiresrc keeps one connection per fd number for all its instances: this
        // duplicate stays open until this stream's pipeline is gone.
        let fd = match cfg.fd {
            Some(fd) => {
                let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                if dup < 0 {
                    return Err(format!("fd do PipeWire inválido: {}", std::io::Error::last_os_error()));
                }
                Some(unsafe { OwnedFd::from_raw_fd(dup) })
            }
            None => None,
        };
        let fd_prop = fd.as_ref().map(|fd| format!("fd={}", std::os::fd::AsRawFd::as_raw_fd(fd))).unwrap_or_default();
        // The queue keeps only the newest picture: when the encoder is behind, raw pictures
        // are skipped instead of latency building up.
        let description = format!(
            "pipewiresrc name=cap_src {fd_prop} path={node} do-timestamp=true keepalive-time={KEEPALIVE_MS} provide-clock=false \
               on-disconnect=error stream-properties=\"props,node.dont-fallback=(string)true\" \
             ! capsfilter name=cap_filter \
             ! capssetter name=cap_set join=true replace=false \
               caps=\"video/x-raw,framerate=(fraction){fps}/1,colorimetry=(string)sRGB,pixel-aspect-ratio=(fraction)1/1\" \
             ! queue name=cap_queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {chain}",
            node = cfg.node,
            chain = EncodeChain::description_scaled(element, vaapi::INITIAL_BITRATE_BPS, Some((width, height))),
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        // From here on dropping the chain stops the pipeline.
        let chain = EncodeChain::from_pipeline(pipeline)?;
        let by_name = |name: &str| chain.pipeline.by_name(name).ok_or(format!("elemento {name} ausente"));
        let filter = by_name("cap_filter")?;
        filter.set_property("caps", source_caps(cfg.memory, cfg.max_fps)?);

        let shared = Arc::new(Shared {
            started: Instant::now(),
            watched: AtomicBool::new(false),
            // The first picture is encoded whoever watches: it proves the whole path.
            pass_next: AtomicBool::new(test_fail != TestFail::First),
            last_buffer_ms: AtomicU64::new(0),
            last_check_ms: AtomicU64::new(0),
            source_shut: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            early_renegotiations: AtomicU64::new(0),
            buffers: AtomicU64::new(0),
            pictures: AtomicU64::new(0),
            uncounted_pictures: AtomicU64::new(0),
            encoded_new: AtomicU64::new(0),
            encoded_repeats: AtomicU64::new(0),
            skipped_pace: AtomicU64::new(0),
            previews: AtomicU64::new(0),
            preview_us: Default::default(),
            stamped_pictures: AtomicU64::new(0),
            source_delay_us: AtomicU64::new(0),
            source_delay_max_us: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            decisions: Mutex::new(VecDeque::new()),
            in_flight: Mutex::new(VecDeque::new()),
        });
        add_arrival_probe(&by_name("cap_set")?, &shared)?;
        add_gate(&by_name("cap_queue")?, &shared, fps)?;
        let (preview_tx, preview_rx) = sync_channel(1);
        add_encoder_probe(&chain.encoder, &shared, preview_tx)?;
        let preview_shared = shared.clone();
        std::thread::Builder::new().name("vaapi-capture-preview".into()).spawn(move || preview_thread(preview_rx, preview_shared)).map_err(|e| e.to_string())?;

        LIVE_PIPELINES.fetch_add(1, Ordering::SeqCst);
        let stream = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            chain: Some(chain),
            fd,
            shared,
            tx: Mutex::new(Tx {
                requested_bps: vaapi::INITIAL_BITRATE_BPS,
                asked_at: Instant::now(),
                applied_bps: vaapi::INITIAL_BITRATE_BPS,
                keyframe_asked_at: None,
                await_keyframe: false,
                ready: VecDeque::new(),
                delivered: 0,
                dropped_before_keyframe: 0,
            }),
            failure: Mutex::new(None),
            buffers_removed: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            start_over: false,
            test_fail,
            source_caps: String::new(),
            converter_caps: String::new(),
            dmabuf: false,
            source_size: (0, 0),
        };
        // pipewiresrc waits up to 30 s in this state change for the PipeWire stream to
        // start: it runs on its own thread, and the wait for the first picture covers it.
        let (state_tx, state_rx) = sync_channel(1);
        let pipeline = stream.chain().pipeline.clone();
        let change = move || {
            let _ = state_tx.send(pipeline.set_state(gst::State::Playing).is_ok());
        };
        std::thread::Builder::new().name("vaapi-capture-start".into()).spawn(change).map_err(|e| e.to_string())?;
        stream.first_picture(filter, state_rx, wanted)
    }

    /// Waits for the first picture to come out of the encoder and checks it is what the
    /// receivers take; the access unit itself is not used (nobody watches yet).
    fn first_picture(mut self, filter: gst::Element, playing: Receiver<bool>, wanted: &dyn Fn() -> bool) -> Result<Self, String> {
        let deadline = Instant::now() + FIRST_PICTURE_TIMEOUT;
        let mut is_playing = false;
        let sample = loop {
            if let Some(sample) = self.chain().sink.try_pull_sample(gst::ClockTime::from_mseconds(20)) {
                break sample;
            }
            if let Some(e) = self.bus_error() {
                return Err(e);
            }
            if !is_playing {
                match playing.try_recv() {
                    Ok(true) => is_playing = true,
                    Ok(false) | Err(TryRecvError::Disconnected) => return Err("o pipeline não chegou a PLAYING".into()),
                    Err(TryRecvError::Empty) => {}
                }
            }
            // (A sink that has not started yet says it is at the end too.)
            if is_playing && self.chain().sink.is_eos() {
                return Err("fim do fluxo".into());
            }
            if !wanted() {
                return Err("o compartilhamento foi encerrado antes do primeiro quadro".into());
            }
            if Instant::now() >= deadline {
                let got = self.shared.buffers.load(Ordering::Relaxed);
                return Err(format!("sem primeiro quadro em {} ms ({got} buffers do PipeWire)", FIRST_PICTURE_TIMEOUT.as_millis()));
            }
        };
        let buffer = sample.buffer().ok_or("amostra sem buffer")?;
        let map = buffer.map_readable().map_err(|e| e.to_string())?;
        vaapi::check_keyframe(map.as_slice())?;
        lock(&self.shared.in_flight).clear();

        let caps = filter.static_pad("src").and_then(|p| p.current_caps()).ok_or("sem caps negociadas")?;
        self.dmabuf = caps.features(0).is_some_and(|f| f.contains("memory:DMABuf"));
        if let Some(s) = caps.structure(0) {
            self.source_size = (s.get::<i32>("width").unwrap_or(0) as u32, s.get::<i32>("height").unwrap_or(0) as u32);
        }
        self.source_caps = caps.to_string();
        let relabelled = self.chain().pipeline.by_name("cap_set").and_then(|e| e.static_pad("src")).and_then(|p| p.current_caps());
        self.converter_caps = relabelled.map(|c| c.to_string()).unwrap_or_default();
        self.start_over = true;
        Ok(self)
    }

    /// Opens or closes the way into the encoder. Opening it asks for a keyframe and throws
    /// away whatever the encoder still had from before.
    fn watch(&self, tx: &mut Tx, watched: bool) {
        if watched == self.shared.watched.load(Ordering::Relaxed) {
            return;
        }
        if watched {
            while self.chain().sink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
            tx.ready.clear();
            lock(&self.shared.in_flight).clear();
            tx.await_keyframe = true;
            self.ask_keyframe(tx);
        } else {
            self.shared.pass_next.store(false, Ordering::Relaxed);
        }
        self.shared.watched.store(watched, Ordering::Relaxed);
    }

    /// The next picture into the encoder becomes an IDR with SPS and PPS, and goes in at
    /// once (on a still screen: the next keepalive buffer, within 25 ms).
    fn ask_keyframe(&self, tx: &mut Tx) {
        if self.chain().request_keyframe() {
            tx.keyframe_asked_at = Some(Instant::now());
        }
        self.shared.pass_next.store(true, Ordering::Relaxed);
    }

    /// The bitrate and keyframe wishes for the pictures from now on.
    fn control(&self, tx: &mut Tx, bitrate_bps: u32, want_idr: bool) {
        if bitrate_bps != tx.requested_bps {
            tx.requested_bps = bitrate_bps;
            tx.asked_at = Instant::now();
        }
        let retry = tx.keyframe_asked_at.is_some_and(|t| t.elapsed() >= KEYFRAME_RETRY);
        if retry {
            warn!("⚠️ [VAAPI] Keyframe pedido à captura na GPU não saiu em {} ms; pedindo de novo", KEYFRAME_RETRY.as_millis());
        }
        let want_idr = want_idr || retry;
        // vah264enc starts a new sequence whenever its bitrate changes: the new rate rides
        // on a keyframe that was going to be made anyway (same rule as VaapiEncoder).
        if tx.applied_bps != tx.requested_bps && (want_idr || tx.asked_at.elapsed() >= vaapi::BITRATE_MAX_WAIT) {
            self.chain().set_bitrate(tx.requested_bps);
            tx.applied_bps = tx.requested_bps;
            tx.keyframe_asked_at = Some(Instant::now());
            self.shared.pass_next.store(true, Ordering::Relaxed);
        } else if want_idr {
            self.ask_keyframe(tx);
        }
    }

    /// The sample as the access unit that goes on the wire.
    fn finish(&self, tx: &mut Tx, sample: &gst::Sample) -> Result<Option<Picture>, String> {
        let buffer = sample.buffer().ok_or("amostra sem buffer")?;
        // The encoder shifts its timestamps and its segment with them; the running time
        // is still the one the picture left pipewiresrc with.
        let segment = sample.segment().and_then(|s| s.downcast_ref::<gst::ClockTime>());
        let stamp = segment.and_then(|s| s.to_running_time(buffer.pts())).or(buffer.pts());
        let mut repeat = false;
        if let Some(stamp) = stamp {
            let mut in_flight = lock(&self.shared.in_flight);
            while in_flight.front().is_some_and(|(pts, _, _)| *pts <= stamp) {
                if let Some((pts, was_repeat, _)) = in_flight.pop_front() {
                    repeat = pts == stamp && was_repeat;
                }
            }
        }
        let latency = match (self.chain().pipeline.current_running_time(), stamp) {
            (Some(now), Some(stamp)) => Duration::from_nanos(now.nseconds().saturating_sub(stamp.nseconds())),
            _ => Duration::ZERO,
        };
        let au = {
            let map = buffer.map_readable().map_err(|e| e.to_string())?;
            vaapi::without_filler(map.as_slice().to_vec())
        };
        if !matches!(au.as_slice(), [0, 0, 1, ..] | [0, 0, 0, 1, ..]) {
            return Err("saída sem start code".into());
        }
        let idr = nal_ranges(&au).iter().any(|r| au[r.start] & 0x1f == 5);
        if !idr {
            if tx.await_keyframe {
                tx.dropped_before_keyframe += 1;
                return Ok(None);
            }
            return Ok(Some(Picture { data: au, idr, repeat, latency }));
        }
        tx.await_keyframe = false;
        tx.keyframe_asked_at = None;
        Ok(Some(Picture { data: crate::encoder::h264_vui::declare_colour(&au, 6, 6, 6), idr, repeat, latency }))
    }

    /// The first error on the pipeline's bus. Errors of pipewiresrc about its PipeWire
    /// stream are remembered: they tell a source that is gone from a GPU path that broke.
    fn bus_error(&self) -> Option<String> {
        let mut first = None;
        while let Some(msg) = self.chain().bus.pop_filtered(&[gst::MessageType::Error]) {
            let gst::MessageView::Error(e) = msg.view() else { continue };
            if msg.src().is_some_and(|s| s.name() == "cap_src") {
                if e.error().matches(gst::ResourceError::Failed) {
                    // "stream error: ...": PipeWire failed the stream (its node is gone).
                    self.gone.store(true, Ordering::SeqCst);
                } else if e.error().matches(gst::ResourceError::NotFound) {
                    // "all buffers have been removed": gone, or a new format. Not an error
                    // when the flush that came with it found no picture handed out yet.
                    if !self.shared.source_shut.load(Ordering::SeqCst) {
                        continue;
                    }
                    self.buffers_removed.store(true, Ordering::SeqCst);
                }
            }
            first.get_or_insert_with(|| format!("{} ({})", e.error(), e.debug().map(|d| d.to_string()).unwrap_or_default()));
        }
        first
    }

    /// What can be told without pulling: errors on the bus, the end of the stream, a
    /// capture that stopped delivering and, for the send loop, an encoder that stopped
    /// answering.
    fn health(&self, encoder_too: bool) -> Result<(), String> {
        if let Some(e) = self.bus_error() {
            return Err(e);
        }
        if self.chain().sink.is_eos() {
            return Err("fim do fluxo".into());
        }
        let now = self.shared.now_ms();
        if now.saturating_sub(self.shared.last_check_ms.swap(now, Ordering::Relaxed)) > CHECK_GAP.as_millis() as u64 {
            // Nobody looked for a while: this process did not run, and neither did the
            // pipeline's threads. They get the full time to deliver again.
            self.shared.last_buffer_ms.fetch_max(now, Ordering::Relaxed);
            lock(&self.shared.in_flight).iter_mut().for_each(|picture| picture.2 = Instant::now());
            return Ok(());
        }
        if encoder_too {
            // A picture that went in long ago and never came out: the encoder (or the
            // driver under it) stopped answering.
            let waiting = lock(&self.shared.in_flight).front().map(|(_, _, since)| since.elapsed());
            if let Some(waited) = waiting.filter(|w| *w >= ENCODER_SILENCE) {
                return Err(format!("o codificador não responde há {} ms", waited.as_millis()));
            }
        }
        let silent = now.saturating_sub(self.shared.last_buffer_ms.load(Ordering::Relaxed));
        if silent >= SOURCE_SILENCE.as_millis() as u64 {
            return Err(format!("a captura não entrega quadros há {silent} ms"));
        }
        Ok(())
    }

    fn stats(&self) -> Stats {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let tx = lock(&self.tx);
        Stats {
            source_caps: self.source_caps.clone(),
            converter_caps: self.converter_caps.clone(),
            dmabuf: self.dmabuf,
            source_size: self.source_size,
            early_renegotiations: load(&self.shared.early_renegotiations),
            buffers: load(&self.shared.buffers),
            pictures: load(&self.shared.pictures),
            encoded_new: load(&self.shared.encoded_new),
            encoded_repeats: load(&self.shared.encoded_repeats),
            skipped_pace: load(&self.shared.skipped_pace),
            previews: load(&self.shared.previews),
            preview_us: [0, 1, 2, 3].map(|i| load(&self.shared.preview_us[i])),
            stamped_pictures: load(&self.shared.stamped_pictures),
            source_delay_us: load(&self.shared.source_delay_us),
            source_delay_max_us: load(&self.shared.source_delay_max_us),
            delivered: tx.delivered,
            dropped_before_keyframe: tx.dropped_before_keyframe,
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Stopping waits for the streaming threads (and for PipeWire): never on the thread
        // that happens to drop the stream. The fd outlives the pipeline's connection.
        self.shared.closing.store(true, Ordering::SeqCst);
        let (chain, fd, start_over) = (self.chain.take(), self.fd.take(), self.start_over);
        let stop = move || {
            if let Some(chain) = chain.as_ref() {
                if !start_over {
                    // The state change to PLAYING may still be waiting inside pipewiresrc
                    // for the PipeWire stream (up to 30 s), and the change to NULL waits
                    // behind it: taking the source's pad out of service ends that wait.
                    if let Some(pad) = chain.pipeline.by_name("cap_src").and_then(|src| src.static_pad("src")) {
                        let _ = pad.set_active(false);
                    }
                }
                let _ = chain.pipeline.set_state(gst::State::Null);
            }
            drop(chain);
            drop(fd);
            LIVE_PIPELINES.fetch_sub(1, Ordering::SeqCst);
        };
        if let Err(e) = std::thread::Builder::new().name("vaapi-capture-teardown".into()).spawn(stop) {
            // The closure went with the failed spawn: its pipeline and fd are dropped there,
            // the pipeline stopping through EncodeChain's own background teardown.
            warn!("⚠️ [VAAPI] Não foi possível encerrar a captura na GPU em segundo plano: {}", e);
            LIVE_PIPELINES.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Numbers the pictures as they leave pipewiresrc: a keepalive buffer is a copy of the
/// buffer before it and shares its memory object, while every buffer taken from PipeWire
/// gets a new one. The number travels in the buffer's offset field.
fn add_arrival_probe(after: &gst::Element, shared: &Arc<Shared>) -> Result<(), String> {
    let shared = shared.clone();
    // The memory of the buffer before, and its picture number.
    let last = Mutex::new((0usize, 0u64));
    let pad = after.static_pad("src").ok_or("pad src ausente")?;
    // pipewiresrc (on-disconnect=error) sends flush-start the moment PipeWire takes buffers
    // back, before their memory is unmapped. Every picture it pushes from then on, the
    // keepalive's copies of the last one above all, may point into memory that is gone.
    // Before the first picture there is nothing of the kind, and the stream just goes on.
    let on_flush = shared.clone();
    pad.add_probe(gst::PadProbeType::EVENT_FLUSH, move |_, info| {
        if matches!(&info.data, Some(gst::PadProbeData::Event(event)) if event.type_() == gst::EventType::FlushStart) {
            if on_flush.buffers.load(Ordering::SeqCst) == 0 {
                on_flush.early_renegotiations.fetch_add(1, Ordering::Relaxed);
            } else {
                on_flush.source_shut.store(true, Ordering::SeqCst);
            }
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if shared.source_shut.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Drop;
        }
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() else { return gst::PadProbeReturn::Ok };
        shared.buffers.fetch_add(1, Ordering::SeqCst);
        shared.last_buffer_ms.store(shared.now_ms(), Ordering::Relaxed);
        // No picture in it (PipeWire marks metadata-only updates this way).
        if buffer.n_memory() == 0 || buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
            return gst::PadProbeReturn::Drop;
        }
        let memory = buffer.peek_memory(0).as_ptr() as usize;
        let mut last = lock(&last);
        let new_picture = memory != last.0;
        if new_picture {
            *last = (memory, last.1 + 1);
            shared.pictures.fetch_add(1, Ordering::Relaxed);
            shared.uncounted_pictures.fetch_add(1, Ordering::Relaxed);
        }
        let stamped = buffer.pts();
        let buffer = buffer.make_mut();
        buffer.set_offset(last.1);
        // pipewiresrc keeps the producer's timestamp when there is one (do-timestamp only
        // fills in missing ones), on whatever clock the producer has. Everything behind
        // this point reads the timestamp as "when the picture arrived here".
        if let Some(now) = pad.parent_element().and_then(|e| e.current_running_time()) {
            // A producer on the same clock (the compositor) tells how old the picture is.
            let age = stamped.and_then(|t| now.checked_sub(t)).map(|d| d.useconds()).filter(|us| *us > 0 && *us < 1_000_000);
            if let Some(us) = age.filter(|_| new_picture) {
                shared.stamped_pictures.fetch_add(1, Ordering::Relaxed);
                shared.source_delay_us.fetch_add(us, Ordering::Relaxed);
                shared.source_delay_max_us.fetch_max(us, Ordering::Relaxed);
            }
            buffer.set_pts(now);
            buffer.set_dts(now);
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    Ok(())
}

/// What goes into vapostproc, and what for. To the encoder: nothing while nobody watches;
/// new pictures at most `fps` times a second (the monitor may refresh faster); the last
/// picture again every 100 ms while the screen is still, and at once when a keyframe waits
/// for a picture to ride on. To the preview: one picture every 150 ms, and only one a
/// second while nothing changes.
fn add_gate(queue: &gst::Element, shared: &Arc<Shared>, fps: u32) -> Result<(), String> {
    struct Gate {
        last_picture: u64,
        last_pass: Instant,
        due: Instant,
        preview_picture: u64,
        last_preview: Instant,
    }
    let shared = shared.clone();
    let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
    let start = Instant::now();
    let gate = Mutex::new(Gate { last_picture: 0, last_pass: start, due: start, preview_picture: 0, last_preview: start.checked_sub(PREVIEW_STILL_EVERY).unwrap_or(start) });
    let pad = queue.static_pad("src").ok_or("pad src ausente")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Some(pts) = buffer.pts() else { return gst::PadProbeReturn::Drop };
        let mut gate = lock(&gate);
        let now = Instant::now();
        let picture = buffer.offset();

        let forced = shared.pass_next.load(Ordering::Relaxed);
        let repeat = picture == gate.last_picture;
        let encode = if !forced && !shared.watched.load(Ordering::Relaxed) {
            false
        } else if forced {
            true
        } else if repeat {
            now.duration_since(gate.last_pass) + Duration::from_millis(2) >= STILL_REPEAT_EVERY
        } else if now + interval / 8 < gate.due {
            // Too early for the target rate. Should the screen stop changing right now,
            // the keepalive brings this picture again and it passes then.
            shared.skipped_pace.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        };
        if encode {
            // The schedule keeps the average at `fps` whatever the monitor's rhythm (at
            // 144 Hz pictures pass 2, 2 and 3 refreshes apart). A picture that came late
            // may be followed sooner; after a pause the schedule starts over.
            gate.due = (gate.due + interval).max(now + interval / 4);
            gate.last_picture = picture;
            gate.last_pass = now;
            shared.pass_next.store(false, Ordering::Relaxed);
            if repeat { &shared.encoded_repeats } else { &shared.encoded_new }.fetch_add(1, Ordering::Relaxed);
        }

        let since_preview = now.duration_since(gate.last_preview);
        let preview = since_preview >= PREVIEW_EVERY && (picture != gate.preview_picture || since_preview >= PREVIEW_STILL_EVERY);
        if preview {
            gate.last_preview = now;
            gate.preview_picture = picture;
        }
        if !encode && !preview {
            return gst::PadProbeReturn::Drop;
        }
        let mut decisions = lock(&shared.decisions);
        // Never long: vapostproc hands each picture on before it takes the next.
        if decisions.len() >= 16 {
            decisions.pop_front();
        }
        decisions.push_back(Decision { pts, encode, repeat, preview });
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    Ok(())
}

/// At the encoder's door, behind vapostproc: pictures meant for the preview are handed to
/// its thread (as NV12 at the stream's size, still on the GPU), and only the ones the gate
/// chose for encoding go in.
fn add_encoder_probe(encoder: &gst::Element, shared: &Arc<Shared>, preview: SyncSender<(gst::Buffer, gst::Caps)>) -> Result<(), String> {
    let shared = shared.clone();
    let pad = encoder.static_pad("sink").ok_or("pad sink ausente")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Some(pts) = buffer.pts() else { return gst::PadProbeReturn::Drop };
        let decision = {
            let mut decisions = lock(&shared.decisions);
            while decisions.front().is_some_and(|d| d.pts < pts) {
                decisions.pop_front();
            }
            match decisions.front() {
                Some(d) if d.pts == pts => decisions.pop_front(),
                _ => None,
            }
        };
        // vapostproc keeps the timestamps; a picture it cannot be matched to is not used.
        let Some(decision) = decision else { return gst::PadProbeReturn::Drop };
        if decision.preview {
            if let Some(caps) = pad.current_caps() {
                // The preview thread still busy with the picture before: this one is skipped.
                let _ = preview.try_send((buffer.clone(), caps));
            }
        }
        if !decision.encode {
            return gst::PadProbeReturn::Drop;
        }
        let mut in_flight = lock(&shared.in_flight);
        // Never long: the encoder answers within milliseconds or the stream is given up.
        if in_flight.len() >= 64 {
            in_flight.pop_front();
        }
        in_flight.push_back((pts, decision.repeat, Instant::now()));
        drop(in_flight);
        if shared.hang.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(20));
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    Ok(())
}

/// Reads each preview picture back from the GPU and hands it to the UI callback as RGBA at
/// the stream's size: the contract of the gst-launch capture's reader loop.
fn preview_thread(pictures: Receiver<(gst::Buffer, gst::Caps)>, shared: Arc<Shared>) {
    use crate::screen_capture::sources::linux_portal::{draw_test_watermark, get_test_watermark_digit, PORTAL_LOCAL_CB};
    // The picture as copied out of its mapping of GPU memory, each plane in one piece:
    // reading such memory a little at a time is several times slower.
    let (mut y_copy, mut uv_copy) = (Vec::new(), Vec::new());
    while !shared.closing.load(Ordering::Relaxed) {
        let (buffer, caps) = match pictures.recv_timeout(Duration::from_millis(250)) {
            Ok(picture) => picture,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        shared.previews.fetch_add(1, Ordering::Relaxed);
        let Some(callback) = lock(&PORTAL_LOCAL_CB).clone() else { continue };
        let started = Instant::now();
        let Ok(info) = gst_video::VideoInfo::from_caps(&caps) else { continue };
        if info.format() != gst_video::VideoFormat::Nv12 {
            continue;
        }
        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &info) else { continue };
        let (w, h) = (info.width() as usize, info.height() as usize);
        let (y_stride, uv_stride) = (frame.plane_stride()[0] as usize, frame.plane_stride()[1] as usize);
        let (Ok(y), Ok(uv)) = (frame.plane_data(0), frame.plane_data(1)) else { continue };
        if w == 0 || h == 0 || w % 2 != 0 || y_stride < w || uv_stride < w || y.len() < y_stride * (h - 1) + w || uv.len() < uv_stride * (h.div_ceil(2) - 1) + w {
            continue;
        }
        y_copy.clear();
        y_copy.extend_from_slice(&y[..y_stride * (h - 1) + w]);
        uv_copy.clear();
        uv_copy.extend_from_slice(&uv[..uv_stride * (h.div_ceil(2) - 1) + w]);
        drop(frame);
        drop(buffer);
        let mapped = Instant::now();
        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
        let allocated = Instant::now();
        nv12_to_rgba(&y_copy, &uv_copy, y_stride, uv_stride, w, h, pixels.make_mut_slice());
        if let Some(digit) = get_test_watermark_digit() {
            // The badge is blended over a BGRA copy; here only the preview shows it.
            let mut bgra: Vec<u8> = pixels.as_bytes().chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
            draw_test_watermark(pixels.make_mut_slice(), &mut bgra, w as u32, h as u32, digit);
        }
        callback(pixels);
        let micros = |d: Duration| d.as_micros() as u64;
        shared.preview_us[0].fetch_add(micros(mapped - started), Ordering::Relaxed);
        shared.preview_us[1].fetch_add(micros(allocated - mapped), Ordering::Relaxed);
        shared.preview_us[2].fetch_add(micros(allocated.elapsed()), Ordering::Relaxed);
        let mut cpu = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut cpu) };
        shared.preview_us[3].store(cpu.tv_sec as u64 * 1_000_000 + cpu.tv_nsec as u64 / 1000, Ordering::Relaxed);
    }
}

/// NV12 in the encoder's colours (BT.601, limited range) to RGBA of the same size, on the
/// calling thread: a parallel conversion wakes every core for a few pictures a second.
/// `w` is even. Plain indexed loops over slices of known length: this crate's release
/// profile (256 codegen units, no LTO) leaves zipped iterators as a call per pixel.
fn nv12_to_rgba(y: &[u8], uv: &[u8], y_stride: usize, uv_stride: usize, w: usize, h: usize, out: &mut [Rgba8Pixel]) {
    // What each pixel's chroma adds to its red, green and blue.
    let (mut red, mut green, mut blue) = (vec![0i32; w], vec![0i32; w], vec![0i32; w]);
    for row in 0..h {
        if row % 2 == 0 {
            // Two rows of two pixels share each chroma sample.
            let uv_row = &uv[row / 2 * uv_stride..][..w];
            let (red, green, blue) = (&mut red[..w], &mut green[..w], &mut blue[..w]);
            for x in (0..w - 1).step_by(2) {
                let (d, e) = (uv_row[x] as i32 - 128, uv_row[x + 1] as i32 - 128);
                let (r, g, b) = (409 * e + 128, -100 * d - 208 * e + 128, 516 * d + 128);
                (red[x], red[x + 1]) = (r, r);
                (green[x], green[x + 1]) = (g, g);
                (blue[x], blue[x + 1]) = (b, b);
            }
        }
        let y_row = &y[row * y_stride..][..w];
        let out_row = &mut out[row * w..][..w];
        let (red, green, blue) = (&red[..w], &green[..w], &blue[..w]);
        for x in 0..w {
            let c = 298 * (y_row[x] as i32 - 16);
            out_row[x] = Rgba8Pixel::new(((c + red[x]) >> 8).clamp(0, 255) as u8, ((c + green[x]) >> 8).clamp(0, 255) as u8, ((c + blue[x]) >> 8).clamp(0, 255) as u8, 255);
        }
    }
}

/// Starts capturing node `node` of the PipeWire connection `fd` on the GPU. `None` (with
/// the reason logged) means the caller runs its gst-launch capture as before; `Some(id)`
/// means pictures flow, the first one having been encoded and checked. Returns within
/// `FIRST_PICTURE_TIMEOUT`, sooner when `wanted` turns false (the share was stopped, or a
/// newer one started): a stream is never put in service for a share that is over.
pub fn begin(fd: Option<RawFd>, node: u32, width: u32, height: u32, fps: u32, wanted: &dyn Fn() -> bool) -> Option<u64> {
    match begin_with(&Config::new(fd, node, width, height, fps), wanted) {
        Ok(id) => Some(id),
        Err(e) => {
            warn!("⚠️ [VAAPI] Captura na GPU indisponível ({}); usando a captura por gst-launch", e);
            None
        }
    }
}

pub fn begin_with(cfg: &Config, wanted: &dyn Fn() -> bool) -> Result<u64, String> {
    let test_fail = test_fail();
    if test_fail == TestFail::Start {
        return Err("falha simulada (LITECORD_VAAPI_CAPTURE_TEST_FAIL=start)".into());
    }
    let started = Instant::now();
    let stream = Stream::start(cfg, test_fail, wanted)?;
    let (id, source_size, dmabuf) = (stream.id, stream.source_size, stream.dmabuf);
    {
        // `end` takes this lock after the share's flag went down: checked under it, a share
        // stopped at any moment either finds the stream here or keeps it from being put here.
        let mut current = lock(&CURRENT);
        if !wanted() {
            return Err("o compartilhamento foi encerrado enquanto a captura iniciava".into());
        }
        *current = Some(Arc::new(stream));
        ACTIVE.store(true, Ordering::SeqCst);
    }
    info!(
        "🚀 [VAAPI] Captura na GPU ativa: nó PipeWire {} {}x{} ({}) -> {}x{} {} FPS, primeiro quadro em {:.0} ms",
        cfg.node,
        source_size.0,
        source_size.1,
        if dmabuf { "DMABuf, sem cópia" } else { "memória do sistema" },
        cfg.width & !1,
        cfg.height & !1,
        cfg.fps,
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(id)
}

/// Whether the send loop should take access units from here instead of encoding frames.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Takes the stream out of service; its pipeline stops in the background.
fn retire(only: Option<u64>) -> Option<Arc<Stream>> {
    let mut current = lock(&CURRENT);
    if only.is_some_and(|id| current.as_ref().map(|s| s.id) != Some(id)) {
        return None;
    }
    let stream = current.take();
    ACTIVE.store(false, Ordering::SeqCst);
    stream
}

/// Takes a failed stream out of service; `run_until_end` reports what became of it.
fn give_up_stream(stream: &Stream, reason: &str) {
    lock(&stream.failure).get_or_insert_with(|| reason.to_string());
    retire(Some(stream.id));
}

/// The share is over (or is starting over): stops the stream, if there is one. Never blocks.
pub fn end() {
    if retire(None).is_some() {
        info!("🛑 [VAAPI] Captura na GPU encerrada");
    }
}

/// How a stream that was running came to its end.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ended {
    /// The share was stopped, or is no longer the caller's.
    Stopped,
    /// The GPU path failed; the source is still there for the gst-launch capture.
    Failed,
    /// PipeWire reported the source gone (the compositor ended the session).
    SourceGone,
}

/// Blocks while stream `id` runs, watching its health, and tells how it ended: stopped
/// (`wanted` turned false, or `end` was called), failed with the source still there (the
/// caller takes the capture over), or its source gone.
pub fn run_until_end(id: u64, wanted: impl Fn() -> bool) -> Ended {
    let Some(stream) = current().filter(|s| s.id == id) else { return Ended::Stopped };
    let reason = loop {
        // A failure the send loop noticed first.
        if let Some(reason) = lock(&stream.failure).clone() {
            break reason;
        }
        if current().map(|s| s.id) != Some(id) {
            return Ended::Stopped;
        }
        if !wanted() {
            if retire(Some(id)).is_some() {
                info!("🛑 [VAAPI] Captura na GPU encerrada");
            }
            return Ended::Stopped;
        }
        if let Err(reason) = stream.health(false) {
            give_up_stream(&stream, &reason);
            break reason;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if stream.buffers_removed.load(Ordering::SeqCst) {
        let deadline = Instant::now() + SOURCE_GONE_WAIT;
        while !stream.gone.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            let _ = stream.bus_error();
        }
    }
    if stream.gone.load(Ordering::SeqCst) {
        warn!("⚠️ [VAAPI] A fonte da captura na GPU deixou de existir ({}); a captura deste compartilhamento terminou", reason);
        Ended::SourceGone
    } else {
        warn!("⚠️ [VAAPI] Captura na GPU falhou ({}); a transmissão continua pela captura por gst-launch", reason);
        Ended::Failed
    }
}

/// Somebody started or stopped watching: nothing is encoded while nobody does.
pub fn set_watched(watched: bool) {
    if let Some(stream) = current() {
        stream.watch(&mut lock(&stream.tx), watched);
    }
}

/// Waits up to `max` for an access unit; true when `next_au` has one to give.
pub fn wait_ready(max: Duration) -> bool {
    let started = Instant::now();
    if let Some(stream) = current() {
        let mut tx = lock(&stream.tx);
        if !tx.ready.is_empty() {
            return true;
        }
        if let Some(sample) = stream.chain().sink.try_pull_sample(gst::ClockTime::from_nseconds(max.as_nanos() as u64)) {
            tx.ready.push_back(sample);
            return true;
        }
    }
    // A sink that stopped answers at once: the caller's loop must not spin on it.
    if let Some(left) = max.checked_sub(started.elapsed()).filter(|d| *d > Duration::from_millis(1)) {
        std::thread::park_timeout(left);
    }
    false
}

/// The send loop's turn while somebody watches: sets the bitrate and keyframe wishes for
/// the pictures to come and returns the oldest finished access unit, if there is one.
pub fn next_picture(bitrate_bps: u32, want_idr: bool) -> Option<Picture> {
    let stream = current()?;
    let mut tx = lock(&stream.tx);
    stream.watch(&mut tx, true);
    stream.control(&mut tx, bitrate_bps, want_idr);
    let swallow = match stream.test_fail {
        TestFail::After(n) if tx.delivered >= n => {
            drop(tx);
            give_up_stream(&stream, "falha simulada (LITECORD_VAAPI_CAPTURE_TEST_FAIL=after)");
            return None;
        }
        TestFail::NoSource(n) if tx.delivered >= n => {
            stream.shared.source_shut.store(true, Ordering::SeqCst);
            false
        }
        TestFail::Hang(n) if tx.delivered >= n => {
            stream.shared.hang.store(true, Ordering::Relaxed);
            false
        }
        TestFail::Stall(n) => tx.delivered >= n,
        _ => false,
    };
    let failure = loop {
        let sample = tx.ready.pop_front().or_else(|| stream.chain().sink.try_pull_sample(gst::ClockTime::ZERO));
        let Some(sample) = sample else {
            // Nothing finished.
            break stream.health(true);
        };
        if swallow {
            continue;
        }
        match stream.finish(&mut tx, &sample) {
            Ok(Some(picture)) => {
                tx.delivered += 1;
                return Some(picture);
            }
            Ok(None) => {}
            Err(e) => break Err(e),
        }
    };
    drop(tx);
    if let Err(reason) = failure {
        give_up_stream(&stream, &reason);
    }
    None
}

/// `next_picture` for the send loop, which only wants the bytes.
pub fn next_au(bitrate_bps: u32, want_idr: bool) -> Option<Vec<u8>> {
    next_picture(bitrate_bps, want_idr).map(|p| p.data)
}

/// New pictures the compositor delivered since the last call (the send loop's telemetry).
pub fn take_new_pictures() -> u32 {
    current().map(|s| s.shared.uncounted_pictures.swap(0, Ordering::Relaxed) as u32).unwrap_or(0)
}

pub fn stats() -> Option<Stats> {
    current().map(|s| s.stats())
}
