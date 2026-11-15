//! Feeds H.264 access units through the app's receive entry point
//! (`video_pipeline::decode_video_frame` with `StreamDecoders`), the way the
//! "video-decoder-worker" thread does, on the GPU (VA-API) path, the CPU (OpenH264) path,
//! or both side by side with a pixel comparison. Run with `--help` for the options.

#[cfg(not(all(target_os = "linux", feature = "vaapi")))]
fn main() {
    eprintln!("test_vaapi_decoder: needs Linux and the `vaapi` cargo feature");
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
    use litecord::encoder::{OpenH264Encoder, VideoEncoder};
    use litecord::screen_capture::hw_decode::h264_state::nal_ranges;
    use litecord::screen_capture::video_pipeline::{decode_video_frame, fit_within, nv12_to_rgba_scaled, set_stream_view_size, Colour, Decoded, StreamDecoders, RX_GPU_FRAMES};
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const HELP: &str = "\
test_vaapi_decoder [options]
  --input LIST     H.264 Annex B files, or directories holding one access unit per file,
                   comma separated; stream k decodes input k (wrapping around)
  --encoder E      instead of --input: encode a moving test picture first with
                   vaapi | openh264 (through the VideoEncoder trait)
  --size S         with --encoder: 480 | 720 | 1080 (default) | WxH
  --frames N       frames to encode (default 300), or access units to take from each input
  --keyint N       with --encoder: a keyframe every N frames (default 2 s of frames)
  --fps N          rate of the stream: pacing, and the length of a second of video (default 60)
  --fast           no pacing (flat out: gives the highest rate a path sustains)
  --path P         gpu | cpu | both (default) | auto. gpu and cpu run one timed pass; both
                   runs the two and then compares them picture by picture; auto leaves
                   LITECORD_VAAPI_DECODE alone, like the app
  --no-compare     with both: skip the comparison pass
  --streams N      N streams at once through one StreamDecoders, interleaved the way the
                   app's decode thread gets them (default 1)
  --threads        with --streams: one thread and one StreamDecoders per stream instead
  --drop LIST      access units (numbers, comma separated) that are lost on the way
  --start N        begin at access unit N (joining mid-stream)
  --release LIST   at these access units nobody watches: the GPU decoders are released
                   (StreamDecoders::release_gpu) and the streams wait for a keyframe, as
                   the decode thread does; they are created again by the next keyframe
  --view SPEC      FRAME:WxH,... size the streams are shown at from those frames on
                   (set_stream_view_size; 0x0 = the stream's own size)
  --fail MODE      init | after:N | stall:N | stall-once:N (sets LITECORD_VAAPI_DEC_TEST_FAIL:
                   break the VA path at construction, from picture N on at once, from picture
                   N on with a real timeout, or picture N alone with a real timeout)
  --expect P       gpu | cpu | fallback: fail unless the gpu/auto pass got its pictures from
                   that path (fallback: the GPU first, then the CPU)
  --flat           flat colours and gradients: every pixel away from an edge (the CPU
                   picture is smooth for 4 pixels around it) must match within 3
  --matrix709      the stream declares BT.709 limited range: compare the GPU pictures with
                   the CPU decoder's YUV converted with BT.709 (what the Windows GPU path
                   shows) instead of the app's CPU path, which always converts with BT.601
  --verbose        one line per access unit that is not a plain picture
Exit status 1 when a hard check fails, 2 when the input cannot be prepared.";

    // ------------------------------------------------------------------ arguments

    struct Args(HashMap<String, String>);

    impl Args {
        fn parse() -> Result<Self, String> {
            let flags = ["fast", "no-compare", "threads", "flat", "matrix709", "verbose", "help"];
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

    fn cpu_secs() -> f64 {
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut ru);
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
            tv(ru.ru_utime) + tv(ru.ru_stime)
        }
    }

    // ------------------------------------------------------------------ input

    fn nal_types(au: &[u8]) -> Vec<u8> {
        nal_ranges(au).iter().map(|r| au[r.start] & 0x1f).collect()
    }

    fn has_idr(au: &[u8]) -> bool {
        nal_types(au).contains(&5)
    }

    fn has_slice(au: &[u8]) -> bool {
        nal_types(au).iter().any(|t| matches!(t, 1 | 5))
    }

    /// What the decode thread looks for while it waits for a keyframe (manager.rs).
    fn has_idr_or_sps(au: &[u8]) -> bool {
        au.windows(5).any(|w| (w[..4] == [0, 0, 0, 1] && (w[4] & 0x1F == 7 || w[4] & 0x1F == 5)) || (w[..3] == [0, 0, 1] && (w[3] & 0x1F == 7 || w[3] & 0x1F == 5)))
    }

    /// Visible size the SPS in this access unit declares.
    fn sps_size(au: &[u8]) -> Option<(u32, u32)> {
        let r = nal_ranges(au).into_iter().find(|r| au[r.start] & 0x1f == 7)?;
        let sps = SeqParameterSet::from_bits(RefNal::new(&au[r], &[], true).rbsp_bits()).ok()?;
        sps.pixel_dimensions().ok()
    }

    /// Splits an Annex B stream into access units: a new one starts at the first parameter
    /// set, SEI or delimiter after a slice, or at a slice that starts a picture.
    fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let (mut start, mut seen_slice) = (0usize, false);
        for r in nal_ranges(data) {
            let t = data[r.start] & 0x1f;
            let mut sc = r.start - 3;
            if sc > 0 && data[sc - 1] == 0 {
                sc -= 1;
            }
            let boundary = match t {
                1 | 5 => seen_slice && data.get(r.start + 1).map(|b| b & 0x80 != 0).unwrap_or(false),
                6..=9 => seen_slice,
                _ => false,
            };
            if boundary {
                out.push(data[start..sc].to_vec());
                start = sc;
                seen_slice = false;
            }
            if matches!(t, 1 | 5) {
                seen_slice = true;
            }
        }
        if start < data.len() {
            out.push(data[start..].to_vec());
        }
        out
    }

    fn load_input(path: &str) -> Result<Vec<Vec<u8>>, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
        if meta.is_dir() {
            let mut files: Vec<_> = std::fs::read_dir(path).map_err(|e| format!("{path}: {e}"))?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
            files.sort();
            return files.iter().map(|f| std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))).collect();
        }
        Ok(split_access_units(&std::fs::read(path).map_err(|e| format!("{path}: {e}"))?))
    }

    /// A moving picture with colours, gradients, hard edges and detail up to every border.
    fn test_frame(i: usize, w: usize, h: usize, out: &mut [u8]) {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                let (b, g, r) = (((x + 2 * i) / 3) as u8, ((y + i) / 2) as u8, ((x + y) / 4 + 3 * i) as u8);
                out[o..o + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        // Scrolling "text": small dark and light marks on a light band.
        let band = (h / 3, 2 * h / 3);
        for y in band.0..band.1 {
            for x in 0..w {
                let o = (y * w + x) * 4;
                let (cx, cy) = ((x + 5 * i) / 3, (y + 2 * i) / 4);
                let hash = (cx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (cy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
                let v = if (hash >> 17) & 3 == 0 { 30 } else { 240 };
                out[o..o + 3].copy_from_slice(&[v, v, v]);
            }
        }
        // Flat colour boxes, and noise in the corner that touches the right and bottom edges.
        let boxes: [[u8; 3]; 6] = [[40, 40, 220], [40, 200, 40], [220, 60, 40], [30, 200, 230], [200, 40, 200], [128, 128, 128]];
        for (k, bgr) in boxes.iter().enumerate() {
            let (bx, by) = (20 + k * (w / 7), 12 + (i * 2) % 40);
            for y in by..(by + h / 8).min(h) {
                for x in bx..(bx + w / 9).min(w) {
                    let o = (y * w + x) * 4;
                    out[o..o + 3].copy_from_slice(bgr);
                }
            }
        }
        for y in h - h / 6..h {
            for x in w - w / 6..w {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let o = (y * w + x) * 4;
                let n = (seed >> 24) as u8;
                out[o..o + 3].copy_from_slice(&[n, n.wrapping_mul(3), n.wrapping_add((x + y) as u8)]);
            }
        }
    }

    fn encode_input(kind: &str, (w, h): (u32, u32), fps: u32, frames: usize, keyint: usize) -> Result<Vec<Vec<u8>>, String> {
        let mut encoder: Box<dyn VideoEncoder> = match kind {
            "vaapi" => Box::new(VaapiEncoder::try_new(fps, true)?),
            "openh264" => Box::new(OpenH264Encoder::new(fps, true)?),
            _ => return Err(format!("unknown encoder {kind}")),
        };
        let mut frame = vec![0u8; (w * h * 4) as usize];
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            test_frame(i, w as usize, h as usize, &mut frame);
            if i % keyint.max(1) == 0 {
                encoder.force_intra_frame();
            }
            match encoder.encode(&frame, w, h) {
                Some(au) if !au.is_empty() => out.push(au),
                _ => return Err(format!("{}: no access unit for frame {i}", encoder.name())),
            }
        }
        println!("encoded {frames} frames {w}x{h} with {} ({} kB)", encoder.name(), out.iter().map(|a| a.len()).sum::<usize>() / 1000);
        Ok(out)
    }

    // ------------------------------------------------------------------ one stream

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Path {
        Gpu,
        Cpu,
        Auto,
    }

    #[derive(Default)]
    struct Stats {
        offered: usize,
        dropped: usize,
        skipped_waiting: usize,
        pictures: usize,
        gpu_pictures: usize,
        cpu_pictures: usize,
        /// Path of the first and the last picture, and pictures from the GPU after one from the CPU.
        first_gpu: Option<bool>,
        last_gpu: Option<bool>,
        gpu_after_cpu: usize,
        nothing_for_picture: usize,
        failed: usize,
        need_keyframe: usize,
        shown_after_gap: usize,
        wrong_size: Vec<String>,
        recovered: Vec<String>,
        latency_ms: Vec<f64>,
        first_picture_ms: Option<f64>,
        slowest_other_ms: f64,
        behind: usize,
    }

    /// One received stream and the decode thread's bookkeeping for it.
    struct Stream {
        peer: u64,
        aus: Arc<Vec<Vec<u8>>>,
        waiting_for_idr: bool,
        /// A picture is missing (lost, or the stream was joined mid-way): nothing may be
        /// shown until an access unit with an IDR.
        gap_since: Option<usize>,
        size: Option<(u32, u32)>,
        stats: Stats,
    }

    impl Stream {
        fn new(peer: u64, aus: Arc<Vec<Vec<u8>>>) -> Self {
            Self { peer, aus, waiting_for_idr: false, gap_since: None, size: None, stats: Stats::default() }
        }
    }

    struct Plan {
        fps: u32,
        fast: bool,
        start: usize,
        frames: usize,
        drops: Vec<usize>,
        releases: Vec<usize>,
        views: Vec<(usize, (u32, u32))>,
        verbose: bool,
    }

    /// What the decode thread does with one access unit of one stream. Returns the picture.
    fn step(decoders: &mut StreamDecoders, s: &mut Stream, i: usize, plan: &Plan, view: (u32, u32), exact_path: bool, label: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        let au = &s.aus[i];
        s.stats.offered += 1;
        if i == plan.start && !has_idr(au) {
            s.gap_since = Some(i);
        }
        if plan.drops.contains(&i) {
            s.stats.dropped += 1;
            s.gap_since.get_or_insert(i);
            return None;
        }
        if let Some(size) = sps_size(au) {
            s.size = Some(size);
        }
        if s.waiting_for_idr {
            if !has_idr_or_sps(au) {
                s.stats.skipped_waiting += 1;
                return None;
            }
            s.waiting_for_idr = false;
        }
        let gpu_before = RX_GPU_FRAMES.load(Ordering::Relaxed);
        let t = Instant::now();
        let decoded = decode_video_frame(decoders, s.peer, au);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match decoded {
            Decoded::Picture(buf, w, h) => {
                s.stats.pictures += 1;
                s.stats.latency_ms.push(ms);
                s.stats.first_picture_ms.get_or_insert(ms);
                if exact_path {
                    let gpu = RX_GPU_FRAMES.load(Ordering::Relaxed) != gpu_before;
                    if gpu {
                        s.stats.gpu_pictures += 1;
                        if s.stats.last_gpu == Some(false) {
                            s.stats.gpu_after_cpu += 1;
                        }
                    } else {
                        s.stats.cpu_pictures += 1;
                    }
                    s.stats.first_gpu.get_or_insert(gpu);
                    s.stats.last_gpu = Some(gpu);
                }
                if let Some(since) = s.gap_since {
                    if has_idr(au) {
                        s.stats.recovered.push(format!("gap at {since} -> picture again at {i}"));
                        s.gap_since = None;
                    } else {
                        s.stats.shown_after_gap += 1;
                        if plan.verbose {
                            println!("  [{label}] peer {} au {i}: PICTURE SHOWN after the gap at {since} without an IDR", s.peer);
                        }
                    }
                }
                if let Some((sw, sh)) = s.size {
                    let want = fit_within(sw, sh, view.0, view.1);
                    if (w, h) != (sw, sh) || (buf.width(), buf.height()) != want {
                        s.stats.wrong_size.push(format!("au {i}: stream {w}x{h} (SPS says {sw}x{sh}), picture {}x{} (expected {}x{})", buf.width(), buf.height(), want.0, want.1));
                    }
                }
                Some(buf)
            }
            other => {
                s.stats.slowest_other_ms = s.stats.slowest_other_ms.max(ms);
                let what = match other {
                    Decoded::Nothing => {
                        if has_slice(au) {
                            s.stats.nothing_for_picture += 1;
                        }
                        "nothing"
                    }
                    Decoded::NeedKeyframe => {
                        s.stats.need_keyframe += 1;
                        s.waiting_for_idr = true;
                        "NeedKeyframe"
                    }
                    Decoded::Failed => {
                        s.stats.failed += 1;
                        "Failed"
                    }
                    _ => "shown by the GPU",
                };
                if plan.verbose {
                    println!("  [{label}] peer {} au {i} (NAL types {:?}): {what} in {ms:.1} ms", s.peer, nal_types(au));
                }
                None
            }
        }
    }

    // ------------------------------------------------------------------ comparison

    #[derive(Default)]
    struct Diff {
        pictures: usize,
        size_mismatch: Vec<String>,
        /// Histogram of |gpu - cpu| per channel.
        hist: Vec<[u64; 256]>,
        signed: [i64; 3],
        sq: u64,
        samples: u64,
        edge_abs: u64,
        edge_samples: u64,
        late: usize,
        /// With --flat: pixels with nothing but smooth colour around them, the largest
        /// difference among them and how many differ by more than `SMOOTH_LIMIT`.
        smooth: u64,
        smooth_max: i64,
        smooth_over: u64,
        worst_psnr: f64,
        worst_at: usize,
        gpu_only: usize,
        cpu_only: usize,
    }

    fn mse(a: &[u8], b: &[u8]) -> f64 {
        let mut sq = 0u64;
        for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            for c in 0..3 {
                let d = pa[c] as i64 - pb[c] as i64;
                sq += (d * d) as u64;
            }
        }
        sq as f64 / (a.len() / 4 * 3).max(1) as f64
    }

    fn psnr(mse: f64) -> f64 {
        if mse <= 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    const SMOOTH_LIMIT: i64 = 3;

    /// The CPU picture changes by at most 3 per channel between this pixel and the ones 4
    /// pixels away: no edge nearby, so chroma interpolation cannot explain a difference.
    fn smooth(b: &[u8], w: usize, h: usize, x: usize, y: usize) -> bool {
        if x < 4 || y < 4 || x + 4 >= w || y + 4 >= h {
            return false;
        }
        let o = (y * w + x) * 4;
        [o - 16, o + 16, o - 4 * w * 4, o + 4 * w * 4, o - 16 - 4 * w * 4, o + 16 + 4 * w * 4].iter().all(|&n| (0..3).all(|c| (b[o + c] as i64 - b[n + c] as i64).abs() <= 3))
    }

    impl Diff {
        fn add(&mut self, i: usize, gpu: &SharedPixelBuffer<Rgba8Pixel>, cpu: &SharedPixelBuffer<Rgba8Pixel>, prev_cpu: Option<&SharedPixelBuffer<Rgba8Pixel>>, flat: bool) {
            if (gpu.width(), gpu.height()) != (cpu.width(), cpu.height()) {
                self.size_mismatch.push(format!("au {i}: GPU {}x{}, CPU {}x{}", gpu.width(), gpu.height(), cpu.width(), cpu.height()));
                return;
            }
            let (w, h) = (gpu.width() as usize, gpu.height() as usize);
            let (a, b) = (gpu.as_bytes(), cpu.as_bytes());
            let (edge_rows, edge_cols) = ((h / 60).max(2), (w / 60).max(2));
            let mut sq = 0u64;
            for y in 0..h {
                let row = y * w * 4;
                for x in 0..w {
                    let o = row + x * 4;
                    let edge = y >= h - edge_rows || x >= w - edge_cols;
                    if flat && smooth(b, w, h, x, y) {
                        let worst = (0..3).map(|c| (a[o + c] as i64 - b[o + c] as i64).abs()).max().unwrap_or(0);
                        self.smooth += 1;
                        self.smooth_max = self.smooth_max.max(worst);
                        if worst > SMOOTH_LIMIT {
                            self.smooth_over += 1;
                        }
                    }
                    for c in 0..3 {
                        let d = a[o + c] as i64 - b[o + c] as i64;
                        self.hist[c][d.unsigned_abs() as usize] += 1;
                        self.signed[c] += d;
                        sq += (d * d) as u64;
                        if edge {
                            self.edge_abs += d.unsigned_abs();
                            self.edge_samples += 1;
                        }
                    }
                }
            }
            self.sq += sq;
            self.samples += (w * h * 3) as u64;
            let this = sq as f64 / (w * h * 3) as f64;
            if self.pictures == 0 || psnr(this) < self.worst_psnr {
                self.worst_psnr = psnr(this);
                self.worst_at = i;
            }
            self.pictures += 1;
            // A picture that arrived a call late matches the CPU's previous picture instead.
            if let Some(prev) = prev_cpu {
                if (prev.width(), prev.height()) == (gpu.width(), gpu.height()) {
                    let against_prev = mse(a, prev.as_bytes());
                    if against_prev * 2.0 < this && this > 4.0 {
                        self.late += 1;
                    }
                }
            }
        }

        fn channel(&self, c: usize) -> (usize, f64, usize, f64) {
            let total: u64 = self.hist[c].iter().sum();
            let max = (0..256).rev().find(|&d| self.hist[c][d] != 0).unwrap_or(0);
            let mean = self.hist[c].iter().enumerate().map(|(d, n)| d as f64 * *n as f64).sum::<f64>() / total.max(1) as f64;
            let mut seen = 0u64;
            let mut p99 = 0;
            for d in 0..256 {
                seen += self.hist[c][d];
                if seen as f64 >= total as f64 * 0.99 {
                    p99 = d;
                    break;
                }
            }
            (max, mean, p99, self.signed[c] as f64 / total.max(1) as f64)
        }
    }

    /// OpenH264's YUV converted with BT.709 limited range, the way the Windows GPU path
    /// converts a stream that declares it (`nv12_to_rgba_scaled`).
    fn reference_709(decoder: &mut openh264::decoder::Decoder, au: &[u8], view: (u32, u32)) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        use openh264::formats::YUVSource;
        let yuv = decoder.decode(au).ok()??;
        let (w, h) = yuv.dimensions();
        let (ys, us, _) = yuv.strides();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut uv = vec![0u8; cw * ch * 2];
        for row in 0..ch {
            for x in 0..cw {
                uv[(row * cw + x) * 2] = yuv.u()[row * us + x];
                uv[(row * cw + x) * 2 + 1] = yuv.v()[row * us + x];
            }
        }
        let (ow, oh) = fit_within(w as u32, h as u32, view.0, view.1);
        let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(ow, oh);
        nv12_to_rgba_scaled(yuv.y(), &uv, ys, cw * 2, w, h, out.make_mut_bytes(), ow as usize, oh as usize, Colour { full_range: false, bt709: true });
        Some(out)
    }

    // ------------------------------------------------------------------ passes

    /// Makes the CPU decoder the only choice for `peer` in `decoders`, whatever the
    /// environment says later: its GPU decoder is refused once, with an access unit that
    /// holds nothing.
    fn pin_to_cpu(decoders: &mut StreamDecoders, peer: u64) {
        std::env::set_var("LITECORD_VAAPI_DECODE", "0");
        let _ = decode_video_frame(decoders, peer, &[0, 0, 0, 1, 0x09, 0xf0]);
        std::env::remove_var("LITECORD_VAAPI_DECODE");
    }

    fn set_path(path: Path) {
        match path {
            Path::Gpu => std::env::remove_var("LITECORD_VAAPI_DECODE"),
            Path::Cpu => std::env::set_var("LITECORD_VAAPI_DECODE", "0"),
            Path::Auto => {}
        }
    }

    fn view_at(plan: &Plan, i: usize) -> (u32, u32) {
        plan.views.iter().filter(|(f, _)| *f <= i).next_back().map(|(_, v)| *v).unwrap_or((0, 0))
    }

    /// What the decode thread does with a frame while no window shows the streams.
    fn unwatched(decoders: &mut StreamDecoders, streams: &mut [Stream], i: usize) {
        let t = Instant::now();
        decoders.release_gpu();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms > 20.0 {
            println!("  release_gpu at {i} took {ms:.0} ms");
        }
        for s in streams.iter_mut() {
            if i < s.aus.len() {
                s.stats.offered += 1;
                s.stats.skipped_waiting += 1;
                s.waiting_for_idr = true;
                s.gap_since.get_or_insert(i);
            }
        }
    }

    /// Threads and open files of this process (pipelines that were not stopped keep both).
    fn threads_and_files() -> (usize, usize) {
        let count = |dir: &str| std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
        (count("/proc/self/task"), count("/proc/self/fd"))
    }

    /// Decodes the streams through one `StreamDecoders`, interleaved like the decode thread
    /// receives them. Returns the wall time.
    fn run_streams(streams: &mut [Stream], plan: &Plan, exact_path: bool, label: &str) -> f64 {
        let mut decoders = StreamDecoders::default();
        let period = Duration::from_secs_f64(1.0 / plan.fps.max(1) as f64);
        let started = Instant::now();
        let end = streams.iter().map(|s| s.aus.len()).max().unwrap_or(0).min(plan.start.saturating_add(plan.frames));
        for i in plan.start..end {
            let view = view_at(plan, i);
            set_stream_view_size(view.0, view.1);
            if plan.releases.contains(&i) {
                unwatched(&mut decoders, streams, i);
                continue;
            }
            for s in streams.iter_mut() {
                if i < s.aus.len() {
                    step(&mut decoders, s, i, plan, view, exact_path, label);
                }
            }
            if !plan.fast {
                let due = started + period * (i - plan.start + 1) as u32;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                } else if now - due > period {
                    for s in streams.iter_mut() {
                        s.stats.behind += 1;
                    }
                }
            }
        }
        let wall = started.elapsed().as_secs_f64();
        // Dropping the decoders here is what the app does when streams stop.
        let t = Instant::now();
        drop(decoders);
        let drop_ms = t.elapsed().as_secs_f64() * 1000.0;
        if drop_ms > 50.0 {
            println!("  [{label}] dropping the decoders took {drop_ms:.0} ms");
        }
        wall
    }

    fn percentile(sorted: &[f64], q: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
    }

    struct PassResult {
        streams: Vec<Stream>,
        gpu_total: u64,
    }

    /// One timed pass over all streams on one path.
    fn timed_pass(path: Path, inputs: &[Arc<Vec<Vec<u8>>>], n_streams: usize, threads: bool, plan: &Arc<Plan>, peer_base: u64) -> PassResult {
        let label = match path {
            Path::Gpu => "GPU",
            Path::Cpu => "CPU",
            Path::Auto => "AUTO",
        };
        set_path(path);
        let mut streams: Vec<Stream> = (0..n_streams).map(|k| Stream::new(peer_base + k as u64, Arc::clone(&inputs[k % inputs.len()]))).collect();
        let gpu_before = RX_GPU_FRAMES.load(Ordering::Relaxed);
        let cpu_before = cpu_secs();
        let wall = if threads && n_streams > 1 {
            let started = Instant::now();
            let handles: Vec<_> = streams
                .drain(..)
                .map(|s| {
                    let plan = Arc::clone(plan);
                    std::thread::spawn(move || {
                        let mut one = [s];
                        run_streams(&mut one, &plan, false, label);
                        let [s] = one;
                        s
                    })
                })
                .collect();
            for h in handles {
                match h.join() {
                    Ok(s) => streams.push(s),
                    Err(_) => println!("  [{label}] a stream thread panicked"),
                }
            }
            started.elapsed().as_secs_f64()
        } else {
            run_streams(&mut streams, plan, true, label)
        };
        let cpu = cpu_secs() - cpu_before;
        let gpu_total = RX_GPU_FRAMES.load(Ordering::Relaxed) - gpu_before;

        let offered: usize = streams.iter().map(|s| s.stats.offered).sum();
        let pictures: usize = streams.iter().map(|s| s.stats.pictures).sum();
        let video_secs = offered as f64 / plan.fps.max(1) as f64;
        println!(
            "[{label}] {} stream(s){}: {} access units in, {} pictures out ({} from the GPU decoder) | wall {:.2} s = {:.0} pictures/s{} | process CPU {:.2} s = {:.3} CPU s per s of video ({:.2} ms per access unit)",
            streams.len(),
            if threads && n_streams > 1 { " on their own threads" } else { "" },
            offered,
            pictures,
            gpu_total,
            wall,
            pictures as f64 / wall.max(1e-9),
            if plan.fast { " flat out" } else { " paced" },
            cpu,
            cpu / video_secs.max(1e-9),
            cpu * 1000.0 / offered.max(1) as f64,
        );
        for s in &mut streams {
            let st = &mut s.stats;
            st.latency_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let path_used = if threads && n_streams > 1 {
                "path: see total".to_string()
            } else {
                format!("decoded by: GPU {} / CPU {}", st.gpu_pictures, st.cpu_pictures)
            };
            println!(
                "  peer {}: in {} (lost {}, skipped waiting for a keyframe {}) -> pictures {} | {} | latency p50 {:.2} p99 {:.2} max {:.2} ms (first picture {:.1} ms, slowest call without a picture {:.1} ms) | NeedKeyframe {} Failed {} | picture a call late (nothing returned for a picture): {} | shown after a gap without IDR: {}{}",
                s.peer,
                st.offered,
                st.dropped,
                st.skipped_waiting,
                st.pictures,
                path_used,
                percentile(&st.latency_ms, 0.5),
                percentile(&st.latency_ms, 0.99),
                st.latency_ms.last().copied().unwrap_or(f64::NAN),
                st.first_picture_ms.unwrap_or(f64::NAN),
                st.slowest_other_ms,
                st.need_keyframe,
                st.failed,
                st.nothing_for_picture,
                st.shown_after_gap,
                if st.behind > 0 { format!(" | fell behind the pacing {} times", st.behind) } else { String::new() },
            );
            for r in &st.recovered {
                println!("    {r}");
            }
        }
        PassResult { streams, gpu_total }
    }

    /// GPU and CPU side by side on the same access units; returns the hard failures.
    fn compare_pass(inputs: &[Arc<Vec<Vec<u8>>>], n_streams: usize, plan: &Plan, flat: bool, matrix709: bool, peer_base: u64) -> Vec<String> {
        let mut failures = Vec::new();
        let mut gpu_decoders = StreamDecoders::default();
        let mut cpu_decoders = StreamDecoders::default();
        let mut gpu: Vec<Stream> = (0..n_streams).map(|k| Stream::new(peer_base + k as u64, Arc::clone(&inputs[k % inputs.len()]))).collect();
        let mut cpu: Vec<Stream> = (0..n_streams).map(|k| Stream::new(peer_base + 500 + k as u64, Arc::clone(&inputs[k % inputs.len()]))).collect();
        for s in &cpu {
            pin_to_cpu(&mut cpu_decoders, s.peer);
        }
        std::env::remove_var("LITECORD_VAAPI_DECODE");
        let mut references: Vec<Option<openh264::decoder::Decoder>> = (0..n_streams).map(|_| if matrix709 { openh264::decoder::Decoder::new().ok() } else { None }).collect();
        let mut diffs: Vec<Diff> = (0..n_streams).map(|_| Diff { hist: vec![[0; 256]; 3], ..Diff::default() }).collect();
        let mut prev_cpu: Vec<Option<SharedPixelBuffer<Rgba8Pixel>>> = vec![None; n_streams];
        let gpu_before = RX_GPU_FRAMES.load(Ordering::Relaxed);
        let end = gpu.iter().map(|s| s.aus.len()).max().unwrap_or(0).min(plan.start.saturating_add(plan.frames));
        for i in plan.start..end {
            let view = view_at(plan, i);
            set_stream_view_size(view.0, view.1);
            if plan.releases.contains(&i) {
                unwatched(&mut gpu_decoders, &mut gpu, i);
                unwatched(&mut cpu_decoders, &mut cpu, i);
                continue;
            }
            for k in 0..n_streams {
                if i >= gpu[k].aus.len() {
                    continue;
                }
                let g = step(&mut gpu_decoders, &mut gpu[k], i, plan, view, true, "compare GPU");
                let c = match references[k].as_mut() {
                    Some(_) if plan.drops.contains(&i) || plan.releases.contains(&i) => None,
                    Some(reference) => reference_709(reference, &cpu[k].aus[i], view),
                    None => step(&mut cpu_decoders, &mut cpu[k], i, plan, view, true, "compare CPU"),
                };
                match (&g, &c) {
                    (Some(g), Some(c)) => diffs[k].add(i, g, c, prev_cpu[k].as_ref(), flat),
                    (Some(_), None) => diffs[k].gpu_only += 1,
                    (None, Some(_)) => diffs[k].cpu_only += 1,
                    (None, None) => {}
                }
                if c.is_some() {
                    prev_cpu[k] = c;
                }
            }
        }
        let gpu_pictures = RX_GPU_FRAMES.load(Ordering::Relaxed) - gpu_before;
        println!(
            "[COMPARE] GPU decoder against {}, same access units ({gpu_pictures} pictures from the GPU decoder)",
            if matrix709 { "OpenH264's YUV converted with BT.709 limited range" } else { "the app's CPU path" }
        );
        for k in 0..n_streams {
            let d = &diffs[k];
            let peer = gpu[k].peer;
            if cpu[k].stats.gpu_pictures != 0 {
                failures.push(format!("compare: the CPU side of peer {peer} decoded on the GPU"));
            }
            if d.pictures == 0 {
                println!("  peer {peer}: no picture decoded by both (GPU side {} pictures, {} of them by the GPU decoder; CPU side {})", gpu[k].stats.pictures, gpu[k].stats.gpu_pictures, cpu[k].stats.pictures);
                continue;
            }
            let ch: Vec<_> = (0..3).map(|c| d.channel(c)).collect();
            let overall_psnr = psnr(d.sq as f64 / d.samples.max(1) as f64);
            let edge_mean = d.edge_abs as f64 / d.edge_samples.max(1) as f64;
            let overall_mean = ch.iter().map(|c| c.1).sum::<f64>() / 3.0;
            let scaled = gpu[k].stats.wrong_size.is_empty() && plan.views.iter().any(|(_, v)| *v != (0, 0));
            println!(
                "  peer {peer}: {} pictures compared ({} only from the GPU side, {} only from the CPU side) | abs diff R max {} mean {:.2} p99 {} | G max {} mean {:.2} p99 {} | B max {} mean {:.2} p99 {} | bias R {:+.2} G {:+.2} B {:+.2} | PSNR {:.1} dB (worst picture {:.1} dB at au {}) | right/bottom edge mean diff {:.2} | pictures that match the previous CPU picture better (late): {}",
                d.pictures, d.gpu_only, d.cpu_only, ch[0].0, ch[0].1, ch[0].2, ch[1].0, ch[1].1, ch[1].2, ch[2].0, ch[2].1, ch[2].2, ch[0].3, ch[1].3, ch[2].3, overall_psnr, d.worst_psnr, d.worst_at, edge_mean, d.late
            );
            // Limits. At the stream's own size the two paths differ by rounding and by how
            // chroma is upsampled at colour edges; scaled, also by the scaling filter.
            let (mean_limit, bias_limit, psnr_limit) = if scaled { (6.0, 2.0, 24.0) } else { (2.5, 1.0, 30.0) };
            for (c, name) in ["R", "G", "B"].iter().enumerate() {
                if ch[c].1 > mean_limit {
                    failures.push(format!("compare peer {peer}: mean {name} difference {:.2} > {mean_limit} (colour shift)", ch[c].1));
                }
                if ch[c].3.abs() > bias_limit {
                    failures.push(format!("compare peer {peer}: {name} is {:+.2} off on average (colour shift)", ch[c].3));
                }
            }
            if flat {
                println!(
                    "    away from edges: {} pixels ({:.0}% of the picture), largest difference {}, {} differ by more than {SMOOTH_LIMIT}",
                    d.smooth,
                    d.smooth as f64 * 300.0 / d.samples.max(1) as f64,
                    d.smooth_max,
                    d.smooth_over
                );
                if d.smooth_over > 0 || d.smooth == 0 {
                    failures.push(format!("compare peer {peer}: {} of {} pixels away from edges differ by more than {SMOOTH_LIMIT} (largest {}): colours are off", d.smooth_over, d.smooth, d.smooth_max));
                }
            }
            if overall_psnr < psnr_limit {
                failures.push(format!("compare peer {peer}: PSNR {overall_psnr:.1} dB < {psnr_limit}"));
            }
            if edge_mean > overall_mean * 3.0 + 4.0 {
                failures.push(format!("compare peer {peer}: right/bottom edge differs by {edge_mean:.2} on average against {overall_mean:.2} overall (cropping)"));
            }
            if d.late > 0 {
                failures.push(format!("compare peer {peer}: {} GPU pictures match the previous CPU picture (shown a call late)", d.late));
            }
            for m in d.size_mismatch.iter().take(3) {
                failures.push(format!("compare peer {peer}: sizes differ, {m}"));
            }
        }
        for s in gpu.iter().chain(cpu.iter()) {
            for w in s.stats.wrong_size.iter().take(3) {
                failures.push(format!("compare peer {}: wrong size, {w}", s.peer));
            }
        }
        for s in &gpu {
            if s.stats.shown_after_gap > 0 {
                failures.push(format!("compare peer {}: {} pictures shown after a gap without an IDR", s.peer, s.stats.shown_after_gap));
            }
        }
        failures
    }

    // ------------------------------------------------------------------ main

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
        match run_checked(&args) {
            Ok(failures) if failures.is_empty() => {
                println!("RESULT: PASS");
                0
            }
            Ok(failures) => {
                for f in &failures {
                    println!("FAIL: {f}");
                }
                println!("RESULT: FAIL ({} hard failures)", failures.len());
                1
            }
            Err(e) => {
                eprintln!("test_vaapi_decoder: {e}");
                2
            }
        }
    }

    fn run_checked(args: &Args) -> Result<Vec<String>, String> {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
        let fps = args.num("fps", 60)? as u32;
        let fail = args.str("fail", "");
        if !fail.is_empty() {
            std::env::set_var("LITECORD_VAAPI_DEC_TEST_FAIL", &fail);
        }

        let inputs: Vec<Arc<Vec<Vec<u8>>>> = if args.has("encoder") {
            let size = parse_size(&args.str("size", "1080"))?;
            let frames = args.num("frames", 300)? as usize;
            let keyint = args.num("keyint", 2 * fps as u64)? as usize;
            vec![Arc::new(encode_input(&args.str("encoder", "vaapi"), size, fps, frames, keyint)?)]
        } else {
            let list = args.str("input", "");
            if list.is_empty() {
                return Err(format!("--input or --encoder is needed\n\n{HELP}"));
            }
            list.split(',').map(|p| load_input(p).map(Arc::new)).collect::<Result<_, _>>()?
        };
        for (k, aus) in inputs.iter().enumerate() {
            let idrs = aus.iter().filter(|a| has_idr(a)).count();
            let size = aus.iter().find_map(|a| sps_size(a));
            println!("input {k}: {} access units, {} keyframes, {:?}, {} kB", aus.len(), idrs, size, aus.iter().map(|a| a.len()).sum::<usize>() / 1000);
            if aus.is_empty() {
                return Err(format!("input {k} holds no access unit"));
            }
        }

        let parse_list = |s: String| -> Result<Vec<usize>, String> { s.split(',').filter(|p| !p.is_empty()).map(|p| p.parse().map_err(|_| format!("bad frame number {p}"))).collect() };
        let mut views = Vec::new();
        for part in args.str("view", "").split(',').filter(|p| !p.is_empty()) {
            let (f, size) = part.split_once(':').ok_or(format!("--view: expected FRAME:WxH, got {part}"))?;
            views.push((f.parse::<usize>().map_err(|_| format!("bad frame number {f}"))?, parse_size(size)?));
        }
        views.sort();
        let plan = Arc::new(Plan {
            fps,
            fast: args.has("fast"),
            start: args.num("start", 0)? as usize,
            frames: if args.has("encoder") { usize::MAX } else { args.num("frames", u64::MAX)? as usize },
            drops: parse_list(args.str("drop", ""))?,
            releases: parse_list(args.str("release", ""))?,
            views,
            verbose: args.has("verbose"),
        });
        let n_streams = args.num("streams", 1)?.max(1) as usize;
        let before = threads_and_files();
        let threads = args.has("threads");
        let path = args.str("path", "both");
        let expect = args.str("expect", "");
        let mut failures = Vec::new();

        let check = |label: &str, r: &PassResult, gpu_path: bool, failures: &mut Vec<String>| {
            for s in &r.streams {
                let st = &s.stats;
                for w in st.wrong_size.iter().take(3) {
                    failures.push(format!("{label} peer {}: wrong size, {w}", s.peer));
                }
                if st.nothing_for_picture > 0 {
                    failures.push(format!("{label} peer {}: {} access units with a picture returned nothing (picture held back)", s.peer, st.nothing_for_picture));
                }
                // The CPU decoder conceals losses its own way; only the GPU path promises this.
                if gpu_path && st.shown_after_gap > 0 {
                    failures.push(format!("{label} peer {}: {} pictures shown after a gap without an IDR", s.peer, st.shown_after_gap));
                }
                if st.pictures == 0 {
                    failures.push(format!("{label} peer {}: no picture at all", s.peer));
                }
            }
            if gpu_path && !expect.is_empty() {
                let pictures: usize = r.streams.iter().map(|s| s.stats.pictures).sum();
                let exact = !(threads && n_streams > 1);
                match expect.as_str() {
                    "gpu" if r.gpu_total as usize != pictures => failures.push(format!("{label}: expected every picture from the GPU decoder, got {} of {pictures}", r.gpu_total)),
                    "cpu" if r.gpu_total != 0 => failures.push(format!("{label}: expected no picture from the GPU decoder, got {}", r.gpu_total)),
                    "fallback" => {
                        if r.gpu_total == 0 || r.gpu_total as usize == pictures {
                            failures.push(format!("{label}: expected the GPU decoder first and the CPU after it, got {} GPU pictures of {pictures}", r.gpu_total));
                        }
                        for s in r.streams.iter().filter(|_| exact) {
                            if s.stats.first_gpu != Some(true) || s.stats.last_gpu != Some(false) || s.stats.gpu_after_cpu != 0 {
                                failures.push(format!("{label} peer {}: expected GPU pictures, then only CPU pictures (first from GPU: {:?}, last from GPU: {:?}, back to the GPU {} times)", s.peer, s.stats.first_gpu, s.stats.last_gpu, s.stats.gpu_after_cpu));
                            }
                        }
                    }
                    "gpu" | "cpu" => {}
                    other => failures.push(format!("--expect {other}: unknown")),
                }
            }
        };

        match path.as_str() {
            "gpu" => {
                let r = timed_pass(Path::Gpu, &inputs, n_streams, threads, &plan, 1000);
                check("GPU", &r, true, &mut failures);
            }
            "cpu" => {
                let r = timed_pass(Path::Cpu, &inputs, n_streams, threads, &plan, 2000);
                check("CPU", &r, false, &mut failures);
            }
            "auto" => {
                let r = timed_pass(Path::Auto, &inputs, n_streams, threads, &plan, 3000);
                check("AUTO", &r, true, &mut failures);
            }
            "both" => {
                let r = timed_pass(Path::Gpu, &inputs, n_streams, threads, &plan, 1000);
                check("GPU", &r, true, &mut failures);
                let r = timed_pass(Path::Cpu, &inputs, n_streams, threads, &plan, 2000);
                check("CPU", &r, false, &mut failures);
                if r.gpu_total != 0 {
                    failures.push(format!("CPU pass: {} pictures came from the GPU decoder", r.gpu_total));
                }
                if !args.has("no-compare") {
                    failures.extend(compare_pass(&inputs, n_streams, &plan, args.has("flat"), args.has("matrix709"), 4000));
                }
            }
            other => return Err(format!("--path {other}: unknown")),
        }
        set_stream_view_size(0, 0);
        // Pipelines stop on background threads; give them a moment before counting.
        std::thread::sleep(Duration::from_millis(300));
        let after = threads_and_files();
        println!("process: {} threads and {} open files before the first pass, {} and {} after the last (decoders dropped)", before.0, before.1, after.0, after.1);
        Ok(failures)
    }
}
