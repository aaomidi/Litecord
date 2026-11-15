//! Feeds H.264 files, access unit by access unit, through `decode_video_frame` on the CPU
//! (OpenH264) path, the way the "video-decoder-worker" thread does, and checks every
//! picture against FFmpeg's decode of the same file. Run with `--help` for the options.

use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::{Nal, RefNal};
use litecord::screen_capture::hw_decode::h264_state::nal_ranges;
use litecord::screen_capture::video_pipeline::{decode_video_frame, ensure_annex_b, openh264_input, Decoded, StreamDecoders};
use openh264::formats::YUVSource;
use std::collections::HashMap;
use std::io::Read;
use std::time::Instant;

const HELP: &str = "\
test_cpu_receive [options] FILE.h264 ...
  Each file is one stream (Annex B), split into access units here.
  --drop N,N,...   access units never fed (lost on the way)
  --start N        start at access unit N (joining a stream that is already running)
  --strip-headers  remove SPS and PPS from every keyframe but the first: the receiver's
                   cache of parameter sets has to supply them
  --avcc           feed 4-byte length prefixes instead of start codes
  --as-sender      rewrite each SPS like the VA-API sender does (colour description)
  --chain          the files are parts of ONE stream to one decoder (a sender changing
                   encoder or profile on a keyframe); default: a new peer per file
  --legacy         decode like decode_on_cpu did before `openh264_input` (for comparison)
  --loops N        decode each file N times (timing; default 1)
  --no-reference   skip FFmpeg (no pixel comparison, no lateness)
  --mangle N       instead of decoding: hand `openh264_input` N damaged copies of each
                   file's first keyframe (bytes flipped, cut short); it must not panic
  --verbose        one line per access unit
A picture is expected for every access unit fed from a keyframe on, until one is missing;
then none until the next keyframe. Exit status 1 when a picture is missing, late, differs
from FFmpeg's, or is shown although its references were lost.";

struct Args {
    flags: Vec<String>,
    values: HashMap<String, String>,
    files: Vec<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let flags = ["strip-headers", "avcc", "as-sender", "chain", "legacy", "no-reference", "verbose", "help"];
        let mut a = Args { flags: Vec::new(), values: HashMap::new(), files: Vec::new() };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.strip_prefix("--") {
                Some(k) if flags.contains(&k) => a.flags.push(k.to_string()),
                Some(k) => {
                    a.values.insert(k.to_string(), it.next().ok_or(format!("--{k} needs a value"))?);
                }
                None => a.files.push(arg),
            }
        }
        Ok(a)
    }
    fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }
    fn num(&self, k: &str, default: usize) -> Result<usize, String> {
        self.values.get(k).map_or(Ok(default), |v| v.parse().map_err(|_| format!("--{k}: not a number: {v}")))
    }
}

/// The stream cut into access units: a new one starts at the first SPS, PPS, SEI or
/// delimiter after a slice, or at a slice that begins a picture (first_mb_in_slice = 0).
fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let (mut from, mut has_slice) = (0, false);
    for r in nal_ranges(data) {
        let kind = data[r.start] & 0x1f;
        let slice = (1..=5).contains(&kind);
        let first_slice = slice && data.get(r.start + 1).is_some_and(|b| b & 0x80 != 0);
        if has_slice && (matches!(kind, 6..=9) || first_slice) {
            // The start code (with its leading zero byte, if any) belongs to the next unit.
            let mut cut = r.start - 3;
            if cut > from && data[cut - 1] == 0 {
                cut -= 1;
            }
            out.push(&data[from..cut]);
            from = cut;
            has_slice = false;
        }
        has_slice |= slice;
    }
    if from < data.len() {
        out.push(&data[from..]);
    }
    out
}

fn kinds(au: &[u8]) -> Vec<u8> {
    nal_ranges(au).iter().map(|r| au[r.start] & 0x1f).collect()
}

fn without_parameter_sets(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len());
    for r in nal_ranges(au) {
        if !matches!(au[r.start] & 0x1f, 7 | 8) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&au[r]);
        }
    }
    out
}

fn as_avcc(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for r in nal_ranges(au) {
        out.extend_from_slice(&(r.len() as u32).to_be_bytes());
        out.extend_from_slice(&au[r]);
    }
    out
}

/// What the first SPS and PPS of the stream declare, and the picture size.
fn describe(data: &[u8]) -> (String, Option<(u32, u32)>) {
    let mut ctx = h264_reader::Context::new();
    let mut text = String::new();
    let mut size = None;
    for r in nal_ranges(&data[..data.len().min(4096)]) {
        let nal = RefNal::new(&data[r.clone()], &[], true);
        match data[r.start] & 0x1f {
            7 => {
                let Ok(sps) = SeqParameterSet::from_bits(nal.rbsp_bits()) else { continue };
                size = sps.pixel_dimensions().ok();
                let reorder = sps.vui_parameters.as_ref().and_then(|v| v.bitstream_restrictions.as_ref()).map(|b| b.max_num_reorder_frames);
                let poc = match sps.pic_order_cnt {
                    h264_reader::nal::sps::PicOrderCntType::TypeZero { .. } => 0,
                    h264_reader::nal::sps::PicOrderCntType::TypeOne { .. } => 1,
                    h264_reader::nal::sps::PicOrderCntType::TypeTwo => 2,
                };
                text = format!(
                    "profile_idc {} constraint flags {:02x} level {} poc type {} refs {} reorder {}",
                    u8::from(sps.profile_idc),
                    data[r.start + 2],
                    sps.level_idc,
                    poc,
                    sps.max_num_ref_frames,
                    reorder.map_or("undeclared".to_string(), |n| n.to_string())
                );
                ctx.put_seq_param_set(sps);
            }
            8 => {
                if let Ok(pps) = h264_reader::nal::pps::PicParameterSet::from_bits(&ctx, nal.rbsp_bits()) {
                    text += &format!(", {}", if pps.entropy_coding_mode_flag { "CABAC" } else { "CAVLC" });
                }
                break;
            }
            _ => {}
        }
    }
    (text, size)
}

/// FFmpeg's pictures of a file, one I420 frame at a time.
struct Reference {
    child: std::process::Child,
    frame_len: usize,
    /// (index, frame) of the last few frames read.
    recent: std::collections::VecDeque<(usize, Vec<u8>)>,
    next: usize,
}

impl Reference {
    fn open(file: &str, (w, h): (u32, u32)) -> Option<Self> {
        let child = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-i", file, "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv420p", "pipe:1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        Some(Self { child, frame_len: (w * h * 3 / 2) as usize, recent: Default::default(), next: 0 })
    }

    /// Reads up to frame `n`; false when the file has fewer.
    fn advance_to(&mut self, n: usize) -> bool {
        while self.next <= n {
            let mut frame = vec![0u8; self.frame_len];
            if self.child.stdout.as_mut().unwrap().read_exact(&mut frame).is_err() {
                return false;
            }
            self.recent.push_back((self.next, frame));
            if self.recent.len() > 5 {
                self.recent.pop_front();
            }
            self.next += 1;
        }
        true
    }

    fn frame(&self, n: usize) -> Option<&[u8]> {
        self.recent.iter().find(|(i, _)| *i == n).map(|(_, f)| f.as_slice())
    }
}

impl Drop for Reference {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// decode_on_cpu's own I420 to RGBA arithmetic (BT.601 limited range, chroma not
/// interpolated), so a bit-exact decode gives byte-identical pictures.
fn i420_to_rgba(yuv: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (y, uv) = yuv.split_at(w * h);
    let (u, v) = uv.split_at((w / 2) * (h / 2));
    let mut out = vec![255u8; w * h * 4];
    for j in 0..h {
        for i in 0..w {
            let c = 298 * (y[j * w + i] as i32 - 16);
            let d = u[(j / 2) * (w / 2) + i / 2] as i32 - 128;
            let e = v[(j / 2) * (w / 2) + i / 2] as i32 - 128;
            let px = &mut out[(j * w + i) * 4..];
            px[0] = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
        }
    }
    out
}

enum Shown {
    Picture(Vec<u8>, u32, u32),
    Nothing,
    Failed,
    NeedKeyframe,
}

/// The receive path before `openh264_input`: the same decoder calls on the unchanged stream.
fn decode_legacy(decoders: &mut HashMap<u64, openh264::decoder::Decoder>, peer: u64, au: &[u8]) -> Shown {
    let annex_b = ensure_annex_b(peer, au);
    let decoder = decoders.entry(peer).or_insert_with(|| openh264::decoder::Decoder::new().expect("decoder"));
    match decoder.decode(&annex_b) {
        Ok(Some(yuv)) => {
            let (w, h) = yuv.dimensions();
            let (ys, us, vs) = yuv.strides();
            let mut planes = Vec::with_capacity(w * h * 3 / 2);
            (0..h).for_each(|r| planes.extend_from_slice(&yuv.y()[r * ys..r * ys + w]));
            (0..h / 2).for_each(|r| planes.extend_from_slice(&yuv.u()[r * us..r * us + w / 2]));
            (0..h / 2).for_each(|r| planes.extend_from_slice(&yuv.v()[r * vs..r * vs + w / 2]));
            Shown::Picture(i420_to_rgba(&planes, w, h), w as u32, h as u32)
        }
        _ => Shown::Failed,
    }
}

#[derive(Default)]
struct Totals {
    fed: usize,
    expected: usize,
    pictures: usize,
    on_time: usize,
    late: usize,
    differs: usize,
    unchecked: usize,
    missing: usize,
    unexpected: usize,
    failed: usize,
    nothing: usize,
    need_keyframe: usize,
    relabelled: usize,
    decode_ms: Vec<f64>,
    prepare_us: Vec<f64>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    // This test is about the CPU path: where a GPU decoder exists, `decode_video_frame`
    // would take it, and its pictures are not byte-identical to the reference.
    std::env::set_var("LITECORD_VAAPI_DECODE", "0");
    let args = match Args::parse() {
        Ok(a) if !a.has("help") && !a.files.is_empty() => a,
        Ok(_) => {
            println!("{HELP}");
            return;
        }
        Err(e) => {
            eprintln!("{e}\n{HELP}");
            std::process::exit(2);
        }
    };
    let run = || -> Result<bool, String> {
        let drops: Vec<usize> = args.values.get("drop").map(|s| s.split(',').filter_map(|n| n.parse().ok()).collect()).unwrap_or_default();
        let start = args.num("start", 0)?;
        let loops = args.num("loops", 1)?.max(1);
        let (legacy, verbose) = (args.has("legacy"), args.has("verbose"));

        // The GPU decoder would take the stream on Windows.
        #[cfg(feature = "automation")]
        litecord::utils::test_hooks::CPU_DECODE.store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(all(windows, not(feature = "automation")))]
        println!("note: build with --features automation to keep the stream off the GPU decoder");

        let mut decoders = StreamDecoders::default();
        let mut legacy_decoders: HashMap<u64, openh264::decoder::Decoder> = HashMap::new();
        let mut all_ok = true;
        // Carried from file to file with --chain: the decoder has pictures to go on from.
        let mut intact = false;
        for (file_no, file) in args.files.iter().enumerate() {
            let data = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
            let aus = access_units(&data);
            let (described, size) = describe(&data);
            let mangle = args.num("mangle", 0)?;
            if mangle > 0 {
                let Some(key) = aus.iter().find(|au| kinds(au).contains(&7)) else { continue };
                let head = &key[..key.len().min(160)];
                let mut seed = 0x9e37_79b9_7f4a_7c15u64;
                let mut next = move || {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as usize
                };
                let mut relabelled = 0;
                for _ in 0..mangle {
                    let mut copy = head[..1 + next() % head.len()].to_vec();
                    for _ in 0..next() % 4 {
                        let at = next() % copy.len();
                        copy[at] ^= 1 << (next() % 8);
                    }
                    relabelled += matches!(openh264_input(&copy), std::borrow::Cow::Owned(_)) as usize;
                }
                println!("{}: {mangle} damaged keyframe heads through openh264_input, no panic ({relabelled} still relabelled)", file.rsplit('/').next().unwrap_or(file));
                continue;
            }
            let peer = if args.has("chain") { 1 } else { 1 + file_no as u64 };
            if !args.has("chain") {
                intact = false;
            }
            let mut t = Totals::default();
            let mut headers_seen = false;
            let mut first_failure_after_loss: Vec<usize> = Vec::new();
            let mut lost_at: Option<usize> = None;
            let mut lines = Vec::new();
            for pass in 0..loops {
                let mut reference = if args.has("no-reference") || pass > 0 { None } else { size.and_then(|s| Reference::open(file, s)) };
                if pass > 0 {
                    // The same pictures again: start over like a new stream would.
                    decoders.reset(peer);
                    legacy_decoders.remove(&peer);
                    intact = false;
                }
                for (n, au) in aus.iter().enumerate() {
                    let kinds = kinds(au);
                    let (idr, has_sps) = (kinds.contains(&5), kinds.contains(&7));
                    let fed = n >= start && !drops.contains(&n);
                    if !fed {
                        if intact {
                            lost_at = Some(n);
                        }
                        intact = false;
                        continue;
                    }
                    let mut au = au.to_vec();
                    if args.has("as-sender") {
                        au = litecord::encoder::h264_vui::declare_colour(&au, 1, 1, 6);
                    }
                    if has_sps {
                        if args.has("strip-headers") && headers_seen {
                            au = without_parameter_sets(&au);
                        }
                        headers_seen = true;
                    }
                    if args.has("avcc") {
                        au = as_avcc(&au);
                    }
                    // A keyframe restarts the stream when its parameter sets are at hand.
                    if idr && (has_sps || headers_seen || intact) {
                        intact = true;
                    }
                    let expected = intact;

                    // What decode_on_cpu is about to do to this access unit, timed on its own.
                    let annex_b = ensure_annex_b(peer, &au);
                    let t0 = Instant::now();
                    let prepared = openh264_input(&annex_b);
                    let prepare_us = t0.elapsed().as_secs_f64() * 1e6;
                    let relabelled = matches!(prepared, std::borrow::Cow::Owned(_));
                    drop(prepared);
                    drop(annex_b);

                    // Timed: the call the worker thread makes (decode and conversion to RGBA).
                    let t0 = Instant::now();
                    let decoded = if legacy { Err(decode_legacy(&mut legacy_decoders, peer, &au)) } else { Ok(decode_video_frame(&mut decoders, peer, &au)) };
                    let decode_ms = t0.elapsed().as_secs_f64() * 1e3;
                    let shown = match decoded {
                        Err(shown) => shown,
                        Ok(Decoded::Picture(buf, w, h)) => Shown::Picture(buf.as_bytes().to_vec(), w, h),
                        Ok(Decoded::ShownByGpu(_)) => return Err("the GPU decoder took the stream".into()),
                        Ok(Decoded::Nothing) => Shown::Nothing,
                        Ok(Decoded::Failed) => Shown::Failed,
                        Ok(Decoded::NeedKeyframe) => Shown::NeedKeyframe,
                    };
                    if pass > 0 || loops == 1 {
                        t.decode_ms.push(decode_ms);
                        t.prepare_us.push(prepare_us);
                    }
                    if pass > 0 {
                        continue;
                    }

                    t.fed += 1;
                    t.expected += expected as usize;
                    t.relabelled += relabelled as usize;
                    let what = match &shown {
                        Shown::Picture(rgba, w, h) => {
                            t.pictures += 1;
                            if !expected {
                                t.unexpected += 1;
                            }
                            let have = reference.as_mut().is_some_and(|r| r.advance_to(n));
                            let verdict = match reference.as_ref().filter(|_| have) {
                                Some(r) => {
                                    let same = |k: usize| r.frame(k).is_some_and(|f| i420_to_rgba(f, *w as usize, *h as usize) == *rgba);
                                    if size != Some((*w, *h)) {
                                        t.differs += 1;
                                        format!("picture of another size ({w}x{h})")
                                    } else if same(n) {
                                        t.on_time += 1;
                                        "picture".to_string()
                                    } else if let Some(d) = (1..=4usize).find(|d| n >= *d && same(n - d)) {
                                        t.late += 1;
                                        format!("picture {d} LATE")
                                    } else {
                                        t.differs += 1;
                                        "picture DIFFERS from FFmpeg".to_string()
                                    }
                                }
                                None => {
                                    t.unchecked += 1;
                                    "picture (unchecked)".to_string()
                                }
                            };
                            if !expected {
                                format!("{verdict}, shown after a loss")
                            } else {
                                verdict
                            }
                        }
                        other => {
                            let name = match other {
                                Shown::Nothing => {
                                    t.nothing += 1;
                                    "nothing"
                                }
                                Shown::NeedKeyframe => {
                                    t.need_keyframe += 1;
                                    "need keyframe"
                                }
                                _ => {
                                    t.failed += 1;
                                    "failed"
                                }
                            };
                            if expected {
                                t.missing += 1;
                                // What the worker does next: the decoder cannot go on from here.
                                intact = false;
                                format!("{name}, PICTURE MISSING")
                            } else {
                                if let Some(lost) = lost_at.take() {
                                    first_failure_after_loss.push(n - lost);
                                }
                                name.to_string()
                            }
                        }
                    };
                    if verbose {
                        lines.push(format!("  {n:4} {:7} bytes nal {kinds:?}{} -> {what} ({decode_ms:.2} ms)", au.len(), if relabelled { " relabelled" } else { "" }));
                    }
                }
            }

            let ok = t.missing == 0 && t.late == 0 && t.differs == 0 && t.unexpected == 0 && t.expected > 0;
            all_ok &= ok;
            let mut ms = t.decode_ms.clone();
            ms.sort_by(|a, b| a.total_cmp(b));
            let mut us = t.prepare_us.clone();
            us.sort_by(|a, b| a.total_cmp(b));
            let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
            println!("{} [{}]", file.rsplit('/').next().unwrap_or(file), described);
            lines.iter().for_each(|l| println!("{l}"));
            println!(
                "  access units {} fed {} (keyframe SPS relabelled: {}) | pictures {} of {} expected: on time {} late {} differ {} unchecked {} | missing {} shown after a loss {}",
                aus.len(), t.fed, t.relabelled, t.pictures, t.expected, t.on_time, t.late, t.differs, t.unchecked, t.missing, t.unexpected
            );
            println!(
                "  without a picture: failed {} need-keyframe {} nothing {}{}",
                t.failed,
                t.need_keyframe,
                t.nothing,
                if first_failure_after_loss.is_empty() { String::new() } else { format!(" | first failure reported {first_failure_after_loss:?} access unit(s) after each loss") }
            );
            println!(
                "  decode+convert ms: avg {:.2} p50 {:.2} p95 {:.2} max {:.2} | openh264_input alone: avg {:.2} us, max {:.1} us | {}",
                avg(&ms), percentile(&ms, 0.5), percentile(&ms, 0.95), percentile(&ms, 1.0), avg(&us), percentile(&us, 1.0),
                if ok { "PASS" } else { "FAIL" }
            );
        }
        Ok(all_ok)
    };
    match run() {
        Ok(true) => println!("RESULT: PASS"),
        Ok(false) => {
            println!("RESULT: FAIL");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}
