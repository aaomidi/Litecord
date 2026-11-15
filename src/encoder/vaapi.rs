//! H.264 on the GPU through GStreamer's VA-API elements (Linux: AMD, Intel).
//!
//!   frames (BGRA) -> vapostproc (to NV12, on the GPU) -> vah264enc -> appsink
//!
//! What the receivers need from it, each found by running them against vah264enc:
//! - Constrained Baseline only: with Main/High the encoder's picture order counts make the
//!   OpenH264 receiver show about three pictures per keyframe.
//! - BT.601 limited range (vapostproc picks BT.709 at 1080p by itself), declared in the SPS
//!   by `h264_vui::declare_colour` since vah264enc writes no colour description. The caps
//!   on both sides of vapostproc name the same transfer function (sRGB): asked for BT.709's
//!   on its output, radeonsi re-curves the picture (grey 128 comes back as 135).
//! - One access unit per frame with no delay. vah264enc holds four frames back unless its
//!   source is live, so whatever feeds this pipeline must be a live source.
//!
//! When the GPU path stops answering mid-stream, the encoder switches itself to OpenH264
//! for the rest of the stream: nothing else would replace it.

use super::{OpenH264Encoder, VideoEncoder};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use log::{info, warn};
use std::time::{Duration, Instant};

/// Preferred first; `vah264lpenc` is the only one Intel's low-power encoders register.
pub(crate) const ENCODER_ELEMENTS: [&str; 2] = ["vah264enc", "vah264lpenc"];
/// Streams start at this rate (types::REQUESTED_BITRATE_BPS).
pub(crate) const INITIAL_BITRATE_BPS: u32 = 8_000_000;
/// A new pipeline creates the VA context and compiles the conversion on its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(1000);
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);
/// Failed frames in a row before the stream goes to OpenH264 (viewers drop a stream after
/// 2.5 s without pictures).
const MAX_FAILURES: u32 = 3;
/// vah264enc starts a new sequence (an IDR) whenever its bitrate changes, so a new rate
/// waits for the next keyframe the caller asks for (at least every 2 s), up to this long.
pub(crate) const BITRATE_MAX_WAIT: Duration = Duration::from_millis(3000);
const PROBE_SIZE: (u32, u32) = (640, 360);

/// Test hook: `LITECORD_VAAPI_TEST_FAIL=init` fails construction, `after:N` makes every GPU
/// frame from the Nth on fail at once, `stall:N` makes them time out for real.
#[derive(Clone, Copy, PartialEq)]
enum TestFail {
    None,
    Init,
    After(u64),
    Stall(u64),
}

fn test_fail() -> TestFail {
    let Ok(v) = std::env::var("LITECORD_VAAPI_TEST_FAIL") else { return TestFail::None };
    let n = |s: &str| s.parse::<u64>().unwrap_or(0);
    match v.split_once(':') {
        Some(("after", s)) => TestFail::After(n(s)),
        Some(("stall", s)) => TestFail::Stall(n(s)),
        _ if v == "init" => TestFail::Init,
        _ => TestFail::None,
    }
}

use crate::screen_capture::hw_decode::h264_state::nal_ranges;

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

/// The pipeline from colour conversion to the encoded access units. The bitrate, keyframe
/// requests and the output only touch this part, so the frame source can be replaced.
pub(crate) struct EncodeChain {
    pub(crate) pipeline: gst::Pipeline,
    pub(crate) bus: gst::Bus,
    pub(crate) encoder: gst::Element,
    pub(crate) sink: gst_app::AppSink,
}

enum PullError {
    Timeout,
    Broken(String),
}

impl EncodeChain {
    fn description(element: &str, bitrate_bps: u32) -> String {
        Self::description_scaled(element, bitrate_bps, None)
    }

    /// With `scale_to`, vapostproc also scales to that size (whatever feeds it, stretched
    /// to fill it): the in-process screen capture hands it pictures of the monitor's size.
    pub(crate) fn description_scaled(element: &str, bitrate_bps: u32, scale_to: Option<(u32, u32)>) -> String {
        let size = scale_to.map(|(w, h)| format!(",width={w},height={h},pixel-aspect-ratio=1/1")).unwrap_or_default();
        // Experiments only: vah264enc properties (gst-launch syntax) replacing or joining ours.
        let extra = std::env::var("LITECORD_VAAPI_ENC_PROPS").unwrap_or_default();
        let key = |prop: &str| prop.split('=').next().unwrap_or("").to_string();
        let props: Vec<&str> = ENCODER_PROPS.split_whitespace().filter(|p| !extra.split_whitespace().any(|e| key(e) == key(p))).collect();
        format!(
            "vapostproc ! video/x-raw(memory:VAMemory),format=NV12,colorimetry=(string)2:4:7:1{size} \
             ! {element} name=enc {} bitrate={} cpb-size={} {extra} \
             ! video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false async=false max-buffers=4 drop=false emit-signals=false enable-last-sample=false",
            props.join(" "),
            bitrate_bps / 1000,
            cpb_size_kbit(bitrate_bps)
        )
    }

    pub(crate) fn from_pipeline(pipeline: gst::Pipeline) -> Result<Self, String> {
        let bus = pipeline.bus().ok_or("pipeline without bus")?;
        let encoder = pipeline.by_name("enc").ok_or("encoder element missing")?;
        let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("appsink missing")?;
        Ok(Self { pipeline, bus, encoder, sink })
    }

    /// Takes effect on the next frame, which the encoder turns into an IDR.
    pub(crate) fn set_bitrate(&self, bitrate_bps: u32) {
        for (name, value) in [("bitrate", bitrate_bps / 1000), ("cpb-size", cpb_size_kbit(bitrate_bps))] {
            // set_property panics on a missing property or another type.
            if self.encoder.find_property(name).map(|p| p.value_type() == u32::static_type()).unwrap_or(false) {
                self.encoder.set_property(name, value);
            }
        }
    }

    /// The next frame entering the encoder becomes an IDR with SPS and PPS.
    pub(crate) fn request_keyframe(&self) -> bool {
        self.sink.send_event(gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build())
    }

    pub(crate) fn bus_error(&self) -> Option<String> {
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
        // Stopping waits for the streaming thread; if that one hangs in the driver (the
        // reason this pipeline is being dropped, perhaps), the stream thread must not hang too.
        let pipeline = self.pipeline.clone();
        let stop = move || {
            let _ = pipeline.set_state(gst::State::Null);
        };
        if let Err(e) = std::thread::Builder::new().name("vaapi-teardown".into()).spawn(stop) {
            warn!("⚠️ [VAAPI] Não foi possível encerrar o pipeline em segundo plano: {}", e);
            let _ = self.pipeline.set_state(gst::State::Null);
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
///   only a QP floor holds it back. `min-qp=30` gave 215 kB keyframes where no floor gave
///   300-440 kB (OpenH264: 170-190 kB, at lower quality in both keyframes and P pictures);
///   each 2 steps of QP are worth about 10% of the size.
/// - `cpb-size` (see `cpb_size_kbit`) caps a keyframe at half a second of the bitrate.
/// - `target-usage=3` encodes a 1080p frame in 8 ms where the default (4) takes 12.5 ms,
///   at the same sizes and quality.
const ENCODER_PROPS: &str =
    "rate-control=cbr min-qp=30 target-usage=3 key-int-max=1024 b-frames=0 i-frames=0 ref-frames=1 num-slices=1 aud=false cc-insert=false";

/// The smallest buffer vah264enc accepts (half a second of the bitrate; smaller values are
/// silently replaced by two seconds). A keyframe never came out larger than it.
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
    frames: u64,
}

impl Session {
    fn start(element: &str, width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self, String> {
        let description = format!("{} ! {}", MemorySource::description(width, height, fps), EncodeChain::description(element, bitrate_bps));
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
        Ok(Self { source, chain, width, height, bitrate_bps, frames: 0 })
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
    session: Option<Session>,
    /// Timestamps come from this clock: calls arrive at any pace, with gaps of any length.
    clock: Instant,
    last_pts_ns: u64,
    gpu_frames: u64,
    failures: u32,
    software: Option<OpenH264Encoder>,
    test_fail: TestFail,
}

impl VaapiEncoder {
    /// Fails unless this machine really encodes Constrained Baseline through VA-API: the
    /// caller can only choose another encoder now, not once the stream runs.
    pub fn try_new(target_fps: u32, is_screen_content: bool) -> Result<Self, String> {
        let test_fail = test_fail();
        if test_fail == TestFail::Init {
            return Err("falha simulada (LITECORD_VAAPI_TEST_FAIL=init)".into());
        }
        gst::init().map_err(|e| format!("GStreamer: {e}"))?;
        if gst::ElementFactory::find("vapostproc").is_none() {
            return Err("plugin VA-API do GStreamer ausente (gst-plugin-va)".into());
        }
        let mut last_error = String::from("nenhum codificador H.264 VA-API registrado");
        for element in ENCODER_ELEMENTS {
            if gst::ElementFactory::find(element).is_none() {
                continue;
            }
            match Self::probe(element, target_fps) {
                Ok(took) => {
                    info!("🚀 [VAAPI] {} respondeu ao teste em {:.0} ms (Constrained Baseline)", element, took.as_secs_f64() * 1000.0);
                    return Ok(Self {
                        element,
                        target_fps,
                        is_screen_content,
                        bitrate_bps: INITIAL_BITRATE_BPS,
                        bitrate_asked_at: Instant::now(),
                        needs_keyframe: false,
                        session: None,
                        clock: Instant::now(),
                        last_pts_ns: 0,
                        gpu_frames: 0,
                        failures: 0,
                        software: None,
                        test_fail,
                    });
                }
                Err(e) => last_error = format!("{element}: {e}"),
            }
        }
        Err(last_error)
    }

    /// Encodes one black frame with the stream's own pipeline and checks what comes out.
    fn probe(element: &str, fps: u32) -> Result<Duration, String> {
        let started = Instant::now();
        let (w, h) = PROBE_SIZE;
        let session = Session::start(element, w, h, fps, INITIAL_BITRATE_BPS)?;
        let frame = vec![0u8; (w * h * 4) as usize];
        session.source.push(&frame, w as usize, h as usize, (w * 4) as usize, gst::ClockTime::ZERO)?;
        let au = match session.chain.pull(gst::ClockTime::ZERO, FIRST_FRAME_TIMEOUT) {
            Ok(au) => au,
            Err(PullError::Timeout) => return Err("sem resposta do codificador".into()),
            Err(PullError::Broken(e)) => return Err(e),
        };
        check_keyframe(&au)?;
        Ok(started.elapsed())
    }

    fn next_pts(&mut self) -> gst::ClockTime {
        let ns = (self.clock.elapsed().as_nanos() as u64).max(self.last_pts_ns + 1);
        self.last_pts_ns = ns;
        gst::ClockTime::from_nseconds(ns)
    }

    fn encode_on_gpu(&mut self, bgra: &[u8], w: u32, h: u32, stride: usize) -> Result<Vec<u8>, String> {
        let stall = match self.test_fail {
            TestFail::After(n) if self.gpu_frames >= n => return Err("falha simulada (LITECORD_VAAPI_TEST_FAIL=after)".into()),
            TestFail::Stall(n) => self.gpu_frames >= n,
            _ => false,
        };
        if self.session.as_ref().map(|s| (s.width, s.height) != (w, h)).unwrap_or(true) {
            // Dropped first: the old pipeline stops in the background while the new one starts.
            self.session = None;
            let session = Session::start(self.element, w, h, self.target_fps, self.bitrate_bps)?;
            info!("🚀 [VAAPI] Pipeline {} iniciado: {}x{} {} FPS, {:.1} Mbps", self.element, w, h, self.target_fps, self.bitrate_bps as f64 / 1e6);
            self.session = Some(session);
            // A new pipeline starts with an IDR by itself.
            self.needs_keyframe = false;
        }
        let pts = self.next_pts();
        let Some(session) = self.session.as_mut() else { return Err("sem pipeline".into()) };

        let rate_due = session.bitrate_bps != self.bitrate_bps && (self.needs_keyframe || self.bitrate_asked_at.elapsed() >= BITRATE_MAX_WAIT);
        if rate_due {
            session.chain.set_bitrate(self.bitrate_bps);
            session.bitrate_bps = self.bitrate_bps;
        }
        let keyframe_expected = session.frames == 0 || rate_due || (self.needs_keyframe && session.chain.request_keyframe());

        if !stall {
            session.source.push(bgra, w as usize, h as usize, stride, pts)?;
        }
        let timeout = if session.frames == 0 { FIRST_FRAME_TIMEOUT } else { FRAME_TIMEOUT };
        let au = match session.chain.pull(pts, timeout) {
            Ok(au) => au,
            Err(PullError::Timeout) => {
                // The encoder may still have encoded this frame, which no viewer will get:
                // the next one must not reference it.
                self.needs_keyframe = true;
                return Err(format!("sem resposta em {} ms", timeout.as_millis()));
            }
            Err(PullError::Broken(e)) => {
                self.session = None;
                return Err(e);
            }
        };
        session.frames += 1;
        self.gpu_frames += 1;

        let au = without_filler(au);
        if !matches!(au.as_slice(), [0, 0, 1, ..] | [0, 0, 0, 1, ..]) {
            self.needs_keyframe = true;
            return Err("saída sem start code".into());
        }
        if !nal_ranges(&au).iter().any(|r| au[r.start] & 0x1f == 5) {
            if keyframe_expected {
                warn!("⚠️ [VAAPI] Keyframe pedido, mas o codificador devolveu um quadro P; pedindo de novo");
                self.needs_keyframe = true;
            }
            return Ok(au);
        }
        self.needs_keyframe = false;
        Ok(super::h264_vui::declare_colour(&au, 6, 6, 6))
    }

    fn switch_to_software(&mut self, reason: &str) {
        warn!("⚠️ [VAAPI] {} falhas seguidas na GPU ({}); a transmissão continua com OpenH264 (CPU)", self.failures, reason);
        self.session = None;
        match OpenH264Encoder::new(self.target_fps, self.is_screen_content) {
            Ok(mut enc) => {
                enc.set_bitrate_bps(self.bitrate_bps);
                // Its first picture is a keyframe.
                enc.force_intra_frame();
                self.software = Some(enc);
            }
            Err(e) => {
                warn!("⚠️ [VAAPI] OpenH264 também falhou ({}); tentando a GPU de novo", e);
                self.failures = 0;
            }
        }
    }
}

/// An access unit that opens a stream the receivers accept: Annex B, IDR with SPS and PPS,
/// Constrained Baseline.
pub(crate) fn check_keyframe(au: &[u8]) -> Result<(), String> {
    if !matches!(au, [0, 0, 1, ..] | [0, 0, 0, 1, ..]) {
        return Err("saída sem start code".into());
    }
    let ranges = nal_ranges(au);
    let has = |t: u8| ranges.iter().any(|r| au[r.start] & 0x1f == t);
    if !(has(7) && has(8) && has(5)) {
        return Err("primeiro quadro sem SPS/PPS/IDR".into());
    }
    let sps = ranges.iter().map(|r| &au[r.clone()]).find(|n| n[0] & 0x1f == 7).filter(|n| n.len() >= 3).ok_or("SPS curto")?;
    // profile_idc 66 with constraint_set1_flag.
    if sps[1] != 66 || sps[2] & 0x40 == 0 {
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
        if let Some(software) = self.software.as_mut() {
            return software.encode(bgra_data, width, height);
        }
        match self.encode_on_gpu(bgra_data, w, h, stride) {
            Ok(au) => {
                self.failures = 0;
                Some(au)
            }
            Err(e) => {
                self.failures += 1;
                if self.failures < MAX_FAILURES {
                    warn!("⚠️ [VAAPI] Quadro perdido na GPU ({}/{}): {}", self.failures, MAX_FAILURES, e);
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
        if let Some(software) = self.software.as_mut() {
            software.set_bitrate_bps(bitrate_bps);
        }
    }

    fn get_bitrate_bps(&self) -> u32 {
        self.bitrate_bps
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
