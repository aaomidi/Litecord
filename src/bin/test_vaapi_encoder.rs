//! Drives a screen-share H.264 encoder through the `VideoEncoder` trait only, the way the
//! "screen-capture-tx" thread does, and checks every access unit against what Litecord's
//! receivers need. Run with `--help` for the options.

#[cfg(not(all(target_os = "linux", feature = "vaapi")))]
fn main() {
    eprintln!("test_vaapi_encoder: needs Linux and the `vaapi` cargo feature");
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
fn main() {
    std::process::exit(harness::run());
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod harness {
    use h264_reader::nal::sps::SeqParameterSet;
    use h264_reader::nal::{Nal, RefNal};
    use litecord::encoder::vaapi::VaapiEncoder;
    use litecord::encoder::{create_best_encoder, OpenH264Encoder, VideoEncoder};
    use litecord::screen_capture::hw_decode::h264_state::{nal_ranges, H264State, StateError, StreamInfo};
    use litecord::screen_capture::video_pipeline::{decode_video_frame, Decoded, StreamDecoders};
    use openh264::formats::YUVSource;
    use std::collections::HashMap;
    use std::io::Write;
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
  --keyframes K    2s (default: like Litecord, every 2 s) | none | every:N | FRAME,FRAME,...
  --gaps SPEC      FRAME:MS,... sleep MS before those frames
  --resize SPEC    FRAME:SIZE,... change the picture size from those frames on
  --fail MODE      init | after:N | stall:N  (sets LITECORD_VAAPI_TEST_FAIL: break the VA path
                   at construction, from GPU frame N on at once, or with real timeouts)
  --enc-props STR  extra vah264enc properties (sets LITECORD_VAAPI_ENC_PROPS), experiments only
  --max-none N     encode() calls allowed to return None (default 0)
  --dump FILE      write the Annex B stream
  --no-verify      skip decoding and parsing (timing runs)
  --psnr           also report the PSNR of what the receiver shows against the input
  --no-colour      skip the solid-colour check
  --verbose        one line per access unit
Exit status 1 when a hard check fails, 2 when the encoder cannot be created.";

    struct Args(HashMap<String, String>);

    impl Args {
        fn parse() -> Result<Self, String> {
            let flags = ["fast", "no-verify", "no-colour", "psnr", "verbose", "help"];
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
            let stop = x + (x_end - x) * (40 + rng.below(60) as usize) / 100;
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
        decoded: usize,
    }

    const PEER: u64 = 0x7E57;

    impl Receivers {
        fn new() -> Self {
            Self { decoders: StreamDecoders::default(), state: H264State::new(), session: None, sps: None, prev_frame_num: None, decoded: 0 }
        }

        /// Checks one access unit; returns its NAL types, its filler bytes and, when `input`
        /// (the BGRA frame) is given, the PSNR of the picture the receiver shows.
        fn check(&mut self, n: usize, au: &[u8], size: (u32, u32), hardware: bool, verify: bool, input: Option<&[u8]>, fails: &mut Failures) -> (Vec<u8>, usize, Option<f64>) {
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
                                "  SPS at frame {n}: profile_idc={} constraint_flags={:#04x} level_idc={} size={}x{} poc={} max_refs={} colour={}",
                                sps.profile_idc,
                                sps.constraint_flags,
                                sps.level_idc,
                                sps.size.0,
                                sps.size.1,
                                sps.poc_type,
                                sps.refs,
                                match sps.colour {
                                    None => "not declared".to_string(),
                                    Some((full, None)) => format!("range={} only", if full { "full" } else { "limited" }),
                                    Some((full, Some((p, t, m)))) => format!("range={} primaries={p} transfer={t} matrix={m}", if full { "full" } else { "limited" }),
                                }
                            );
                        }
                        if sps.size != size {
                            fails.add(n, format!("SPS size {}x{}, input {}x{}", sps.size.0, sps.size.1, size.0, size.1));
                        }
                        if sps.profile_idc != 66 || sps.constraint_flags & 0x40 == 0 {
                            fails.add(n, format!("not Constrained Baseline: profile_idc={} flags={:#04x}", sps.profile_idc, sps.constraint_flags));
                        }
                        if hardware && sps.colour != Some((false, Some((6, 6, 6)))) {
                            fails.add(n, format!("SPS colours {:?}, expected limited range 6/6/6", sps.colour));
                        }
                        if matches!(sps.colour, Some((true, _)) | Some((_, Some((_, _, 1))))) {
                            fails.add(n, format!("SPS declares full range or BT.709: {:?}", sps.colour));
                        }
                        self.sps = Some(sps);
                    }
                }
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
                } else {
                    None
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

    fn run_with(args: &Args) -> Result<i32, String> {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
        if let Some(mode) = args.0.get("fail") {
            std::env::set_var("LITECORD_VAAPI_TEST_FAIL", mode);
        }
        if let Some(props) = args.0.get("enc-props") {
            std::env::set_var("LITECORD_VAAPI_ENC_PROPS", props);
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
        let gaps: HashMap<usize, u64> = parse_pairs(&args.str("gaps", ""))?.into_iter().map(|(f, v)| v.parse().map(|v| (f, v)).map_err(|_| format!("bad gap {v}"))).collect::<Result<_, _>>()?;
        let resizes: HashMap<usize, (u32, u32)> = parse_pairs(&args.str("resize", ""))?.into_iter().map(|(f, v)| parse_size(&v).map(|s| (f, s))).collect::<Result<_, _>>()?;

        println!(
            "== {kind}: {}x{} {fps} fps, {frames} frames, {}, pattern {pattern_spec}, bitrate {bitrate_spec} kbps, keyframes {key_spec} ==",
            size.0,
            size.1,
            if fast { "as fast as possible".to_string() } else { format!("one frame per {:.1} ms", interval.as_secs_f64() * 1000.0) }
        );
        let t_new = Instant::now();
        let mut enc = make_encoder(&kind, fps).map_err(|e| format!("encoder not created: {e}"))?;
        println!("encoder: {} (hardware: {}), created in {:.0} ms", enc.name(), enc.is_hardware_accelerated(), t_new.elapsed().as_secs_f64() * 1000.0);
        let mut current_name = enc.name();
        let mut fails = Failures::default();
        if enc.encode(&[0u8; 64], size.0, size.1).is_some() {
            fails.add(0, "a buffer shorter than the picture was encoded".into());
        }

        let mut receivers = Receivers::new();
        let mut pattern = Pattern::new(&pattern_spec, size.0 as usize, size.1 as usize, pan)?;
        let mut frame = vec![255u8; (size.0 * size.1 * 4) as usize];
        let mut dump = match args.0.get("dump") {
            Some(path) => Some(std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?),
            None => None,
        };

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

            // The same three steps as the screen-capture-tx thread.
            let target = bitrates.iter().rev().find(|(f, _)| *f <= n).map(|(_, b)| *b).unwrap_or(8_000_000);
            if segments.last().map(|s| s.label != format!("{} kbps", target / 1000)).unwrap_or(true) {
                segments.push(Segment { label: format!("{} kbps", target / 1000), ..Default::default() });
            }
            if enc.get_bitrate_bps() != target {
                enc.set_bitrate_bps(target);
                set_calls += 1;
                if enc.is_hardware_accelerated() && enc.get_bitrate_bps() != target {
                    fails.add(n, format!("get_bitrate_bps() is {} after set_bitrate_bps({target})", enc.get_bitrate_bps()));
                }
            }
            let force = match key_spec.as_str() {
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
            let segment = segments.last_mut().ok_or("no segment")?;
            segment.frames += 1;
            match out {
                None => {
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
                    let (types, filler, psnr) = receivers.check(n, &au, (size.0 & !1, size.1 & !1), hardware, verify, input, &mut fails);
                    let idr = types.contains(&5);
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
            println!("receiver: {} of {} access units decoded at the input size", receivers.decoded, frames - nones);
        }
        if nones > max_none {
            fails.add(frames, format!("{nones} encode() calls returned None (allowed: {max_none})"));
        }

        // The stream's encoder is still alive: two instances at once.
        if !args.has("no-colour") {
            colour_check(&kind, (size.0 & !1, size.1 & !1), fps, &mut fails);
        }
        let t_drop = Instant::now();
        drop(enc);
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
