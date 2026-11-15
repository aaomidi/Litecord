//! Linux backend: H.264 decoding on the GPU through GStreamer's VA-API elements (AMD, Intel).
//!
//!   access units -> vah264dec -> vapostproc (scaling and RGB, on the GPU) -> RGBx in memory
//!                                                                      or -> RGB DMABuf
//!
//! The DMABuf form is used while the windows can draw it as it is (`gl_present`, GPU
//! renderer on Wayland): the picture never leaves the GPU. One pipeline produces one form;
//! when the windows stop taking DMABufs the stream waits for its next keyframe and a new
//! pipeline returns pixels.
//!
//! One access unit goes in and its picture comes out of the same call. What that took, each
//! found by running the elements against the streams Litecord senders make (radeonsi,
//! GStreamer 1.28):
//! - vah264dec keeps a picture until the next one arrives whenever the stream's picture
//!   buffer is as small as its reference count (OpenH264, x264 and ffmpeg's h264_vaapi all
//!   write that), whatever its `compliance` says: the early output is skipped while the
//!   buffer is full. Each SPS is therefore given to the decoder with its VUI replaced by one that
//!   declares no reordering and a buffer two frames larger than the references
//!   (`without_reordering`): one frame larger is enough until a picture that is not a
//!   reference arrives (OpenH264 with temporal layers), which then fills the buffer again.
//!   No reordering is true of every stream without B pictures; a stream with B pictures
//!   goes to the CPU decoder instead of being shown late.
//! - The early output also needs a live source or `compliance=normal`. Both are set.
//! - The decoder takes its colours from the caps, not from the SPS, and vapostproc assumes
//!   BT.709 for HD. The input caps pin BT.601 (or BT.709 when the stream declares it, as on
//!   Windows) limited range, with the same transfer function and primaries as the RGB side:
//!   named differently, radeonsi re-curves the picture.
//! - vapostproc places chroma samples by picture height unless told (centred up to 576
//!   lines, half a pixel to the left above). The decoder's output is labelled centred at
//!   every size, which is where the CPU decoder puts them.
//! - Read back as "RGBA" the bytes arrive as A,B,G,R on radeonsi; "RGBx" is right, with a
//!   zero fourth byte. A self-test decodes a small picture once per run and checks its size
//!   and colours, so a driver that answers differently sends every stream to the CPU.
//! - vah264dec conceals missing references and shows the smear; lost pictures are noticed
//!   here (`H264State`, as on Windows) and nothing is decoded until the next IDR.
//! - A picture that does not come back in time is forgiven once: the pipeline is rebuilt at
//!   the next keyframe. A second one within `STALL_MEMORY` hands the stream to the CPU.

use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt;
use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::gl_present;
use super::h264_state::{nal_ranges, H264State, Parsed, StateError, StreamInfo};
use super::HwError;

/// A new pipeline creates the VA context and compiles the conversion on its first picture.
const FIRST_PICTURE_TIMEOUT: Duration = Duration::from_millis(1500);
/// A picture takes a few milliseconds, tens on a busy machine; one that takes this long
/// means the GPU path is stuck.
const PICTURE_TIMEOUT: Duration = Duration::from_millis(400);
/// One late picture (a busy GPU, a scheduling hiccup) restarts the pipeline at the next
/// keyframe; another one this soon after it means the GPU path cannot keep up here.
const STALL_MEMORY: Duration = Duration::from_secs(300);
/// The rewritten SPS needs a picture buffer two frames larger than the references, and 16
/// is the largest H.264 allows: streams with more references go to the CPU decoder.
const MAX_REF_FRAMES: u32 = 14;
/// Parameter sets kept for the next picture (they are a few dozen bytes each).
const MAX_PENDING_PARAMS: usize = 16 * 1024;

/// `LITECORD_VAAPI_DECODE=0` keeps every received stream on the CPU decoder.
fn turned_off() -> bool {
    std::env::var("LITECORD_VAAPI_DECODE").map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")).unwrap_or(false)
}

/// Test hook: `LITECORD_VAAPI_DEC_TEST_FAIL=init` fails construction, `after:N` makes every
/// picture from the Nth on fail at once, `stall:N` makes them time out for real and
/// `stall-once:N` only the Nth.
#[derive(Clone, Copy, PartialEq)]
enum TestFail {
    None,
    Init,
    After(u64),
    Stall(u64),
    StallOnce(u64),
}

fn test_fail() -> TestFail {
    let Ok(v) = std::env::var("LITECORD_VAAPI_DEC_TEST_FAIL") else { return TestFail::None };
    let n = |s: &str| s.parse::<u64>().unwrap_or(0);
    match v.split_once(':') {
        Some(("after", s)) => TestFail::After(n(s)),
        Some(("stall", s)) => TestFail::Stall(n(s)),
        Some(("stall-once", s)) => TestFail::StallOnce(n(s)),
        _ if v == "init" => TestFail::Init,
        _ => TestFail::None,
    }
}

impl From<StateError> for HwError {
    fn from(e: StateError) -> Self {
        match e {
            StateError::Unsupported(s) => HwError::Unsupported(s),
            StateError::NeedKeyframe(s) => HwError::NeedKeyframe(s),
        }
    }
}

/// What a decoded picture comes back as, at the size asked for.
pub enum Output {
    /// Pixels in memory.
    Rgba(SharedPixelBuffer<Rgba8Pixel>),
    /// The GPU's own buffer (a `video/x-raw(memory:DMABuf)` sample, RGB).
    Dmabuf(gst::Sample),
}

/// A decoded picture, and the stream's own width and height.
pub struct Picture {
    pub output: Output,
    pub width: u32,
    pub height: u32,
}

pub struct VaapiDecoder {
    state: H264State,
    session: Option<Session>,
    /// The last SPS seen and what the decoder is given in its place.
    sps: Option<(Vec<u8>, Vec<u8>)>,
    /// Parameter sets that arrived without a picture: they go in front of the next one.
    pending_params: Vec<u8>,
    pictures: u64,
    /// When a picture last failed to come back in time.
    last_stall: Option<Instant>,
    test_fail: TestFail,
    /// The self-test's decoder: nothing it does is announced in the log.
    probe: bool,
}

impl VaapiDecoder {
    /// Fails (`Unsupported`) unless this machine decodes H.264 through VA-API and shows it
    /// with the right colours; checked once per run.
    pub fn new() -> Result<Self, HwError> {
        if turned_off() {
            return Err(HwError::Unsupported("turned off by LITECORD_VAAPI_DECODE=0".into()));
        }
        let test_fail = test_fail();
        if test_fail == TestFail::Init {
            return Err(HwError::Unsupported("simulated failure (LITECORD_VAAPI_DEC_TEST_FAIL=init)".into()));
        }
        static MACHINE: OnceLock<Result<(), String>> = OnceLock::new();
        MACHINE.get_or_init(self_test).clone().map_err(HwError::Unsupported)?;
        Ok(Self::with(test_fail, false))
    }

    fn with(test_fail: TestFail, probe: bool) -> Self {
        Self { state: H264State::new(), session: None, sps: None, pending_params: Vec::new(), pictures: 0, last_stall: None, test_fail, probe }
    }

    /// Decodes one access unit (Annex B) and returns its picture at the size `fit(width,
    /// height)` chooses. `None` when it holds no picture (parameter sets only).
    pub fn decode(&mut self, au: &[u8], fit: impl Fn(u32, u32) -> (u32, u32)) -> Result<Option<Picture>, HwError> {
        let ranges = nal_ranges(au);
        let Some(p) = self.state.parse(au)? else {
            self.append_units(au, &ranges, false)?;
            return Ok(None);
        };
        let info = H264State::stream_info(&p)?;
        if p.sps.max_num_ref_frames > MAX_REF_FRAMES {
            return Err(HwError::Unsupported(format!("{} reference frames (at most {MAX_REF_FRAMES} without delay)", p.sps.max_num_ref_frames)));
        }
        check_slice_types(au, &ranges)?;
        // A pipeline that makes DMABufs is of no use once the windows stop taking them.
        let usable = |s: &Session| s.info == info && (s.drm_format.is_none() || gl_present::ready());
        if !self.session.as_ref().map(usable).unwrap_or(false) {
            if !p.idr {
                return Err(HwError::NeedKeyframe("waiting for a keyframe".into()));
            }
            // Dropped first: the old pipeline stops in the background while the new one starts.
            self.session = None;
            let profile = caps_profile(&p);
            let session = Session::start(&info, profile, !self.probe && gl_present::ready()).map_err(HwError::Device)?;
            self.state.reset(&info);
            if !self.probe {
                let matrix = if info.bt709 { "BT.709" } else { "BT.601" };
                let output = match &session.drm_format {
                    Some(drm) => format!("pictures stay on the GPU ({drm} DMABuf)"),
                    None => "pictures are read back as pixels".to_string(),
                };
                log::info!("🎞️ [VAAPI DEC] vah264dec pipeline started: {}x{} {}, {} limited range; {}", info.width, info.height, profile, matrix, output);
            }
            self.session = Some(session);
        }
        let (pic, _) = self.state.begin(&p)?;
        self.append_units(au, &ranges, true)?;
        let feed = std::mem::take(&mut self.pending_params);

        let stall = match self.test_fail {
            TestFail::After(n) if self.pictures >= n => {
                self.fail();
                return Err(HwError::Device("simulated failure (LITECORD_VAAPI_DEC_TEST_FAIL=after)".into()));
            }
            TestFail::Stall(n) => self.pictures >= n,
            TestFail::StallOnce(n) if self.pictures == n => {
                self.test_fail = TestFail::None;
                true
            }
            _ => false,
        };
        let (out_w, out_h) = output_size(fit(info.width, info.height), &info);
        let Some(session) = self.session.as_mut() else { return Err(HwError::Device("no pipeline".into())) };
        // What went wrong while pictures stay on the GPU may be that very hand-over (a
        // format the driver cannot make after all, buffers the windows hold on to): such
        // a stream first goes back to pixels, on this decoder, before it is given up on.
        let (dmabuf, first) = (session.drm_format.is_some(), session.pictures == 0);
        match session.decode(feed, out_w, out_h, stall) {
            Ok(output) => {
                self.state.finish(&p, &pic);
                self.pictures += 1;
                Ok(Some(Picture { output, width: info.width, height: info.height }))
            }
            Err(Failure::Stalled(e)) => {
                self.fail();
                let again = self.last_stall.replace(Instant::now()).map(|t| t.elapsed() < STALL_MEMORY).unwrap_or(false);
                if again && dmabuf {
                    self.last_stall = None;
                    gl_present::give_up(&format!("the decoder stalled twice while its pictures stayed on the GPU ({e})"));
                    return Err(HwError::NeedKeyframe(e));
                }
                if again {
                    return Err(HwError::Device(format!("{e}, twice within {} s", STALL_MEMORY.as_secs())));
                }
                if !self.probe {
                    log::warn!("⚠️ [VAAPI DEC] {e}: restarting the pipeline at the next keyframe (once more within {} s and this stream moves to the CPU decoder)", STALL_MEMORY.as_secs());
                }
                Err(HwError::NeedKeyframe(e))
            }
            Err(Failure::Broken(e)) => {
                self.fail();
                if dmabuf && first {
                    gl_present::give_up(&format!("the decoder could not leave its pictures on the GPU ({e})"));
                    return Err(HwError::NeedKeyframe(e));
                }
                Err(HwError::Device(e))
            }
        }
    }

    /// After a failed picture nothing in the pipeline can be trusted: the next picture
    /// shown (by whichever decoder) starts from a keyframe.
    fn fail(&mut self) {
        self.state.need_keyframe();
        self.session = None;
    }

    /// Queues what the decoder needs of the access unit: parameter sets (each SPS in its
    /// rewritten form) and, with `slices`, the picture. SEI, delimiters and filler stay out.
    fn append_units(&mut self, au: &[u8], ranges: &[Range<usize>], slices: bool) -> Result<(), HwError> {
        for r in ranges {
            let nal = &au[r.clone()];
            match nal[0] & 0x1f {
                7 => {
                    if self.sps.as_ref().map(|(seen, _)| seen.as_slice() != nal).unwrap_or(true) {
                        let rewritten = without_reordering(nal).ok_or_else(|| HwError::Unsupported("SPS could not be rewritten".into()))?;
                        self.sps = Some((nal.to_vec(), rewritten));
                    }
                    if let Some((_, rewritten)) = &self.sps {
                        self.pending_params.extend_from_slice(&[0, 0, 0, 1]);
                        self.pending_params.extend_from_slice(rewritten);
                    }
                }
                8 => {
                    self.pending_params.extend_from_slice(&[0, 0, 0, 1]);
                    self.pending_params.extend_from_slice(nal);
                }
                1 | 5 if slices => {
                    self.pending_params.extend_from_slice(&[0, 0, 0, 1]);
                    self.pending_params.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if !slices && self.pending_params.len() > MAX_PENDING_PARAMS {
            // Parameter sets and never a picture: keep the newest ones only.
            self.pending_params.clear();
        }
        Ok(())
    }
}

/// The profile named in the caps (the decoder only accepts caps that name one it has).
fn caps_profile(p: &Parsed) -> &'static str {
    match u8::from(p.sps.profile_idc) {
        66 if p.sps.constraint_flags.flag1() => "constrained-baseline",
        66 => "baseline",
        77 => "main",
        88 => "extended",
        _ => "high",
    }
}

/// B pictures are shown after pictures decoded later; SP/SI slices are not decoded by
/// VA-API. Either sends the stream to the CPU decoder.
fn check_slice_types(au: &[u8], ranges: &[Range<usize>]) -> Result<(), HwError> {
    for r in ranges {
        let nal = &au[r.clone()];
        if !matches!(nal[0] & 0x1f, 1 | 5) {
            continue;
        }
        let head = unescape(&nal[1..nal.len().min(17)]);
        let mut bits = BitReader::new(&head);
        // first_mb_in_slice, slice_type
        let slice_type = bits.ue().and_then(|_| bits.ue()).ok_or_else(|| HwError::NeedKeyframe("slice header cut short".into()))?;
        match slice_type % 5 {
            1 => return Err(HwError::Unsupported("B pictures (shown out of decoding order)".into())),
            3 | 4 => return Err(HwError::Unsupported("SP/SI slices".into())),
            _ => {}
        }
    }
    Ok(())
}

/// The size vapostproc is asked for: what the caller wants, never larger than the picture.
fn output_size((w, h): (u32, u32), info: &StreamInfo) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (info.width, info.height);
    }
    (w.min(info.width).max(2), h.min(info.height).max(2))
}

/// Why a picture did not come back.
enum Failure {
    /// Nothing arrived in time, and the pipeline reported no error.
    Stalled(String),
    /// The pipeline reported an error, or returned something unusable.
    Broken(String),
}

/// One running pipeline, for one stream format.
struct Session {
    info: StreamInfo,
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    appsrc: gst_app::AppSrc,
    size_filter: gst::Element,
    sink: gst_app::AppSink,
    /// The DMABuf format pictures come out in (with its modifier, as vapostproc names
    /// it), or `None` for pixels in memory.
    drm_format: Option<String>,
    out_size: (u32, u32),
    pushed: u64,
    pictures: u64,
    /// Since the last report: time spent waiting for pictures (decoding and conversion on
    /// the GPU), mapping them (the driver's copy out of the GPU) and copying them into the
    /// buffers the UI gets, and how many were read back at all.
    waited: Duration,
    mapped: Duration,
    copied: Duration,
    read_back: u64,
}

/// Pictures between two timing reports in the log (ten seconds at 60 FPS).
const REPORT_EVERY: u64 = 600;

impl Session {
    /// `dmabuf`: leave the pictures on the GPU, if vapostproc has an RGB DMABuf format.
    fn start(info: &StreamInfo, profile: &str, dmabuf: bool) -> Result<Self, String> {
        // Range, matrix, transfer, primaries. Limited range always (see h264_state); the
        // transfer function and primaries are sRGB's on both sides, so vapostproc applies
        // the matrix and nothing else. Chroma samples sit in the middle of their four
        // pixels ("jpeg"): vah264dec 1.28 says nothing about them and ignores what its input
        // caps say, so the filter after it adds that to its output.
        let colorimetry = if info.bt709 { "2:3:7:1" } else { "2:4:7:1" };
        let description = format!(
            "appsrc name=src is-live=true format=time block=false stream-type=stream \
             caps=\"video/x-h264,stream-format=byte-stream,alignment=au,profile={profile},width={},height={},colorimetry=(string){colorimetry},chroma-site=(string)jpeg\" \
             ! vah264dec name=dec qos=false \
             ! capsfilter caps=\"video/x-raw(ANY),chroma-site=(string)jpeg\" \
             ! vapostproc name=post qos=false \
             ! capsfilter name=size \
             ! appsink name=sink sync=false async=false max-buffers=4 drop=false emit-signals=false enable-last-sample=false",
            info.width, info.height
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        let bus = pipeline.bus().ok_or("pipeline without bus")?;
        let appsrc = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or("appsrc missing")?;
        let size_filter = pipeline.by_name("size").ok_or("capsfilter missing")?;
        let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("appsink missing")?;
        if let Some(dec) = pipeline.by_name("dec") {
            // set_property panics on a missing property (GStreamer before 1.20 has none).
            if dec.find_property("compliance").is_some() {
                dec.set_property_from_str("compliance", "normal");
            }
        }
        let drm_format = if dmabuf { rgb_dmabuf_format(&pipeline) } else { None };
        if drm_format.is_some() {
            // vapostproc only hands out DMABufs to a sink that says it reads VideoMeta
            // (strides and offsets); appsink says nothing by itself. Not installed for
            // pixels, where the pipeline is what the self-test checked.
            if let Some(pad) = sink.static_pad("sink") {
                pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_, probe| {
                    if let Some(query) = probe.query_mut() {
                        if let gst::QueryViewMut::Allocation(allocation) = query.view_mut() {
                            allocation.add_allocation_meta::<gst_video::VideoMeta>(None);
                            return gst::PadProbeReturn::Handled;
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        // From here on dropping the session stops the pipeline.
        let mut session = Self { info: *info, pipeline, bus, appsrc, size_filter, sink, drm_format, out_size: (0, 0), pushed: 0, pictures: 0, waited: Duration::ZERO, mapped: Duration::ZERO, copied: Duration::ZERO, read_back: 0 };
        session.set_output_size(info.width, info.height);
        if let Err(e) = session.pipeline.set_state(gst::State::Playing) {
            return Err(format!("{e}: {}", session.bus_error().unwrap_or_default()));
        }
        Ok(session)
    }

    /// Takes effect on the next picture (vapostproc renegotiates; the decoder is untouched).
    fn set_output_size(&mut self, w: u32, h: u32) {
        let caps = match &self.drm_format {
            None => gst::Caps::builder("video/x-raw")
                .field("format", "RGBx")
                .field("width", w as i32)
                .field("height", h as i32)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .field("colorimetry", "1:1:7:1")
                .build(),
            Some(drm) => gst::Caps::builder("video/x-raw")
                .features(["memory:DMABuf"])
                .field("format", "DMA_DRM")
                .field("drm-format", drm.as_str())
                .field("width", w as i32)
                .field("height", h as i32)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .field("colorimetry", "1:1:7:1")
                .build(),
        };
        self.size_filter.set_property("caps", &caps);
        self.out_size = (w, h);
    }

    /// The first error on the bus; everything else waiting there is thrown away.
    fn bus_error(&self) -> Option<String> {
        while let Some(msg) = self.bus.pop() {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({})", e.error(), e.debug().map(|d| d.to_string()).unwrap_or_default()));
            }
        }
        None
    }

    /// Pushes one access unit and waits for its picture. `stall` leaves the push out (test).
    fn decode(&mut self, feed: Vec<u8>, w: u32, h: u32, stall: bool) -> Result<Output, Failure> {
        if let Some(e) = self.bus_error() {
            return Err(Failure::Broken(e));
        }
        if (w, h) != self.out_size {
            self.set_output_size(w, h);
        }
        // A live source waits for the clock to reach each timestamp: these stay far behind it.
        let pts = gst::ClockTime::from_useconds(self.pushed);
        self.pushed += 1;
        let mut buffer = gst::Buffer::from_mut_slice(feed);
        buffer.get_mut().ok_or_else(|| Failure::Broken("access unit buffer is shared".into()))?.set_pts(pts);
        if !stall {
            self.appsrc.push_buffer(buffer).map_err(|e| Failure::Broken(format!("appsrc: {e:?}")))?;
        }
        let timeout = if self.pictures == 0 { FIRST_PICTURE_TIMEOUT } else { PICTURE_TIMEOUT };
        let pushed_at = Instant::now();
        let sample = self.pull(pts, timeout)?;
        let pulled_at = Instant::now();
        self.waited += pulled_at - pushed_at;
        let output = if self.drm_format.is_some() {
            Output::Dmabuf(sample)
        } else {
            let (rgba, mapped) = copy_rgbx(&sample).map_err(Failure::Broken)?;
            self.mapped += mapped;
            self.copied += pulled_at.elapsed().saturating_sub(mapped);
            self.read_back += 1;
            Output::Rgba(rgba)
        };
        self.pictures += 1;
        if self.pictures % REPORT_EVERY == 0 {
            let ms = |d: Duration, n: u64| d.as_secs_f64() * 1000.0 / n.max(1) as f64;
            log::info!(
                "🎞️ [VAAPI DEC] {}x{} -> {}x{}, per picture: {:.2} ms decoding and converting on the GPU, {:.2} ms mapping it into memory, {:.2} ms copying it out ({} of {} read back)",
                self.info.width,
                self.info.height,
                w,
                h,
                ms(self.waited, REPORT_EVERY),
                ms(self.mapped, self.read_back),
                ms(self.copied, self.read_back),
                self.read_back,
                REPORT_EVERY
            );
            self.waited = Duration::ZERO;
            self.mapped = Duration::ZERO;
            self.copied = Duration::ZERO;
            self.read_back = 0;
        }
        Ok(output)
    }

    /// Waits for the picture stamped `pts`. A picture of an earlier access unit is thrown
    /// away: nothing is ever shown a call late.
    fn pull(&self, pts: gst::ClockTime, timeout: Duration) -> Result<gst::Sample, Failure> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            // Short waits, so a pipeline error is noticed before the whole timeout passes.
            let wait = left.min(Duration::from_millis(20));
            match self.sink.try_pull_sample(gst::ClockTime::from_nseconds(wait.as_nanos() as u64)) {
                Some(sample) => {
                    if sample.buffer().and_then(|b| b.pts()) == Some(pts) {
                        return Ok(sample);
                    }
                }
                None => {
                    if let Some(e) = self.bus_error() {
                        return Err(Failure::Broken(e));
                    }
                    if self.sink.is_eos() {
                        return Err(Failure::Broken("end of stream".into()));
                    }
                    if left.is_zero() {
                        return Err(Failure::Stalled(format!("no picture after {} ms", timeout.as_millis())));
                    }
                }
            }
        }
    }
}

/// The RGB DMABuf format vapostproc offers on this driver, with its modifier (for example
/// `XB24:0x0200000028a01f04` on radeonsi). One without alpha is preferred: the picture is
/// opaque whatever the fourth byte holds. `None` turns GPU presentation off.
fn rgb_dmabuf_format(pipeline: &gst::Pipeline) -> Option<String> {
    let template = pipeline.by_name("post")?.static_pad("src")?.pad_template_caps();
    let mut offered = Vec::new();
    for (structure, features) in template.iter_with_features() {
        if !features.contains("memory:DMABuf") {
            continue;
        }
        if let Ok(list) = structure.get::<gst::List>("drm-format") {
            offered.extend(list.iter().filter_map(|v| v.get::<String>().ok()));
        } else if let Ok(one) = structure.get::<String>("drm-format") {
            offered.push(one);
        }
    }
    let found = ["XB24", "XR24", "AB24", "AR24"].iter().find_map(|want| offered.iter().find(|o| o.starts_with(want)).cloned());
    if found.is_none() {
        gl_present::give_up(&format!("vapostproc offers no RGB DMABuf format (it has {offered:?})"));
    }
    found
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stopping waits for the streaming thread; if that one hangs in the driver (the
        // reason this pipeline is being dropped, perhaps), the decode thread must not hang too.
        let pipeline = self.pipeline.clone();
        let stop = move || {
            let _ = pipeline.set_state(gst::State::Null);
        };
        if let Err(e) = std::thread::Builder::new().name("vaapi-dec-teardown".into()).spawn(stop) {
            log::warn!("⚠️ [VAAPI DEC] Could not stop the pipeline in the background: {}", e);
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }
}

/// The sample's picture as RGBA (the fourth byte of RGBx is not alpha), and how long
/// mapping it took.
fn copy_rgbx(sample: &gst::Sample) -> Result<(SharedPixelBuffer<Rgba8Pixel>, Duration), String> {
    let caps = sample.caps().ok_or("picture without caps")?;
    let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
    if info.format() != gst_video::VideoFormat::Rgbx {
        return Err(format!("unexpected picture format {:?}", info.format()));
    }
    let buffer = sample.buffer().ok_or("picture without buffer")?;
    let map_started = Instant::now();
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).map_err(|e| e.to_string())?;
    let mapped = map_started.elapsed();
    let (w, h) = (info.width() as usize, info.height() as usize);
    let stride = frame.plane_stride().first().copied().unwrap_or(0) as usize;
    let data = frame.plane_data(0).map_err(|e| e.to_string())?;
    let row = w * 4;
    if w == 0 || h == 0 || stride < row || data.len() < stride * (h - 1) + row {
        return Err(format!("picture buffer too small ({} bytes for {w}x{h}, stride {stride})", data.len()));
    }
    // Reading the mapped picture is the slow part, and a memcpy reads it fastest: one copy
    // of the whole picture into a buffer that is not cleared first (0.8 ms for 1080p
    // here; clearing, copying row by row and patching each row took 1.9 ms, and writing
    // `pixel | alpha` pixel by pixel 11.6 ms). The alpha byte is set afterwards, in
    // ordinary memory.
    let mut out = if stride == row {
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&data[..row * h], w as u32, h as u32)
    } else {
        let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
        for (dst, src) in out.make_mut_bytes().chunks_exact_mut(row).zip(data.chunks(stride)) {
            dst.copy_from_slice(&src[..row]);
        }
        out
    };
    for px in out.make_mut_bytes().chunks_exact_mut(4) {
        px[3] = 255;
    }
    Ok((out, mapped))
}

// ---------------------------------------------------------------------------- self-test

/// A 320x180 Constrained Baseline keyframe (x264): four flat quarters whose BT.601 limited
/// range colours are `PROBE_COLOURS`. Coded as 320x192, so cropping is exercised too.
const PROBE_AU: &[u8] = b"\x00\x00\x00\x01\x67\x42\xc0\x0c\xda\x05\x06\x7e\x78\x40\x00\x00\x03\x00\x40\x00\x00\x0c\xa3\xc5\x0a\xa8\
\x00\x00\x00\x01\x68\xce\x0b\x2c\x80\
\x00\x00\x00\x01\x65\x88\x84\x3a\x11\x8a\x00\x02\x16\xb1\xc0\x00\x41\x36\x38\x00\x08\x92\x49\xc9\xc9\xc9\xc9\xc9\xc9\xc9\xc9\xc4\
\x62\x80\x00\x88\x14\x70\x00\x10\x47\x8e\x00\x02\x46\xb2\x72\x72\x72\x72\x72\x72\x72\x72\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x4f\x5d\
\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd3\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x74\xf5\xd7\x5d\
\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x3d\x75\xd7\x5d\x75\xd7\x5d\x7f\xfc\x7f\x82\xd8\x1c\x10\x43\x59\x7c\x40\x00\x10\
\x83\x00\x01\x06\x64\x20\x00\x08\x36\x00\x00\x82\x8a\xc0\xe0\x00\x20\x02\x00\x0a\x01\x72\xc3\x80\x00\x80\x08\x00\x28\x05\xcb\xc0\
\xe0\x00\x70\x00\x60\x15\x2c\x38\x00\x1c\x00\x18\x05\x4b\x6a\x5c\x6b\x6b\x6b\x6b\x6b\x6b\x6b\x6b\x87\xf8\x60\x1b\x05\xb0\x3a\x11\
\x59\x7c\x40\x00\x10\x2f\x00\x01\x01\x24\x20\x00\x08\x22\x80\x00\x81\x42\xc0\xe0\x00\x81\x42\x65\x87\x00\x04\x0a\x13\x2f\x03\x80\
\x01\x50\x0a\x09\x96\x1c\x00\x0a\x80\x50\x4c\xb6\xa5\xc6\xb6\xb6\xb6\xb6\xb6\xb6\xb6\xb5\x8a\xd7\x5d\x75\xd7\x5d\x75\xd7\x33\x8a\
\xda\x96\x96\x96\x96\x96\x96\x96\x96\x96\xba\xeb\xae\xba\xeb\xae\xba\xe9\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\
\xba\x7a\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\x9e\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xa7\
\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xeb\xae\xba\xe9\xeb\xae\xba\xeb\xae\xba\xeb\xc0";
/// The picture after it, in which nothing changes (a P picture of skipped macroblocks).
const PROBE_P: &[u8] = b"\x00\x00\x00\x01\x41\x9a\x22\x80\x78\xc0";
const PROBE_SIZE: (u32, u32) = (320, 180);
/// (x, y) and the RGB the CPU decoder shows there.
const PROBE_COLOURS: [((u32, u32), [u8; 3]); 4] = [((80, 45), [255, 0, 0]), ((240, 45), [0, 255, 1]), ((80, 135), [0, 0, 255]), ((240, 135), [128, 128, 128])];
const PROBE_TOLERANCE: i32 = 6;

/// Whether this machine can take the GPU path at all: the elements exist, VA-API starts,
/// and a keyframe and the picture after it each come back from their own call with the
/// size, byte order and colours the CPU decoder would show. Run once; a few tens of
/// milliseconds.
fn self_test() -> Result<(), String> {
    gst::init().map_err(|e| format!("GStreamer: {e}"))?;
    for element in ["vah264dec", "vapostproc", "appsrc", "appsink", "capsfilter"] {
        if gst::ElementFactory::find(element).is_none() {
            return Err(format!("GStreamer element {element} is missing (gst-plugin-va, gst-plugins-base)"));
        }
    }
    let started = Instant::now();
    let mut decoder = VaapiDecoder::with(TestFail::None, true);
    for (name, au) in [("keyframe", PROBE_AU), ("second picture", PROBE_P)] {
        let (picture, rgba) = match decoder.decode(au, |w, h| (w, h)) {
            Ok(Some(Picture { output: Output::Rgba(rgba), width, height })) => ((width, height), rgba),
            Ok(_) => return Err(format!("self-test: no {name}")),
            Err(e) => return Err(format!("self-test, {name}: {e:?}")),
        };
        let (w, h) = (rgba.width(), rgba.height());
        if (w, h) != PROBE_SIZE || picture != PROBE_SIZE {
            return Err(format!("self-test: {name} is {w}x{h}, expected {}x{}", PROBE_SIZE.0, PROBE_SIZE.1));
        }
        let bytes = rgba.as_bytes();
        for ((x, y), expected) in PROBE_COLOURS {
            let at = ((y * w + x) * 4) as usize;
            let got = &bytes[at..at + 4];
            if got[3] != 255 || (0..3).any(|c| (got[c] as i32 - expected[c] as i32).abs() > PROBE_TOLERANCE) {
                return Err(format!("self-test: colour of the {name} at ({x},{y}) is {got:?}, expected {expected:?} (driver converts differently)"));
            }
        }
    }
    log::info!("🎞️ [VAAPI DEC] vah264dec + vapostproc passed the self-test in {:.0} ms: received streams decode on the GPU", started.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

// ---------------------------------------------------------------------------- SPS rewrite

/// The SPS NAL unit (header byte included) with its VUI replaced by one that only says: no
/// picture is shown after a picture decoded later, and the picture buffer holds two frames
/// more than the references (the picture just decoded, reference or not, and one free
/// frame: vah264dec only outputs early while a frame is free). `None` when the SPS cannot
/// be read.
///
/// The colour description, aspect ratio and timing that were there are dropped: the
/// decoder takes colours from the caps, pixels are shown square as the CPU decoder shows
/// them, and nothing here uses timing. SEI units that depend on the dropped fields are not
/// passed on either.
fn without_reordering(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(nal.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?;
        if r.bits(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bits(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let vui_flag_at = r.pos;
    r.bits(1)?;

    let mut w = BitWriter::default();
    let mut src = BitReader::new(&rbsp);
    for _ in 0..vui_flag_at {
        w.bit(src.bits(1)? as u8);
    }
    w.bit(1); // vui_parameters_present_flag
    // No aspect ratio, overscan, video signal type, chroma location, timing, NAL HRD,
    // VCL HRD, pic_struct.
    for _ in 0..8 {
        w.bit(0);
    }
    w.bit(1); // bitstream_restriction_flag
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom: no limit
    w.ue(0); // max_bits_per_mb_denom: no limit
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.clamp(1, MAX_REF_FRAMES) + 2); // max_dec_frame_buffering (16 at most)
    w.bit(1); // RBSP stop bit
    while w.n % 8 != 0 {
        w.bit(0);
    }
    let mut out = vec![nal[0]];
    out.extend_from_slice(&escape(&w.bytes));
    Some(out)
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// RBSP without emulation prevention bytes (00 00 03 -> 00 00).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention: 00 00 followed by 00..03 gets a 03 in between.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -k / 2 })
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    n: usize,
}

impl BitWriter {
    fn bit(&mut self, b: u8) {
        if self.n % 8 == 0 {
            self.bytes.push(0);
        }
        if b != 0 {
            if let Some(last) = self.bytes.last_mut() {
                *last |= 0x80 >> (self.n % 8);
            }
        }
        self.n += 1;
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit((x >> i & 1) as u8);
        }
    }
}
