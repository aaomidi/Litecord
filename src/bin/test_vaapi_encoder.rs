//! Drives a screen-share H.264 encoder through the `VideoEncoder` trait only, the way the
//! "screen-capture-tx" thread does, and checks every access unit against what Litecord's
//! receivers need. Run with `--help` for the options.

#[cfg(not(all(target_os = "linux", feature = "vaapi")))]
fn main() {
    eprintln!("test_vaapi_encoder: needs Linux and the `vaapi` cargo feature");
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
fn main() {
    let code = harness::run();
    litecord::encoder::before_exit();
    std::process::exit(code);
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod harness {
    use h264_reader::nal::sps::SeqParameterSet;
    use h264_reader::nal::{Nal, RefNal};
    use litecord::encoder::vaapi::VaapiEncoder;
    use litecord::encoder::{create_best_encoder, OpenH264Encoder, VideoEncoder};
    use litecord::screen_capture::hw_decode::h264_state::{nal_ranges, H264State, StateError, StreamInfo};
    use litecord::screen_capture::video_pipeline::{decode_video_frame, Decoded, StreamDecoders};
    use litecord::screen_capture::watch::{ProfileChoice, RateControl, Verdict, Viewers, CAP_H264_MAIN};
    use litecord::utils::video_settings::{self, StreamProfile};
    use openh264::formats::YUVSource;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    const HELP: &str = "\
test_vaapi_encoder [options]
  --encoder E      vaapi (default) | openh264 | factory[:SETTING]
                   factory runs encoder::create_best_encoder with the video_encoder setting
                   SETTING (auto, vaapi, openh264, nvenc, ...; kept in memory only)
  --size S         480 | 720 | 1080 (default) | WxH
  --fps N          15 | 30 (default) | 60 ...
  --frames N       frames to encode (default 300)
  --fast           no pacing (default: one frame every 1/fps, or --interval-ms)
  --interval-ms N  pace frames this far apart instead (100 = Litecord on a still screen)
  --pattern P      text (default: scrolling document + terminal) | still (same screen, a
                   cursor blinks, the page flips every 300 frames) | hard (moving gradient
                   with noise) | file:PATH (raw BGRA frames of the stream size, looped)
  --pan N          with file: roll the picture N pixels sideways per frame
  --bitrate SPEC   kbps, or a schedule FRAME:KBPS,... (default 8000), e.g. 0:8000,100:3000,200:8000
  --min-qp N       the 'quality floor' setting (18..36, default 30; kept in memory only)
  --profile P      the 'stream profile' setting: compatible (default) | efficient | auto
  --viewers SPEC   FRAME:LIST,... who watches from those frames on, driving the send loop's
                   own rules (watch::Viewers + watch::ProfileChoice on a clock of one frame per
                   1/fps). LIST: viewers joined by '+': m (native, decodes Main), c (native,
                   does not), w (a browser), or - for nobody (nothing is encoded). A position
                   keeps its identity: 0:m,100:c is one viewer changing its capability byte.
                   Default: one native viewer that decodes Main.
  --loopback SPEC  send every access unit like the send loop does (encrypted, 1200-byte
                   chunks, one XOR parity packet) over UDP on 127.0.0.1 to a thread that puts
                   it together like the receive loop. SPEC: app (socket buffers asked for as
                   the app does, 16 MB each) | SND:RCV in bytes (0 = system default)
  --ffmpeg         decode the whole stream with ffmpeg (an independent decoder): one picture
                   per access unit, PSNR against the input
  --rate-model M:C:L  nothing is encoded: the send loop's loss back-off (watch::RateControl)
                   for 120 s with maximum M, pictures that use up to C and a viewer whose link
                   carries L (kbps). A model: the loss reported once a second is the share
                   sent above L (real reports count lost frames, which grows faster)
  --keyframes K    2s (default: like Litecord, every 2 s) | none | every:N | FRAME,FRAME,...
  --gaps SPEC      FRAME:MS,... sleep MS before those frames
  --resize SPEC    FRAME:SIZE,... change the picture size from those frames on
  --fail MODE      init | after:N[:COUNT[:EVERY]] | stall:N[:COUNT[:EVERY]]  (sets
                   LITECORD_VAAPI_TEST_FAIL: break the VA path at construction, from GPU frame
                   N on at once, or with real timeouts; for good, or COUNT frames, one in EVERY)
                   | main  (Main pipelines do not start)
  --main-may-fail  keyframes may be Constrained Baseline while Main is wanted (implied by
                   --fail main): the encoder gave Main up, e.g. --fail stall:0:2:1 (its
                   first picture late twice), or returns from OpenH264 before Main was seen
  --max-none N     encode() calls allowed to return None (default 0)
  --exit-race US   at the end, drop the encoder on another thread and exit US microseconds
                   later, like the app quitting during a stream (a hang is the failure)
  --dump FILE      write the Annex B stream
  --no-verify      skip decoding and parsing (timing runs)
  --psnr           also report the PSNR of what the receiver shows against the input
  --no-colour      skip the solid-colour check
  --verbose        one line per access unit
Exit status 1 when a hard check fails, 2 when the encoder cannot be created.";

    struct Args(HashMap<String, String>);

    impl Args {
        fn parse() -> Result<Self, String> {
            let flags = ["fast", "no-verify", "no-colour", "psnr", "verbose", "help", "ffmpeg", "main-may-fail"];
            let mut map = HashMap::new();
            let mut it = std::env::args().skip(1);
            while let Some(a) = it.next() {
                let key = a.strip_prefix("--").ok_or(format!("unexpected argument {a}"))?.to_string();
                if flags.contains(&key.as_str()) {
                    map.insert(key, String::new());
                } else {
                    let v = it.next().ok_or(format!("--{key} needs a value"))?;
                    map.insert(key, v);
                }
            }
            Ok(Self(map))
        }
        fn has(&self, k: &str) -> bool {
            self.0.contains_key(k)
        }
        fn str(&self, k: &str, default: &str) -> String {
            self.0.get(k).cloned().unwrap_or_else(|| default.to_string())
        }
        fn num(&self, k: &str, default: u64) -> Result<u64, String> {
            match self.0.get(k) {
                Some(v) => v.parse().map_err(|_| format!("--{k}: not a number: {v}")),
                None => Ok(default),
            }
        }
    }

    fn parse_size(s: &str) -> Result<(u32, u32), String> {
        match s {
            "480" => Ok((854, 480)),
            "720" => Ok((1280, 720)),
            "1080" => Ok((1920, 1080)),
            _ => {
                let (w, h) = s.split_once('x').ok_or(format!("bad size {s}"))?;
                Ok((w.parse().map_err(|_| format!("bad size {s}"))?, h.parse().map_err(|_| format!("bad size {s}"))?))
            }
        }
    }

    /// "A:B,C:D" as (frame, value) pairs.
    fn parse_pairs(s: &str) -> Result<Vec<(usize, String)>, String> {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (a, b) = p.split_once(':').ok_or(format!("expected FRAME:VALUE, got {p}"))?;
                Ok((a.parse().map_err(|_| format!("bad frame number {a}"))?, b.to_string()))
            })
            .collect()
    }

    fn cpu_secs() -> f64 {
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut ru);
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
            tv(ru.ru_utime) + tv(ru.ru_stime)
        }
    }

    // ------------------------------------------------------------------ content

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n.max(1)
        }
    }

    /// A BGRA canvas with a tiny bitmap "font": glyph shapes are made up (5x7 dots from a
    /// hash), but lines of them have the edges and spacing of small UI text.
    struct Canvas {
        w: usize,
        h: usize,
        px: Vec<u8>,
    }

    impl Canvas {
        fn new(w: usize, h: usize, bgr: [u8; 3]) -> Self {
            let mut c = Self { w, h, px: vec![255; w * h * 4] };
            c.rect(0, 0, w, h, bgr);
            c
        }
        fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, bgr: [u8; 3]) {
            for yy in y..(y + h).min(self.h) {
                for xx in x..(x + w).min(self.w) {
                    let o = (yy * self.w + xx) * 4;
                    self.px[o..o + 3].copy_from_slice(&bgr);
                }
            }
        }
        fn glyph(&mut self, x: usize, y: usize, ch: u32, scale: usize, bgr: [u8; 3]) {
            let mut bits = ch.wrapping_mul(0x9E37_79B9) ^ (ch.wrapping_mul(0x85EB_CA6B) >> 7);
            bits |= 0x0000_4210; // a stem, so glyphs are not just dust
            for row in 0..7 {
                for col in 0..5 {
                    if bits >> (row * 5 + col) & 1 == 1 {
                        self.rect(x + col * scale, y + row * scale, scale, scale, bgr);
                    }
                }
            }
        }
        /// A line of made-up words from `x` to `x_end`; returns nothing, the text is noise.
        fn text_line(&mut self, x: usize, y: usize, x_end: usize, scale: usize, bgr: [u8; 3], rng: &mut Rng) {
            let advance = 6 * scale + scale / 2;
            let mut cx = x;
            let stop = x + x_end.saturating_sub(x) * (40 + rng.below(60) as usize) / 100;
            while cx + advance < stop {
                for _ in 0..2 + rng.below(8) {
                    if cx + advance >= stop {
                        break;
                    }
                    self.glyph(cx, y, 1 + rng.below(60), scale, bgr);
                    cx += advance;
                }
                cx += advance;
            }
        }
        fn paragraphs(&mut self, x: usize, y: usize, w: usize, h: usize, palette: &[[u8; 3]], rng: &mut Rng) {
            let mut cy = y + 6;
            while cy + 12 < y + h {
                match rng.below(12) {
                    0 => cy += 10,
                    1 if cy + 24 < y + h => {
                        self.text_line(x + 8, cy, x + w, 2, palette[0], rng);
                        cy += 22;
                    }
                    _ => {
                        let colour = palette[rng.below(palette.len() as u32) as usize];
                        self.text_line(x + 8 + 16 * rng.below(3) as usize, cy, x + w, 1, colour, rng);
                        cy += 13;
                    }
                }
            }
        }
    }

    /// A desktop-like screen: title bar, a dark side list, a light document that scrolls and
    /// a dark terminal whose lines move up.
    struct TextScreen {
        chrome: Canvas,
        document: Canvas,
        terminal: Canvas,
        doc_x: usize,
        doc_y: usize,
        doc_h: usize,
        term_y: usize,
        term_h: usize,
    }

    impl TextScreen {
        fn new(w: usize, h: usize) -> Self {
            let mut rng = Rng(0x2545_F491_4F6C_DD1D);
            let bar = 36.min(h / 8);
            let side = w * 22 / 100;
            let term_h = h * 30 / 100;
            let doc_h = h - bar - term_h;
            let mut chrome = Canvas::new(w, h, [48, 40, 36]);
            chrome.rect(0, 0, w, bar, [32, 28, 26]);
            chrome.text_line(12, bar / 2 - 7, w / 2, 2, [220, 220, 220], &mut rng);
            for i in 0..5 {
                chrome.rect(w - 30 - i * 28, bar / 2 - 6, 14, 12, [90 + 30 * i as u8, 120, 200 - 30 * i as u8]);
            }
            chrome.paragraphs(0, bar, side, h - bar, &[[235, 235, 235], [170, 170, 170], [120, 200, 240]], &mut rng);
            chrome.rect(side - 1, bar, 1, h - bar, [70, 70, 70]);
            let doc_w = w - side;
            let mut document = Canvas::new(doc_w, doc_h * 4, [250, 250, 250]);
            document.paragraphs(8, 0, doc_w - 16, doc_h * 4, &[[20, 20, 20], [40, 40, 40], [180, 80, 20], [30, 30, 30]], &mut rng);
            for i in 0..8 {
                // Flat-coloured boxes: pictures, buttons.
                let (bx, by) = (20 + rng.below(doc_w as u32 / 2) as usize, rng.below((doc_h * 4 - 90) as u32) as usize);
                document.rect(bx, by, 160, 80, [200 - 20 * i as u8, 120 + 10 * i as u8, 60 + 20 * i as u8]);
            }
            let mut terminal = Canvas::new(doc_w, term_h * 3, [30, 24, 20]);
            terminal.paragraphs(4, 0, doc_w - 8, term_h * 3, &[[210, 210, 210], [120, 220, 120], [220, 180, 90], [200, 200, 200]], &mut rng);
            Self { chrome, document, terminal, doc_x: side, doc_y: bar, doc_h, term_y: bar + doc_h, term_h }
        }

        fn blit(dst: &mut [u8], dst_w: usize, x: usize, y: usize, src: &Canvas, src_y: usize, rows: usize) {
            for r in 0..rows {
                let s = ((src_y + r) % src.h) * src.w * 4;
                let d = ((y + r) * dst_w + x) * 4;
                dst[d..d + src.w * 4].copy_from_slice(&src.px[s..s + src.w * 4]);
            }
        }

        /// `doc_scroll`/`term_scroll` in pixels; `cursor` draws the terminal's block cursor.
        fn draw(&self, out: &mut [u8], doc_scroll: usize, term_scroll: usize, cursor: bool) {
            out.copy_from_slice(&self.chrome.px);
            let w = self.chrome.w;
            Self::blit(out, w, self.doc_x, self.doc_y, &self.document, doc_scroll, self.doc_h);
            Self::blit(out, w, self.doc_x, self.term_y, &self.terminal, term_scroll, self.term_h);
            if cursor {
                for yy in self.term_y + self.term_h - 20..self.term_y + self.term_h - 6 {
                    let o = (yy * w + self.doc_x + 12) * 4;
                    out[o..o + 8 * 4].fill(230);
                }
            }
        }
    }

    enum Pattern {
        Text(TextScreen),
        Still(TextScreen),
        Hard(Rng),
        File { frames: Vec<u8>, count: usize, pan: usize },
    }

    impl Pattern {
        fn new(spec: &str, w: usize, h: usize, pan: usize) -> Result<Self, String> {
            match spec {
                "text" => Ok(Self::Text(TextScreen::new(w, h))),
                "still" => Ok(Self::Still(TextScreen::new(w, h))),
                "hard" => Ok(Self::Hard(Rng(0x1234_5678_9ABC_DEF1))),
                _ => {
                    let path = spec.strip_prefix("file:").ok_or(format!("unknown pattern {spec}"))?;
                    let frames = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
                    let count = frames.len() / (w * h * 4);
                    if count == 0 {
                        return Err(format!("{path}: smaller than one {w}x{h} BGRA frame"));
                    }
                    Ok(Self::File { frames, count, pan })
                }
            }
        }

        fn fill(&mut self, i: usize, w: usize, h: usize, out: &mut [u8]) {
            match self {
                // 4 px of document scroll per frame, a terminal line every 8 frames.
                Self::Text(s) => s.draw(out, i * 4, (i / 8) * 13, (i / 15) % 2 == 0),
                Self::Still(s) => s.draw(out, (i / 300) * s.doc_h, 0, (i / 15) % 2 == 0),
                Self::Hard(rng) => {
                    for y in 0..h {
                        for x in 0..w {
                            let n = rng.next();
                            let o = (y * w + x) * 4;
                            out[o] = (((x + y) / 2 + 7 * i) as u8 >> 1).wrapping_add((n & 63) as u8);
                            out[o + 1] = ((y * 2 + 5 * i) as u8 >> 1).wrapping_add((n >> 8 & 63) as u8);
                            out[o + 2] = ((x + 3 * i) as u8 >> 1).wrapping_add((n >> 16 & 63) as u8);
                            out[o + 3] = 255;
                        }
                    }
                }
                Self::File { frames, count, pan } => {
                    let frame = &frames[(i % *count) * w * h * 4..][..w * h * 4];
                    let shift = (i * *pan) % w * 4;
                    if shift == 0 {
                        out.copy_from_slice(frame);
                    } else {
                        for (dst, src) in out.chunks_exact_mut(w * 4).zip(frame.chunks_exact(w * 4)) {
                            dst[..w * 4 - shift].copy_from_slice(&src[shift..]);
                            dst[w * 4 - shift..].copy_from_slice(&src[..shift]);
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ encoders

    fn make_encoder(kind: &str, fps: u32) -> Result<Box<dyn VideoEncoder>, String> {
        make_encoder_with(kind, fps, None)
    }

    /// `min_qp`: the quality floor setting for this encoder (in memory only).
    fn make_encoder_with(kind: &str, fps: u32, min_qp: Option<u32>) -> Result<Box<dyn VideoEncoder>, String> {
        if let Some(qp) = min_qp {
            video_settings::get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).stream_min_qp = Some(qp);
        }
        match kind {
            "vaapi" => VaapiEncoder::try_new(fps, true).map(|e| Box::new(e) as Box<dyn VideoEncoder>),
            "openh264" => OpenH264Encoder::new(fps, true).map(|e| Box::new(e) as Box<dyn VideoEncoder>),
            _ => {
                let setting = match kind.split_once(':') {
                    Some(("factory", s)) => s,
                    None if kind == "factory" => "auto",
                    _ => return Err(format!("unknown encoder {kind}")),
                };
                // In memory only: the settings file is not written.
                litecord::utils::video_settings::get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).video_encoder = setting.to_string();
                Ok(create_best_encoder(fps, true))
            }
        }
    }

    // ------------------------------------------------------------------ checks

    #[derive(Default)]
    struct Failures {
        list: Vec<String>,
        count: usize,
    }

    impl Failures {
        fn add(&mut self, frame: usize, what: String) {
            self.count += 1;
            if self.list.len() < 25 {
                println!("  FAIL frame {frame}: {what}");
                self.list.push(format!("frame {frame}: {what}"));
            }
        }
    }

    #[derive(Clone, PartialEq, Debug)]
    struct SpsSummary {
        profile_idc: u8,
        constraint_flags: u8,
        level_idc: u8,
        size: (u32, u32),
        /// (full range, primaries, transfer, matrix) when the VUI describes the colours.
        colour: Option<(bool, Option<(u8, u8, u8)>)>,
        poc_type: String,
        refs: u32,
        /// max_num_reorder_frames when the VUI has bitstream restrictions.
        reorder: Option<u32>,
        /// The VUI declares HRD parameters (a rate the stream then has to keep).
        hrd: bool,
    }

    impl SpsSummary {
        fn constrained_baseline(&self) -> bool {
            self.profile_idc == 66 && self.constraint_flags & 0x40 != 0
        }
        fn main(&self) -> bool {
            self.profile_idc == 77
        }
    }

    /// (level_idc, MaxMBPS, MaxFS, MaxBR kbit/s, MaxCPB kbit) of H.264 table A-1; Baseline
    /// and Main streams may carry 1.2 times MaxBR and MaxCPB counting whole NAL units.
    const LEVELS: [(u8, u32, u32, u32, u32); 12] = [
        (20, 11880, 396, 2000, 2000),
        (21, 19800, 792, 4000, 4000),
        (22, 20250, 1620, 4000, 4000),
        (30, 40500, 1620, 10000, 10000),
        (31, 108000, 3600, 14000, 14000),
        (32, 216000, 5120, 20000, 20000),
        (40, 245760, 8192, 20000, 25000),
        (41, 245760, 8192, 50000, 62500),
        (42, 522240, 8704, 50000, 62500),
        (50, 589824, 22080, 135000, 135000),
        (51, 983040, 36864, 240000, 240000),
        (52, 2073600, 36864, 240000, 240000),
    ];

    /// Whether `level_idc` allows this picture size, frame rate, bitrate and buffer.
    fn level_verdict(level_idc: u8, size: (u32, u32), fps: u32, bitrate_bps: u32) -> String {
        let Some(&(_, max_mbps, max_fs, max_br, max_cpb)) = LEVELS.iter().find(|l| l.0 == level_idc) else {
            return format!("level_idc {level_idc} is not in the table");
        };
        let mbs = size.0.div_ceil(16) * size.1.div_ceil(16);
        let (kbps, cpb_kbit) = (bitrate_bps / 1000, (bitrate_bps / 1000).div_ceil(2));
        let mut over = Vec::new();
        if mbs > max_fs {
            over.push(format!("{mbs} macroblocks per picture > {max_fs}"));
        }
        if mbs * fps > max_mbps {
            over.push(format!("{} macroblocks/s > {max_mbps}", mbs * fps));
        }
        if kbps > max_br * 12 / 10 {
            over.push(format!("{kbps} kbps > {}", max_br * 12 / 10));
        }
        if cpb_kbit > max_cpb * 12 / 10 {
            over.push(format!("buffer {cpb_kbit} kbit > {}", max_cpb * 12 / 10));
        }
        let limits = format!("level {}.{} allows {} MB/s, {} kbps (NAL), buffer {} kbit; stream: {} MB/s, {kbps} kbps, buffer {cpb_kbit} kbit", level_idc / 10, level_idc % 10, max_mbps, max_br * 12 / 10, max_cpb * 12 / 10, mbs * fps);
        if over.is_empty() {
            format!("legal ({limits})")
        } else {
            format!("NOT legal: {} ({limits})", over.join("; "))
        }
    }

    fn read_sps(nal: &[u8]) -> Option<SpsSummary> {
        let sps = SeqParameterSet::from_bits(RefNal::new(nal, &[], true).rbsp_bits()).ok()?;
        let signal = sps.vui_parameters.as_ref().and_then(|v| v.video_signal_type.as_ref());
        Some(SpsSummary {
            profile_idc: nal[1],
            constraint_flags: nal[2],
            level_idc: nal[3],
            size: sps.pixel_dimensions().ok()?,
            colour: signal.map(|s| (s.video_full_range_flag, s.colour_description.as_ref().map(|c| (c.colour_primaries, c.transfer_characteristics, c.matrix_coefficients)))),
            poc_type: format!("{:?}", sps.pic_order_cnt).split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_string(),
            refs: sps.max_num_ref_frames,
            reorder: sps.vui_parameters.as_ref().and_then(|v| v.bitstream_restrictions.as_ref()).map(|b| b.max_num_reorder_frames),
            hrd: sps.vui_parameters.as_ref().is_some_and(|v| v.nal_hrd_parameters.is_some() || v.vcl_hrd_parameters.is_some()),
        })
    }

    /// The receivers of one stream: Litecord's own decode path (OpenH264 on Linux) and the
    /// bookkeeping of its Windows GPU decoder, driven like DxvaDecoder::decode.
    struct Receivers {
        decoders: StreamDecoders,
        state: H264State,
        session: Option<StreamInfo>,
        sps: Option<SpsSummary>,
        prev_frame_num: Option<u16>,
        /// pic_order_cnt_lsb of the picture before, in decode order.
        prev_poc: Option<u32>,
        decoded: usize,
        /// Main access units: not given to Litecord's own (OpenH264) decode path.
        main_units: usize,
        /// What the stream is being checked against (for the level).
        fps: u32,
        bitrate_bps: u32,
        level_checked: Option<(u8, (u32, u32), u32)>,
    }

    const PEER: u64 = 0x7E57;

    impl Receivers {
        fn new() -> Self {
            Self {
                decoders: StreamDecoders::default(),
                state: H264State::new(),
                session: None,
                sps: None,
                prev_frame_num: None,
                prev_poc: None,
                decoded: 0,
                main_units: 0,
                fps: 30,
                bitrate_bps: 8_000_000,
                level_checked: None,
            }
        }

        /// The stream is H.264 Main from its last SPS on.
        fn main(&self) -> bool {
            self.sps.as_ref().is_some_and(|s| s.main())
        }

        /// Checks one access unit; returns its NAL types, its filler bytes and, when `input`
        /// (the BGRA frame) is given, the PSNR of the picture the receiver shows. `main_ok`:
        /// every viewer of this access unit decodes Main.
        #[allow(clippy::too_many_arguments)]
        fn check(&mut self, n: usize, au: &[u8], size: (u32, u32), hardware: bool, verify: bool, main_ok: bool, input: Option<&[u8]>, fails: &mut Failures) -> (Vec<u8>, usize, Option<f64>) {
            if !(au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1])) {
                fails.add(n, format!("does not start with a start code: {:02x?}", &au[..au.len().min(6)]));
            }
            let ranges = nal_ranges(au);
            let types: Vec<u8> = ranges.iter().map(|r| au[r.start] & 0x1f).collect();
            let idr = types.contains(&5);
            let filler: usize = ranges.iter().filter(|r| au[r.start] & 0x1f == 12).map(|r| r.len()).sum();
            let vcl: Vec<&[u8]> = ranges.iter().map(|r| &au[r.clone()]).filter(|nal| matches!(nal[0] & 0x1f, 1 | 5)).collect();
            // first_mb_in_slice == 0 (ue(v) "1") starts a picture: exactly one per access unit.
            let pictures = vcl.iter().filter(|nal| nal.len() > 1 && nal[1] & 0x80 != 0).count();
            if pictures != 1 {
                fails.add(n, format!("{pictures} pictures in one encode() result (NAL types {types:?})"));
            }
            if hardware && vcl.len() != 1 {
                fails.add(n, format!("{} slices, expected 1", vcl.len()));
            }
            if let Some(t) = types.iter().find(|t| !matches!(t, 1 | 5 | 7 | 8)) {
                if hardware {
                    fails.add(n, format!("unexpected NAL type {t} (types {types:?})"));
                }
            }
            if idr && !(types.contains(&7) && types.contains(&8)) {
                fails.add(n, format!("IDR without SPS+PPS (types {types:?})"));
            }
            if !idr && (types.contains(&7) || types.contains(&8)) && hardware {
                fails.add(n, format!("parameter sets without IDR (types {types:?})"));
            }
            for nal in ranges.iter().map(|r| &au[r.clone()]).filter(|nal| nal[0] & 0x1f == 7) {
                match read_sps(nal) {
                    None => fails.add(n, "SPS does not parse".into()),
                    Some(sps) => {
                        if self.sps.as_ref() != Some(&sps) {
                            println!(
                                "  SPS at frame {n}: profile_idc={} constraint_flags={:#04x} level_idc={} size={}x{} poc={} max_refs={} max_reorder={:?} hrd={} colour={}",
                                sps.profile_idc,
                                sps.constraint_flags,
                                sps.level_idc,
                                sps.size.0,
                                sps.size.1,
                                sps.poc_type,
                                sps.refs,
                                sps.reorder,
                                sps.hrd,
                                match sps.colour {
                                    None => "not declared".to_string(),
                                    Some((full, None)) => format!("range={} only", if full { "full" } else { "limited" }),
                                    Some((full, Some((p, t, m)))) => format!("range={} primaries={p} transfer={t} matrix={m}", if full { "full" } else { "limited" }),
                                }
                            );
                        }
                        // Checked again whenever the rate changes under the same level.
                        let level = (sps.level_idc, sps.size, self.bitrate_bps);
                        if hardware && self.level_checked != Some(level) {
                            self.level_checked = Some(level);
                            let verdict = level_verdict(sps.level_idc, sps.size, self.fps, self.bitrate_bps);
                            println!("  level_idc {} at frame {n}, {} fps, {} kbps: {verdict}", sps.level_idc, self.fps, self.bitrate_bps / 1000);
                            if verdict.starts_with("NOT") {
                                fails.add(n, format!("level_idc {}: {verdict}", sps.level_idc));
                            }
                        }
                        if sps.size != size {
                            fails.add(n, format!("SPS size {}x{}, input {}x{}", sps.size.0, sps.size.1, size.0, size.1));
                        }
                        if !(sps.constrained_baseline() || sps.main()) {
                            fails.add(n, format!("neither Constrained Baseline nor Main: profile_idc={} flags={:#04x}", sps.profile_idc, sps.constraint_flags));
                        }
                        if sps.main() && sps.reorder != Some(0) {
                            fails.add(n, format!("Main SPS without max_num_reorder_frames=0 ({:?}): decoders may hold pictures back", sps.reorder));
                        }
                        if hardware && sps.hrd {
                            // The filler that kept a declared constant rate is removed.
                            fails.add(n, "SPS declares HRD parameters".into());
                        }
                        if hardware && sps.colour != Some((false, Some((1, 1, 6)))) {
                            fails.add(n, format!("SPS colours {:?}, expected limited range 1/1/6", sps.colour));
                        }
                        if matches!(sps.colour, Some((true, _)) | Some((_, Some((_, _, 1))))) {
                            fails.add(n, format!("SPS declares full range or BT.709: {:?}", sps.colour));
                        }
                        self.sps = Some(sps);
                    }
                }
            }
            if self.main() && !main_ok {
                fails.add(n, "a Main access unit while a viewer that cannot decode Main is watching".into());
            }
            if !verify {
                return (types, filler, None);
            }

            // Windows GPU decoder bookkeeping.
            let gpu = (|| -> Result<(), StateError> {
                let Some(p) = self.state.parse(au)? else { return Err(StateError::Unsupported("no picture in the access unit".into())) };
                let info = H264State::stream_info(&p)?;
                if self.session != Some(info) {
                    if !p.idr {
                        return Err(StateError::NeedKeyframe("stream changed without a keyframe".into()));
                    }
                    self.state.reset(&info);
                    self.session = Some(info);
                }
                let (pic, _) = self.state.begin(&p)?;
                self.state.finish(&p, &pic);
                let max_frame_num = 1u32 << (p.sps.log2_max_frame_num_minus4 + 4);
                let expected = if p.idr { Some(0) } else { self.prev_frame_num.map(|f| ((f as u32 + 1) % max_frame_num) as u16) };
                let problem = if expected.is_some() && expected != Some(p.header.frame_num) {
                    Some(format!("frame_num {} after {:?}", p.header.frame_num, self.prev_frame_num))
                } else if p.nal_ref_idc == 0 {
                    Some("picture is not a reference (nal_ref_idc 0)".to_string())
                } else if !p.idr && p.intra() {
                    Some("I picture that is not an IDR".to_string())
                } else if info.reorder_depth != 0 {
                    Some(format!("reorder depth {}", info.reorder_depth))
                } else if p.header.slice_type.family == h264_reader::nal::slice::SliceFamily::B {
                    Some("B picture".to_string())
                } else {
                    // Output order is decode order when the picture order count only grows
                    // (it wraps at max_poc_lsb; an IDR starts again).
                    let poc = match p.header.pic_order_cnt_lsb {
                        Some(h264_reader::nal::slice::PicOrderCountLsb::Frame(lsb)) => Some(lsb),
                        _ => None,
                    };
                    let max_lsb = match p.sps.pic_order_cnt {
                        h264_reader::nal::sps::PicOrderCntType::TypeZero { log2_max_pic_order_cnt_lsb_minus4 } => 1u32 << (log2_max_pic_order_cnt_lsb_minus4 + 4),
                        _ => 0,
                    };
                    let before = std::mem::replace(&mut self.prev_poc, poc);
                    match (before, poc) {
                        (Some(a), Some(b)) if !p.idr && max_lsb > 0 && !(1..max_lsb / 2).contains(&(b.wrapping_sub(a) % max_lsb)) => {
                            Some(format!("picture order count {b} after {a}: a decoder would reorder"))
                        }
                        _ => None,
                    }
                };
                self.prev_frame_num = Some(p.header.frame_num);
                if let Some(problem) = problem {
                    // OpenH264 may code things its own way; these are rules for the GPU encoder.
                    if hardware {
                        return Err(StateError::Unsupported(problem));
                    }
                }
                Ok(())
            })();
            if let Err(e) = gpu {
                fails.add(n, format!("Windows decoder parser: {e:?}"));
            }

            let mut psnr = None;
            if self.main() {
                // Litecord's own decode path (OpenH264) is the viewer side's to fix for
                // Main; these are checked with the independent decoder (--ffmpeg).
                self.main_units += 1;
                return (types, filler, psnr);
            }
            match decode_video_frame(&mut self.decoders, PEER, au) {
                Decoded::Picture(buf, w, h) => {
                    self.decoded += 1;
                    if (w, h) != size || (buf.width(), buf.height()) != size {
                        fails.add(n, format!("decoded {w}x{h} (picture {}x{}), input {}x{}", buf.width(), buf.height(), size.0, size.1));
                    } else if let Some(bgra) = input {
                        let sse: u64 = buf.as_bytes().chunks_exact(4).zip(bgra.chunks_exact(4)).map(|(o, i)| {
                            let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2) as u64;
                            d(o[0], i[2]) + d(o[1], i[1]) + d(o[2], i[0])
                        }).sum();
                        let mse = sse as f64 / (size.0 as f64 * size.1 as f64 * 3.0);
                        psnr = Some(if mse > 0.0 { 10.0 * (255.0f64 * 255.0 / mse).log10() } else { 99.0 });
                    }
                }
                Decoded::ShownByGpu(_) => self.decoded += 1,
                Decoded::Nothing => fails.add(n, "receiver decoded nothing".into()),
                Decoded::Failed => fails.add(n, "receiver failed to decode".into()),
                Decoded::NeedKeyframe => fails.add(n, "receiver asks for a keyframe".into()),
            }
            (types, filler, psnr)
        }
    }

    /// Solid colours in, decoded samples out: the stream must be BT.601 limited range, which
    /// is what every Litecord receiver converts with.
    fn colour_check(kind: &str, size: (u32, u32), fps: u32, fails: &mut Failures) {
        println!("\n== colour check ({}x{}): solid colours through the encoder, decoded Y/U/V against BT.601 limited ==", size.0, size.1);
        let mut enc = match make_encoder(kind, fps) {
            Ok(e) => e,
            Err(e) => return fails.add(0, format!("colour check: encoder not created: {e}")),
        };
        let mut decoders = StreamDecoders::default();
        let Ok(mut raw) = openh264::decoder::Decoder::new() else { return fails.add(0, "colour check: no OpenH264 decoder".into()) };
        let colours: [(&str, [i32; 3]); 10] = [
            ("black", [0, 0, 0]),
            ("white", [255, 255, 255]),
            ("grey", [128, 128, 128]),
            ("red", [255, 0, 0]),
            ("green", [0, 255, 0]),
            ("blue", [0, 0, 255]),
            ("yellow", [255, 255, 0]),
            ("cyan", [0, 255, 255]),
            ("magenta", [255, 0, 255]),
            ("orange", [230, 120, 40]),
        ];
        let (w, h) = (size.0 as usize, size.1 as usize);
        let mut frame = vec![255u8; w * h * 4];
        for (i, (name, [r, g, b])) in colours.iter().enumerate() {
            for p in frame.chunks_exact_mut(4) {
                p[0] = *b as u8;
                p[1] = *g as u8;
                p[2] = *r as u8;
            }
            enc.force_intra_frame();
            let Some(au) = (0..4).find_map(|_| enc.encode(&frame, size.0, size.1)) else {
                fails.add(i, format!("colour check: no output for {name}"));
                continue;
            };
            let expected = [
                ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16,
                ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128,
                ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128,
            ];
            let yuv = match raw.decode(&au) {
                Ok(Some(yuv)) => {
                    let (ys, us, vs) = yuv.strides();
                    let (cx, cy) = (w / 2, h / 2);
                    [yuv.y()[cy * ys + cx] as i32, yuv.u()[cy / 2 * us + cx / 2] as i32, yuv.v()[cy / 2 * vs + cx / 2] as i32]
                }
                other => {
                    fails.add(i, format!("colour check: {name} did not decode ({:?})", other.map(|o| o.is_some())));
                    continue;
                }
            };
            let rgb = match decode_video_frame(&mut decoders, PEER + 1, &au) {
                Decoded::Picture(buf, _, _) => {
                    let o = ((buf.height() / 2 * buf.width() + buf.width() / 2) * 4) as usize;
                    let px = buf.as_bytes();
                    [px[o] as i32, px[o + 1] as i32, px[o + 2] as i32]
                }
                _ => {
                    fails.add(i, format!("colour check: the receiver did not show {name}"));
                    continue;
                }
            };
            let yuv_err = (0..3).map(|k| (yuv[k] - expected[k]).abs()).max().unwrap_or(0);
            let rgb_err = [*r, *g, *b].iter().zip(rgb).map(|(a, b)| (a - b).abs()).max().unwrap_or(0);
            let ok = yuv_err <= 3 && rgb_err <= 6;
            println!(
                "  {name:8} in RGB {:3},{:3},{:3} -> YUV {:3},{:3},{:3} (BT.601 limited expects {:3},{:3},{:3}) -> receiver RGB {:3},{:3},{:3}  {}",
                r, g, b, yuv[0], yuv[1], yuv[2], expected[0], expected[1], expected[2], rgb[0], rgb[1], rgb[2],
                if ok { "ok" } else { "MISMATCH" }
            );
            if !ok {
                fails.add(i, format!("colour check: {name} is off by {yuv_err} (YUV) / {rgb_err} (RGB)"));
            }
        }
        println!("  encoder at the end: {}", enc.name());
    }

    // ------------------------------------------------------------------ viewers

    /// The viewers of the stream from some frame on: one token per position.
    fn parse_viewers(spec: &str) -> Result<Vec<(usize, Vec<char>)>, String> {
        parse_pairs(spec)?
            .into_iter()
            .map(|(frame, list)| {
                let tokens: Vec<char> = if list == "-" { Vec::new() } else { list.split('+').filter_map(|t| t.chars().next()).collect() };
                match tokens.iter().find(|t| !matches!(t, 'm' | 'c' | 'w')) {
                    Some(t) => Err(format!("--viewers: unknown viewer '{t}' (m, c, w or -)")),
                    None => Ok((frame, tokens)),
                }
            })
            .collect()
    }

    // ------------------------------------------------------------------ loopback

    /// The packets of the send loop ("screen-capture-tx", manager.rs) over UDP on 127.0.0.1
    /// into the reassembly of the receive loop ("screen-capture-rx"): the same layout, chunk
    /// size, parity packet and encryption, copied here because both loops live inside the
    /// app's threads.
    struct Loopback {
        socket: std::net::UdpSocket,
        to: std::net::SocketAddr,
        key: [u8; 32],
        seq: u32,
        /// (sequence number, bytes, checksum, when sending began) of every frame sent.
        sent: Vec<(u32, usize, u64, Instant)>,
        send_ms: Vec<f64>,
        send_errors: usize,
        packets: usize,
        most_packets: usize,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        receiver: Option<std::thread::JoinHandle<Vec<(u32, usize, u64, Instant, bool)>>>,
        buffers: String,
    }

    fn checksum(data: &[u8]) -> u64 {
        data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
    }

    /// UDP datagrams the kernel dropped at a full socket buffer, system-wide (receive, send).
    fn udp_buffer_errors() -> (u64, u64) {
        let text = std::fs::read_to_string("/proc/net/snmp").unwrap_or_default();
        let mut lines = text.lines().filter(|l| l.starts_with("Udp:"));
        let (Some(names), Some(values)) = (lines.next(), lines.next()) else { return (0, 0) };
        let field = |name: &str| names.split_whitespace().position(|n| n == name).and_then(|i| values.split_whitespace().nth(i)).and_then(|v| v.parse().ok()).unwrap_or(0);
        (field("RcvbufErrors"), field("SndbufErrors"))
    }

    impl Loopback {
        fn socket(send_buffer: usize, recv_buffer: usize) -> Result<socket2::Socket, String> {
            let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).map_err(|e| e.to_string())?;
            if send_buffer > 0 {
                let _ = sock.set_send_buffer_size(send_buffer);
            }
            if recv_buffer > 0 {
                let _ = sock.set_recv_buffer_size(recv_buffer);
            }
            // Like the app's P2P socket: blocking, reads give up after 2 ms.
            let _ = sock.set_read_timeout(Some(Duration::from_millis(2)));
            sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into()).map_err(|e| e.to_string())?;
            Ok(sock)
        }

        fn start(spec: &str) -> Result<Self, String> {
            use litecord::screen_capture::crypto::decrypt_signaling_payload;
            use litecord::screen_capture::types::{CHUNK_SIZE, MAGIC, OP_FEC_PARITY, OP_VIDEO_CHUNK};
            let (snd, rcv) = match spec {
                "app" => (16 << 20, 16 << 20),
                _ => {
                    let (a, b) = spec.split_once(':').ok_or("--loopback: app or SND:RCV")?;
                    (a.parse().map_err(|_| "--loopback: bad size")?, b.parse().map_err(|_| "--loopback: bad size")?)
                }
            };
            let tx = Self::socket(snd, rcv)?;
            let rx = Self::socket(snd, rcv)?;
            let buffers = format!(
                "asked for send {snd} / receive {rcv} bytes (0 = default); the kernel gave send {} / receive {}",
                tx.send_buffer_size().unwrap_or(0),
                rx.recv_buffer_size().unwrap_or(0)
            );
            let to = rx.local_addr().ok().and_then(|a| a.as_socket()).ok_or("no receive address")?;
            let rx: std::net::UdpSocket = rx.into();
            let key: [u8; 32] = [0x5a; 32];
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_rx = stop.clone();
            let receiver = std::thread::Builder::new()
                .name("loopback-rx".into())
                .spawn(move || {
                    struct InFlight {
                        total_len: usize,
                        received: HashMap<u16, Vec<u8>>,
                        parity: Option<Vec<u8>>,
                        first_seen: Instant,
                        recovered: bool,
                    }
                    let mut in_flight: HashMap<u32, InFlight> = HashMap::new();
                    let mut done = Vec::new();
                    let mut buf = vec![0u8; 65535];
                    while !stop_rx.load(std::sync::atomic::Ordering::Relaxed) {
                        let Ok((len, _)) = rx.recv_from(&mut buf) else { continue };
                        if len < 37 || &buf[0..4] != MAGIC {
                            continue;
                        }
                        let op = buf[8];
                        let seq = u32::from_be_bytes(buf[25..29].try_into().unwrap());
                        let total = u16::from_be_bytes(buf[33..35].try_into().unwrap());
                        if in_flight.len() > 30 {
                            let now = Instant::now();
                            in_flight.retain(|_, f| now.duration_since(f.first_seen) < Duration::from_millis(500));
                        }
                        let entry = in_flight.entry(seq).or_insert_with(|| InFlight { total_len: 0, received: HashMap::with_capacity(total as usize), parity: None, first_seen: Instant::now(), recovered: false });
                        if op == OP_VIDEO_CHUNK {
                            let idx = u16::from_be_bytes(buf[35..37].try_into().unwrap());
                            entry.received.insert(idx, buf[37..len].to_vec());
                        } else if op == OP_FEC_PARITY && len >= 39 {
                            entry.total_len = u32::from_be_bytes(buf[35..39].try_into().unwrap()) as usize;
                            entry.parity = Some(buf[39..len].to_vec());
                        } else {
                            continue;
                        }
                        // One missing chunk is rebuilt from the parity packet.
                        if entry.received.len() == (total as usize).saturating_sub(1) && total > 1 {
                            if let (Some(parity), Some(missing)) = (entry.parity.as_ref(), (0..total).find(|i| !entry.received.contains_key(i))) {
                                let mut recovered = parity.clone();
                                for chunk in entry.received.values() {
                                    for (i, b) in chunk.iter().enumerate() {
                                        if i < recovered.len() {
                                            recovered[i] ^= b;
                                        }
                                    }
                                }
                                if missing == total - 1 && entry.total_len > 0 {
                                    recovered.truncate(entry.total_len.saturating_sub((total as usize - 1) * CHUNK_SIZE).max(1));
                                }
                                entry.received.insert(missing, recovered);
                                entry.recovered = true;
                            }
                        }
                        if entry.received.len() == total as usize {
                            let mut frame = Vec::new();
                            for i in 0..total {
                                if let Some(c) = entry.received.get(&i) {
                                    frame.extend_from_slice(c);
                                }
                            }
                            let recovered = entry.recovered;
                            in_flight.remove(&seq);
                            if let Some(plain) = decrypt_signaling_payload(&key, &frame) {
                                done.push((seq, plain.len(), checksum(&plain), Instant::now(), recovered));
                            }
                        }
                    }
                    done
                })
                .map_err(|e| e.to_string())?;
            Ok(Self { socket: tx.into(), to, key, seq: 0, sent: Vec::new(), send_ms: Vec::new(), send_errors: 0, packets: 0, most_packets: 0, stop, receiver: Some(receiver), buffers })
        }

        /// One access unit, sent the way the send loop sends it to one viewer.
        fn send(&mut self, au: &[u8]) {
            use litecord::screen_capture::crypto::encrypt_signaling_payload;
            use litecord::screen_capture::types::{CHUNK_SIZE, MAGIC, OP_FEC_PARITY, OP_VIDEO_CHUNK};
            let started = Instant::now();
            let Some(frame) = encrypt_signaling_payload(&self.key, au) else { return };
            self.seq = self.seq.wrapping_add(1);
            self.sent.push((self.seq, au.len(), checksum(au), started));
            let total_len = frame.len();
            let total_chunks = total_len.div_ceil(CHUNK_SIZE) as u16;
            let header = |op: u8| {
                let mut pkt = Vec::with_capacity(41 + CHUNK_SIZE);
                pkt.extend_from_slice(MAGIC);
                pkt.extend_from_slice(&1u32.to_be_bytes());
                pkt.push(op);
                pkt.extend_from_slice(&7u64.to_be_bytes());
                pkt.extend_from_slice(&PEER.to_be_bytes());
                pkt.extend_from_slice(&self.seq.to_be_bytes());
                pkt.extend_from_slice(&0u32.to_be_bytes());
                pkt.extend_from_slice(&total_chunks.to_be_bytes());
                pkt
            };
            let mut parity = vec![0u8; CHUNK_SIZE];
            for (idx, chunk) in frame.chunks(CHUNK_SIZE).enumerate() {
                for (i, b) in chunk.iter().enumerate() {
                    parity[i] ^= b;
                }
                let mut pkt = header(OP_VIDEO_CHUNK);
                pkt.extend_from_slice(&(idx as u16).to_be_bytes());
                pkt.extend_from_slice(chunk);
                if self.socket.send_to(&pkt, self.to).is_err() {
                    self.send_errors += 1;
                }
                if (idx + 1) % 4 == 0 && idx + 1 < total_chunks as usize {
                    std::thread::yield_now();
                }
            }
            let mut packets = total_chunks as usize;
            if total_chunks > 1 {
                let mut pkt = header(OP_FEC_PARITY);
                pkt.extend_from_slice(&(total_len as u32).to_be_bytes());
                pkt.extend_from_slice(&parity);
                if self.socket.send_to(&pkt, self.to).is_err() {
                    self.send_errors += 1;
                }
                packets += 1;
            }
            self.packets += packets;
            self.most_packets = self.most_packets.max(packets);
            self.send_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }

        fn finish(mut self, kernel_before: (u64, u64), fails: &mut Failures) {
            std::thread::sleep(Duration::from_millis(300));
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let done = self.receiver.take().and_then(|r| r.join().ok()).unwrap_or_default();
            let kernel = udp_buffer_errors();
            let arrived: HashMap<u32, (usize, u64, Instant, bool)> = done.into_iter().map(|(seq, len, sum, at, rec)| (seq, (len, sum, at, rec))).collect();
            let mut lost = Vec::new();
            let mut wrong = 0usize;
            let mut recovered = 0usize;
            let mut delays = Vec::new();
            for (seq, len, sum, at) in &self.sent {
                match arrived.get(seq) {
                    None => lost.push((*seq, *len)),
                    Some((l, s, when, rec)) => {
                        if l != len || s != sum {
                            wrong += 1;
                        }
                        recovered += *rec as usize;
                        delays.push(when.duration_since(*at).as_secs_f64() * 1000.0);
                    }
                }
            }
            delays.sort_by(|a, b| a.total_cmp(b));
            self.send_ms.sort_by(|a, b| a.total_cmp(b));
            println!("\n== loopback: the send loop's packets over UDP on 127.0.0.1 into the receive loop's reassembly ==");
            println!("  socket buffers: {}", self.buffers);
            println!(
                "  frames sent {}, arrived whole and authentic {}, lost {}, different bytes {}, rebuilt from parity {}; {} packets, most in one frame {}; send errors {}",
                self.sent.len(),
                delays.len() - wrong,
                lost.len(),
                wrong,
                recovered,
                self.packets,
                self.most_packets,
                self.send_errors
            );
            println!(
                "  time to send one frame ms: p50={:.3} p99={:.3} max={:.3}; send start to reassembled ms: p50={:.3} p99={:.3} max={:.3}",
                percentile(&self.send_ms, 0.5),
                percentile(&self.send_ms, 0.99),
                self.send_ms.last().copied().unwrap_or(0.0),
                percentile(&delays, 0.5),
                percentile(&delays, 0.99),
                delays.last().copied().unwrap_or(0.0)
            );
            println!(
                "  kernel UDP drops during the run (whole system): receive buffer {}, send buffer {}",
                kernel.0.saturating_sub(kernel_before.0),
                kernel.1.saturating_sub(kernel_before.1)
            );
            if !lost.is_empty() {
                println!("  lost frames (sequence number: bytes): {:?}", &lost[..lost.len().min(12)]);
                fails.add(0, format!("loopback: {} of {} frames did not arrive", lost.len(), self.sent.len()));
            }
            if wrong > 0 {
                fails.add(0, format!("loopback: {wrong} frames arrived with different bytes"));
            }
        }
    }

    // ------------------------------------------------------------------ independent decoder

    /// Decodes the dumped stream with ffmpeg and compares every picture with the frame it
    /// was encoded from. `units`: (input frame, IDR, Main) of each access unit, in order.
    fn ffmpeg_check(path: &str, size: (u32, u32), units: &[(usize, bool, bool)], pattern_spec: &str, pan: usize, fails: &mut Failures) -> Result<(), String> {
        println!("\n== independent decoder: ffmpeg on the dumped stream, every picture against its input ==");
        let mut child = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "h264", "-i", path, "-fps_mode", "passthrough", "-sws_flags", "accurate_rnd+full_chroma_int+bitexact", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let mut out = child.stdout.take().ok_or("ffmpeg: no stdout")?;
        let (w, h) = (size.0 as usize, size.1 as usize);
        let mut pattern = Pattern::new(pattern_spec, w, h, pan)?;
        let mut input = vec![255u8; w * h * 4];
        let mut picture = vec![0u8; w * h * 4];
        let mut next_input = 0usize;
        // [Constrained Baseline, Main] x [IDR, P]
        let mut psnr: [[Vec<f64>; 2]; 2] = Default::default();
        let mut pictures = 0usize;
        for &(frame, idr, main) in units {
            // The noise pattern is a sequence: every frame is drawn, in order.
            while next_input <= frame {
                pattern.fill(next_input, w, h, &mut input);
                next_input += 1;
            }
            if out.read_exact(&mut picture).is_err() {
                break;
            }
            pictures += 1;
            let sse: u64 = picture.chunks_exact(4).zip(input.chunks_exact(4)).map(|(o, i)| (0..3).map(|k| (o[k] as i32 - i[k] as i32).pow(2) as u64).sum::<u64>()).sum();
            let mse = sse as f64 / (w * h * 3) as f64;
            psnr[main as usize][!idr as usize].push(if mse > 0.0 { 10.0 * (255.0f64 * 255.0 / mse).log10() } else { 99.0 });
        }
        let mut extra = Vec::new();
        let _ = out.read_to_end(&mut extra);
        let mut errors = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut errors);
        }
        let _ = child.wait();
        // The timestamps are ffmpeg's guess (no container): its output complains about them
        // where a VA-API part follows an OpenH264 one (no frame rate in its SPS).
        let errors = errors.lines().filter(|l| !l.contains("non monotonically increasing dts")).collect::<Vec<_>>().join("\n");
        let extra_pictures = extra.len() / (w * h * 4);
        println!("  {} access units in, {} pictures out{}", units.len(), pictures + extra_pictures, if errors.trim().is_empty() { String::new() } else { format!("; ffmpeg said: {}", errors.trim().lines().take(4).collect::<Vec<_>>().join(" | ")) });
        if pictures + extra_pictures != units.len() {
            fails.add(0, format!("ffmpeg decoded {} pictures from {} access units", pictures + extra_pictures, units.len()));
        }
        if !errors.trim().is_empty() {
            fails.add(0, "ffmpeg reported errors in the stream".into());
        }
        for (main, name) in [(0, "Constrained Baseline"), (1, "Main")] {
            for (kind, label) in [(0, "IDR"), (1, "P  ")] {
                let v = &psnr[main][kind];
                if !v.is_empty() {
                    println!("  {name:20} {label} n={:4}{}", v.len(), psnr_line("", v));
                    // A picture that decoded wrong is far below anything the encoder produces.
                    if let Some(bad) = v.iter().find(|db| **db < 12.0) {
                        fails.add(0, format!("ffmpeg: a {name} {label} picture is {bad:.1} dB from its input"));
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ report

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn size_line(label: &str, sizes: &[usize]) -> String {
        if sizes.is_empty() {
            return format!("{label}: none");
        }
        let mut s: Vec<f64> = sizes.iter().map(|&b| b as f64 / 1000.0).collect();
        s.sort_by(|a, b| a.total_cmp(b));
        format!(
            "{label}: n={} min={:.1} p50={:.1} p95={:.1} max={:.1} mean={:.1} kB",
            s.len(),
            s[0],
            percentile(&s, 0.5),
            percentile(&s, 0.95),
            s[s.len() - 1],
            s.iter().sum::<f64>() / s.len() as f64
        )
    }

    #[derive(Default)]
    struct Segment {
        label: String,
        frames: usize,
        bytes: usize,
        idr: Vec<usize>,
        p: Vec<usize>,
        filler: usize,
        psnr_idr: Vec<f64>,
        psnr_p: Vec<f64>,
    }

    fn psnr_line(label: &str, values: &[f64]) -> String {
        if values.is_empty() {
            return String::new();
        }
        let min = values.iter().copied().fold(f64::MAX, f64::min);
        format!("  {label} PSNR mean={:.1} min={:.1} dB", values.iter().sum::<f64>() / values.len() as f64, min)
    }

    pub fn run() -> i32 {
        let args = match Args::parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}\n\n{HELP}");
                return 2;
            }
        };
        if args.has("help") {
            println!("{HELP}");
            return 0;
        }
        match run_with(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        }
    }

    /// `--rate-model`: the target after each loss report, with the loss that led to it.
    fn rate_model(spec: &str) -> Result<i32, String> {
        let kbps: Vec<u32> = spec.split(':').map(|v| v.parse().map_err(|_| format!("--rate-model: not a number: {v}"))).collect::<Result<_, _>>()?;
        let [max, content, link] = kbps[..] else { return Err("--rate-model needs MAX:CONTENT:LINK".into()) };
        let mut rate = RateControl::new(max * 1000);
        let t0 = Instant::now();
        let (mut heavy, mut moderate, mut line) = (0, 0, String::new());
        for second in 0..120u64 {
            let sent = (rate.current() / 1000).min(content).max(1);
            let loss = (sent.saturating_sub(link) as u64 * 1000 / sent as u64) as u16;
            heavy += (loss > 80) as u32;
            moderate += (loss > 20) as u32;
            if second < 40 {
                line += &format!("{:.1}({}) ", rate.current() as f64 / 1e6, loss / 10);
            }
            rate.update(Some(loss), t0 + Duration::from_secs(second));
        }
        println!("== rate control: maximum {max} kbps, pictures use up to {content}, the viewer's link carries {link} ==");
        println!("  of 120 s: {heavy} s above 8% loss, {moderate} s above 2%; target after 120 s: {:.1} Mbps", rate.current() as f64 / 1e6);
        println!("  target Mbps (loss %) in each of the first 40 s: {line}");
        Ok(0)
    }

    fn run_with(args: &Args) -> Result<i32, String> {
        if let Some(spec) = args.0.get("rate-model") {
            return rate_model(spec);
        }
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
        if let Some(mode) = args.0.get("fail") {
            std::env::set_var("LITECORD_VAAPI_TEST_FAIL", mode);
        }
        let kind = args.str("encoder", "vaapi");
        let mut size = parse_size(&args.str("size", "1080"))?;
        let fps = args.num("fps", 30)?.max(1) as u32;
        let frames = args.num("frames", 300)? as usize;
        let fast = args.has("fast");
        let interval = Duration::from_micros(match args.num("interval-ms", 0)? {
            0 => 1_000_000 / fps as u64,
            ms => ms * 1000,
        });
        let verify = !args.has("no-verify");
        let verbose = args.has("verbose");
        let pattern_spec = args.str("pattern", "text");
        let pan = args.num("pan", 0)? as usize;
        let max_none = args.num("max-none", 0)? as usize;

        let bitrate_spec = args.str("bitrate", "8000");
        let bitrates: Vec<(usize, u32)> = if bitrate_spec.contains(':') {
            parse_pairs(&bitrate_spec)?.into_iter().map(|(f, v)| v.parse::<u32>().map(|v| (f, v * 1000)).map_err(|_| format!("bad bitrate {v}"))).collect::<Result<_, _>>()?
        } else {
            vec![(0, bitrate_spec.parse::<u32>().map_err(|_| format!("bad bitrate {bitrate_spec}"))? * 1000)]
        };
        let key_spec = args.str("keyframes", "2s");
        let key_frames: Vec<usize> = if matches!(key_spec.as_str(), "2s" | "none") || key_spec.starts_with("every:") {
            Vec::new()
        } else {
            key_spec.split(',').map(|f| f.parse().map_err(|_| format!("bad keyframe list {key_spec}"))).collect::<Result<_, _>>()?
        };
        let key_every: usize = match key_spec.strip_prefix("every:") {
            Some(n) => n.parse().map_err(|_| format!("bad --keyframes {key_spec}"))?,
            None => 0,
        };
        let min_qp = match args.0.get("min-qp") {
            Some(v) => Some(v.parse::<u32>().map_err(|_| format!("--min-qp: not a number: {v}"))?),
            None => None,
        };
        let profile_spec = args.str("profile", "compatible");
        let mode = match profile_spec.as_str() {
            "compatible" => StreamProfile::Compatible,
            "efficient" => StreamProfile::Efficient,
            "auto" => StreamProfile::Auto,
            _ => return Err(format!("--profile: compatible, efficient or auto, not {profile_spec}")),
        };
        let viewers_spec = args.str("viewers", "0:m");
        let viewer_steps: HashMap<usize, Vec<char>> = parse_viewers(&viewers_spec)?.into_iter().collect();
        let gaps: HashMap<usize, u64> = parse_pairs(&args.str("gaps", ""))?.into_iter().map(|(f, v)| v.parse().map(|v| (f, v)).map_err(|_| format!("bad gap {v}"))).collect::<Result<_, _>>()?;
        let resizes: HashMap<usize, (u32, u32)> = parse_pairs(&args.str("resize", ""))?.into_iter().map(|(f, v)| parse_size(&v).map(|s| (f, s))).collect::<Result<_, _>>()?;

        println!(
            "== {kind}: {}x{} {fps} fps, {frames} frames, {}, pattern {pattern_spec}, bitrate {bitrate_spec} kbps, keyframes {key_spec}, profile {profile_spec}, viewers {viewers_spec}, min-qp {} ==",
            size.0,
            size.1,
            if fast { "as fast as possible".to_string() } else { format!("one frame per {:.1} ms", interval.as_secs_f64() * 1000.0) },
            min_qp.map(|q| q.to_string()).unwrap_or_else(|| "default".into())
        );
        let t_new = Instant::now();
        let mut enc = make_encoder_with(&kind, fps, min_qp).map_err(|e| format!("encoder not created: {e}"))?;
        println!("encoder: {} (hardware: {}), created in {:.0} ms", enc.name(), enc.is_hardware_accelerated(), t_new.elapsed().as_secs_f64() * 1000.0);
        let mut current_name = enc.name();
        let mut fails = Failures::default();
        if enc.encode(&[0u8; 64], size.0, size.1).is_some() {
            fails.add(0, "a buffer shorter than the picture was encoded".into());
        }

        let mut receivers = Receivers::new();
        receivers.fps = fps;
        let mut pattern = Pattern::new(&pattern_spec, size.0 as usize, size.1 as usize, pan)?;
        let mut frame = vec![255u8; (size.0 * size.1 * 4) as usize];
        let use_ffmpeg = args.has("ffmpeg");
        if use_ffmpeg && !resizes.is_empty() {
            return Err("--ffmpeg cannot follow --resize".into());
        }
        let dump_path = match args.0.get("dump") {
            Some(path) => Some(path.clone()),
            None if use_ffmpeg => Some(std::env::temp_dir().join(format!("test_vaapi_encoder-{}.h264", std::process::id())).to_string_lossy().into_owned()),
            None => None,
        };
        let mut dump = match dump_path.as_ref() {
            Some(path) => Some(std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?),
            None => None,
        };
        // (input frame, IDR, Main) of every access unit written.
        let mut units: Vec<(usize, bool, bool)> = Vec::new();
        let kernel_before = udp_buffer_errors();
        let mut loopback = match args.0.get("loopback") {
            Some(spec) => Some(Loopback::start(spec)?),
            None => None,
        };

        // Who watches, by the send loop's own rules, on a clock of one frame per 1/fps.
        let mut viewers = Viewers::new();
        let mut choice = ProfileChoice::new(mode);
        let mut tokens: Vec<char> = Vec::new();
        let clock = Instant::now();
        let mut profile_changes: Vec<(usize, bool)> = Vec::new();
        let mut not_watched = 0usize;

        let mut segments: Vec<Segment> = Vec::new();
        let mut latencies: Vec<f64> = Vec::new();
        let mut encode_cpu = 0.0;
        let mut nones = 0usize;
        let mut set_calls = 0usize;
        let mut forced = 0usize;
        let mut unrequested_idr: Vec<usize> = Vec::new();
        let mut slowest = (0usize, 0.0f64);
        let mut slept = Duration::ZERO;

        let cpu_start = cpu_secs();
        let started = Instant::now();
        let mut last_idr_request = Instant::now();
        let mut next_due = Instant::now();
        // Set when the next output must be a keyframe, with the reason.
        let mut must_be_idr: Option<&str> = Some("first frame");

        for n in 0..frames {
            if let Some(ms) = gaps.get(&n) {
                std::thread::sleep(Duration::from_millis(*ms));
                slept += Duration::from_millis(*ms);
                next_due = Instant::now();
            }
            if let Some(new_size) = resizes.get(&n) {
                size = *new_size;
                pattern = Pattern::new(&pattern_spec, size.0 as usize, size.1 as usize, pan)?;
                frame = vec![255u8; (size.0 * size.1 * 4) as usize];
                must_be_idr = Some("size change");
                println!("  frame {n}: size is now {}x{}", size.0, size.1);
            }
            pattern.fill(n, size.0 as usize, size.1 as usize, &mut frame);

            // Watch requests: a change in the list, and every viewer's renewal once a second.
            let now = clock + Duration::from_secs_f64(n as f64 / fps as f64);
            let mut new_viewer = false;
            if let Some(list) = viewer_steps.get(&n) {
                // A browser that connects asks for a keyframe (web_share).
                new_viewer |= list.iter().filter(|t| **t == 'w').count() > tokens.iter().filter(|t| **t == 'w').count();
                for i in list.len()..tokens.len() {
                    viewers.unwatch(i as u64 + 1);
                }
                for (i, t) in list.iter().enumerate() {
                    if *t == 'w' && tokens.get(i).is_some_and(|old| *old != 'w') {
                        viewers.unwatch(i as u64 + 1);
                    }
                }
                tokens = list.clone();
                println!("  frame {n}: viewers are now {}", if tokens.is_empty() { "-".to_string() } else { tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>().join("+") });
            }
            if viewer_steps.contains_key(&n) || n % fps as usize == 0 {
                for (i, t) in tokens.iter().enumerate().filter(|(_, t)| **t != 'w') {
                    let uid = i as u64 + 1;
                    // What watch::on_watch does with a request.
                    if let Verdict::Accepted { new } = viewers.watch(uid, [uid as u8; 32], std::net::SocketAddr::from(([10, 0, 0, uid as u8], 50005)), true, now) {
                        viewers.set_caps(uid, if *t == 'm' { CAP_H264_MAIN } else { 0 });
                        new_viewer |= new;
                    }
                }
            }
            // What the send loop does each turn.
            viewers.expire(now);
            let web_viewers = tokens.iter().filter(|t| **t == 'w').count();
            let watched = viewers.len() > 0 || web_viewers > 0;
            let all_decode_main = viewers.all_decode_main() && web_viewers == 0;
            let efficient = choice.update(watched, all_decode_main, now);
            if !watched {
                not_watched += 1;
                continue;
            }
            let main_ok = mode == StreamProfile::Efficient || (mode == StreamProfile::Auto && all_decode_main);
            if receivers.main() && !efficient {
                // No keyframe is asked for; the encoder must start one by itself.
                must_be_idr = must_be_idr.or(Some("back to Constrained Baseline"));
            }

            // The same steps as the screen-capture-tx thread.
            let target = bitrates.iter().rev().find(|(f, _)| *f <= n).map(|(_, b)| *b).unwrap_or(8_000_000);
            receivers.bitrate_bps = target;
            if segments.is_empty() {
                segments.push(Segment { label: format!("{} kbps", target / 1000), ..Default::default() });
            }
            enc.set_efficient_profile(efficient);
            if enc.get_bitrate_bps() != target {
                enc.set_bitrate_bps(target);
                set_calls += 1;
                if enc.is_hardware_accelerated() && enc.get_bitrate_bps() != target {
                    fails.add(n, format!("get_bitrate_bps() is {} after set_bitrate_bps({target})", enc.get_bitrate_bps()));
                }
            }
            let force = new_viewer && n > 0
                || match key_spec.as_str() {
                    "none" => false,
                    "2s" if fast => n > 0 && n % (2 * fps as usize) == 0,
                    "2s" => last_idr_request.elapsed() >= Duration::from_millis(2000),
                    _ if key_every > 0 => n > 0 && n % key_every == 0,
                    _ => key_frames.contains(&n),
                };
            if force {
                last_idr_request = Instant::now();
                enc.force_intra_frame();
                forced += 1;
                must_be_idr = Some("force_intra_frame");
            }

            let cpu0 = cpu_secs();
            let t0 = Instant::now();
            let out = enc.encode(&frame, size.0, size.1);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            encode_cpu += cpu_secs() - cpu0;
            latencies.push(ms);
            if ms > slowest.1 {
                slowest = (n, ms);
            }

            if enc.name() != current_name {
                println!("  frame {n}: encoder is now \"{}\" (hardware: {})", enc.name(), enc.is_hardware_accelerated());
                current_name = enc.name();
                must_be_idr = Some("encoder switch");
            }
            let hardware = enc.is_hardware_accelerated();
            match out {
                None => {
                    segments.last_mut().ok_or("no segment")?.frames += 1;
                    nones += 1;
                    if verbose || nones <= 10 {
                        println!("  frame {n}: encode() returned None after {ms:.1} ms");
                    }
                    // Whatever was lost, the stream may only go on from a keyframe.
                    must_be_idr = must_be_idr.or(Some("picture lost before"));
                }
                Some(au) if au.is_empty() => fails.add(n, "encode() returned Some(empty)".into()),
                Some(au) => {
                    let input = (args.has("psnr") && size.0 % 2 == 0).then_some(frame.as_slice());
                    let (was_main, first_unit) = (receivers.main(), receivers.sps.is_none());
                    let (types, filler, psnr) = receivers.check(n, &au, (size.0 & !1, size.1 & !1), hardware, verify, main_ok, input, &mut fails);
                    let idr = types.contains(&5);
                    let main = receivers.main();
                    if main != was_main && !first_unit {
                        println!("  frame {n}: the stream is now {}{}", if main { "Main" } else { "Constrained Baseline" }, if force { " (on a keyframe that was asked for)" } else { "" });
                        profile_changes.push((n, main));
                        if main && !force {
                            fails.add(n, "the stream went to Main without a keyframe being asked for".into());
                        }
                    }
                    if hardware && idr && force && efficient && !main && args.str("fail", "") != "main" && !args.has("main-may-fail") {
                        fails.add(n, "a keyframe was asked for while every viewer decodes Main, and it is not Main".into());
                    }
                    // Sizes are reported per bitrate and profile.
                    let label = format!("{} kbps{}", target / 1000, if main { ", Main" } else if mode != StreamProfile::Compatible { ", Constrained Baseline" } else { "" });
                    if segments.last().is_some_and(|s| s.label != label) {
                        if segments.last().is_some_and(|s| s.frames == 0) {
                            segments.pop();
                        }
                        segments.push(Segment { label, ..Default::default() });
                    }
                    let segment = segments.last_mut().ok_or("no segment")?;
                    segment.frames += 1;
                    segment.filler += filler;
                    if let Some(db) = psnr {
                        if idr { segment.psnr_idr.push(db) } else { segment.psnr_p.push(db) }
                    }
                    match (idr, must_be_idr) {
                        (false, Some(why)) => fails.add(n, format!("expected an IDR ({why}), got NAL types {types:?}")),
                        (true, None) => unrequested_idr.push(n),
                        _ => {}
                    }
                    must_be_idr = None;
                    if verbose {
                        let db = psnr.map(|db| format!("  {db:.1} dB")).unwrap_or_default();
                        println!("  frame {n:4}: {:7} bytes  {:5.1} ms  NAL {types:?}{}{db}", au.len(), ms, if idr { "  IDR" } else { "" });
                    }
                    segment.bytes += au.len();
                    if idr { segment.idr.push(au.len()) } else { segment.p.push(au.len()) }
                    if let Some(f) = dump.as_mut() {
                        f.write_all(&au).map_err(|e| format!("dump: {e}"))?;
                        units.push((n, idr, main));
                    }
                    if let Some(l) = loopback.as_mut() {
                        l.send(&au);
                    }
                }
            }

            if !fast {
                next_due += interval;
                let now = Instant::now();
                if next_due > now {
                    std::thread::sleep(next_due - now);
                } else {
                    next_due = now;
                }
            }
        }
        let wall = started.elapsed().as_secs_f64();
        let cpu_total = cpu_secs() - cpu_start;
        let name_at_end = enc.name();

        // ---- report
        let video_secs = frames as f64 / fps as f64;
        let encode_secs = latencies.iter().sum::<f64>() / 1000.0;
        let mut sorted = latencies.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        println!("\n== results ==");
        println!("encoder at the end: {name_at_end}");
        println!(
            "frames: {frames} in {wall:.2} s wall ({:.1} fps, {:.2} s of it scheduled gaps); encode() alone: {:.1} fps",
            frames as f64 / (wall - slept.as_secs_f64()).max(1e-9),
            slept.as_secs_f64(),
            frames as f64 / encode_secs.max(1e-9)
        );
        println!(
            "encode latency ms: p50={:.2} p99={:.2} max={:.2} (frame {}), first frame {:.1}",
            percentile(&sorted, 0.5),
            percentile(&sorted, 0.99),
            slowest.1,
            slowest.0,
            latencies.first().copied().unwrap_or(0.0)
        );
        println!(
            "CPU: {:.3} cpu-s per second of video inside encode() (whole process, all threads); {:.3} for the whole harness{}",
            encode_cpu / video_secs,
            cpu_total / video_secs,
            if verify { " (includes content generation and the decode checks)" } else { " (includes content generation)" }
        );
        for s in &segments {
            println!(
                "[asked for {}] {} frames, {:.0} kbps at {fps} fps nominal",
                s.label,
                s.frames,
                s.bytes as f64 * 8.0 / 1000.0 / (s.frames as f64 / fps as f64).max(1e-9)
            );
            println!("    {}{}", size_line("IDR", &s.idr), psnr_line("", &s.psnr_idr));
            println!("    {}{}", size_line("P  ", &s.p), psnr_line("", &s.psnr_p));
            if s.filler > 0 {
                println!("    filler data (NAL type 12): {:.1} kB, {:.1}% of the bytes", s.filler as f64 / 1000.0, 100.0 * s.filler as f64 / s.bytes.max(1) as f64);
            }
        }
        println!(
            "keyframes forced: {forced}; IDRs nobody asked for: {} {:?}; set_bitrate_bps calls: {set_calls}; encode() -> None: {nones}",
            unrequested_idr.len(),
            &unrequested_idr[..unrequested_idr.len().min(12)]
        );
        if verify {
            println!(
                "receiver: {} of {} access units decoded at the input size ({} Main access units not given to it; {} frames with nobody watching not encoded)",
                receivers.decoded,
                frames - nones - not_watched - receivers.main_units,
                receivers.main_units,
                not_watched
            );
        }
        if !profile_changes.is_empty() {
            println!("profile changes: {}", profile_changes.iter().map(|(n, main)| format!("frame {n} -> {}", if *main { "Main" } else { "Constrained Baseline" })).collect::<Vec<_>>().join(", "));
        }
        if let Some(l) = loopback.take() {
            l.finish(kernel_before, &mut fails);
        }
        drop(dump);
        if let (true, Some(path)) = (use_ffmpeg, dump_path.as_ref()) {
            ffmpeg_check(path, (size.0 & !1, size.1 & !1), &units, &pattern_spec, pan, &mut fails)?;
            if !args.has("dump") {
                let _ = std::fs::remove_file(path);
            }
        }
        if nones > max_none {
            fails.add(frames, format!("{nones} encode() calls returned None (allowed: {max_none})"));
        }

        // The stream's encoder is still alive: two instances at once.
        if !args.has("no-colour") {
            colour_check(&kind, (size.0 & !1, size.1 & !1), fps, &mut fails);
        }
        let t_drop = Instant::now();
        match args.0.get("exit-race") {
            Some(us) => {
                let us = us.parse().map_err(|_| format!("--exit-race: not a number: {us}"))?;
                std::thread::spawn(move || drop(enc));
                std::thread::sleep(Duration::from_micros(us));
            }
            None => drop(enc),
        }
        // Only true when something in this process called gst::init.
        let gst_up = unsafe { gstreamer::ffi::gst_is_initialized() } != 0;
        println!("\ndrop took {:.1} ms; GStreamer initialised in this process: {gst_up}", t_drop.elapsed().as_secs_f64() * 1000.0);

        if fails.count == 0 {
            println!("\nRESULT: PASS");
            Ok(0)
        } else {
            println!("\nRESULT: FAIL ({} hard check(s) failed)", fails.count);
            Ok(1)
        }
    }
}
