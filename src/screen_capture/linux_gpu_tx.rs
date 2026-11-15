//! Linux screen share captured and encoded on the GPU, inside this process:
//!
//!   pipewiresrc -> queue -> vapostproc (scale, NV12) -> vah264enc -> appsink   access units
//!                                                    \-> a few pictures a second, read back
//!                                                        for the local preview
//!
//! The stream's frame is the preset's (854x480, 1280x720, 1920x1080) whatever the source's
//! shape: the picture is scaled to fit it, centred, and the rest is black bars, where the
//! gst-launch capture's `videoscale` puts them (`linux_portal::letterbox`), so a share that
//! falls back to that capture keeps its geometry. A bar a single pixel thin is not made
//! (`picture_rect`): the picture fills the frame then, and the gst-launch capture does the
//! same for a monitor (a shared window in such a shape gets the one-pixel bar there).
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
//!   DMABuf negotiation. So nothing is asked and the pace is kept here, in a pad probe
//!   (`add_gate`; which pictures pass is `cadence::Cadence`: evenly spaced, a whole number
//!   of refreshes apart and, for a source that holds its rate, not faster than the stream,
//!   so that monitor gives a stream of 60 its 48 pictures a second, each three refreshes
//!   after the one before).
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
//! - When the source renegotiates (a synthetic one changing its size) PipeWire takes its
//!   buffers away, unmapping memory that pictures still on their way to vapostproc point
//!   into, and the keepalive goes on pushing the last one: a crash in vapostproc's upload.
//!   `on-disconnect=error` makes pipewiresrc send flush-start first (which waits for the
//!   picture being converted) and then report an error; from that flush on nothing more is
//!   taken from the source and the stream is given up. The gst-launch capture follows such
//!   changes, so the share goes on there.
//! - A shared window is not such a source. Mutter (GNOME 51, RecordWindow, what the portal
//!   uses) gives a window's stream the size of the window's monitor once and for all and
//!   never renegotiates: each buffer carries the window's rectangle in it (PipeWire's crop
//!   region; pipewiresrc makes it a GstVideoCropMeta). Resizing in one step or as a drag
//!   does (sizes 3-16 ms apart), maximising and back, fullscreen on either monitor and back,
//!   minimising and another window on top change that rectangle at most and nothing else:
//!   same connection, same buffers, same format, no gap (the longest between two access
//!   units was 92 ms). Only closing the window ends the stream, as a source that is gone.
//!   (Bringing a minimised window back could not be tried without a person: Wayland lets
//!   no client do it.) vapostproc takes the
//!   rectangle as its input region, but where it puts the picture in the frame comes from
//!   the format alone, so a window of another shape than its monitor came out stretched
//!   over the whole frame. `add_gate` tells vapostproc the window's shape with a format of
//!   its own in front of each picture that has a new one (see there), and the bars follow
//!   (`rect_generation`). The shape told is the window's to within two pixels: the one
//!   whose bars are an even number of pixels wide (`even_fit`), a window having any size
//!   and an odd edge being a coloured line in NV12. At a change in one step Mutter now and
//!   then sends one buffer whose rectangle is a frame ahead of its contents; the gate
//!   leaves the first buffer of a much different size out (see there). A minimised window
//!   goes on being drawn into its DMABuf stream, after 0.3 s of black while Mutter
//!   animates. gst-launch knows nothing of the rectangle: it shows the whole monitor-sized
//!   buffer, the window small in its top left corner. So the capture a window share falls
//!   back to, or is kept on, is that pipeline inside this process with a videocrop that
//!   follows the rectangle (`linux_portal::WindowCapture`). In system memory Mutter also
//!   draws its own maximise and restore animations into the stream (about 0.3 s each).
//! - Mutter renegotiates the very same format right after the stream first starts, about
//!   one start in ten. No picture has been handed out then, so that one is let pass.
//! - `path=N` is only a wish: with the node gone, the session manager links the stream to
//!   whatever video source it likes best. `node.dont-fallback` makes that an error.
//! - `vapostproc add-borders=true` keeps the picture's shape, but radeonsi (Mesa 26.2) only
//!   draws the picture: the bars keep whatever the surface held (new surfaces show green).
//!   Nothing else writes those surfaces either, and the buffer pool hands the same few out
//!   again and again, so each surface's bars are painted here once, the first time it comes
//!   out of vapostproc (`paint_bars`: the surface is mapped and written by the CPU, 1-2 ms
//!   at 1080p, five surfaces per encoder; keyframes and previews checked over whole shares
//!   stayed black). A driver that fills the borders itself fills them black, as gst-va asks
//!   it to: the same colour, which is why the bars are black and not the #111214 of the
//!   Windows capture (`videoscale` on the gst-launch path only does black as well). Should
//!   a surface refuse to be painted, its picture goes to nobody, the stream is given up and
//!   gst-launch takes over.
//! - Where the picture's edge falls on an odd row or column, it shares a chroma sample with
//!   the bar next to it. radeonsi writes the picture's chroma from ceil(start / 2) to
//!   ceil(end / 2) (plane dumps of vapostproc alone): at an odd left or top edge the shared
//!   sample is never written and keeps the bar's paint, so the picture's first column or
//!   row has the right brightness and no colour; at an odd right or bottom edge it is
//!   written with every picture, so the bar's first row or column is black with the
//!   picture's colour over it (pure red: 153,0,0 where the gst-launch path, whose encoder
//!   averages the chroma, shows 86,0,0), and the picture's last row is grey only in the
//!   first picture of each surface. One line of pixels either way; taking the tint away
//!   would mean mapping and painting every picture.
//! - The H.264 profile follows the viewers (Main only while all of them decode it), and
//!   vah264enc cannot change it while it runs. Only the encoder is replaced
//!   (`vaapi::EncoderSection`): the capture, the queue and vapostproc go on, and the new
//!   encoder's first picture is an IDR one or two frame slots after the old encoder's last
//!   picture (starting a whole pipeline again takes 70-520 ms to its first picture, on a
//!   connection the old one may not have left yet). The new encoder is linked
//!   in while its input pad is idle (a picture held in that pad's probe at that moment goes
//!   to the new encoder), and the renegotiation request that linking sends
//!   upstream is stopped at the queue (see above for what pipewiresrc does with one).
//!   The encoder is replaced in 1-6 ms and the picture that was in the old one is not sent,
//!   so viewers wait about one picture's slot longer than between two pictures: measured
//!   from the last access unit of the old profile to the IDR of the new one, in both
//!   directions, 19-41 ms at 60 pictures a second (Mutter at 144 Hz, synthetic sources at
//!   60; some 150 changes), 34-55 ms at 30, up to 83 ms at 15, and 47-125 ms on a still
//!   screen (the 100 ms repeat and one keepalive).
//!   The new encoder is made and told the format on a thread of its own, waited for 300 ms
//!   at most (`prepare_section`): the send loop is not left inside a driver that hangs.
//!   Its rate control starts anew: on a game at 8 Mbit/s (QP floor 30) the new encoder's
//!   first keyframe came out at a mean QP of 41 (50-60 KB) where the keyframes that follow
//!   are at 30-36 (95-150 KB), and the pictures behind it were back at the floor after
//!   10-12 of them; a keyframe of the running encoder is followed by much the same climb
//!   back, from 36-39.
//! - Main is only used once it is proven, by the rules of `VaapiEncoder`: the new encoder is
//!   told the format before the old one is touched (it opens its VA context for the profile
//!   then; refused, the stream stays as it is), its first keyframe must say profile_idc 77,
//!   one late first picture is forgiven, and until that keyframe an error inside it is kept
//!   from the rest of the pipeline (a flow error would stop the capture's thread). Whatever
//!   fails, the stream goes back to a Constrained Baseline encoder on the GPU. The one
//!   exception is the capture's own thread not coming back from the encoder (a driver that
//!   hangs): no encoder can be fed then, and after `ENCODER_SILENCE` the share goes to
//!   gst-launch, as it does for an encoder that has proven itself.
//!
//! - NVIDIA (`vaapi::Backend::Nvenc`, nvh264enc): glupload takes the DMABuf, gltransformation
//!   fits the picture into the frame and clears the bars to black (no painting), glcolorconvert
//!   makes the NV12 in the encoder's colours. DMABuf only: on NVIDIA's EGL glupload from
//!   system memory gave black pictures, so without DMABuf the share stays with gst-launch.
//!   Found against Hyprland (xdg-desktop-portal-hyprland) on an RTX 2080 Ti, driver 615.
//!   A window share from xdph comes as pictures of the window's own size (no crop region),
//!   which gltransformation fits like a monitor's; a resize renegotiates the stream, which
//!   ends this capture as for any source. The GL elements draw a whole buffer whatever its
//!   crop region, so a window in a monitor-sized stream (Mutter's) would show the monitor:
//!   such a stream is given up at its first cropped picture (`crop_refused`) and the share
//!   goes to the window capture, which follows the crop.
//!
//! Threads: the portal thread calls `begin`, then `run_until_end` (which watches the
//! pipeline's health) and takes the capture over with gst-launch when that reports a failure;
//! the "screen-capture-tx" thread calls `wait_ready` / `next_au` / `set_watched`.

use crate::encoder::vaapi::{self, EncodeChain, EncoderSection};
pub use crate::screen_capture::sources::linux_portal::{letterbox, picture_rect};
use crate::screen_capture::hw_decode::h264_state::nal_ranges;
use gst::prelude::*;
use gst_video::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::{info, warn};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::VecDeque;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use super::cadence::{self, Cadence};
use super::live_controls::EncoderFloor;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const CROP_REFUSED: &str = "a janela vem recortada de um quadro do tamanho do monitor, e o caminho GL (NVENC) mostra o quadro inteiro";

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
/// Asked for this many times in a row without coming out, the keyframe is not going to:
/// pictures no longer reach the encoder (the streaming thread is stuck somewhere the other
/// checks do not look, such as in vapostproc).
const KEYFRAME_TRIES: u32 = 3;
/// The health checks run every 25 ms or less. A longer pause between two of them means this
/// process itself did not run (stopped, suspended, starved): silence is then counted anew.
const CHECK_GAP: Duration = Duration::from_millis(150);
/// The compositor ending the session takes the buffers away and reports the stream's error
/// a moment later (7 ms with Mutter); a source that only changed its format reports none.
/// How long "buffers taken away" waits to learn which it was.
const SOURCE_GONE_WAIT: Duration = Duration::from_millis(100);
/// PipeWire's stream error when the node it is linked to has gone (`node.dont-fallback`):
/// "stream error: defined target not found".
const SOURCE_GONE_ERROR: &str = "target not found";
/// Replacing the encoder waits for the picture on its way into the old one: a frame's time
/// at most. After this long the streaming thread is stuck in it.
const SWITCH_IDLE_TIMEOUT: Duration = Duration::from_millis(300);
/// Building the new encoder and telling it the format (it opens its VA context then) takes
/// 1-5 ms. After this long the driver is not answering and the send loop stops waiting.
const SWITCH_PREPARE_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a Main encoder that has not proven itself may take for its first keyframe.
const MAIN_FIRST_PICTURE: Duration = vaapi::FIRST_FRAME_TIMEOUT;

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
    /// The encoder's QP floor (the stream quality setting).
    pub min_qp: u32,
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
        // (The profile is not a setting of the capture: it starts in Constrained Baseline
        // and follows what the send loop asks for with every `next_picture`.)
        let min_qp = super::live_controls::min_qp_for_new_encoder();
        Self { fd, node, width, height, fps, memory, max_fps, min_qp }
    }
}

/// Test hook `LITECORD_VAAPI_CAPTURE_TEST_FAIL`: `start` fails `begin`, `first` lets no
/// picture through at the start, and from the Nth access unit on `after:N` reports an
/// error, `stall:N` swallows the encoder's output, `nosrc:N` swallows the capture's and
/// `hang:N` blocks the streaming thread at the encoder for 20 s (a driver that hangs).
/// H.264 Main going wrong: `nomain` makes every Main encoder ask for a profile the driver
/// does not have, `mainrefuse` makes it refuse the pictures' format, `mainprofile` makes it
/// produce Constrained Baseline, `mainbreak` makes it fail on its first picture, and
/// `mainlate:N` withholds the first picture of a Main encoder N times (one is forgiven).
/// `prepstuck:N` blocks the making of a new encoder from the Nth replacement on (a driver
/// that hangs while a VA context is created), and `nobars` makes the painting of the bars
/// around the picture fail. A change of quality floor going wrong: `floorrefuse` makes the
/// encoder for a new floor refuse the pictures' format (the old one goes on), `floormute`
/// makes it put out nothing and `floorbreak` makes it fail on its first picture (the floor
/// before it comes back with another encoder).
#[derive(Clone, Copy, PartialEq)]
enum TestFail {
    None,
    Start,
    First,
    After(u64),
    Stall(u64),
    NoSource(u64),
    Hang(u64),
    NoMain,
    MainRefuse,
    MainProfile,
    MainBreak,
    MainLate(u64),
    PrepStuck(u64),
    NoBars,
    FloorRefuse,
    FloorMute,
    FloorBreak,
}

fn test_fail() -> TestFail {
    let Ok(v) = std::env::var("LITECORD_VAAPI_CAPTURE_TEST_FAIL") else { return TestFail::None };
    let n = |s: &str| s.parse::<u64>().unwrap_or(0);
    match v.split_once(':') {
        Some(("after", s)) => TestFail::After(n(s)),
        Some(("stall", s)) => TestFail::Stall(n(s)),
        Some(("nosrc", s)) => TestFail::NoSource(n(s)),
        Some(("hang", s)) => TestFail::Hang(n(s)),
        Some(("mainlate", s)) => TestFail::MainLate(n(s)),
        Some(("prepstuck", s)) => TestFail::PrepStuck(n(s)),
        _ if v == "nobars" => TestFail::NoBars,
        _ if v == "floorrefuse" => TestFail::FloorRefuse,
        _ if v == "floormute" => TestFail::FloorMute,
        _ if v == "floorbreak" => TestFail::FloorBreak,
        _ if v == "nomain" => TestFail::NoMain,
        _ if v == "mainrefuse" => TestFail::MainRefuse,
        _ if v == "mainprofile" => TestFail::MainProfile,
        _ if v == "mainbreak" => TestFail::MainBreak,
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
    /// What the gate has settled on and the spacing of the pictures it passed lately
    /// (made once a second; `None` before two pictures have passed).
    pub cadence: Option<cadence::Report>,
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
    /// The encoder now in the pipeline produces H.264 Main; Main was found not to work on
    /// this stream (it stays on Constrained Baseline); times the encoder was replaced, and
    /// how long the last replacement kept the caller, in microseconds.
    pub main: bool,
    pub main_unavailable: bool,
    pub profile_switches: u64,
    pub last_switch_us: u64,
    /// The quality floor (min-qp) of the encoder now in the pipeline, and whether a change
    /// of floor still waits for its encoder's first keyframe.
    pub min_qp: u32,
    pub floor_in_flight: bool,
    /// Where the picture lies in the stream's frame (x, y, width, height); surfaces whose
    /// bars were painted, the microseconds that took in all, and surfaces that could not be
    /// painted.
    pub picture_rect: (u32, u32, u32, u32),
    pub bars_painted: u64,
    pub bars_us: u64,
    pub bars_failed: u64,
    /// Times vapostproc was told a new shape of the picture (a shared window resized), and
    /// the microseconds it took over that in all.
    pub shape_changes: u64,
    pub shape_us: u64,
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
    /// Test hooks: the streaming thread blocks at the encoder; the bars cannot be painted.
    hang: AtomicBool,
    /// The bars are painted by the CPU (vapostproc leaves them unwritten); gltransformation
    /// clears them itself.
    bars_by_cpu: bool,
    /// The postproc takes each buffer's crop region (vapostproc); without that (the GL chain)
    /// a buffer whose crop is a part of it is not shown and `crop_refused` is set.
    follows_crop: bool,
    crop_refused: AtomicBool,
    no_bars: bool,
    /// Milliseconds after `started` (never 0) at which the streaming thread began handing a
    /// picture to the encoder, while it has not come back from that; 0 otherwise. A thread
    /// that stays there is stuck (a driver that hangs), whatever the encoder still owes.
    at_encoder_ms: AtomicU64,
    /// Times the source took its buffers back before it had delivered any.
    early_renegotiations: AtomicU64,
    buffers: AtomicU64,
    pictures: AtomicU64,
    /// Pictures the send loop has not counted yet.
    uncounted_pictures: AtomicU64,
    encoded_new: AtomicU64,
    encoded_repeats: AtomicU64,
    skipped_pace: AtomicU64,
    cadence: Mutex<Option<cadence::Report>>,
    previews: AtomicU64,
    preview_us: [AtomicU64; 4],
    stamped_pictures: AtomicU64,
    source_delay_us: AtomicU64,
    source_delay_max_us: AtomicU64,
    /// The size of the pictures pipewiresrc delivers (width << 32 | height; 0 before the
    /// first format), and the stream's frame.
    source_size: AtomicU64,
    frame: (u32, u32),
    /// A size of the shape vapostproc was told for a shared window (`even_fit`), which is
    /// what its place in the frame follows from; 0 for pictures that fill their buffers.
    shape_size: AtomicU64,
    /// Counts the changes of the picture's rectangle in the frame (never 0): bars painted
    /// around one rectangle are of no use around the next.
    rect_generation: AtomicU64,
    /// vapostproc did not take the format that goes with a picture of a new shape.
    shape_refused: AtomicBool,
    shape_changes: AtomicU64,
    shape_us: AtomicU64,
    bars_painted: AtomicU64,
    bars_us: AtomicU64,
    bars_failed: AtomicU64,
    /// Set when the stream is being dropped (the preview thread ends).
    closing: AtomicBool,
    /// A new encoder is being linked in: the renegotiation request this sends upstream is
    /// stopped before it reaches pipewiresrc.
    drop_reconfigure: AtomicBool,
    /// The encoder is a Main one that has not put out its keyframe yet: pictures are handed
    /// to it in a way that keeps its errors from the capture (`add_encoder_probe`), and
    /// `main_broke` says it failed. Both hold that encoder's number (`Tx::switches` when it
    /// was put in; 0 for none): what a replaced encoder still reports counts for nothing.
    guard_main: AtomicU64,
    main_broke: AtomicU64,
    /// That encoder's elements, set with `guard_main` at the moment it is linked in: an error
    /// message of theirs is the encoder's own.
    guarded: Mutex<Vec<gst::Object>>,
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

    /// Where the picture lies in the stream's frame: all of it until the source's size is
    /// known, and whenever the two have the same shape.
    fn picture_rect(&self) -> (u32, u32, u32, u32) {
        let shape = self.shape_size.load(Ordering::Relaxed);
        let source = if shape != 0 { shape } else { self.source_size.load(Ordering::Relaxed) };
        picture_rect(((source >> 32) as u32, source as u32), self.frame)
    }
}

/// The send thread's side: bitrate, keyframes, output.
struct Tx {
    requested_bps: u32,
    asked_at: Instant,
    applied_bps: u32,
    /// The highest rate applied so far: what the level in the SPS has to allow.
    level_bps: u32,
    /// A keyframe was asked of the encoder and has not come out yet; and how many times it
    /// has been asked for.
    keyframe_asked_at: Option<Instant>,
    keyframe_tries: u32,
    /// Output is thrown away until a keyframe (a viewer just arrived).
    await_keyframe: bool,
    ready: VecDeque<gst::Sample>,
    delivered: u64,
    dropped_before_keyframe: u64,
    /// The encoder in the pipeline is a Main one (the same as `Stream::section`'s).
    main: bool,
    /// The log line naming this encoder (profile, rate, quality floor) has been written.
    encoder_said: bool,
    /// The encoder was replaced and its first keyframe has not been checked yet.
    first_keyframe_due: bool,
    /// The encoder was replaced and nothing has been taken from the new one yet: whatever
    /// it puts out first is an IDR with SPS and PPS, without being asked.
    fresh: bool,
    /// The rules of `VaapiEncoder` for Main: not available on this stream; a Main encoder
    /// of this stream has put out its keyframe; the one late first picture that is forgiven
    /// has happened.
    main_failed: bool,
    main_proven: bool,
    main_was_late: bool,
    /// Since when an unproven Main encoder has been asked for its first keyframe.
    main_wait: Option<Instant>,
    /// Test hook `mainlate`: first pictures of a Main encoder still to be withheld.
    mute_main: u64,
    switches: u64,
    last_switch_us: u64,
    /// The quality floor (min-qp) of the encoder in the pipeline; the one change of floor
    /// that may be on its way (`follow_floor`), and since when its encoder has been asked
    /// for its first keyframe.
    min_qp: u32,
    floor: EncoderFloor,
    floor_wait: Option<Instant>,
    /// Nobody watched when the encoder for the floor on its way was put in: no viewer went
    /// without pictures for it.
    floor_idle: bool,
}

/// Why the encoder could not be replaced.
enum SwitchError {
    /// The new encoder was not put in: the old one goes on as if nothing had been tried.
    Refused(String),
    /// The pipeline is of no use any more.
    Broken(String),
}

struct Stream {
    id: u64,
    /// `None` only while being dropped. Its `encoder` and `sink` are the ones the pipeline
    /// started with: the ones in use are `section`'s.
    chain: Option<EncodeChain>,
    /// The encoder now in the pipeline, with its appsink. Locked for a moment at a time
    /// (after `tx`, where both are held).
    section: Mutex<EncoderSection>,
    element: &'static str,
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
/// What `set_min_qp` asked for last; 0 until it is called (the encoder keeps its floor).
static WANTED_MIN_QP: AtomicU32 = AtomicU32::new(0);
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

/// Whether this share is captured here, with the reason logged when it is not. Shared
/// windows are: resized, maximised, made fullscreen or minimised, a window of Mutter's
/// keeps its stream and only the picture's place in the frame follows (see the module's
/// notes; run against real windows with `test_vaapi_capture --source mutter-window`).
/// `LITECORD_VAAPI_CAPTURE_WINDOWS=0` keeps windows off it, on the capture in system memory
/// (`linux_portal::WindowCapture`).
pub fn allowed(window: bool) -> bool {
    let reason = match enabled() {
        Err(why) => why,
        Ok(()) if window && std::env::var("LITECORD_VAAPI_CAPTURE_WINDOWS").as_deref() == Ok("0") => "janela compartilhada e LITECORD_VAAPI_CAPTURE_WINDOWS=0".into(),
        Ok(()) => return true,
    };
    info!("🎯 [VAAPI] Captura na GPU desativada ({}); usando a captura por gst-launch", reason);
    false
}

/// The caps pipewiresrc may negotiate: what vapostproc (glupload for NVENC) takes as DMABuf
/// (with its `drm-format` list) and/or in system memory (not for NVENC: see the module's notes).
fn source_caps(backend: vaapi::Backend, memory: Memory, max_fps: Option<u32>) -> Result<gst::Caps, String> {
    let (first, memory) = match backend {
        vaapi::Backend::Va => ("vapostproc", memory),
        vaapi::Backend::Nvenc if memory == Memory::System => return Err("NVENC: só DMABuf (glupload da memória do sistema dá quadros pretos)".into()),
        vaapi::Backend::Nvenc => ("glupload", Memory::DmaBuf),
    };
    let probe = gst::ElementFactory::make(first).build().map_err(|e| e.to_string())?;
    let accepted = probe.static_pad("sink").ok_or(format!("{first} sem pad de entrada"))?.query_caps(None);
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
        return Err(format!("{first} não aceita DMABuf nem memória do sistema"));
    }
    Ok(caps)
}

impl Stream {
    fn chain(&self) -> &EncodeChain {
        // Only `drop` takes it.
        self.chain.as_ref().expect("stream chain")
    }

    /// The appsink of the encoder now in the pipeline.
    fn sink(&self) -> gst_app::AppSink {
        lock(&self.section).sink.clone()
    }

    fn start(cfg: &Config, test_fail: TestFail, wanted: &dyn Fn() -> bool) -> Result<Self, String> {
        gst::init().map_err(|e| format!("GStreamer: {e}"))?;
        for element in ["pipewiresrc", "capssetter"] {
            if gst::ElementFactory::find(element).is_none() {
                return Err(format!("elemento GStreamer ausente: {element}"));
            }
        }
        let element = vaapi::usable_encoders(true)?[0];
        let backend = vaapi::Backend::of(element);
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
        let relabel = format!("video/x-raw,framerate=(fraction){fps}/1,colorimetry=(string)sRGB,pixel-aspect-ratio=(fraction)1/1");
        // glupload only knows the DMABuf modifiers it imports once it has its GL context, in
        // the running pipeline: capssetter would hide it from pipewiresrc's caps query (both
        // sides of it failed to negotiate). For NVENC the relabelling is done on the caps
        // event instead (`add_relabel_probe`), past an element that hides nothing.
        let set = match backend {
            vaapi::Backend::Va => format!("capssetter name=cap_set join=true replace=false caps=\"{relabel}\""),
            vaapi::Backend::Nvenc => "identity name=cap_set".to_string(),
        };
        // xdg-desktop-portal-hyprland hands out 4 DMABufs unless asked for more. The keepalive
        // holds one, the queue and the GL import one each: under load the compositor found
        // none free and skipped pictures (19-60 FPS from one run to the next, 60 in every run
        // with 8 asked for).
        let buffers = match backend {
            vaapi::Backend::Va => "",
            vaapi::Backend::Nvenc => " min-buffers=8",
        };
        // The queue keeps only the newest picture: when the encoder is behind, raw pictures
        // are skipped instead of latency building up.
        let description = format!(
            "pipewiresrc name=cap_src {fd_prop} path={node} do-timestamp=true keepalive-time={KEEPALIVE_MS} provide-clock=false{buffers} \
               on-disconnect=error stream-properties=\"props,node.dont-fallback=(string)true\" \
             ! capsfilter name=cap_filter \
             ! {set} \
             ! queue name=cap_queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {chain}",
            node = cfg.node,
            chain = EncodeChain::description_scaled(element, width, height, vaapi::INITIAL_BITRATE_BPS, cfg.min_qp, false),
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        // From here on dropping the chain stops the pipeline.
        let chain = EncodeChain::from_pipeline(pipeline)?;
        let by_name = |name: &str| chain.pipeline.by_name(name).ok_or(format!("elemento {name} ausente"));
        let filter = by_name("cap_filter")?;
        filter.set_property("caps", source_caps(backend, cfg.memory, cfg.max_fps)?);

        let shared = Arc::new(Shared {
            started: Instant::now(),
            watched: AtomicBool::new(false),
            // The first picture is encoded whoever watches: it proves the whole path.
            pass_next: AtomicBool::new(test_fail != TestFail::First),
            last_buffer_ms: AtomicU64::new(0),
            last_check_ms: AtomicU64::new(0),
            source_shut: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            bars_by_cpu: backend == vaapi::Backend::Va,
            follows_crop: backend == vaapi::Backend::Va,
            crop_refused: AtomicBool::new(false),
            no_bars: test_fail == TestFail::NoBars,
            at_encoder_ms: AtomicU64::new(0),
            early_renegotiations: AtomicU64::new(0),
            buffers: AtomicU64::new(0),
            pictures: AtomicU64::new(0),
            uncounted_pictures: AtomicU64::new(0),
            encoded_new: AtomicU64::new(0),
            encoded_repeats: AtomicU64::new(0),
            skipped_pace: AtomicU64::new(0),
            cadence: Mutex::new(None),
            previews: AtomicU64::new(0),
            preview_us: Default::default(),
            stamped_pictures: AtomicU64::new(0),
            source_delay_us: AtomicU64::new(0),
            source_delay_max_us: AtomicU64::new(0),
            source_size: AtomicU64::new(0),
            shape_size: AtomicU64::new(0),
            frame: (width, height),
            rect_generation: AtomicU64::new(1),
            shape_refused: AtomicBool::new(false),
            shape_changes: AtomicU64::new(0),
            shape_us: AtomicU64::new(0),
            bars_painted: AtomicU64::new(0),
            bars_us: AtomicU64::new(0),
            bars_failed: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            drop_reconfigure: AtomicBool::new(false),
            guard_main: AtomicU64::new(0),
            main_broke: AtomicU64::new(0),
            guarded: Mutex::new(Vec::new()),
            decisions: Mutex::new(VecDeque::new()),
            in_flight: Mutex::new(VecDeque::new()),
        });
        if backend == vaapi::Backend::Nvenc {
            add_relabel_probe(&by_name("cap_set")?, &relabel)?;
        }
        add_arrival_probe(&by_name("cap_set")?, &shared)?;
        add_gate(&by_name("cap_queue")?, &shared, fps, by_name(vaapi::POSTPROC)?)?;
        let (preview_tx, preview_rx) = sync_channel(1);
        add_encoder_probe(&by_name(vaapi::ENCODER_INPUT)?, &shared, preview_tx)?;
        let section = Mutex::new(EncoderSection::of_chain(&chain, false));
        let preview_shared = shared.clone();
        std::thread::Builder::new().name("vaapi-capture-preview".into()).spawn(move || preview_thread(preview_rx, preview_shared)).map_err(|e| e.to_string())?;

        LIVE_PIPELINES.fetch_add(1, Ordering::SeqCst);
        let stream = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            chain: Some(chain),
            section,
            element,
            fd,
            shared,
            tx: Mutex::new(Tx {
                requested_bps: vaapi::INITIAL_BITRATE_BPS,
                asked_at: Instant::now(),
                applied_bps: vaapi::INITIAL_BITRATE_BPS,
                level_bps: 0,
                keyframe_asked_at: None,
                keyframe_tries: 0,
                await_keyframe: false,
                ready: VecDeque::new(),
                delivered: 0,
                dropped_before_keyframe: 0,
                main: false,
                encoder_said: false,
                first_keyframe_due: false,
                fresh: false,
                main_failed: false,
                main_proven: false,
                main_was_late: false,
                main_wait: None,
                mute_main: if let TestFail::MainLate(n) = test_fail { n } else { 0 },
                switches: 0,
                last_switch_us: 0,
                min_qp: cfg.min_qp,
                floor: EncoderFloor::new(cfg.min_qp),
                floor_wait: None,
                floor_idle: false,
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
        let sink = self.sink();
        let sample = loop {
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(20)) {
                break sample;
            }
            if let Some(e) = self.bus_error() {
                return Err(e);
            }
            if self.shared.bars_failed.load(Ordering::Relaxed) > 0 {
                // (Such a picture is not let into the encoder: none would ever come.)
                return Err("as barras em volta da imagem não puderam ser pintadas".into());
            }
            if self.shared.crop_refused.load(Ordering::Relaxed) {
                return Err(CROP_REFUSED.into());
            }
            if !is_playing {
                match playing.try_recv() {
                    Ok(true) => is_playing = true,
                    Ok(false) | Err(TryRecvError::Disconnected) => return Err("o pipeline não chegou a PLAYING".into()),
                    Err(TryRecvError::Empty) => {}
                }
            }
            // (A sink that has not started yet says it is at the end too.)
            if is_playing && sink.is_eos() {
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
        vaapi::check_keyframe(map.as_slice(), false)?;
        lock(&self.shared.in_flight).clear();

        let caps = filter.static_pad("src").and_then(|p| p.current_caps()).ok_or("sem caps negociadas")?;
        self.dmabuf = caps.features(0).is_some_and(|f| f.contains("memory:DMABuf"));
        // (The pictures' own size: a shared window's, not that of the frame it comes in.)
        let picture = self.shared.source_size.load(Ordering::Relaxed);
        self.source_size = ((picture >> 32) as u32, picture as u32);
        self.source_caps = caps.to_string();
        let relabelled = self.chain().pipeline.by_name("cap_set").and_then(|e| e.static_pad("src")).and_then(|p| p.current_caps());
        self.converter_caps = relabelled.map(|c| c.to_string()).unwrap_or_default();
        self.start_over = true;
        Ok(self)
    }

    /// The next access unit of the encoder now in the pipeline, waiting up to `wait` for it.
    fn pull(tx: &mut Tx, sink: &gst_app::AppSink, wait: Duration) -> Option<gst::Sample> {
        let sample = sink.try_pull_sample(gst::ClockTime::from_nseconds(wait.as_nanos() as u64))?;
        tx.fresh = false;
        Some(sample)
    }

    /// Opens or closes the way into the encoder. Opening it asks for a keyframe and throws
    /// away whatever the encoder still had from before.
    fn watch(&self, tx: &mut Tx, watched: bool) {
        if watched == self.shared.watched.load(Ordering::Relaxed) {
            return;
        }
        if watched {
            let sink = self.sink();
            while Self::pull(tx, &sink, Duration::ZERO).is_some() {}
            tx.ready.clear();
            lock(&self.shared.in_flight).clear();
            tx.await_keyframe = true;
            // A Main encoder still to prove itself is given its time from now.
            tx.main_wait = tx.main_wait.map(|_| Instant::now());
            tx.floor_wait = tx.floor_wait.map(|_| Instant::now());
            // (Its viewers left before its keyframe: nobody went without pictures for it.)
            tx.floor_idle |= tx.floor.in_flight().is_some();
            tx.keyframe_tries = 0;
            self.want_keyframe(tx, false);
        } else {
            self.shared.pass_next.store(false, Ordering::Relaxed);
        }
        self.shared.watched.store(watched, Ordering::Relaxed);
    }

    /// The next picture into the encoder becomes an IDR with SPS and PPS, and goes in at
    /// once (on a still screen: the next keepalive buffer, within 25 ms).
    fn ask_keyframe(&self, tx: &mut Tx) {
        if vaapi::request_next_keyframe(&self.sink()) {
            tx.keyframe_asked_at = Some(Instant::now());
        }
        self.shared.pass_next.store(true, Ordering::Relaxed);
    }

    /// `ask_keyframe`, except of an encoder that has put out nothing yet: its first picture is
    /// an IDR with SPS and PPS as it is, and asked for one after that picture went in, it
    /// would make the one behind it an IDR too (seen once in four changes to Main: 40-60 KB
    /// sent twice). `again`: the keyframe is overdue, and asked for whatever the encoder is.
    fn want_keyframe(&self, tx: &mut Tx, again: bool) {
        if tx.fresh && !again {
            tx.keyframe_asked_at.get_or_insert_with(Instant::now);
            self.shared.pass_next.store(true, Ordering::Relaxed);
        } else {
            self.ask_keyframe(tx);
        }
    }

    /// The bitrate and keyframe wishes for the pictures from now on.
    fn control(&self, tx: &mut Tx, bitrate_bps: u32, want_idr: bool) -> Result<(), String> {
        if bitrate_bps != tx.requested_bps {
            tx.requested_bps = bitrate_bps;
            tx.asked_at = Instant::now();
        }
        let retry = tx.keyframe_asked_at.is_some_and(|t| t.elapsed() >= KEYFRAME_RETRY);
        if retry {
            tx.keyframe_tries += 1;
            if tx.keyframe_tries >= KEYFRAME_TRIES {
                return Err(format!("keyframe pedido {} vezes e nenhum saiu em {} ms", KEYFRAME_TRIES, KEYFRAME_TRIES * KEYFRAME_RETRY.as_millis() as u32));
            }
            warn!("⚠️ [VAAPI] Keyframe pedido à captura na GPU não saiu em {} ms; pedindo de novo", KEYFRAME_RETRY.as_millis());
        }
        let want_idr = want_idr || retry;
        // vah264enc starts a new sequence whenever its bitrate changes: the new rate rides
        // on a keyframe that was going to be made anyway (same rule as VaapiEncoder).
        if tx.applied_bps != tx.requested_bps && (want_idr || tx.asked_at.elapsed() >= vaapi::BITRATE_MAX_WAIT) {
            vaapi::set_encoder_bitrate(&lock(&self.section).encoder, tx.requested_bps);
            tx.applied_bps = tx.requested_bps;
            tx.keyframe_asked_at = Some(Instant::now());
            self.shared.pass_next.store(true, Ordering::Relaxed);
        } else if want_idr {
            self.want_keyframe(tx, retry);
        }
        Ok(())
    }

    /// Main turned out not to work on this stream: it stays on Constrained Baseline.
    fn main_unavailable(&self, tx: &mut Tx, why: &str) {
        if !std::mem::replace(&mut tx.main_failed, true) {
            warn!("⚠️ [VAAPI] Perfil Main indisponível na captura na GPU ({}); a transmissão segue em Constrained Baseline", why);
        }
    }

    /// Makes the encoder's profile the one this turn's viewers decode (`main`), by the rules
    /// of `VaapiEncoder`: to Constrained Baseline at once, since a viewer that cannot decode
    /// Main is watching (the Main pictures still on their way are thrown away with their
    /// encoder); to Main only with a keyframe that was asked for anyway. An error means the
    /// stream is to be given up.
    fn follow_profile(&self, tx: &mut Tx, main: bool, want_idr: bool) -> Result<(), String> {
        let opening = !self.shared.watched.load(Ordering::Relaxed);
        if tx.main && !tx.main_proven && !opening {
            // A Main encoder that has not shown its keyframe yet.
            if self.shared.main_broke.load(Ordering::SeqCst) == tx.switches {
                self.main_unavailable(tx, "o codificador falhou no primeiro quadro");
            } else if tx.main_wait.is_some_and(|since| since.elapsed() >= MAIN_FIRST_PICTURE) && self.shared.at_encoder_ms.load(Ordering::Relaxed) == 0 {
                // (While the streaming thread has not come back from the encoder there is
                // nobody to try again with: `health` gives the stream up when that lasts.)
                if std::mem::replace(&mut tx.main_was_late, true) {
                    self.main_unavailable(tx, &format!("sem primeiro quadro em {} ms, pela segunda vez", MAIN_FIRST_PICTURE.as_millis()));
                } else {
                    // Forgiven once: a busy GPU does that to any new encoder.
                    warn!("⚠️ [VAAPI] Primeiro quadro Main da captura na GPU não saiu em {} ms; mais uma tentativa", MAIN_FIRST_PICTURE.as_millis());
                    tx.mute_main = tx.mute_main.saturating_sub(1);
                    tx.main_wait = Some(Instant::now());
                    tx.keyframe_tries = 0;
                    lock(&self.shared.in_flight).clear();
                    self.ask_keyframe(tx);
                }
            }
        }
        let want_main = main && !tx.main_failed;
        if want_main == tx.main || (want_main && !want_idr && !opening) {
            return Ok(());
        }
        match self.replace_encoder(tx, want_main, tx.min_qp) {
            Ok(()) => Ok(()),
            Err(SwitchError::Refused(why)) if want_main => {
                self.main_unavailable(tx, &why);
                Ok(())
            }
            Err(SwitchError::Refused(why)) | Err(SwitchError::Broken(why)) => Err(format!("a troca do codificador para {} falhou: {why}", vaapi::profile_name(want_main))),
        }
    }

    /// Makes the encoder's quality floor the one the stream controls ask for (`set_min_qp`).
    /// vah264enc does not take `min-qp` while it runs, so the encoder is replaced as for a
    /// change of profile, with the same care: one replacement at a time (none while a new
    /// encoder of any kind still owes its first keyframe), the old encoder goes on when the
    /// new one is refused, and one that shows no keyframe within `MAIN_FIRST_PICTURE` is
    /// replaced by one with the floor before it. An error means the stream is to be given up
    /// (the gst-launch capture's encoder then starts with the last floor that worked).
    fn follow_floor(&self, tx: &mut Tx) -> Result<(), String> {
        use super::live_controls::{floor_refused, Path};
        if tx.floor.in_flight().is_some() {
            // The encoder that carries the new floor failed (an error of its own, or a first
            // keyframe that is not what the viewers were promised: `guard_main`), or it has
            // shown nothing in its time.
            let broke = tx.first_keyframe_due && self.shared.main_broke.load(Ordering::SeqCst) == tx.switches;
            let late = tx.floor_wait.is_some_and(|since| since.elapsed() >= MAIN_FIRST_PICTURE) && self.shared.at_encoder_ms.load(Ordering::Relaxed) == 0;
            if !(broke || (late && tx.first_keyframe_due && self.shared.watched.load(Ordering::Relaxed))) {
                return Ok(());
            }
            let Some((refused, back)) = tx.floor.failed() else { return Ok(()) };
            tx.floor_wait = None;
            let why = if broke { "the new encoder failed before its first keyframe".to_string() } else { format!("no keyframe from the new encoder in {} ms", MAIN_FIRST_PICTURE.as_millis()) };
            floor_refused(Path::GpuCapture, refused, back, &why);
            lock(&self.shared.in_flight).clear();
            return match self.replace_encoder(tx, tx.main, back) {
                Ok(()) => Ok(()),
                Err(SwitchError::Refused(why)) | Err(SwitchError::Broken(why)) => Err(format!("a volta do codificador para min-qp {back} falhou: {why}")),
            };
        }
        let wanted = WANTED_MIN_QP.load(Ordering::Relaxed);
        if wanted == 0 || tx.first_keyframe_due {
            return Ok(());
        }
        let Some(min_qp) = tx.floor.next(wanted, Instant::now()) else { return Ok(()) };
        tx.floor_idle = !self.shared.watched.load(Ordering::Relaxed);
        let failure = match self.replace_encoder(tx, tx.main, min_qp) {
            Ok(()) => {
                tx.floor_wait = Some(Instant::now());
                return Ok(());
            }
            Err(failure) => failure,
        };
        let back = tx.floor.failed().map_or(tx.min_qp, |(_, back)| back);
        match failure {
            // The encoder with the floor before it was never taken out.
            SwitchError::Refused(why) => {
                floor_refused(Path::GpuCapture, min_qp, back, &why);
                Ok(())
            }
            SwitchError::Broken(why) => {
                floor_refused(Path::GpuCapture, min_qp, back, &why);
                Err(format!("a troca do codificador para min-qp {min_qp} falhou: {why}"))
            }
        }
    }

    /// Replaces the encoder (with its profile filter and appsink) by one for profile `main`
    /// and quality floor `min_qp` (a change of either: neither is something vah264enc takes
    /// while it runs), while everything in front of it keeps running. Its first picture is
    /// an IDR with SPS and PPS; whatever the old encoder still held is gone with it.
    fn replace_encoder(&self, tx: &mut Tx, main: bool, min_qp: u32) -> Result<(), SwitchError> {
        use SwitchError::{Broken, Refused};
        let started = Instant::now();
        let pipeline = &self.chain().pipeline;
        let input = pipeline.by_name(vaapi::ENCODER_INPUT).and_then(|e| e.static_pad("src")).ok_or_else(|| Refused("sem a entrada do codificador".into()))?;
        let mut description = EncodeChain::encoder_description(self.element, tx.requested_bps, min_qp, main && self.test_fail != TestFail::MainProfile);
        if self.test_fail == TestFail::FloorRefuse && min_qp != tx.floor.applied() {
            description = format!("capsfilter caps=\"video/x-raw(memory:VAMemory),width=640,height=368\" ! {description}");
        } else if self.test_fail == TestFail::FloorMute && min_qp != tx.floor.applied() {
            description = description.replace("appsink name=sink", "identity drop-probability=1.0 ! appsink name=sink");
        } else if self.test_fail == TestFail::FloorBreak && min_qp != tx.floor.applied() {
            description = format!("identity error-after=1 ! {description}");
        }
        if main && self.test_fail == TestFail::NoMain {
            description = description.replace("profile=main", "profile=high-4:4:4");
        } else if main && self.test_fail == TestFail::MainRefuse {
            description = format!("capsfilter caps=\"video/x-raw(memory:VAMemory),width=640,height=368\" ! {description}");
        } else if main && self.test_fail == TestFail::MainBreak {
            description = format!("identity error-after=1 ! {description}");
        }
        // Told the format, the encoder picks its profile and opens its VA context: one that
        // cannot do it says so here, while the old one still encodes. (The same events come
        // again once it is linked, and change nothing then.)
        let start = input.sticky_event::<gst::event::StreamStart>(0).map(gst::Event::from);
        let Some(caps) = input.sticky_event::<gst::event::Caps>(0).map(gst::Event::from) else { return Err(Refused("sem formato na entrada do codificador".into())) };
        let stuck = matches!(self.test_fail, TestFail::PrepStuck(n) if tx.switches >= n);
        let new = prepare_section(pipeline, description, main, start.into_iter().chain([caps]).collect(), stuck).map_err(Refused)?;
        let Some(new_input) = new.input() else {
            new.remove(pipeline, vaapi::TEARDOWN_WAIT);
            return Err(Refused("codificador sem entrada".into()));
        };

        // The pad is idle between two pictures: the callback runs at once on this thread, or
        // on the streaming thread when the picture now going through is done.
        let (old_input, old_sink) = {
            let old = lock(&self.section);
            (old.input(), old.sink.clone())
        };
        // This encoder's number, when it is a Main one still to prove itself.
        // (Or one that carries a quality floor still to be proven: an error of its own puts
        // the floor before it back, `follow_floor`, and is not one of the capture either.)
        let unproven_main = main && !tx.main_proven;
        let guard = if unproven_main || tx.floor.in_flight().is_some_and(|(qp, _)| qp == min_qp) { tx.switches + 1 } else { 0 };
        let guarded = if guard != 0 { new.objects() } else { Vec::new() };
        let abandoned = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = sync_channel::<Result<(), String>>(1);
        let (shared, gave_up) = (self.shared.clone(), abandoned.clone());
        input.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
            if gave_up.load(Ordering::SeqCst) {
                return gst::PadProbeReturn::Remove;
            }
            if let Some(old) = &old_input {
                let _ = pad.unlink(old);
            }
            // Linking asks everything upstream to negotiate again.
            shared.drop_reconfigure.store(true, Ordering::SeqCst);
            let result = match pad.link(&new_input) {
                Ok(_) => {
                    // Before the first picture can go in (pictures wait for this callback).
                    *lock(&shared.guarded) = guarded.clone();
                    shared.guard_main.store(guard, Ordering::SeqCst);
                    Ok(())
                }
                Err(e) => match old_input.as_ref().map(|old| pad.link(old)) {
                    // The old encoder is back in its place.
                    Some(Ok(_)) => Err(format!("{e:?}")),
                    _ => Err(String::new()),
                },
            };
            shared.drop_reconfigure.store(false, Ordering::SeqCst);
            let _ = done_tx.send(result);
            gst::PadProbeReturn::Remove
        });
        let deadline = Instant::now() + SWITCH_IDLE_TIMEOUT;
        let linked = loop {
            match done_rx.recv_timeout(Duration::from_millis(2)) {
                Ok(result) => break result,
                Err(_) if Instant::now() >= deadline => {
                    // The whole pipeline is given up by the caller, the new encoder with it.
                    abandoned.store(true, Ordering::SeqCst);
                    return Err(Broken(format!("o codificador não liberou a entrada em {} ms", SWITCH_IDLE_TIMEOUT.as_millis())));
                }
                // The streaming thread may be waiting for room in the old appsink.
                Err(_) => while old_sink.try_pull_sample(gst::ClockTime::ZERO).is_some() {},
            }
        };
        match linked {
            Ok(()) => {}
            Err(why) if why.is_empty() => return Err(Broken("o codificador novo não pôde ser ligado, nem o anterior de volta".into())),
            Err(why) => {
                new.remove(pipeline, vaapi::TEARDOWN_WAIT);
                return Err(Refused(format!("o codificador novo não pôde ser ligado: {why}")));
            }
        }
        let old = std::mem::replace(&mut *lock(&self.section), new);
        old.remove(pipeline, vaapi::TEARDOWN_WAIT);

        // The pictures that went into the old encoder stay on the list of those awaited:
        // the new encoder's first one clears them, and one that the streaming thread is
        // stuck with still tells the health check so. (Not those of a Main encoder given
        // up before its first keyframe: it had its own deadline.)
        let unproven = tx.main && !tx.main_proven;
        if unproven {
            lock(&self.shared.in_flight).clear();
        }
        let profile_changed = std::mem::replace(&mut tx.main, main) != main;
        let old_min_qp = std::mem::replace(&mut tx.min_qp, min_qp);
        tx.ready.clear();
        tx.applied_bps = tx.requested_bps;
        tx.encoder_said = false;
        tx.first_keyframe_due = true;
        tx.fresh = true;
        tx.await_keyframe = true;
        tx.keyframe_asked_at = Some(Instant::now());
        // The viewers of a Main encoder given up for being late have been without pictures
        // for twice `MAIN_FIRST_PICTURE` already: the encoder that follows it gets one
        // `KEYFRAME_RETRY` for its keyframe, not all of them, before gst-launch takes over.
        tx.keyframe_tries = if unproven && tx.main_failed && tx.main_was_late { KEYFRAME_TRIES - 1 } else { 0 };
        tx.main_wait = unproven_main.then(Instant::now);
        // A floor in flight is judged on the encoder that carries it now.
        tx.floor_wait = tx.floor_wait.map(|_| Instant::now());
        tx.switches += 1;
        tx.last_switch_us = started.elapsed().as_micros() as u64;
        // The new encoder's first picture, at once.
        self.shared.pass_next.store(true, Ordering::Relaxed);
        if profile_changed || old_min_qp == min_qp {
            info!("🔀 [VAAPI] Perfil H.264 da captura na GPU muda para {} (codificador trocado em {:.1} ms)", vaapi::profile_name(main), started.elapsed().as_secs_f64() * 1000.0);
        }
        if old_min_qp != min_qp {
            info!("🔀 [VAAPI] GPU capture encoder replaced for min-qp {} -> {} in {:.1} ms; its first keyframe is awaited", old_min_qp, min_qp, started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(())
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
        if tx.first_keyframe_due {
            // The first keyframe of a new encoder: what the viewers were promised?
            if let Err(e) = vaapi::check_keyframe(&au, tx.main) {
                if tx.main && !tx.main_proven {
                    // Thrown away, and so is whatever follows it: the next turn replaces
                    // this encoder.
                    self.main_unavailable(tx, &e);
                    return Ok(None);
                }
                if tx.floor.in_flight().is_some() {
                    // Thrown away too: the next turn puts the floor before it back.
                    warn!("⚠️ [VAAPI] First keyframe of the encoder made for the new quality floor is not usable: {}", e);
                    self.shared.main_broke.store(tx.switches, Ordering::SeqCst);
                    return Ok(None);
                }
                return Err(e);
            }
            tx.first_keyframe_due = false;
            if let Some((old, new, since)) = tx.floor.proven() {
                tx.floor_wait = None;
                super::live_controls::floor_applied(super::live_controls::Path::GpuCapture, old, new, Some(since.elapsed()).filter(|_| !tx.floor_idle));
            }
            if tx.main && !tx.main_proven {
                tx.main_proven = true;
            }
            self.shared.guard_main.store(0, Ordering::SeqCst);
            tx.main_wait = None;
        }
        if !std::mem::replace(&mut tx.encoder_said, true) {
            // One line per encoder (the first of a share, and each one a change of profile
            // puts in its place): what the viewers are being sent, read from its first SPS.
            let profile = nal_ranges(&au).iter().map(|r| &au[r.clone()]).find(|n| n[0] & 0x1f == 7 && n.len() >= 3).map(|sps| match (sps[1], sps[2] & 0x40 != 0) {
                (66, true) => "Constrained Baseline".to_string(),
                (77, _) => "Main".to_string(),
                (idc, _) => format!("profile_idc {}", idc),
            });
            info!(
                "🎬 [VAAPI] Codificador da captura na GPU: {} H.264 {}, {:.2} Mbps pedidos, QP mínimo {}, primeiro keyframe com {} bytes",
                self.element,
                profile.as_deref().unwrap_or("sem SPS"),
                tx.applied_bps as f64 / 1e6,
                tx.min_qp,
                au.len()
            );
            super::live_controls::encoder_in_effect(super::live_controls::Path::GpuCapture, tx.main, tx.min_qp);
        }
        tx.await_keyframe = false;
        tx.keyframe_asked_at = None;
        tx.keyframe_tries = 0;
        // vah264enc takes the level from size and frame rate alone (same rule as VaapiEncoder).
        tx.level_bps = tx.level_bps.max(tx.applied_bps);
        let mut au = crate::encoder::h264_vui::declare_no_reordering(&crate::encoder::h264_vui::declare_colour(&au, 1, 1, 6));
        vaapi::raise_level(&mut au, tx.level_bps);
        Ok(Some(Picture { data: au, idr, repeat, latency }))
    }

    /// The first error on the pipeline's bus. Errors of pipewiresrc about its PipeWire
    /// stream are remembered: they tell a source that is gone from a GPU path that broke.
    fn bus_error(&self) -> Option<String> {
        let mut first = None;
        while let Some(msg) = self.chain().bus.pop_filtered(&[gst::MessageType::Error]) {
            let gst::MessageView::Error(e) = msg.view() else { continue };
            if let Some(src) = msg.src() {
                // An encoder that was replaced since has nothing to say about the stream.
                let pipeline = self.chain().pipeline.upcast_ref::<gst::Object>();
                if src != pipeline && !src.has_as_ancestor(pipeline) {
                    continue;
                }
                // A Main encoder that fails before its first keyframe is replaced by a
                // Constrained Baseline one (`follow_profile`).
                let guarded = self.shared.guard_main.load(Ordering::SeqCst);
                if guarded != 0 && lock(&self.shared.guarded).iter().any(|part| src == part || src.has_as_ancestor(part)) {
                    warn!("⚠️ [VAAPI] Erro do codificador novo (Main, ou de outro QP mínimo) antes do primeiro keyframe: {}", e.error());
                    self.shared.main_broke.store(guarded, Ordering::SeqCst);
                    continue;
                }
            }
            if msg.src().is_some_and(|s| s.name() == "cap_src") {
                if e.error().matches(gst::ResourceError::Failed) {
                    // "stream error: ...": PipeWire failed the stream. Its node is gone only when
                    // it says so; xdg-desktop-portal-hyprland renegotiates a window's stream at
                    // each resize, and a renegotiation that fails ("no more input formats")
                    // leaves the node there for the capture that takes over.
                    warn!("⚠️ [VAAPI] Fluxo do PipeWire falhou: {}", e.error());
                    if e.error().message().contains(SOURCE_GONE_ERROR) {
                        self.gone.store(true, Ordering::SeqCst);
                    }
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
    /// capture that stopped delivering and, for the send loop (`sending`: it has just found
    /// nothing to take, so the encoder is not waiting for it), a streaming thread that does
    /// not come back from the encoder and, with `encoder_too`, an encoder that stopped
    /// answering.
    fn health(&self, sending: bool, encoder_too: bool) -> Result<(), String> {
        if let Some(e) = self.bus_error() {
            return Err(e);
        }
        if self.sink().is_eos() {
            return Err("fim do fluxo".into());
        }
        if self.shared.bars_failed.load(Ordering::Relaxed) > 0 {
            // Pictures with whatever the surfaces held around them: not to be shown.
            return Err("as barras em volta da imagem não puderam ser pintadas".into());
        }
        if self.shared.crop_refused.load(Ordering::Relaxed) {
            return Err(CROP_REFUSED.into());
        }
        if self.shared.shape_refused.load(Ordering::Relaxed) {
            // Pictures that would be stretched over the frame: not shown either.
            return Err("o vapostproc recusou o formato da imagem depois de uma mudança de tamanho".into());
        }
        let now = self.shared.now_ms();
        if now.saturating_sub(self.shared.last_check_ms.swap(now, Ordering::Relaxed)) > CHECK_GAP.as_millis() as u64 {
            // Nobody looked for a while: this process did not run, and neither did the
            // pipeline's threads. They get the full time to deliver again.
            self.shared.last_buffer_ms.fetch_max(now, Ordering::Relaxed);
            lock(&self.shared.in_flight).iter_mut().for_each(|picture| picture.2 = Instant::now());
            let entered = self.shared.at_encoder_ms.load(Ordering::Relaxed);
            if entered != 0 {
                // (Not if the thread came back in the meantime.)
                let _ = self.shared.at_encoder_ms.compare_exchange(entered, now.max(1), Ordering::Relaxed, Ordering::Relaxed);
            }
            return Ok(());
        }
        // The streaming thread went to hand a picture to the encoder and has not come back.
        let entered = self.shared.at_encoder_ms.load(Ordering::Relaxed);
        if sending && entered != 0 && now.saturating_sub(entered) >= ENCODER_SILENCE.as_millis() as u64 {
            return Err(format!("a captura está parada no codificador há {} ms", now.saturating_sub(entered)));
        }
        if sending && encoder_too {
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
            source_size: {
                let picture = load(&self.shared.source_size);
                ((picture >> 32) as u32, picture as u32)
            },
            early_renegotiations: load(&self.shared.early_renegotiations),
            buffers: load(&self.shared.buffers),
            pictures: load(&self.shared.pictures),
            encoded_new: load(&self.shared.encoded_new),
            encoded_repeats: load(&self.shared.encoded_repeats),
            skipped_pace: load(&self.shared.skipped_pace),
            cadence: lock(&self.shared.cadence).clone(),
            previews: load(&self.shared.previews),
            preview_us: [0, 1, 2, 3].map(|i| load(&self.shared.preview_us[i])),
            stamped_pictures: load(&self.shared.stamped_pictures),
            source_delay_us: load(&self.shared.source_delay_us),
            source_delay_max_us: load(&self.shared.source_delay_max_us),
            delivered: tx.delivered,
            dropped_before_keyframe: tx.dropped_before_keyframe,
            main: tx.main,
            main_unavailable: tx.main_failed,
            profile_switches: tx.switches,
            last_switch_us: tx.last_switch_us,
            min_qp: tx.min_qp,
            floor_in_flight: tx.floor.in_flight().is_some(),
            picture_rect: self.shared.picture_rect(),
            bars_painted: load(&self.shared.bars_painted),
            bars_us: load(&self.shared.bars_us),
            bars_failed: load(&self.shared.bars_failed),
            shape_changes: load(&self.shared.shape_changes),
            shape_us: load(&self.shared.shape_us),
        }
    }
}

/// A new encoder for `pipeline`, running in it, linked to nothing and told its format
/// (`events`: the stream's start and its caps, with which it opens its VA context). All of
/// that is the driver's work, so it is done on a thread of its own and waited for no longer
/// than `SWITCH_PREPARE_TIMEOUT`: a driver that hangs there must not hold the send loop,
/// which is also the one that sends the gst-launch capture's pictures. An encoder that
/// comes too late takes itself out of the pipeline again.
fn prepare_section(pipeline: &gst::Pipeline, description: String, main: bool, events: Vec<gst::Event>, test_stuck: bool) -> Result<EncoderSection, String> {
    let given_up = Arc::new(Mutex::new(false));
    let (done_tx, done_rx) = sync_channel::<Result<EncoderSection, String>>(1);
    let (target, too_late) = (pipeline.clone(), given_up.clone());
    let make = move || {
        if test_stuck {
            std::thread::sleep(Duration::from_secs(5));
        }
        let made = EncoderSection::build(&description, main).and_then(|new| {
            let refused = match new.input() {
                None => Some("codificador sem entrada".to_string()),
                Some(input) => match new.add_to(&target) {
                    Err(e) => Some(e),
                    Ok(()) if events.into_iter().all(|event| input.send_event(event)) => None,
                    Ok(()) => Some("o codificador recusou o formato".to_string()),
                },
            };
            match refused {
                Some(why) => {
                    new.remove(&target, vaapi::TEARDOWN_WAIT);
                    Err(why)
                }
                None => Ok(new),
            }
        });
        // (Held while the result is handed over: the caller gives up under the same lock.)
        let too_late = lock(&too_late);
        match made {
            Ok(new) if *too_late => new.remove(&target, vaapi::TEARDOWN_WAIT),
            made => {
                let _ = done_tx.send(made);
            }
        }
    };
    std::thread::Builder::new().name("vaapi-capture-switch".into()).spawn(make).map_err(|e| e.to_string())?;
    if let Ok(made) = done_rx.recv_timeout(SWITCH_PREPARE_TIMEOUT) {
        return made;
    }
    let mut given_up = lock(&given_up);
    done_rx.try_recv().unwrap_or_else(|_| {
        *given_up = true;
        Err(format!("o codificador novo não ficou pronto em {} ms", SWITCH_PREPARE_TIMEOUT.as_millis()))
    })
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Stopping waits for the streaming threads (and for PipeWire): never on the thread
        // that happens to drop the stream. The fd outlives the pipeline's connection.
        self.shared.closing.store(true, Ordering::SeqCst);
        let (chain, fd, start_over) = (self.chain.take(), self.fd.take(), self.start_over);
        let stop = move || {
            // Mesa's VA driver deadlocks when the process exits while a display closes.
            let Some(_stopping) = vaapi::begin_teardown() else {
                std::mem::forget(chain);
                LIVE_PIPELINES.fetch_sub(1, Ordering::SeqCst);
                return;
            };
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

/// The pixel shape that makes a frame of `frame` size as wide, for its height, as a picture of
/// `picture` size with square pixels is: what vapostproc is told so that a picture which is
/// only a part of its buffer (a shared window) keeps its shape. `None` for sizes that make
/// no fraction.
fn pixel_shape(picture: (u32, u32), frame: (u32, u32)) -> Option<(i32, i32)> {
    if picture == frame {
        return Some((1, 1));
    }
    let (mut n, mut d) = (picture.0 as u64 * frame.1 as u64, picture.1 as u64 * frame.0 as u64);
    if n == 0 || d == 0 {
        return None;
    }
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    n /= a;
    d /= a;
    Some((i32::try_from(n).ok()?, i32::try_from(d).ok()?))
}

/// A size of the shape a shared window is shown in: its own, but for a bar on each side that
/// is an even number of pixels wide. In NV12 two rows and two columns share their colour:
/// at an odd edge the bar's first line takes the picture's colour and the picture's own
/// first line loses it, a coloured line for as long as the window keeps that size (and a
/// window has any size). The picture is at most two pixels wider or narrower for it.
fn even_fit(picture: (u32, u32), frame: (u32, u32)) -> (u32, u32) {
    let (_, _, w, h) = picture_rect(picture, frame);
    let even = |fit: u32, full: u32| {
        let gap = full - fit;
        let gap = match gap % 4 {
            0 => gap,
            1 => gap - 1,
            3 => gap + 1,
            _ if fit > 2 => gap + 2,
            _ => gap - 2,
        };
        full - gap
    };
    (even(w, frame.0), even(h, frame.1))
}

/// capssetter's relabelling (`join=true replace=false`) done on the caps event leaving
/// `element`: every field of `relabel` is set in what the source negotiated.
fn add_relabel_probe(element: &gst::Element, relabel: &str) -> Result<(), String> {
    let relabel = relabel.parse::<gst::Caps>().map_err(|e| e.to_string())?;
    let fields = relabel.structure(0).ok_or("caps vazias")?.to_owned();
    let pad = element.static_pad("src").ok_or("pad src ausente")?;
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &mut info.data {
            if let gst::EventView::Caps(caps) = event.view() {
                let mut caps = caps.caps_owned();
                for s in caps.make_mut().iter_mut() {
                    for (name, value) in fields.iter() {
                        s.set_value(name, value.clone());
                    }
                }
                *event = gst::event::Caps::new(&caps);
            }
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    Ok(())
}

/// What goes into vapostproc, and what for. To the encoder: nothing while nobody watches;
/// new pictures evenly spaced and about `fps` times a second at most (the monitor may
/// refresh faster: `Cadence` picks them, a whole number of refreshes apart, so a 144 Hz
/// monitor gives a stream of 60 its 48 pictures a second; a source that wanders a little
/// above `fps` passes whole, see there); the last picture again every 100 ms
/// while the screen is still, and at once when a keyframe waits for a picture to ride on.
/// To the preview: one picture every 150 ms, and only one a second while nothing changes.
fn add_gate(queue: &gst::Element, shared: &Arc<Shared>, fps: u32, postproc: gst::Element) -> Result<(), String> {
    struct Gate {
        pace: Cadence,
        preview_picture: u64,
        last_preview: Instant,
        /// The size of the part of the last buffer that was its picture, and whether that
        /// buffer was left out for it.
        crop: Option<(u32, u32)>,
        withheld: bool,
    }
    let shared = shared.clone();
    let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
    let start = Instant::now();
    // (LITECORD_VAAPI_CAPTURE_PACE=average: the schedule before, 60 on average with late
    // pictures made up for, to compare the two in a live stream.)
    let policy = if std::env::var("LITECORD_VAAPI_CAPTURE_PACE").is_ok_and(|v| v == "average") { cadence::Policy::Average } else { cadence::Policy::Even };
    let gate = Mutex::new(Gate { pace: Cadence::new(policy, interval, STILL_REPEAT_EVERY), preview_picture: 0, last_preview: start.checked_sub(PREVIEW_STILL_EVERY).unwrap_or(start), crop: None, withheld: false });
    let pad = queue.static_pad("src").ok_or("pad src ausente")?;
    // While a new encoder is linked in, its request to negotiate again ends here: passed
    // on, pipewiresrc would reconnect its stream (see the module's notes). vapostproc has
    // seen it by then and settles the format and the buffers with the new encoder itself.
    let switching = shared.clone();
    pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        let reconfigure = matches!(&info.data, Some(gst::PadProbeData::Event(event)) if event.type_() == gst::EventType::Reconfigure);
        if reconfigure && switching.drop_reconfigure.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Drop;
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    // The size of the pictures to come, for the bars around them, and vapostproc told
    // whether to keep the picture's shape (bars) or to fill the frame (`picture_rect`). The
    // format is taken here, on the thread that carries the pictures through vapostproc to
    // the encoder's door and right before vapostproc gets it: every picture of the format
    // before has had its bars by then, painted around its own rectangle.
    //
    // A shared window does not come as pictures of its own size: Mutter's stream keeps the
    // size of the window's monitor for good, and each picture says which part of it is the
    // window (PipeWire's crop region, a GstVideoCropMeta here). vapostproc takes that part
    // and scales it to wherever the *format* says the picture goes, so a window of another
    // shape than its monitor would be stretched over the frame. The format vapostproc is
    // given therefore names the pixel shape that makes the whole frame as wide as the
    // window is (`Shape::told`): it then leaves the same bars as for a source of the
    // window's size. A new shape is a new format event from here, in front of the picture
    // that has it: nothing upstream hears of it, the encoder's format stays, and vapostproc
    // takes about a millisecond over it (0.99 ms each over 160 shapes of two dragged resizes).
    struct Shape {
        /// The format as pipewiresrc negotiated it, and its size.
        caps: gst::Caps,
        size: (u32, u32),
        /// The pixel shape vapostproc was last told.
        told: (i32, i32),
    }
    let shape: Arc<Mutex<Option<Shape>>> = Arc::new(Mutex::new(None));
    // The format event passing is this probe's own.
    let own_event = Arc::new(AtomicBool::new(false));
    let (on_caps, on_caps_shape, on_caps_own, on_caps_postproc) = (shared.clone(), shape.clone(), own_event.clone(), postproc.clone());
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if on_caps_own.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Ok;
        }
        if let Some(gst::PadProbeData::Event(event)) = &info.data {
            if let gst::EventView::Caps(caps) = event.view() {
                let structure = caps.caps().structure(0);
                let size = structure.map(|s| (s.get::<i32>("width").unwrap_or(0).max(0) as u32, s.get::<i32>("height").unwrap_or(0).max(0) as u32)).unwrap_or((0, 0));
                let par = structure.and_then(|s| s.get::<gst::Fraction>("pixel-aspect-ratio").ok()).map(|f| (f.numer(), f.denom())).unwrap_or((1, 1));
                *lock(&on_caps_shape) = Some(Shape { caps: caps.caps_owned(), size, told: par });
                on_caps.source_size.store((size.0 as u64) << 32 | size.1 as u64, Ordering::Relaxed);
                on_caps.shape_size.store(0, Ordering::Relaxed);
                on_caps.rect_generation.fetch_add(1, Ordering::Relaxed);
                let rect = on_caps.picture_rect();
                // (set_property panics on a property that is not there; the pipeline's
                // description named it, so it is.)
                if on_caps_postproc.find_property("add-borders").is_some() {
                    on_caps_postproc.set_property("add-borders", (rect.2, rect.3) != on_caps.frame);
                } else if on_caps_postproc.find_property("scale-x").is_some() {
                    // gltransformation: the picture spans the frame unless shrunk to its
                    // rectangle, which `letterbox` centres.
                    on_caps_postproc.set_property("scale-x", rect.2 as f32 / on_caps.frame.0 as f32);
                    on_caps_postproc.set_property("scale-y", rect.3 as f32 / on_caps.frame.1 as f32);
                }
            }
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buffer) = info.buffer() else { return gst::PadProbeReturn::Ok };
        let Some(pts) = buffer.pts() else { return gst::PadProbeReturn::Drop };
        // The part of the buffer that is the picture, when the buffer says so.
        let crop = buffer.meta::<gst_video::VideoCropMeta>().map(|meta| meta.rect()).filter(|r| r.2 > 0 && r.3 > 0).map(|r| (r.2, r.3));
        if !shared.follows_crop {
            let whole = lock(&shape).as_ref().map(|shape| shape.size);
            if crop.is_some_and(|crop| whole.is_some_and(|whole| crop != whole)) {
                // The whole monitor-sized buffer would be shown: see the module's notes.
                shared.crop_refused.store(true, Ordering::Relaxed);
                return gst::PadProbeReturn::Drop;
            }
        }
        let mut gate = lock(&gate);
        // When a window changes size in one step (maximised, restored, out of fullscreen)
        // Mutter now and then hands over one buffer whose rectangle is a frame ahead of
        // what is drawn in it: the old small window in a corner of black, or a corner of the
        // old large one, for one picture (6 of 34 such changes). The first buffer of a much
        // different size is therefore left out. The next passes whatever its size (a drag in
        // large steps loses every other picture at most), and so does this one when the
        // keepalive brings it again because nothing followed.
        let far = |a: u32, b: u32| a.abs_diff(b) * 10 > a.max(b);
        let jump = matches!((gate.crop, crop), (Some(before), Some(now)) if far(before.0, now.0) || far(before.1, now.1));
        gate.crop = crop;
        gate.withheld = jump && !gate.withheld;
        if gate.withheld {
            return gst::PadProbeReturn::Drop;
        }
        let now = Instant::now();
        let picture = buffer.offset();

        let forced = shared.pass_next.load(Ordering::Relaxed);
        // Which pictures pass, and how far apart, is `Cadence`'s business: evenly spaced,
        // never a short step to make up for a late picture.
        let since_start = now.duration_since(start);
        let cadence::Verdict { encode, repeat, too_early } = gate.pace.arrived(since_start, picture, forced, shared.watched.load(Ordering::Relaxed));
        if too_early {
            shared.skipped_pace.fetch_add(1, Ordering::Relaxed);
        }
        if encode {
            shared.pass_next.store(false, Ordering::Relaxed);
            if repeat { &shared.encoded_repeats } else { &shared.encoded_new }.fetch_add(1, Ordering::Relaxed);
        }
        if let Some((report, announce)) = gate.pace.take_report(since_start) {
            if announce {
                info!("🎞️ [VAAPI] Capture cadence: {report}");
            }
            *lock(&shared.cadence) = Some(report);
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
        // This picture goes into vapostproc: told its shape first, when that is a new one.
        let new_format = {
            let mut shape = lock(&shape);
            shape.as_mut().and_then(|shape| {
                let picture = crop.unwrap_or(shape.size);
                // (A part of the buffer is shown in a shape with even bars; a picture that
                // is its whole buffer, a monitor, as before.)
                let shown = crop.map(|picture| even_fit(picture, shared.frame));
                let wanted = pixel_shape(shown.unwrap_or(picture), shape.size)?;
                shared.source_size.store((picture.0 as u64) << 32 | picture.1 as u64, Ordering::Relaxed);
                shared.shape_size.store(shown.map_or(0, |s| (s.0 as u64) << 32 | s.1 as u64), Ordering::Relaxed);
                if wanted == shape.told {
                    // (Another size of the same shape: the same place in the frame.)
                    return None;
                }
                shape.told = wanted;
                let mut caps = shape.caps.clone();
                caps.make_mut().set("pixel-aspect-ratio", gst::Fraction::new(wanted.0, wanted.1));
                Some(caps)
            })
        };
        if let Some(caps) = new_format {
            shared.rect_generation.fetch_add(1, Ordering::Relaxed);
            let rect = shared.picture_rect();
            if postproc.find_property("add-borders").is_some() {
                postproc.set_property("add-borders", (rect.2, rect.3) != shared.frame);
            }
            let told_at = Instant::now();
            own_event.store(true, Ordering::SeqCst);
            let taken = pad.push_event(gst::event::Caps::new(&caps));
            own_event.store(false, Ordering::SeqCst);
            shared.shape_changes.fetch_add(1, Ordering::Relaxed);
            shared.shape_us.fetch_add(told_at.elapsed().as_micros() as u64, Ordering::Relaxed);
            if !taken {
                // Stretched over the frame it would be: `health` gives the stream up.
                shared.shape_refused.store(true, Ordering::Relaxed);
                return gst::PadProbeReturn::Drop;
            }
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

/// At the encoder's door, behind vapostproc (on the pad that stays when the encoder is
/// replaced): pictures meant for the preview are handed to its thread (as NV12 at the
/// stream's size, still on the GPU), and only the ones the gate chose for encoding go in.
fn add_encoder_probe(encoder_input: &gst::Element, shared: &Arc<Shared>, preview: SyncSender<(gst::Buffer, gst::Caps)>) -> Result<(), String> {
    let shared = shared.clone();
    let pad = encoder_input.static_pad("src").ok_or("pad src ausente")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        // First of all, for the encoder and the preview alike: the bars around a picture
        // of another shape than the frame's.
        let rect = shared.picture_rect();
        if shared.bars_by_cpu && (rect.2, rect.3) != shared.frame {
            if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() {
                if !paint_new_surface(pad, buffer, rect, &shared) {
                    // Whatever the surface held would show around the picture (green, when
                    // new): shown to nobody. `health` gives the stream up.
                    return gst::PadProbeReturn::Drop;
                }
            }
        }
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
        shared.at_encoder_ms.store(shared.now_ms().max(1), Ordering::Relaxed);
        if shared.hang.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(20));
        }
        let guarded = shared.guard_main.load(Ordering::SeqCst);
        if guarded != 0 {
            // A Main encoder that has not proven itself: the picture is handed to it from
            // here, so that an error of its own is not one of the capture's (the flow error
            // would stop the streaming threads up to pipewiresrc for good). Nothing tells a
            // late first picture from a thread that never comes back out of the driver but
            // this call returning (vah264enc encodes inside it): `at_encoder_ms`.
            if let (Some(encoder), Some(gst::PadProbeData::Buffer(buffer))) = (pad.peer(), info.data.take()) {
                if encoder.chain(buffer).is_err() {
                    shared.main_broke.store(guarded, Ordering::SeqCst);
                }
                shared.at_encoder_ms.store(0, Ordering::Relaxed);
                info.flow_res = Ok(gst::FlowSuccess::Ok);
                return gst::PadProbeReturn::Handled;
            }
        }
        // (An encoder that has proven itself gets the picture when this returns: one that
        // keeps it is found by the pictures still awaited, `in_flight`.)
        shared.at_encoder_ms.store(0, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    })
    .ok_or("probe não instalado")?;
    Ok(())
}

/// Paints the bars of a picture fresh out of vapostproc, unless its surface already has them:
/// the surface's memory object, which the buffer pool hands out again with the surface,
/// carries the number of the rectangle its bars were painted around (`rect_generation`:
/// when the rectangle changes, every surface is painted anew the next time it comes).
fn paint_new_surface(pad: &gst::Pad, buffer: &mut gst::Buffer, rect: (u32, u32, u32, u32), shared: &Shared) -> bool {
    use gst::glib::translate::IntoGlib;
    static PAINTED: std::sync::OnceLock<gst::glib::Quark> = std::sync::OnceLock::new();
    let quark = PAINTED.get_or_init(|| gst::glib::Quark::from_str("litecord-bars-painted")).into_glib();
    if buffer.n_memory() == 0 {
        return true;
    }
    // Never 0. Not the rectangle itself: a surface painted for one rectangle, then filled by
    // a picture without bars (the window maximised), then used for the first rectangle
    // again (restored) would keep that picture where its bars belong.
    let mark = shared.rect_generation.load(Ordering::Relaxed) as usize as gst::glib::ffi::gpointer;
    let memory = buffer.peek_memory(0).as_ptr() as *mut gst::ffi::GstMiniObject;
    // (The memory outlives this call: the buffer holds it.)
    if unsafe { gst::ffi::gst_mini_object_get_qdata(memory, quark) } == mark {
        return true;
    }
    let started = Instant::now();
    let painted = if shared.no_bars {
        Err("falha simulada (LITECORD_VAAPI_CAPTURE_TEST_FAIL=nobars)".to_string())
    } else {
        pad.current_caps().ok_or("sem caps".to_string()).and_then(|caps| paint_bars(buffer.make_mut(), &caps, rect))
    };
    match painted {
        Ok(()) => {
            // The mapping may have replaced the memory by a copy: the one in the buffer now.
            let memory = buffer.peek_memory(0).as_ptr() as *mut gst::ffi::GstMiniObject;
            unsafe { gst::ffi::gst_mini_object_set_qdata(memory, quark, mark, None) };
            shared.bars_painted.fetch_add(1, Ordering::Relaxed);
            shared.bars_us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            true
        }
        Err(e) => {
            if shared.bars_failed.fetch_add(1, Ordering::Relaxed) == 0 {
                warn!("⚠️ [VAAPI] As barras em volta da imagem não puderam ser pintadas ({})", e);
            }
            false
        }
    }
}

/// Black (BT.601 limited range: Y 16, Cb and Cr 128) over everything outside `rect` in an
/// NV12 picture. Where the picture's edge is odd, a chroma sample is shared by its outermost
/// row or column and the bar's first; those are painted with the bar. radeonsi writes the
/// picture's chroma from ceil(start / 2) to ceil(end / 2): the shared sample at the left or
/// the top stays as painted, the one at the right or the bottom is the picture's again from
/// the surface's next use on (see the module's notes). Painting it all the same costs
/// nothing and leaves no sample unwritten on a driver that rounds the other way.
fn paint_bars(buffer: &mut gst::BufferRef, caps: &gst::Caps, (x, y, width, height): (u32, u32, u32, u32)) -> Result<(), String> {
    let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
    if info.format() != gst_video::VideoFormat::Nv12 {
        return Err(format!("formato {:?}", info.format()));
    }
    let (w, h) = (info.width() as usize, info.height() as usize);
    let (x, y, width, height) = (x as usize, y as usize, width as usize, height as usize);
    if w % 2 != 0 || x + width > w || y + height > h {
        return Err("retângulo fora do quadro".into());
    }
    let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info).map_err(|e| e.to_string())?;
    let (y_stride, uv_stride) = (frame.plane_stride()[0] as usize, frame.plane_stride()[1] as usize);
    // Rows `rows` of a plane hold the picture in their bytes `cols`; the rest is bars.
    let fill = |plane: &mut [u8], stride: usize, lines: usize, rows: std::ops::Range<usize>, cols: std::ops::Range<usize>, value: u8| -> Result<(), String> {
        if stride < w || plane.len() < stride * (lines - 1) + w {
            return Err("plano menor que o quadro".into());
        }
        for (row, line) in plane.chunks_mut(stride).take(lines).enumerate() {
            let line = &mut line[..w];
            if rows.contains(&row) {
                line[..cols.start].fill(value);
                line[cols.end..].fill(value);
            } else {
                line.fill(value);
            }
        }
        Ok(())
    };
    fill(frame.plane_data_mut(0).map_err(|e| e.to_string())?, y_stride, h, y..y + height, x..x + width, 16)?;
    // Chroma samples wholly inside the picture: two bytes each, for two columns and two rows.
    let (cols, rows) = (x.div_ceil(2) * 2..(x + width) / 2 * 2, y.div_ceil(2)..(y + height) / 2);
    fill(frame.plane_data_mut(1).map_err(|e| e.to_string())?, uv_stride, h.div_ceil(2), rows, cols, 128)
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
    let (id, source_size, dmabuf, element) = (stream.id, stream.source_size, stream.dmabuf, stream.element);
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
        "🚀 [VAAPI] Captura na GPU ativa ({} {}): nó PipeWire {} {}x{} ({}) -> {}x{} {} FPS, primeiro quadro em {:.0} ms",
        vaapi::Backend::of(element).name(),
        element,
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
    // The next share asks for its own floor.
    WANTED_MIN_QP.store(0, Ordering::Relaxed);
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
        if let Err(reason) = stream.health(false, false) {
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

/// Somebody started or stopped watching: nothing is encoded while nobody does. The way into
/// the encoder is closed here and opened by the next `next_picture`, which knows the profile
/// the new viewers decode.
pub fn set_watched(watched: bool) {
    if watched {
        return;
    }
    if let Some(stream) = current() {
        stream.watch(&mut lock(&stream.tx), false);
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
        let sink = stream.sink();
        if let Some(sample) = Stream::pull(&mut tx, &sink, max) {
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
/// `main`: every viewer of this turn decodes H.264 Main (`watch::ProfileChoice`). When that
/// is false no Main access unit is returned, from this call on.
pub fn next_picture(bitrate_bps: u32, want_idr: bool, main: bool) -> Option<Picture> {
    let stream = current()?;
    let mut tx = lock(&stream.tx);
    if bitrate_bps != tx.requested_bps {
        // A new encoder starts at the rate asked for now.
        tx.requested_bps = bitrate_bps;
        tx.asked_at = Instant::now();
    }
    if let Err(reason) = stream.follow_profile(&mut tx, main, want_idr).and_then(|()| stream.follow_floor(&mut tx)) {
        drop(tx);
        give_up_stream(&stream, &reason);
        return None;
    }
    stream.watch(&mut tx, true);
    if let Err(reason) = stream.control(&mut tx, bitrate_bps, want_idr) {
        drop(tx);
        give_up_stream(&stream, &reason);
        return None;
    }
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
    let sink = stream.sink();
    let failure = loop {
        let sample = match tx.ready.pop_front() {
            Some(sample) => Some(sample),
            None => Stream::pull(&mut tx, &sink, Duration::ZERO),
        };
        let Some(sample) = sample else {
            // Nothing finished. (A Main encoder that has not shown its first keyframe has
            // its own deadline, in `follow_profile`; one made for a new quality floor has
            // its own in `follow_floor`.)
            break stream.health(true, (!tx.main || tx.main_proven) && tx.floor.in_flight().is_none());
        };
        if swallow || (tx.mute_main > 0 && tx.main && !tx.main_proven) {
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

/// The quality floor (min-qp) the stream controls ask for now, told by the send loop before
/// each `next_au`: the stream in service follows it (`Stream::follow_floor`). It is not a
/// property of a stream: one that starts later is made with `Config::min_qp` and follows
/// what is asked from then on.
pub fn set_min_qp(min_qp: u32) {
    WANTED_MIN_QP.store(min_qp, Ordering::Relaxed);
}

/// `next_picture` for the send loop, which only wants the bytes.
pub fn next_au(bitrate_bps: u32, want_idr: bool, main: bool) -> Option<Vec<u8>> {
    next_picture(bitrate_bps, want_idr, main).map(|p| p.data)
}

/// New pictures the compositor delivered since the last call (the send loop's telemetry).
pub fn take_new_pictures() -> u32 {
    current().map(|s| s.shared.uncounted_pictures.swap(0, Ordering::Relaxed) as u32).unwrap_or(0)
}

pub fn stats() -> Option<Stats> {
    current().map(|s| s.stats())
}
