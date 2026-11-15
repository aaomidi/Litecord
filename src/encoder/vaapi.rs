//! H.264 on the GPU through GStreamer's VA-API elements (Linux: AMD, Intel).
//!
//!   frames (BGRA) -> vapostproc (to NV12, on the GPU) -> vah264enc -> appsink
//!
//! What the receivers need from it, each found by running them against vah264enc:
//! - Constrained Baseline unless every viewer says it handles Main: with Main/High the
//!   encoder's picture order counts make the OpenH264 receiver show about three pictures per
//!   keyframe. Main (`set_efficient_profile`) is the same pipeline with other caps: CABAC,
//!   still one P picture per frame, padded and declared the same way.
//! - BT.601 limited range (vapostproc picks BT.709 at 1080p by itself), declared in the SPS
//!   by `h264_vui::declare_colour` since vah264enc writes no colour description: matrix 6
//!   (what every receiver converts with) with the desktop's own primaries and transfer (1),
//!   as the FFmpeg path intends, so colour-managed browsers do not re-map them. The caps
//!   on both sides of vapostproc name the same transfer function (sRGB): asked for BT.709's
//!   on its output, radeonsi re-curves the picture (grey 128 comes back as 135).
//! - One access unit per frame with no delay. vah264enc holds four frames back unless its
//!   source is live, so whatever feeds this pipeline must be a live source.
//!
//! When the GPU path stops answering mid-stream, the encoder switches itself to OpenH264
//! (nothing else would replace it) and tries the GPU again later.

use super::{OpenH264Encoder, VideoEncoder};
use crate::screen_capture::hw_decode::h264_state::nal_ranges;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Preferred first; `vah264lpenc` is the only one Intel's low-power encoders register.
pub(crate) const ENCODER_ELEMENTS: [&str; 2] = ["vah264enc", "vah264lpenc"];
/// Streams start at this rate (types::REQUESTED_BITRATE_BPS).
pub(crate) const INITIAL_BITRATE_BPS: u32 = 8_000_000;
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);
/// A new pipeline creates the VA context on its first frame (measured: 16-82 ms at 1080p).
pub(crate) const FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(400);
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);
/// Time without a picture before the stream goes to OpenH264. Viewers drop a stream after
/// 2.5 s; a GPU hiccup shorter than this only costs the frames it lasted and a keyframe.
const STALL_BUDGET: Duration = Duration::from_millis(1000);
/// Every separate loss costs a keyframe: more than this many within `LOSS_WINDOW` also
/// send the stream to OpenH264.
const MAX_LOSSES: u32 = 10;
const LOSS_WINDOW: Duration = Duration::from_secs(10);
/// On OpenH264, the GPU is tried again after this long (on a keyframe the caller asks
/// for); the wait doubles with every attempt, up to `GPU_RETRY_MAX`.
const GPU_RETRY_AFTER: Duration = Duration::from_secs(30);
const GPU_RETRY_MAX: Duration = Duration::from_secs(480);
/// vah264enc starts a new sequence (an IDR) whenever its bitrate changes, so a new rate
/// waits for the next keyframe the caller asks for (at least every 2 s), up to this long.
pub(crate) const BITRATE_MAX_WAIT: Duration = Duration::from_millis(3000);
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_millis(300);
/// The capsfilter between vapostproc and the encoder: the last element that stays when the
/// encoder is replaced.
pub(crate) const ENCODER_INPUT: &str = "enc_in";
/// vapostproc in a pipeline made from `EncodeChain::description_scaled`.
pub(crate) const POSTPROC: &str = "enc_scale";
const PROBE_SIZE: (u32, u32) = (640, 360);

/// Test hook: `LITECORD_VAAPI_TEST_FAIL=init` fails construction, `after:N` makes every GPU
/// frame from the Nth on fail at once, `stall:N` makes them time out for real. Two more
/// numbers (`stall:N:COUNT:EVERY`) limit it to COUNT frames, one in EVERY: GPU hiccups.
/// `main` makes every Main pipeline fail to start (a GPU without that profile).
#[derive(Clone, Copy, PartialEq)]
enum TestFail {
    None,
    Init,
    After,
    Stall,
    Main,
}

struct TestHook {
    fail: TestFail,
    from: u64,
    left: u64,
    every: u64,
    calls: u64,
}

impl TestHook {
    fn from_env() -> Self {
        let v = std::env::var("LITECORD_VAAPI_TEST_FAIL").unwrap_or_default();
        let mut parts = v.split(':');
        let fail = match parts.next() {
            Some("init") => TestFail::Init,
            Some("after") => TestFail::After,
            Some("stall") => TestFail::Stall,
            Some("main") => TestFail::Main,
            _ => TestFail::None,
        };
        let mut number = |default: u64| parts.next().and_then(|s| s.parse().ok()).unwrap_or(default);
        Self { fail, from: number(0), left: number(u64::MAX), every: number(1).max(1), calls: 0 }
    }

    /// The failure to simulate for the next GPU frame, if any.
    fn next(&mut self, gpu_frames: u64) -> TestFail {
        if matches!(self.fail, TestFail::None | TestFail::Main) || gpu_frames < self.from || self.left == 0 {
            return TestFail::None;
        }
        self.calls += 1;
        if (self.calls - 1) % self.every != 0 {
            return TestFail::None;
        }
        self.left -= 1;
        self.fail
    }
}

/// The access unit without filler data NAL units (type 12): in CBR mode radeonsi pads
/// every picture smaller than the frame budget with them, megabits of nothing on a still
/// screen.
pub(crate) fn without_filler(au: Vec<u8>) -> Vec<u8> {
    let ranges = nal_ranges(&au);
    if !ranges.iter().any(|r| au[r.start] & 0x1f == 12) {
        return au;
    }
    let mut out = Vec::with_capacity(au.len());
    // Each unit is copied with the start code before it.
    let mut from = 0;
    for r in ranges {
        if au[r.start] & 0x1f != 12 {
            out.extend_from_slice(&au[from..r.end]);
        }
        from = r.end;
    }
    out
}

/// (level_idc, MaxBR in kbit/s) of H.264 table A-1. Baseline and Main streams may carry
/// 1.2 times that, counting whole NAL units.
const LEVEL_MAX_KBPS: [(u8, u32); 16] = [
    (10, 64),
    (11, 192),
    (12, 384),
    (13, 768),
    (20, 2_000),
    (21, 4_000),
    (22, 4_000),
    (30, 10_000),
    (31, 14_000),
    (32, 20_000),
    (40, 20_000),
    (41, 50_000),
    (42, 50_000),
    (50, 135_000),
    (51, 240_000),
    (52, 240_000),
];

/// vah264enc picks the level of a stream from its picture size and frame rate alone:
/// 1080p30 comes out as level 4.0 and 720p60 as 3.2 (both end at 24 Mbps) whatever the
/// bitrate. Every SPS of `au` whose level does not allow `bitrate_bps` is given the lowest
/// one that does. Only that byte changes: the frame buffering is in the VUI.
fn raise_level(au: &mut [u8], bitrate_bps: u32) {
    let allows = |max_kbps: u32| max_kbps as u64 * 1200 >= bitrate_bps as u64;
    for r in nal_ranges(au) {
        if au[r.start] & 0x1f != 7 || r.len() < 4 {
            continue;
        }
        let written = au[r.start + 3];
        if LEVEL_MAX_KBPS.iter().any(|&(level, max)| level == written && !allows(max)) {
            if let Some(&(level, _)) = LEVEL_MAX_KBPS.iter().find(|&&(level, max)| level > written && allows(max)) {
                au[r.start + 3] = level;
            }
        }
    }
}

/// Pipelines being stopped right now, and whether the process is about to exit. Mesa's VA
/// driver deadlocks when `exit()` runs while another thread closes a VA display (its exit
/// handler joins the driver threads the closing thread waits for).
static TEARDOWNS: AtomicUsize = AtomicUsize::new(0);
static EXITING: AtomicBool = AtomicBool::new(false);

pub(crate) struct Teardown;

impl Drop for Teardown {
    fn drop(&mut self) {
        TEARDOWNS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// For a VA pipeline stopped outside `EncodeChain`'s own drop: hold the result until it
/// has stopped. `None` means the process is exiting and the pipeline must be left alone.
pub(crate) fn begin_teardown() -> Option<Teardown> {
    TEARDOWNS.fetch_add(1, Ordering::SeqCst);
    let teardown = Teardown;
    (!EXITING.load(Ordering::SeqCst)).then_some(teardown)
}

/// To be called right before `std::process::exit`: pipelines still running are left to the
/// system from now on, and one being stopped gets a moment to finish.
pub fn before_exit() {
    EXITING.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_millis(500);
    while TEARDOWNS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The pipeline from colour conversion to the encoded access units. The bitrate, keyframe
/// requests and the output only touch this part, so the frame source can be replaced.
pub(crate) struct EncodeChain {
    pub(crate) pipeline: gst::Pipeline,
    pub(crate) bus: gst::Bus,
    pub(crate) encoder: gst::Element,
    pub(crate) sink: gst_app::AppSink,
    /// How long dropping this waits for the pipeline to stop.
    teardown_wait: Duration,
    /// Set while it stops. Last field: released after the pipeline.
    teardown: Option<Arc<Teardown>>,
}

enum PullError {
    Timeout,
    Broken(String),
}

impl EncodeChain {
    /// The size is named after vapostproc: an encoder that cannot take it fails to
    /// negotiate instead of getting a scaled picture.
    fn description(element: &str, width: u32, height: u32, bitrate_bps: u32, min_qp: u32, main: bool) -> String {
        Self::description_with(element, width, height, bitrate_bps, min_qp, main, "", "")
    }

    /// For a source of any size (the in-process screen capture hands vapostproc pictures
    /// of the monitor's size): vapostproc scales the picture to fit this size, its shape
    /// kept and centred (`linux_portal::letterbox` says where). With square pixels named on both sides,
    /// `add-borders` leaves the rest of the frame to "borders" that some drivers never write
    /// (radeonsi: whatever the surface held, green when new): the caller paints them.
    pub(crate) fn description_scaled(element: &str, width: u32, height: u32, bitrate_bps: u32, min_qp: u32, main: bool) -> String {
        Self::description_with(element, width, height, bitrate_bps, min_qp, main, &format!(" name={POSTPROC} add-borders=true"), ",pixel-aspect-ratio=1/1")
    }

    fn description_with(element: &str, width: u32, height: u32, bitrate_bps: u32, min_qp: u32, main: bool, postproc_props: &str, more_caps: &str) -> String {
        format!(
            "vapostproc{postproc_props} ! capsfilter name={ENCODER_INPUT} caps=\"video/x-raw(memory:VAMemory),format=NV12,width={width},height={height},colorimetry=(string)2:4:7:1{more_caps}\" \
             ! {}",
            Self::encoder_description(element, bitrate_bps, min_qp, main)
        )
    }

    /// The chain from the encoder on: what is replaced, behind the element named
    /// `ENCODER_INPUT`, when a running pipeline changes its profile (`EncoderSection`).
    pub(crate) fn encoder_description(element: &str, bitrate_bps: u32, min_qp: u32, main: bool) -> String {
        format!(
            "{element} name=enc {ENCODER_PROPS} min-qp={min_qp} max-qp={} bitrate={} cpb-size={} \
             ! video/x-h264,profile={},stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false async=false max-buffers=4 drop=false emit-signals=false enable-last-sample=false",
            MAX_QP.max(min_qp),
            bitrate_bps / 1000,
            cpb_size_kbit(bitrate_bps),
            if main { "main" } else { "constrained-baseline" }
        )
    }

    pub(crate) fn from_pipeline(pipeline: gst::Pipeline) -> Result<Self, String> {
        let bus = pipeline.bus().ok_or("pipeline without bus")?;
        let encoder = pipeline.by_name("enc").ok_or("encoder element missing")?;
        let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("appsink missing")?;
        Ok(Self { pipeline, bus, encoder, sink, teardown_wait: TEARDOWN_WAIT, teardown: None })
    }

    /// Takes effect on the next frame, which the encoder turns into an IDR.
    pub(crate) fn set_bitrate(&self, bitrate_bps: u32) {
        set_encoder_bitrate(&self.encoder, bitrate_bps);
    }

    /// The frame stamped `pts` becomes an IDR with SPS and PPS; not an earlier one that is
    /// still on its way to the encoder (given up on, its output is thrown away).
    fn request_keyframe(&self, pts: gst::ClockTime) -> bool {
        self.sink.send_event(gst_video::UpstreamForceKeyUnitEvent::builder().running_time(pts).all_headers(true).build())
    }

    fn bus_error(&self) -> Option<String> {
        let msg = self.bus.pop_filtered(&[gst::MessageType::Error])?;
        match msg.view() {
            gst::MessageView::Error(e) => Some(format!("{} ({})", e.error(), e.debug().map(|d| d.to_string()).unwrap_or_default())),
            _ => None,
        }
    }

    /// Waits for the access unit of the frame stamped `pts`. Output of frames given up on
    /// earlier (it arrived after their timeout) is thrown away.
    fn pull(&self, pts: gst::ClockTime, timeout: Duration) -> Result<Vec<u8>, PullError> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            // Short waits, so a pipeline error is noticed before the whole timeout passes.
            let wait = left.min(Duration::from_millis(20));
            match self.sink.try_pull_sample(gst::ClockTime::from_nseconds(wait.as_nanos() as u64)) {
                Some(sample) => {
                    let Some(buffer) = sample.buffer() else { continue };
                    // The encoder shifts its timestamps (and its segment with them) to leave
                    // room for decode times; the running time is still the frame's own.
                    let segment = sample.segment().and_then(|s| s.downcast_ref::<gst::ClockTime>());
                    let stamp = segment.and_then(|s| s.to_running_time(buffer.pts())).or(buffer.pts());
                    if stamp != Some(pts) {
                        continue;
                    }
                    let map = buffer.map_readable().map_err(|e| PullError::Broken(e.to_string()))?;
                    return Ok(map.as_slice().to_vec());
                }
                None => {
                    if let Some(e) = self.bus_error() {
                        return Err(PullError::Broken(e));
                    }
                    if self.sink.is_eos() {
                        return Err(PullError::Broken("end of stream".into()));
                    }
                    if left.is_zero() {
                        return Err(PullError::Timeout);
                    }
                }
            }
        }
    }
}

impl Drop for EncodeChain {
    fn drop(&mut self) {
        let teardown = Arc::new(Teardown);
        TEARDOWNS.fetch_add(1, Ordering::SeqCst);
        if EXITING.load(Ordering::SeqCst) {
            // Never closed: the process is exiting (see `before_exit`).
            std::mem::forget(self.pipeline.clone());
            return;
        }
        self.teardown = Some(teardown.clone());
        // Stopping waits for the streaming thread. Should that one hang in the driver (the
        // reason for this drop, perhaps), the stream thread only waits a moment and leaves
        // the rest to the helper thread.
        let pipeline = self.pipeline.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop = move || {
            let _ = pipeline.set_state(gst::State::Null);
            // Released first: when the stream thread waited, the VA display closes there,
            // before the drop returns.
            drop((pipeline, teardown));
            let _ = done_tx.send(());
        };
        match std::thread::Builder::new().name("vaapi-teardown".into()).spawn(stop) {
            Ok(_) => {
                if done_rx.recv_timeout(self.teardown_wait).is_err() && !self.teardown_wait.is_zero() {
                    warn!("⚠️ [VAAPI] O pipeline não parou em {} ms; segue encerrando em segundo plano", self.teardown_wait.as_millis());
                }
            }
            Err(_) => {
                let _ = self.pipeline.set_state(gst::State::Null);
            }
        }
    }
}

/// Sets the rate of a running encoder element; it takes effect on the next frame, which the
/// encoder turns into an IDR.
pub(crate) fn set_encoder_bitrate(encoder: &gst::Element, bitrate_bps: u32) {
    for (name, value) in [("bitrate", bitrate_bps / 1000), ("cpb-size", cpb_size_kbit(bitrate_bps))] {
        // set_property panics on a missing property, another type or a value out of range.
        if let Some(spec) = encoder.find_property(name) {
            if let Some(range) = spec.downcast_ref::<gst::glib::ParamSpecUInt>() {
                encoder.set_property(name, value.clamp(range.minimum(), range.maximum()));
            }
        }
    }
}

/// The next frame entering the encoder in front of `sink` becomes an IDR with SPS and PPS
/// (for a source that stamps its own pictures).
pub(crate) fn request_next_keyframe(sink: &gst_app::AppSink) -> bool {
    sink.send_event(gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build())
}

/// The encoder of a running pipeline with what follows it (profile filter, appsink). vah264enc
/// cannot change its profile while it runs, but the part of the pipeline in front of it can
/// stay as it is: measured on radeonsi at 1080p60, taking one section out and linking a new
/// one in takes under 5 ms and the new encoder's first picture (an IDR with SPS and PPS)
/// comes out in the next frame's slot, where a whole new capture pipeline takes 70-520 ms to
/// its first picture.
pub(crate) struct EncoderSection {
    pub(crate) encoder: gst::Element,
    pub(crate) sink: gst_app::AppSink,
    /// What `remove` takes out of the pipeline: the elements `gst::parse::launch` made, or
    /// the one bin of a section built by `build`.
    parts: Vec<gst::Element>,
    pub(crate) main: bool,
}

impl EncoderSection {
    /// The section a pipeline made from `EncodeChain::description*` starts with.
    pub(crate) fn of_chain(chain: &EncodeChain, main: bool) -> Self {
        // The encoder, and everything linked behind it.
        let mut parts = vec![chain.encoder.clone()];
        while let Some(next) = parts.last().and_then(|e| e.static_pad("src")).and_then(|p| p.peer()).and_then(|p| p.parent_element()) {
            parts.push(next);
        }
        Self { encoder: chain.encoder.clone(), sink: chain.sink.clone(), parts, main }
    }

    /// A new section, in no pipeline yet. `description`: `EncodeChain::encoder_description`.
    pub(crate) fn build(description: &str, main: bool) -> Result<Self, String> {
        let bin = gst::parse::bin_from_description(description, true).map_err(|e| e.to_string())?;
        let encoder = bin.by_name("enc").ok_or("encoder element missing")?;
        let sink = bin.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("appsink missing")?;
        Ok(Self { encoder, sink, parts: vec![bin.upcast()], main })
    }

    /// The pad pictures go in through.
    pub(crate) fn input(&self) -> Option<gst::Pad> {
        self.parts.first().and_then(|e| e.static_pad("sink"))
    }

    /// The section's elements (the source of a bus message is one of them, or inside one).
    pub(crate) fn objects(&self) -> Vec<gst::Object> {
        self.parts.iter().map(|part| part.clone().upcast()).collect()
    }

    /// Puts the section into `pipeline`, running, linked to nothing.
    pub(crate) fn add_to(&self, pipeline: &gst::Pipeline) -> Result<(), String> {
        for part in &self.parts {
            pipeline.add(part).map_err(|e| e.to_string())?;
        }
        // The sink first, as a pipeline does.
        for part in self.parts.iter().rev() {
            part.sync_state_with_parent().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Takes the section out of `pipeline` (already unlinked from what feeds it, or never
    /// linked) and stops it, waiting up to `wait` for that: the VA context of an encoder
    /// should be gone before the next one is made, but a driver that hangs must not hold
    /// the caller. Measured: 0.2-3 ms.
    pub(crate) fn remove(self, pipeline: &gst::Pipeline, wait: Duration) {
        for part in &self.parts {
            let _ = pipeline.remove(part);
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop = move || {
            // (Mesa's VA driver and a process that exits meanwhile: see `begin_teardown`.)
            let Some(_stopping) = begin_teardown() else {
                std::mem::forget(self);
                return;
            };
            for part in &self.parts {
                let _ = part.set_state(gst::State::Null);
            }
            drop(self);
            let _ = done_tx.send(());
        };
        if std::thread::Builder::new().name("vaapi-teardown".into()).spawn(stop).is_ok() && done_rx.recv_timeout(wait).is_err() {
            warn!("⚠️ [VAAPI] O codificador substituído não parou em {} ms; segue encerrando em segundo plano", wait.as_millis());
        }
    }
}

/// vah264enc settings shared by every stream. `ref-frames=1` and no B-frames give P
/// pictures that each reference the one before; the caller asks for its own keyframes, so
/// the encoder's GOP is as long as it allows.
///
/// Rate control, measured on radeonsi (RX 7900 XTX) with 1080p screens of text. Nothing
/// vah264enc exposes limits one picture's size directly, and a keyframe goes out as one
/// burst of UDP packets:
/// - `cbr` keeps pictures near the frame budget on busy content (IDR about 4 frames' worth);
///   `vbr` let single keyframes reach 800 kB. CBR's padding is removed (`without_filler`).
/// - On a quiet screen the rate control has bits to spare and spends them on the keyframe:
///   only a QP floor holds it back. `min-qp=30` (the default of the "quality floor"
///   setting) gave 215 kB keyframes where no floor gave 300-440 kB (OpenH264: 170-190 kB,
///   at lower quality in both keyframes and P pictures); each 2 steps of QP are worth
///   about 10% of the size.
/// - `cpb-size` (see `cpb_size_kbit`) caps a keyframe at half a second of the bitrate.
/// - `max-qp=44` is for the pictures the rate control starves: the first keyframe of a
///   pipeline (25-26 dB instead of 21-25 on an IDE screen) and every keyframe of dense text
///   at 3 Mbps (21 dB instead of 15-19). Its price is paid on that last content only:
///   keyframes pass the cap by 12% and the stream its target by 12% at 30 FPS, 24% at 60
///   (5% and 12% without the bound). `max-qp=42` tripled the excess for 1.5 dB more.
const ENCODER_PROPS: &str =
    "rate-control=cbr target-usage=3 key-int-max=1024 b-frames=0 i-frames=0 ref-frames=1 num-slices=1 aud=false cc-insert=false";
/// `max-qp` (see above), unless the QP floor is set higher.
const MAX_QP: u32 = 44;

/// The smallest buffer vah264enc accepts (half a second of the bitrate; smaller values are
/// silently replaced by two seconds). Not a hard limit: keyframes came out up to 3% larger
/// than it, 12% where `max-qp` holds the quality up.
fn cpb_size_kbit(bitrate_bps: u32) -> u32 {
    (bitrate_bps / 1000).div_ceil(2)
}

/// Frames copied from memory into the pipeline (appsrc).
struct MemorySource {
    appsrc: gst_app::AppSrc,
    pool: gst::BufferPool,
}

impl MemorySource {
    fn description(width: u32, height: u32, fps: u32) -> String {
        format!(
            "appsrc name=src is-live=true format=time block=false stream-type=stream max-buffers=4 \
             caps=\"video/x-raw,format=BGRA,width={width},height={height},framerate={}/1,colorimetry=(string)sRGB\"",
            fps.max(1)
        )
    }

    fn from_pipeline(pipeline: &gst::Pipeline, width: u32, height: u32) -> Result<Self, String> {
        let appsrc = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or("appsrc missing")?;
        // Recycled buffers: a fresh 8 MB allocation per 1080p frame costs page faults.
        let pool = gst::BufferPool::new();
        let mut config = pool.config();
        config.set_params(None, width * height * 4, 2, 0);
        pool.set_config(config).map_err(|e| e.to_string())?;
        pool.set_active(true).map_err(|e| e.to_string())?;
        Ok(Self { appsrc, pool })
    }

    /// `bgra` holds `height` rows of `stride` bytes; `width` pixels of each are sent.
    fn push(&self, bgra: &[u8], width: usize, height: usize, stride: usize, pts: gst::ClockTime) -> Result<(), String> {
        let mut buffer = self.pool.acquire_buffer(None).map_err(|e| format!("buffer pool: {e:?}"))?;
        {
            let buf = buffer.get_mut().ok_or("frame buffer is shared")?;
            buf.set_pts(pts);
            let mut map = buf.map_writable().map_err(|e| e.to_string())?;
            let row = width * 4;
            let out = map.as_mut_slice().get_mut(..row * height).ok_or("frame buffer too small")?;
            if stride == row {
                out.copy_from_slice(&bgra[..row * height]);
            } else {
                for (dst, src) in out.chunks_exact_mut(row).zip(bgra.chunks(stride)) {
                    dst.copy_from_slice(&src[..row]);
                }
            }
        }
        self.appsrc.push_buffer(buffer).map(|_| ()).map_err(|e| format!("appsrc: {e:?}"))
    }
}

impl Drop for MemorySource {
    fn drop(&mut self) {
        let _ = self.pool.set_active(false);
    }
}

/// One running pipeline, for one picture size.
struct Session {
    source: MemorySource,
    chain: EncodeChain,
    width: u32,
    height: u32,
    /// Rate the encoder element runs at now.
    bitrate_bps: u32,
    /// H.264 Main instead of Constrained Baseline.
    main: bool,
    frames: u64,
}

impl Session {
    fn start(element: &str, width: u32, height: u32, fps: u32, bitrate_bps: u32, min_qp: u32, main: bool) -> Result<Self, String> {
        let description =
            format!("{} ! {}", MemorySource::description(width, height, fps), EncodeChain::description(element, width, height, bitrate_bps, min_qp, main));
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        // From here on dropping the chain stops the pipeline.
        let chain = EncodeChain::from_pipeline(pipeline)?;
        let source = MemorySource::from_pipeline(&chain.pipeline, width, height)?;
        if let Err(e) = chain.pipeline.set_state(gst::State::Playing) {
            return Err(format!("{e}: {}", chain.bus_error().unwrap_or_default()));
        }
        Ok(Self { source, chain, width, height, bitrate_bps, main, frames: 0 })
    }
}

pub struct VaapiEncoder {
    element: &'static str,
    target_fps: u32,
    is_screen_content: bool,
    /// Last rate asked for (what `get_bitrate_bps` answers).
    bitrate_bps: u32,
    bitrate_asked_at: Instant,
    needs_keyframe: bool,
    /// A picture was lost on the way: viewers can only go on from a keyframe.
    lost_picture: bool,
    session: Option<Session>,
    /// Timestamps come from this clock: calls arrive at any pace, with gaps of any length.
    clock: Instant,
    last_pts_ns: u64,
    gpu_frames: u64,
    /// Start of the first of the failed calls in a row.
    failing_since: Option<Instant>,
    /// Separate losses since `loss_window`.
    losses: u32,
    loss_window: Instant,
    software: Option<OpenH264Encoder>,
    software_since: Instant,
    gpu_retry_after: Duration,
    /// QP floor of this stream (the "quality floor" setting when the encoder was created).
    min_qp: u32,
    /// What `set_efficient_profile` asked for last.
    want_main: bool,
    /// Main is not available on this machine: Constrained Baseline only (`main_unavailable`).
    main_failed: bool,
    /// A Main pipeline of this encoder has put out its keyframe.
    main_proven: bool,
    /// The one late first picture a Main pipeline is forgiven has happened.
    main_was_late: bool,
    /// The rate the level written in the SPS allows (see `raise_level`).
    level_bitrate_bps: u32,
    test: TestHook,
}

impl VaapiEncoder {
    /// Fails unless this machine really encodes Constrained Baseline through VA-API: the
    /// caller can only choose another encoder now, not once the stream runs.
    pub fn try_new(target_fps: u32, is_screen_content: bool) -> Result<Self, String> {
        let test = TestHook::from_env();
        if test.fail == TestFail::Init {
            return Err("falha simulada (LITECORD_VAAPI_TEST_FAIL=init)".into());
        }
        gst::init().map_err(|e| format!("GStreamer: {e}"))?;
        if gst::ElementFactory::find("vapostproc").is_none() {
            return Err("plugin VA-API do GStreamer ausente (gst-plugin-va)".into());
        }
        let min_qp = crate::video_settings::get_stream_min_qp();
        let mut last_error = String::from("nenhum codificador H.264 VA-API registrado");
        for element in ENCODER_ELEMENTS {
            if gst::ElementFactory::find(element).is_none() {
                continue;
            }
            match Self::probe(element, target_fps, min_qp) {
                Ok(took) => {
                    info!("🚀 [VAAPI] {} respondeu ao teste em {:.0} ms (Constrained Baseline)", element, took.as_secs_f64() * 1000.0);
                    return Ok(Self {
                        element,
                        target_fps,
                        is_screen_content,
                        bitrate_bps: INITIAL_BITRATE_BPS,
                        bitrate_asked_at: Instant::now(),
                        needs_keyframe: false,
                        lost_picture: false,
                        session: None,
                        clock: Instant::now(),
                        last_pts_ns: 0,
                        gpu_frames: 0,
                        failing_since: None,
                        losses: 0,
                        loss_window: Instant::now(),
                        software: None,
                        software_since: Instant::now(),
                        gpu_retry_after: GPU_RETRY_AFTER,
                        min_qp,
                        want_main: false,
                        main_failed: false,
                        main_proven: false,
                        main_was_late: false,
                        level_bitrate_bps: 0,
                        test,
                    });
                }
                Err(e) => last_error = format!("{element}: {e}"),
            }
        }
        Err(last_error)
    }

    /// Encodes one black frame with the stream's own pipeline and checks what comes out.
    fn probe(element: &str, fps: u32, min_qp: u32) -> Result<Duration, String> {
        let started = Instant::now();
        let (w, h) = PROBE_SIZE;
        let session = Session::start(element, w, h, fps, INITIAL_BITRATE_BPS, min_qp, false)?;
        let frame = vec![0u8; (w * h * 4) as usize];
        session.source.push(&frame, w as usize, h as usize, (w * 4) as usize, gst::ClockTime::ZERO)?;
        let au = match session.chain.pull(gst::ClockTime::ZERO, PROBE_TIMEOUT) {
            Ok(au) => au,
            Err(PullError::Timeout) => return Err("sem resposta do codificador".into()),
            Err(PullError::Broken(e)) => return Err(e),
        };
        check_keyframe(&au, false)?;
        Ok(started.elapsed())
    }

    fn next_pts(&mut self) -> gst::ClockTime {
        let ns = (self.clock.elapsed().as_nanos() as u64).max(self.last_pts_ns + 1);
        self.last_pts_ns = ns;
        gst::ClockTime::from_nseconds(ns)
    }

    fn encode_on_gpu(&mut self, bgra: &[u8], w: u32, h: u32, stride: usize) -> Result<Vec<u8>, String> {
        let stall = match self.test.next(self.gpu_frames) {
            TestFail::After => return Err("falha simulada (LITECORD_VAAPI_TEST_FAIL=after)".into()),
            fail => fail == TestFail::Stall,
        };
        // Coming back from OpenH264, only with a profile this GPU has already produced.
        let main = self.want_main && !self.main_failed && (self.main_proven || self.software.is_none());
        // vah264enc does not change its profile while it runs (asked to, the pipeline stops
        // with an error): the pipeline is replaced, which starts with a keyframe. To Main
        // only when the caller asked for a keyframe anyway; back to Constrained Baseline at
        // once, since a viewer that cannot decode Main is watching.
        let switch_profile = self.session.as_ref().is_some_and(|s| s.main != main && (self.needs_keyframe || !main));
        if switch_profile {
            info!("🔀 [VAAPI] Perfil H.264 da transmissão muda para {}", profile_name(main));
        }
        if switch_profile || self.session.as_ref().map(|s| (s.width, s.height) != (w, h)).unwrap_or(true) {
            // The old pipeline is stopped before the new one starts.
            self.session = None;
            let started = if main && self.test.fail == TestFail::Main {
                Err("falha simulada (LITECORD_VAAPI_TEST_FAIL=main)".to_string())
            } else {
                Session::start(self.element, w, h, self.target_fps, self.bitrate_bps, self.min_qp, main)
            };
            let session = match started {
                Ok(session) => session,
                Err(e) => {
                    self.main_unavailable(main, false);
                    return Err(e);
                }
            };
            info!(
                "🚀 [VAAPI] Pipeline {} iniciado: {}x{} {} FPS, {:.1} Mbps, perfil {}, min-qp {}",
                self.element,
                w,
                h,
                self.target_fps,
                self.bitrate_bps as f64 / 1e6,
                profile_name(main),
                self.min_qp
            );
            self.session = Some(session);
            // A new pipeline starts with an IDR by itself.
            self.needs_keyframe = false;
        }
        let pts = self.next_pts();
        let Some(session) = self.session.as_mut() else { return Err("sem pipeline".into()) };
        let first = session.frames == 0;
        let first_of_main = first && session.main;
        // A first picture that came late was thrown away: the keyframe after it is checked.
        let check_main = session.main && (first || !self.main_proven);

        let rate_due = session.bitrate_bps != self.bitrate_bps && (self.needs_keyframe || self.bitrate_asked_at.elapsed() >= BITRATE_MAX_WAIT);
        if rate_due {
            session.chain.set_bitrate(self.bitrate_bps);
            session.bitrate_bps = self.bitrate_bps;
        }
        let keyframe_expected = first || rate_due || (self.needs_keyframe && session.chain.request_keyframe(pts));

        if !stall {
            if let Err(e) = session.source.push(bgra, w as usize, h as usize, stride, pts) {
                self.session = None;
                self.main_unavailable(first_of_main, false);
                return Err(e);
            }
        }
        let timeout = if first { FIRST_FRAME_TIMEOUT } else { FRAME_TIMEOUT };
        let au = match session.chain.pull(pts, timeout) {
            Ok(au) => au,
            Err(PullError::Timeout) => {
                // The encoder may still encode this frame, which no viewer will get: the
                // next one must not reference it.
                self.needs_keyframe = true;
                self.lost_picture = true;
                self.main_unavailable(first_of_main, true);
                return Err(format!("sem resposta em {} ms", timeout.as_millis()));
            }
            Err(PullError::Broken(e)) => {
                self.session = None;
                self.main_unavailable(first_of_main, false);
                return Err(e);
            }
        };
        session.frames += 1;
        self.gpu_frames += 1;

        let au = without_filler(au);
        if !matches!(au.as_slice(), [0, 0, 1, ..] | [0, 0, 0, 1, ..]) {
            self.needs_keyframe = true;
            self.lost_picture = true;
            return Err("saída sem start code".into());
        }
        if !nal_ranges(&au).iter().any(|r| au[r.start] & 0x1f == 5) {
            if first || self.lost_picture {
                // Viewers do not have the picture this one was predicted from.
                self.needs_keyframe = true;
                self.lost_picture = true;
                return Err("quadro P no lugar do keyframe".into());
            }
            if keyframe_expected {
                warn!("⚠️ [VAAPI] Keyframe pedido, mas o codificador devolveu um quadro P; pedindo de novo");
                self.needs_keyframe = true;
            }
            return Ok(au);
        }
        if check_main {
            if let Err(e) = check_keyframe(&au, true) {
                // Not the Main stream the viewers said they decode.
                self.session = None;
                self.needs_keyframe = true;
                self.main_unavailable(true, false);
                return Err(e);
            }
            self.main_proven = true;
        }
        self.needs_keyframe = false;
        self.lost_picture = false;
        // The highest rate of the stream so far, so the level does not follow every change.
        self.level_bitrate_bps = self.level_bitrate_bps.max(session.bitrate_bps);
        let mut au = super::h264_vui::declare_colour(&au, 1, 1, 6);
        raise_level(&mut au, self.level_bitrate_bps);
        Ok(au)
    }

    /// A Main pipeline gave no first picture (`failed`). Unless Main has worked on this
    /// encoder before (then the GPU is in trouble, like with any lost frame), the stream
    /// stays on Constrained Baseline, which the construction test proved. A picture that
    /// is only `late` is forgiven once: a busy GPU does that to any new pipeline.
    fn main_unavailable(&mut self, failed: bool, late: bool) {
        if !failed || self.main_failed || self.main_proven || (late && !std::mem::replace(&mut self.main_was_late, true)) {
            return;
        }
        self.main_failed = true;
        warn!("⚠️ [VAAPI] Perfil Main indisponível neste codificador; a transmissão segue em Constrained Baseline");
    }

    /// Drops the pipeline without waiting for it to stop: it is being given up on.
    fn abandon_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.chain.teardown_wait = Duration::ZERO;
        }
    }

    fn switch_to_software(&mut self, reason: &str) {
        warn!(
            "⚠️ [VAAPI] GPU sem entregar quadros ({}); a transmissão continua com OpenH264 (CPU), nova tentativa na GPU em {} s",
            reason,
            self.gpu_retry_after.as_secs()
        );
        self.abandon_session();
        self.failing_since = None;
        self.losses = 0;
        match OpenH264Encoder::new(self.target_fps, self.is_screen_content) {
            Ok(mut enc) => {
                // Its first picture is a keyframe.
                enc.force_intra_frame();
                self.software = Some(enc);
                self.software_since = Instant::now();
            }
            Err(e) => warn!("⚠️ [VAAPI] OpenH264 também falhou ({}); tentando a GPU de novo", e),
        }
    }

    /// On OpenH264, with a keyframe due: one frame through a new GPU pipeline, which starts
    /// with a keyframe too. The stream goes back to the GPU when it answers.
    fn retry_gpu(&mut self, bgra: &[u8], w: u32, h: u32, stride: usize) -> Option<Vec<u8>> {
        self.software_since = Instant::now();
        self.gpu_retry_after = (self.gpu_retry_after * 2).min(GPU_RETRY_MAX);
        match self.encode_on_gpu(bgra, w, h, stride) {
            Ok(au) => {
                info!("🚀 [VAAPI] A GPU voltou a responder; a transmissão volta para o VA-API");
                self.software = None;
                Some(au)
            }
            Err(e) => {
                warn!("⚠️ [VAAPI] GPU ainda indisponível ({}); nova tentativa em {} s", e, self.gpu_retry_after.as_secs());
                self.abandon_session();
                self.lost_picture = false;
                None
            }
        }
    }
}

pub(crate) fn profile_name(main: bool) -> &'static str {
    if main {
        "Main"
    } else {
        "Constrained Baseline"
    }
}

/// An access unit that opens a stream the receivers accept: Annex B, IDR with SPS and PPS,
/// Constrained Baseline (or Main, when that was asked for).
pub(crate) fn check_keyframe(au: &[u8], main: bool) -> Result<(), String> {
    if !matches!(au, [0, 0, 1, ..] | [0, 0, 0, 1, ..]) {
        return Err("saída sem start code".into());
    }
    let ranges = nal_ranges(au);
    let has = |t: u8| ranges.iter().any(|r| au[r.start] & 0x1f == t);
    if !(has(7) && has(8) && has(5)) {
        return Err("primeiro quadro sem SPS/PPS/IDR".into());
    }
    let sps = ranges.iter().map(|r| &au[r.clone()]).find(|n| n[0] & 0x1f == 7).filter(|n| n.len() >= 3).ok_or("SPS curto")?;
    // profile_idc 66 with constraint_set1_flag, or 77.
    if if main { sps[1] != 77 } else { sps[1] != 66 || sps[2] & 0x40 == 0 } {
        return Err(format!("perfil H.264 inesperado (profile_idc {}, flags {:#04x})", sps[1], sps[2]));
    }
    Ok(())
}

impl VideoEncoder for VaapiEncoder {
    fn encode(&mut self, bgra_data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
        let (w, h) = (width & !1, height & !1);
        let stride = width as usize * 4;
        if w == 0 || h == 0 || bgra_data.len() < stride * (h as usize - 1) + w as usize * 4 {
            return None;
        }
        if self.software.is_some() {
            let keyframe_due = std::mem::take(&mut self.needs_keyframe);
            if keyframe_due && self.software_since.elapsed() >= self.gpu_retry_after {
                if let Some(au) = self.retry_gpu(bgra_data, w, h, stride) {
                    return Some(au);
                }
            }
            let software = self.software.as_mut()?;
            // It ignores changes in its first 15 s: asked on every frame, like the send
            // loop does with a plain OpenH264 encoder.
            software.set_bitrate_bps(self.bitrate_bps);
            return software.encode(bgra_data, width, height);
        }
        let started = Instant::now();
        match self.encode_on_gpu(bgra_data, w, h, stride) {
            Ok(au) => {
                self.failing_since = None;
                Some(au)
            }
            Err(e) => {
                let since = match self.failing_since {
                    Some(since) => since,
                    None => {
                        if self.loss_window.elapsed() >= LOSS_WINDOW {
                            self.loss_window = started;
                            self.losses = 0;
                        }
                        self.losses += 1;
                        self.failing_since = Some(started);
                        warn!("⚠️ [VAAPI] Quadro perdido na GPU: {}", e);
                        started
                    }
                };
                if since.elapsed() < STALL_BUDGET && self.losses <= MAX_LOSSES {
                    return None;
                }
                self.switch_to_software(&e);
                self.software.as_mut()?.encode(bgra_data, width, height)
            }
        }
    }

    fn force_intra_frame(&mut self) {
        self.needs_keyframe = true;
        if let Some(software) = self.software.as_mut() {
            software.force_intra_frame();
        }
    }

    fn set_bitrate_bps(&mut self, bitrate_bps: u32) {
        if bitrate_bps != self.bitrate_bps {
            self.bitrate_bps = bitrate_bps;
            self.bitrate_asked_at = Instant::now();
        }
    }

    fn get_bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    fn set_efficient_profile(&mut self, on: bool) {
        self.want_main = on;
    }

    fn name(&self) -> &'static str {
        if self.software.is_some() {
            "OpenH264 (CPU, após falha do VA-API)"
        } else {
            "VA-API H.264 (GPU)"
        }
    }

    fn is_hardware_accelerated(&self) -> bool {
        self.software.is_none()
    }
}
