//! Drives the in-process GPU screen capture (`screen_capture::linux_gpu_tx`) the way the
//! portal thread and the "screen-capture-tx" thread do, on a PipeWire node obtained without
//! the portal dialog, and checks every access unit with the app's receive path. `--legacy`
//! runs today's gst-launch capture feeding the frames-from-memory encoder on the same node,
//! for comparison. Run with `--help` for the options.

#[cfg(not(all(target_os = "linux", feature = "vaapi")))]
fn main() {
    eprintln!("test_vaapi_capture: needs Linux and the `vaapi` cargo feature");
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
fn main() {
    std::process::exit(harness::run());
}

#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod harness {
    use ashpd::zbus;
    use futures_util::StreamExt;
    use h264_reader::nal::sps::SeqParameterSet;
    use h264_reader::nal::{Nal, RefNal};
    use litecord::encoder::{create_best_encoder, VideoEncoder};
    use litecord::screen_capture::hw_decode::h264_state::nal_ranges;
    use litecord::screen_capture::linux_gpu_tx as gpu;
    use litecord::screen_capture::sources::linux_portal::PORTAL_LOCAL_CB;
    use litecord::screen_capture::video_pipeline::{decode_video_frame, Decoded, StreamDecoders};
    use openh264::formats::YUVSource;
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    use std::collections::HashMap;
    use std::io::Read;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const HELP: &str = "\
test_vaapi_capture [options]
  --source S       mutter (default: Mutter ScreenCast RecordMonitor of the primary monitor, cursor
                   embedded, no dialog) | mutter:CONNECTOR | node:ID | synthetic (a
                   `videotestsrc ! pipewiresink` this spawns)
  --src-props STR  synthetic: videotestsrc properties (default \"pattern=ball motion=sweep\")
  --src-size WxH   synthetic: picture size (default 2560x1440)
  --src-fps N      synthetic: frame rate (default 60; 1 approximates a still screen)
  --src-colorimetry C  synthetic: colorimetry of the provider's caps (Mutter declares 0:0:4:1)
  --fd             reach PipeWire through an own socket fd, the way the portal's fd is used
  --legacy         today's path only: gst-launch subprocess -> pipe -> create_best_encoder()
  --encoder-setting S  the video_encoder setting, in memory only (auto, vaapi, openh264, ...)
  --size S         480 | 720 | 1080 (default)
  --fps N          15 | 30 | 60 (default)
  --duration S     seconds watched (default 10); ignored with --watch
  --watch SPEC     u2,w8,u3,w5: unwatched 2 s, watched 8 s, ... (default u1,w<duration>)
  --bitrate SPEC   kbps, or SEC:KBPS,... (default 8000), e.g. 0:8000,4:3000,8:8000
  --pli SECS       extra keyframe requests at these times, e.g. 3.3,5.1 (besides one every 2 s)
  --memory M       auto (default) | dmabuf | system
  --max-fps N      ask the compositor for at most N pictures a second (experiment)
  --fail MODE      start | first | after:N | stall:N | nosrc:N | hang:N
                   (LITECORD_VAAPI_CAPTURE_TEST_FAIL): break the GPU path at its start or from
                   access unit N on (hang: its streaming thread blocks for 20 s, like a driver
                   that hangs); the share must go on
  --freeze-source S  synthetic: the provider is stopped (SIGSTOP) before the capture connects
                   and continued after S seconds: the capture's start must stay bounded
  --end-source S   end the source S seconds into the share (Mutter: Session.Stop, as GNOME's
                   indicator does; synthetic: the provider is killed): the capture must report
                   the source gone and no other source may be taken
  --as-window      tell the capture the source is a shared window
  --cycles N       stop the share and start it again N times in all (default 1). Not with the
                   synthetic source: its provider stops delivering to anybody once a consumer
                   left mid-stream (plain gst-launch consumers do the same to it)
  --cycle-gap MS   pause between stop and start (default 200)
  --colours        synthetic colour bars: check decoded colours against BT.601 limited
  --no-verify      do not decode (CPU measurements)
  --verbose        one line per access unit
Exit status 1 when a check fails, 2 when the source cannot be opened.";

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    struct Args(HashMap<String, String>);

    impl Args {
        fn parse() -> Result<Self, String> {
            let flags = ["fd", "legacy", "colours", "no-verify", "verbose", "help", "as-window"];
            let mut map = HashMap::new();
            let mut it = std::env::args().skip(1);
            while let Some(a) = it.next() {
                let key = a.strip_prefix("--").ok_or(format!("unexpected argument {a}"))?.to_string();
                if flags.contains(&key.as_str()) {
                    map.insert(key, String::new());
                } else {
                    map.insert(key.clone(), it.next().ok_or(format!("--{key} needs a value"))?);
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
        fn num(&self, k: &str, default: f64) -> Result<f64, String> {
            match self.0.get(k) {
                Some(v) => v.parse().map_err(|_| format!("--{k}: not a number: {v}")),
                None => Ok(default),
            }
        }
    }

    // ------------------------------------------------------------------ CPU accounting

    fn ticks() -> f64 {
        unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
    }

    /// CPU seconds a process has used so far (user + system).
    fn pid_cpu(pid: u32) -> Option<f64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        Some((fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?) / ticks())
    }

    fn self_cpu() -> f64 {
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut ru);
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
            tv(ru.ru_utime) + tv(ru.ru_stime)
        }
    }

    fn thread_cpu() -> f64 {
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
            ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
        }
    }

    fn find_pid(comm: &str) -> Option<u32> {
        std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            (std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim() == comm).then_some(pid)
        })
    }

    // ------------------------------------------------------------------ sources

    /// A Mutter ScreenCast session recording one monitor. The compositor ends it when this
    /// process goes away for any reason (it watches the D-Bus name); `stop` ends it at once.
    struct Mutter {
        conn: zbus::Connection,
        session: zbus::zvariant::OwnedObjectPath,
        stopped: AtomicBool,
    }

    static MUTTER: Mutex<Option<Arc<Mutter>>> = Mutex::new(None);
    const MUTTER_BUS: &str = "org.gnome.Mutter.ScreenCast";

    impl Mutter {
        fn open(connector: &str) -> Result<(Arc<Self>, u32), String> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
            let (mutter, node) = rt.block_on(async {
                let e = |e: zbus::Error| format!("Mutter ScreenCast: {e}");
                let conn = zbus::Connection::session().await.map_err(e)?;
                let no_props: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
                let reply = conn.call_method(Some(MUTTER_BUS), "/org/gnome/Mutter/ScreenCast", Some(MUTTER_BUS), "CreateSession", &(no_props,)).await.map_err(e)?;
                let session: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().map_err(e)?;
                // From here on the session is stopped whatever happens.
                let mutter = Arc::new(Mutter { conn: conn.clone(), session, stopped: AtomicBool::new(false) });
                *MUTTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(mutter.clone());
                let session_iface = "org.gnome.Mutter.ScreenCast.Session";
                // RecordMonitor only: RecordVirtual would add a monitor to the desktop.
                let mut props: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
                props.insert("cursor-mode", zbus::zvariant::Value::U32(1));
                let reply = conn.call_method(Some(MUTTER_BUS), mutter.session.as_str(), Some(session_iface), "RecordMonitor", &(connector, props)).await.map_err(e)?;
                let stream: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().map_err(e)?;
                let proxy = zbus::Proxy::new(&conn, MUTTER_BUS, stream.as_str(), "org.gnome.Mutter.ScreenCast.Stream").await.map_err(e)?;
                let mut added = proxy.receive_signal("PipeWireStreamAdded").await.map_err(e)?;
                conn.call_method(Some(MUTTER_BUS), mutter.session.as_str(), Some(session_iface), "Start", &()).await.map_err(e)?;
                let msg = tokio::time::timeout(Duration::from_secs(3), added.next()).await.map_err(|_| "Mutter did not announce a PipeWire node".to_string())?.ok_or("signal stream ended")?;
                let (node,): (u32,) = msg.body().deserialize().map_err(e)?;
                Ok::<_, String>((mutter, node))
            })?;
            Ok((mutter, node))
        }

        fn stop(&self) {
            if self.stopped.swap(true, Ordering::SeqCst) {
                return;
            }
            let (conn, session) = (self.conn.clone(), self.session.clone());
            // Its own thread and runtime: this also runs from the panic hook.
            let stopper = std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                rt.block_on(async {
                    let call = conn.call_method(Some(MUTTER_BUS), session.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Stop", &());
                    match tokio::time::timeout(Duration::from_secs(2), call).await {
                        Ok(Ok(_)) => eprintln!("mutter: screencast session stopped"),
                        other => eprintln!("mutter: Stop did not answer ({:?}); the session ends with this process", other.map(|r| r.map(|_| ()))),
                    }
                });
            });
            let _ = stopper.join();
        }
    }

    fn stop_mutter() {
        let mutter = MUTTER.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(m) = mutter {
            m.stop();
        }
    }

    /// Whether PipeWire has a video node with this id. On the session's own connection a
    /// capture asked for a node that is not there would be linked to some other video source.
    fn video_node_present(id: u32) -> bool {
        let Ok(out) = Command::new("pw-dump").output() else { return false };
        let Ok(dump) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return false };
        dump.as_array().into_iter().flatten().any(|o| {
            o["id"].as_u64() == Some(id as u64) && o["type"].as_str().is_some_and(|t| t.ends_with(":Node")) && o["info"]["props"]["media.class"].as_str().is_some_and(|c| c.contains("Video"))
        })
    }

    /// A connection to PipeWire of one's own, as the portal's OpenPipeWireRemote hands out.
    fn open_remote() -> Result<OwnedFd, String> {
        let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
        let socket: OwnedFd = std::os::unix::net::UnixStream::connect(format!("{dir}/pipewire-0")).map_err(|e| format!("pipewire-0: {e}"))?.into();
        unsafe {
            let flags = libc::fcntl(socket.as_raw_fd(), libc::F_GETFD);
            libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
        Ok(socket)
    }

    /// The child is killed when this process dies, however it dies.
    fn dies_with_us(command: &mut Command) -> &mut Command {
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            })
        }
    }

    /// A PipeWire video node fed by `videotestsrc`, for everything that needs no compositor.
    struct Synthetic {
        child: Child,
    }

    impl Synthetic {
        fn spawn(props: &str, size: (u32, u32), fps: u32, colorimetry: Option<&str>) -> Result<(Self, u32), String> {
            let name = format!("litecord-capture-test-{}", std::process::id());
            let colorimetry = colorimetry.map(|c| format!(",colorimetry=(string){c}")).unwrap_or_default();
            let mut args: Vec<String> = vec!["-q".into(), "videotestsrc".into(), "is-live=true".into()];
            args.extend(props.split_whitespace().map(String::from));
            args.extend([
                "!".into(),
                format!("video/x-raw,format=BGRx,width={},height={},framerate={fps}/1{colorimetry}", size.0, size.1),
                "!".into(),
                "pipewiresink".into(),
                "mode=provide".into(),
                format!("stream-properties=props,media.class=Video/Source,node.name={name}"),
            ]);
            let child = dies_with_us(Command::new("gst-launch-1.0").args(&args).stdout(Stdio::null()).stderr(Stdio::inherit())).spawn().map_err(|e| format!("gst-launch-1.0: {e}"))?;
            let synthetic = Self { child };
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                let Ok(out) = Command::new("pw-dump").output() else { continue };
                let Ok(dump) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { continue };
                let node = dump.as_array().into_iter().flatten().find(|o| o["info"]["props"]["node.name"] == name.as_str()).and_then(|o| o["id"].as_u64());
                if let Some(id) = node {
                    return Ok((synthetic, id as u32));
                }
            }
            Err("the synthetic PipeWire node did not appear".into())
        }
    }

    impl Drop for Synthetic {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    // ------------------------------------------------------------------ today's capture path

    /// The frame the gst-launch reader publishes (PORTAL_FRAME in the app) and its number.
    struct LegacyShared {
        frame: Mutex<Option<Vec<u8>>>,
        frames_read: AtomicU64,
        first_frame_at: Mutex<Option<Instant>>,
        child_pid: AtomicU32,
        previews: AtomicU64,
    }

    /// linux_portal.rs's gst-launch pipeline and reader loop, on `node`.
    fn legacy_capture(shared: &LegacyShared, fd: Option<RawFd>, node: u32, size: (u32, u32), fps: u32, running: &AtomicBool) {
        let (dst_w, dst_h) = size;
        let frame_size = (dst_w * dst_h * 4) as usize;
        let mut gst_args = vec!["-q".to_string(), "pipewiresrc".to_string()];
        if let Some(fd) = fd {
            gst_args.push(format!("fd={fd}"));
        }
        // Not in the app's arguments: on the session's own connection (the portal's shows
        // only its node) a node that is gone must not be replaced by another video source.
        gst_args.push("stream-properties=props,node.dont-fallback=(string)true".into());
        gst_args.extend([format!("path={node}"), "do-timestamp=true".into(), "!".into(), "videoconvert".into(), "n-threads=4".into(), "!".into(), "videoscale".into(), "!".into()]);
        if fps > 0 && fps < 120 {
            gst_args.extend(["videorate".to_string(), "!".to_string(), format!("video/x-raw,format=BGRA,width={dst_w},height={dst_h},framerate={fps}/1")]);
        } else {
            gst_args.push(format!("video/x-raw,format=BGRA,width={dst_w},height={dst_h}"));
        }
        gst_args.extend(["!", "queue", "max-size-buffers=1", "max-size-bytes=0", "max-size-time=0", "leaky=downstream", "!", "fdsink", "fd=1", "sync=false"].map(String::from));
        let spawned = dies_with_us(Command::new("gst-launch-1.0").env_remove("PIPEWIRE_NODE").args(&gst_args).stdout(Stdio::piped()).stderr(Stdio::inherit())).spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return eprintln!("legacy: gst-launch-1.0 did not start: {e}"),
        };
        shared.child_pid.store(child.id(), Ordering::SeqCst);
        let Some(mut stdout) = child.stdout.take() else { return };
        let mut raw_buf = vec![0u8; frame_size];
        let mut last_preview = Instant::now() - Duration::from_secs(1);
        while running.load(Ordering::Relaxed) && stdout.read_exact(&mut raw_buf).is_ok() {
            if shared.frames_read.fetch_add(1, Ordering::Relaxed) == 0 {
                *shared.first_frame_at.lock().unwrap() = Some(Instant::now());
            }
            let now = Instant::now();
            if now.duration_since(last_preview) >= Duration::from_millis(150) {
                last_preview = now;
                if let Some(cb) = PORTAL_LOCAL_CB.lock().unwrap().as_ref() {
                    let mut pixel_buffer = SharedPixelBuffer::<Rgba8Pixel>::new(dst_w, dst_h);
                    let bytes = pixel_buffer.make_mut_bytes();
                    bytes.copy_from_slice(&raw_buf);
                    for chunk in bytes.chunks_exact_mut(4) {
                        chunk.swap(0, 2);
                    }
                    shared.previews.fetch_add(1, Ordering::Relaxed);
                    cb(pixel_buffer);
                }
            }
            let mut slot = shared.frame.lock().unwrap();
            match slot.as_mut() {
                Some(dest) => {
                    dest.resize(raw_buf.len(), 0);
                    dest.copy_from_slice(&raw_buf);
                }
                None => *slot = Some(raw_buf.clone()),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        shared.child_pid.store(0, Ordering::SeqCst);
        *shared.frame.lock().unwrap() = None;
    }

    // ------------------------------------------------------------------ checks

    #[derive(Default)]
    struct Failures {
        list: Vec<String>,
        count: usize,
    }

    impl Failures {
        fn add(&mut self, what: String) {
            self.count += 1;
            if self.list.len() < 30 {
                println!("  FAIL: {what}");
                self.list.push(what);
            }
        }
    }

    const PEER: u64 = 0x7E57;

    /// 100% colour bars (videotestsrc pattern=smpte100, upper two thirds), left to right.
    const BARS: [(&str, [i32; 3]); 7] = [
        ("white", [255, 255, 255]),
        ("yellow", [255, 255, 0]),
        ("cyan", [0, 255, 255]),
        ("green", [0, 255, 0]),
        ("magenta", [255, 0, 255]),
        ("red", [255, 0, 0]),
        ("blue", [0, 0, 255]),
    ];

    /// What the verifier thread found; it decodes every access unit like a receiver.
    #[derive(Default)]
    struct Verdict {
        decoded: usize,
        failures: Vec<String>,
        sps: String,
        colour_lines: Vec<String>,
        cpu: f64,
    }

    /// `strict` access units come from the GPU: one slice, no filler, colours declared.
    fn verifier(rx: std::sync::mpsc::Receiver<(usize, Vec<u8>, bool)>, size: (u32, u32), colours: bool) -> Verdict {
        let mut v = Verdict::default();
        let mut decoders = StreamDecoders::default();
        let mut raw = if colours { openh264::decoder::Decoder::new().ok() } else { None };
        let mut colours_done = 0;
        let fail = |v: &mut Verdict, n: usize, what: String| {
            if v.failures.len() < 30 {
                v.failures.push(format!("access unit {n}: {what}"));
            }
        };
        while let Ok((n, au, strict)) = rx.recv() {
            if !(au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1])) {
                fail(&mut v, n, "does not start with a start code".into());
            }
            let ranges = nal_ranges(&au);
            let types: Vec<u8> = ranges.iter().map(|r| au[r.start] & 0x1f).collect();
            let idr = types.contains(&5);
            let vcl: Vec<&[u8]> = ranges.iter().map(|r| &au[r.clone()]).filter(|nal| matches!(nal[0] & 0x1f, 1 | 5)).collect();
            let pictures = vcl.iter().filter(|nal| nal.len() > 1 && nal[1] & 0x80 != 0).count();
            if pictures != 1 || (strict && vcl.len() != 1) {
                fail(&mut v, n, format!("{pictures} pictures / {} slices in one access unit (NAL types {types:?})", vcl.len()));
            }
            if types.contains(&12) {
                fail(&mut v, n, "filler data left in".into());
            }
            if (idr && !(types.contains(&7) && types.contains(&8))) || (strict && !idr && (types.contains(&7) || types.contains(&8))) {
                fail(&mut v, n, format!("IDR and parameter sets do not go together (NAL types {types:?})"));
            }
            for nal in ranges.iter().map(|r| &au[r.clone()]).filter(|nal| nal[0] & 0x1f == 7) {
                let Ok(sps) = SeqParameterSet::from_bits(RefNal::new(nal, &[], true).rbsp_bits()) else {
                    fail(&mut v, n, "SPS does not parse".into());
                    continue;
                };
                let signal = sps.vui_parameters.as_ref().and_then(|v| v.video_signal_type.as_ref());
                let colour = signal.map(|s| (s.video_full_range_flag, s.colour_description.as_ref().map(|c| (c.colour_primaries, c.transfer_characteristics, c.matrix_coefficients))));
                let dims = sps.pixel_dimensions().unwrap_or((0, 0));
                let sar = sps.vui_parameters.as_ref().and_then(|v| v.aspect_ratio_info.as_ref()).map(|a| format!("{a:?}")).unwrap_or_else(|| "none".into());
                let line = format!("profile_idc={} constraint_flags={:#04x} level_idc={} size={}x{} max_refs={} colour={:?} aspect={}", nal[1], nal[2], nal[3], dims.0, dims.1, sps.max_num_ref_frames, colour, sar);
                if line != v.sps {
                    println!("  SPS at access unit {n}: {line}");
                    v.sps = line;
                }
                if dims != size {
                    fail(&mut v, n, format!("SPS size {}x{}, stream {}x{}", dims.0, dims.1, size.0, size.1));
                }
                if nal[1] != 66 || nal[2] & 0x40 == 0 {
                    fail(&mut v, n, format!("not Constrained Baseline: profile_idc={} flags={:#04x}", nal[1], nal[2]));
                }
                if strict && colour != Some((false, Some((1, 1, 6)))) {
                    fail(&mut v, n, format!("SPS colours {colour:?}, expected limited range 1/1/6"));
                }
            }
            let yuv = raw.as_mut().and_then(|d| match d.decode(&au) {
                Ok(Some(yuv)) => {
                    let (ys, us, vs) = yuv.strides();
                    let (w, h) = yuv.dimensions();
                    Some(BARS.iter().enumerate().map(|(i, _)| {
                        let (x, y) = ((2 * i + 1) * w / 14, h / 3);
                        [yuv.y()[y * ys + x] as i32, yuv.u()[y / 2 * us + x / 2] as i32, yuv.v()[y / 2 * vs + x / 2] as i32]
                    }).collect::<Vec<_>>())
                }
                _ => None,
            });
            match decode_video_frame(&mut decoders, PEER, &au) {
                Decoded::Picture(buf, w, h) => {
                    v.decoded += 1;
                    if (w, h) != size || (buf.width(), buf.height()) != size {
                        fail(&mut v, n, format!("decoded {w}x{h} (picture {}x{}), stream {}x{}", buf.width(), buf.height(), size.0, size.1));
                    } else if colours && idr && colours_done < 2 {
                        colours_done += 1;
                        let px = buf.as_bytes();
                        for (i, (name, rgb)) in BARS.iter().enumerate() {
                            let (x, y) = ((2 * i + 1) * size.0 as usize / 14, size.1 as usize / 3);
                            let o = (y * size.0 as usize + x) * 4;
                            let shown = [px[o] as i32, px[o + 1] as i32, px[o + 2] as i32];
                            let [r, g, b] = *rgb;
                            let expected = [((66 * r + 129 * g + 25 * b + 128) >> 8) + 16, ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128, ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128];
                            let got = yuv.as_ref().map(|y| y[i]).unwrap_or([0, 0, 0]);
                            let yuv_err = (0..3).map(|k| (got[k] - expected[k]).abs()).max().unwrap_or(0);
                            let rgb_err = (0..3).map(|k| (shown[k] - rgb[k]).abs()).max().unwrap_or(0);
                            let ok = yuv_err <= 3 && rgb_err <= 6;
                            v.colour_lines.push(format!(
                                "  {name:8} RGB {:3},{:3},{:3} -> YUV {:3},{:3},{:3} (BT.601 limited expects {:3},{:3},{:3}) -> receiver RGB {:3},{:3},{:3}  {}",
                                r, g, b, got[0], got[1], got[2], expected[0], expected[1], expected[2], shown[0], shown[1], shown[2], if ok { "ok" } else { "MISMATCH" }
                            ));
                            if !ok {
                                fail(&mut v, n, format!("colour {name} is off by {yuv_err} (YUV) / {rgb_err} (RGB)"));
                            }
                        }
                    }
                }
                Decoded::ShownByGpu(_) => v.decoded += 1,
                Decoded::Nothing => fail(&mut v, n, "receiver decoded nothing".into()),
                Decoded::Failed => fail(&mut v, n, "receiver failed to decode".into()),
                Decoded::NeedKeyframe => fail(&mut v, n, "receiver asks for a keyframe".into()),
            }
            v.cpu = thread_cpu();
        }
        v
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn dist(label: &str, values: &mut Vec<f64>) -> String {
        values.sort_by(|a, b| a.total_cmp(b));
        if values.is_empty() {
            return format!("{label}: none");
        }
        format!("{label}: n={} p50={:.1} p95={:.1} max={:.1}", values.len(), percentile(values, 0.5), percentile(values, 0.95), values[values.len() - 1])
    }

    /// A cheap fingerprint of a frame (one byte in 4099), to count distinct pictures.
    fn fingerprint(frame: &[u8]) -> u64 {
        frame.iter().step_by(4099).fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
    }

    // ------------------------------------------------------------------ one share

    struct Setup {
        /// Reach PipeWire through an own connection per share, as with the portal's fd.
        own_fd: bool,
        as_window: bool,
        /// Nothing was arranged to keep the GPU capture from starting.
        expect_start: bool,
        /// Seconds into the share at which the source is ended, and how.
        end_source: Option<(f64, Arc<dyn Fn() + Send + Sync>)>,
        node: u32,
        size: (u32, u32),
        fps: u32,
        legacy_only: bool,
        memory: gpu::Memory,
        max_fps: Option<u32>,
        /// (watched, seconds) in order.
        watch: Vec<(bool, f64)>,
        /// (second, bps).
        bitrates: Vec<(f64, u32)>,
        pli: Vec<f64>,
        verify: bool,
        colours: bool,
        verbose: bool,
        helper_pids: Vec<u32>,
    }

    #[derive(Default)]
    struct Totals {
        watched_secs: f64,
        aus: usize,
        gpu_aus: usize,
        idrs: usize,
        bytes: usize,
        repeats: usize,
        cpu_self: f64,
        cpu_helpers: f64,
        cpu_shell: f64,
        secs: f64,
    }

    fn run_share(setup: &Setup, cycle: usize, fails: &mut Failures) -> Totals {
        let running = Arc::new(AtomicBool::new(true));
        let legacy = Arc::new(LegacyShared { frame: Mutex::new(None), frames_read: AtomicU64::new(0), first_frame_at: Mutex::new(None), child_pid: AtomicU32::new(0), previews: AtomicU64::new(0) });
        let preview_count = Arc::new(AtomicU64::new(0));
        let preview_bad = Arc::new(AtomicU64::new(0));
        {
            // What ScreenCaptureManager::start does with the UI's callback.
            let (count, bad, size) = (preview_count.clone(), preview_bad.clone(), setup.size);
            *PORTAL_LOCAL_CB.lock().unwrap() = Some(Arc::new(move |p: SharedPixelBuffer<Rgba8Pixel>| {
                count.fetch_add(1, Ordering::Relaxed);
                let opaque = p.as_bytes().chunks_exact(4).step_by(997).all(|px| px[3] == 255);
                if (p.width(), p.height()) != size || !opaque {
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // The portal thread of linux_portal.rs from the point where it has the fd and node.
        let gpu_gave_up_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let gpu_started = Arc::new(AtomicBool::new(false));
        let gpu_ended: Arc<Mutex<Option<gpu::Ended>>> = Arc::new(Mutex::new(None));
        let own_fd = match setup.own_fd.then(open_remote).transpose() {
            Ok(fd) => fd,
            Err(e) => {
                fails.add(format!("no PipeWire connection of its own: {e}"));
                None
            }
        };
        if let Some(fd) = &own_fd {
            println!("  PipeWire reached through fd {} (own socket)", fd.as_raw_fd());
        }
        let portal = {
            let (running, legacy, gave_up, gpu_started, gpu_ended) = (running.clone(), legacy.clone(), gpu_gave_up_at.clone(), gpu_started.clone(), gpu_ended.clone());
            let (node, size, fps, legacy_only, memory, max_fps, as_window) = (setup.node, setup.size, setup.fps, setup.legacy_only, setup.memory, setup.max_fps, setup.as_window);
            std::thread::Builder::new().name("portal".into()).spawn(move || {
                // Both connections are closed when this thread ends, like the portal's.
                let mut fd = own_fd;
                let mut _old_fd = None;
                let raw = |fd: &Option<OwnedFd>| fd.as_ref().map(|f| f.as_raw_fd());
                let wanted = || running.load(Ordering::Relaxed);
                if !legacy_only && gpu::allowed(as_window) {
                    let began = Instant::now();
                    let begun = if memory == gpu::Memory::Auto && max_fps.is_none() {
                        gpu::begin(raw(&fd), node, size.0, size.1, fps, &wanted)
                    } else {
                        gpu::begin_with(&gpu::Config { fd: raw(&fd), node, width: size.0, height: size.1, fps, memory, max_fps, min_qp: 30, main: std::env::var("LITECORD_TEST_CAPTURE_MAIN").is_ok() }, &wanted).map_err(|e| println!("  GPU capture did not start: {e}")).ok()
                    };
                    println!("  begin() returned after {} ms: {}", began.elapsed().as_millis(), if begun.is_some() { "started" } else { "not started" });
                    let ended = match begun {
                        Some(id) => {
                            gpu_started.store(true, Ordering::SeqCst);
                            let ended = gpu::run_until_end(id, wanted);
                            *gpu_ended.lock().unwrap() = Some(ended);
                            ended
                        }
                        None => gpu::Ended::Failed,
                    };
                    if ended != gpu::Ended::Failed || !wanted() {
                        return;
                    }
                    *gave_up.lock().unwrap() = Some(Instant::now());
                    // A second OpenPipeWireRemote, and the first connection shut.
                    if let Some(old) = fd.take() {
                        match open_remote() {
                            Ok(new) => {
                                unsafe { libc::shutdown(old.as_raw_fd(), libc::SHUT_RDWR) };
                                println!("  the gst-launch capture gets fd {} (a second connection); fd {} is shut", new.as_raw_fd(), old.as_raw_fd());
                                fd = Some(new);
                                _old_fd = Some(old);
                            }
                            Err(e) => {
                                println!("  no second connection ({e}): the gst-launch capture uses the first");
                                fd = Some(old);
                            }
                        }
                    }
                }
                if wanted() {
                    legacy_capture(&legacy, raw(&fd), node, size, fps, &running);
                }
            })
        }
        .expect("portal thread");

        // The "screen-capture-worker" thread: polls the published frame every tick.
        let tx_watched = Arc::new(AtomicBool::new(false));
        let (tx_frame, rx_frame) = std::sync::mpsc::sync_channel::<(Vec<u8>, u64)>(6);
        let worker = {
            let (running, legacy, tx_watched) = (running.clone(), legacy.clone(), tx_watched.clone());
            let (total, fps) = ((setup.size.0 * setup.size.1 * 4) as usize, setup.fps);
            std::thread::Builder::new().name("capture-worker".into()).spawn(move || {
                let frame_interval = Duration::from_nanos(1_000_000_000 / fps as u64);
                let mut next = Instant::now() + frame_interval;
                while running.load(Ordering::Relaxed) {
                    let mut cur = Vec::with_capacity(total);
                    let got = {
                        let slot = legacy.frame.lock().unwrap();
                        slot.as_ref().filter(|f| f.len() == total).map(|f| {
                            cur.resize(total, 0);
                            cur.copy_from_slice(f);
                        })
                    };
                    if got.is_some() {
                        let _ = tx_frame.try_send((cur, legacy.frames_read.load(Ordering::Relaxed)));
                    }
                    let interval = if tx_watched.load(Ordering::Relaxed) { frame_interval } else { Duration::from_millis(250) };
                    let now = Instant::now();
                    if now < next {
                        std::thread::sleep(next - now);
                        next += interval;
                    } else if now - next > Duration::from_millis(100) {
                        next = now + interval;
                    } else {
                        next += interval;
                    }
                }
            })
        }
        .expect("worker thread");

        // The receivers.
        let (tx_verify, rx_verify) = std::sync::mpsc::sync_channel::<(usize, Vec<u8>, bool)>(600);
        let verify_thread = {
            let (size, colours) = (setup.size, setup.colours);
            std::thread::Builder::new().name("verifier".into()).spawn(move || verifier(rx_verify, size, colours)).expect("verifier thread")
        };

        // The "screen-capture-tx" loop of manager.rs.
        let frame_target_interval = Duration::from_micros(1_000_000 / setup.fps as u64);
        let still_repeat_every = Duration::from_millis(100);
        let mut h264_encoder: Option<Box<dyn VideoEncoder>> = None;
        let mut latest: Option<(Vec<u8>, u64)> = None;
        let mut fresh_picture = false;
        let mut last_fresh = Instant::now();
        let mut last_encode = Instant::now() - still_repeat_every;
        let mut last_idr = Instant::now() - Duration::from_secs(10);
        let mut next_tick = Instant::now();
        let mut was_watched = false;
        let mut keyframe_requested = false;

        let share_start = Instant::now();
        let total_secs: f64 = setup.watch.iter().map(|w| w.1).sum();
        let shell_pid = find_pid("gnome-shell");
        let mut pli_left = setup.pli.clone();
        let mut totals = Totals::default();

        // Bookkeeping for the report.
        let mut sec = 0u64;
        let mut cpu_mark = (self_cpu(), shell_pid.and_then(pid_cpu).unwrap_or(0.0));
        let mut helper_last: HashMap<u32, f64> = HashMap::new();
        let (mut s_aus, mut s_idr, mut s_bytes, mut s_repeat, mut s_turns, mut s_dup) = (0, 0, 0usize, 0, 0, 0);
        // While nobody watches: when that began, and the number of pictures that had gone
        // into the encoder shortly after (what was in flight when the viewers left is theirs).
        let mut unwatched_mark: Option<(Instant, Option<u64>)> = None;
        let encoded_total = || gpu::stats().map(|st| st.encoded_new + st.encoded_repeats);
        let mut s_lat: Vec<f64> = Vec::new();
        let mut s_enc: Vec<f64> = Vec::new();
        let mut all_lat: Vec<f64> = Vec::new();
        let mut all_enc: Vec<f64> = Vec::new();
        let mut all_gap: Vec<f64> = Vec::new();
        let mut idr_latency: Vec<f64> = Vec::new();
        let mut watch_latency: Vec<f64> = Vec::new();
        let mut idr_pending: Option<(Instant, &str)> = None;
        let mut watch_started: Option<Instant> = None;
        let mut first_after_watch = false;
        let mut last_stats = gpu::Stats::default();
        let mut last_gpu_stats: Option<gpu::Stats> = None;
        let mut last_frames_read = 0u64;
        let mut last_fp = 0u64;
        let mut last_encoded_seq = u64::MAX;
        let mut last_au_at: Option<Instant> = None;
        let mut last_gpu_au_at: Option<Instant> = None;
        let mut fallback_reported = false;
        let mut unwatched_aus = 0usize;
        let mut au_index = 0usize;
        let mut path_now = "-";
        let mut encoder_name = String::new();
        let mut distinct = 0u64;
        let mut verify_dropped = 0usize;
        let mut printed_caps = false;
        let mut s_label = 0;
        let mut end_source = setup.end_source.clone();

        while !STOP.load(Ordering::Relaxed) {
            let t = share_start.elapsed().as_secs_f64();
            if t >= total_secs {
                break;
            }
            if end_source.as_ref().is_some_and(|(at, _)| t >= *at) {
                if let Some((_, end)) = end_source.take() {
                    println!("  t={t:.2}s the source is ended");
                    end();
                }
            }
            // The watch schedule stands in for watch::viewer_addrs().
            let mut acc = 0.0;
            let watched = setup.watch.iter().find(|(_, secs)| {
                acc += secs;
                t < acc
            }).map(|w| w.0).unwrap_or(false);
            let bitrate = setup.bitrates.iter().rev().find(|(s, _)| *s <= t).map(|b| b.1).unwrap_or(8_000_000);
            if pli_left.first().is_some_and(|p| *p <= t) {
                pli_left.remove(0);
                keyframe_requested = true;
            }

            let gpu_path = gpu::active();
            if gpu_path && !printed_caps {
                if let Some(st) = gpu::stats() {
                    printed_caps = true;
                    last_stats = st.clone();
                    println!("  negotiated with the source: {}", st.source_caps);
                    println!("  relabelled for vapostproc:  {}", st.converter_caps);
                    println!("  memory: {}, source {}x{} -> stream {}x{}", if st.dmabuf { "DMABuf (zero copy)" } else { "system memory" }, st.source_size.0, st.source_size.1, setup.size.0, setup.size.1);
                }
            }
            if let Some(st) = gpu::stats() {
                last_gpu_stats = Some(st);
            }
            if gpu_path {
                let _ = gpu::take_new_pictures();
            }
            while let Ok(new_frame) = rx_frame.try_recv() {
                fresh_picture = true;
                last_fresh = Instant::now();
                latest = Some(new_frame);
            }
            if latest.is_none() && !gpu_path {
                match rx_frame.recv_timeout(Duration::from_millis(20)) {
                    Ok(first) => latest = Some(first),
                    Err(_) => continue,
                }
            }

            let interval = if was_watched { frame_target_interval } else { Duration::from_millis(250) };
            let now = Instant::now();
            let mut waited = false;
            if now < next_tick {
                let sleep_dur = (next_tick - now).min(interval);
                if gpu_path && was_watched {
                    gpu::wait_ready(sleep_dur);
                    waited = true;
                } else if was_watched {
                    std::thread::park_timeout(sleep_dur);
                } else {
                    // The app is unparked by wake_tx() when a viewer arrives: the schedule
                    // is looked at again every 20 ms instead.
                    std::thread::park_timeout(sleep_dur.min(Duration::from_millis(20)));
                    if Instant::now() < next_tick && !watched {
                        continue;
                    }
                }
            }
            next_tick += interval;
            if now > next_tick + interval * 2 {
                next_tick = now + interval;
            }
            if waited {
                next_tick = next_tick.min(Instant::now() + interval);
            }

            if watched != was_watched {
                was_watched = watched;
                tx_watched.store(watched, Ordering::Relaxed);
                gpu::set_watched(watched);
                println!("  t={t:.2}s {}", if watched { "a viewer arrives" } else { "the viewers leave" });
                if let (Some((_, Some(before))), Some(now)) = (unwatched_mark.take(), encoded_total()) {
                    unwatched_aus += now.saturating_sub(before) as usize;
                }
                if watched {
                    // The first viewer to arrive asks for a keyframe (watch.rs).
                    keyframe_requested = true;
                    watch_started = Some(Instant::now());
                    first_after_watch = true;
                    next_tick = Instant::now();
                } else {
                    idr_pending = None;
                }
            }

            let encode_now = fresh_picture || last_fresh.elapsed() < still_repeat_every || last_encode.elapsed() >= still_repeat_every || keyframe_requested;
            if watched && encode_now {
                fresh_picture = false;
                last_encode = Instant::now();
            }
            let enc_start = Instant::now();
            let mut repeat = false;
            let mut latency = None;
            let out: Option<Vec<u8>> = if !watched {
                None
            } else if gpu_path {
                let want_idr = std::mem::take(&mut keyframe_requested) || last_idr.elapsed() >= Duration::from_millis(2000);
                if want_idr {
                    last_idr = Instant::now();
                    idr_pending.get_or_insert((Instant::now(), "request"));
                }
                gpu::next_picture(bitrate, want_idr).map(|p| {
                    repeat = p.repeat;
                    latency = Some(p.latency.as_secs_f64() * 1000.0);
                    p.data
                })
            } else if let Some((frame, seq)) = latest.as_ref().filter(|_| encode_now) {
                let enc = h264_encoder.get_or_insert_with(|| create_best_encoder(setup.fps, true));
                if enc.get_bitrate_bps() != bitrate {
                    enc.set_bitrate_bps(bitrate);
                }
                if std::mem::take(&mut keyframe_requested) || last_idr.elapsed() >= Duration::from_millis(2000) {
                    last_idr = Instant::now();
                    enc.force_intra_frame();
                    idr_pending.get_or_insert((Instant::now(), "request"));
                }
                let fp = fingerprint(frame);
                repeat = *seq == last_encoded_seq || fp == last_fp;
                if fp != last_fp {
                    distinct += 1;
                }
                last_fp = fp;
                last_encoded_seq = *seq;
                let au = enc.encode(frame, setup.size.0, setup.size.1);
                encoder_name = enc.name().to_string();
                au
            } else {
                None
            };
            let enc_ms = enc_start.elapsed().as_secs_f64() * 1000.0;
            s_turns += 1;
            // What the app shows as the stream's FPS.
            if !(gpu_path && watched) || out.is_some() {
                s_label += 1;
            }

            if !was_watched && gpu_path {
                let mark = unwatched_mark.get_or_insert((Instant::now(), None));
                if mark.1.is_none() && mark.0.elapsed() >= Duration::from_millis(150) {
                    mark.1 = encoded_total();
                }
            }

            if let Some(au) = out {
                let now = Instant::now();
                let types: Vec<u8> = nal_ranges(&au).iter().map(|r| au[r.start] & 0x1f).collect();
                let idr = types.contains(&5);
                path_now = if gpu_path { "GPU" } else { "legacy" };
                if gpu_path {
                    totals.gpu_aus += 1;
                    last_gpu_au_at = Some(now);
                } else {
                    s_enc.push(enc_ms);
                    all_enc.push(enc_ms);
                    if let (Some(gave_up), false) = (*gpu_gave_up_at.lock().unwrap(), fallback_reported) {
                        fallback_reported = true;
                        let gap = last_gpu_au_at.map(|t| now.duration_since(t).as_secs_f64() * 1000.0);
                        println!(
                            "  t={t:.2}s FALLBACK: first access unit of the gst-launch path {:.0} ms after the GPU path gave up ({} ms after its last access unit); NAL types {types:?}",
                            now.duration_since(gave_up).as_secs_f64() * 1000.0,
                            gap.map(|g| format!("{g:.0}")).unwrap_or_else(|| "-".into())
                        );
                        if !idr {
                            fails.add("the first picture after the fallback is not a keyframe".into());
                        }
                        // The viewers were without pictures: this is the stream's gap.
                        idr_pending = None;
                        last_au_at = None;
                    }
                }
                if let Some(ms) = latency {
                    s_lat.push(ms);
                    all_lat.push(ms);
                }
                if let Some(prev) = last_au_at {
                    all_gap.push(now.duration_since(prev).as_secs_f64() * 1000.0);
                }
                last_au_at = Some(now);
                if first_after_watch {
                    first_after_watch = false;
                    let ms = watch_started.map(|w| now.duration_since(w).as_secs_f64() * 1000.0).unwrap_or(0.0);
                    watch_latency.push(ms);
                    println!("  t={t:.2}s first access unit {ms:.0} ms after the viewer arrived: {} bytes, NAL types {types:?}", au.len());
                    if !idr {
                        fails.add(format!("first access unit after a viewer arrived is not an IDR (NAL types {types:?})"));
                    }
                    last_au_at = Some(now);
                    all_gap.pop();
                }
                if idr {
                    if let Some((asked, _)) = idr_pending.take() {
                        idr_latency.push(now.duration_since(asked).as_secs_f64() * 1000.0);
                    }
                    s_idr += 1;
                    totals.idrs += 1;
                }
                if repeat {
                    s_repeat += 1;
                    totals.repeats += 1;
                    if !gpu_path {
                        s_dup += 1;
                    }
                }
                if setup.verbose {
                    println!("  t={t:.3}s {path_now} au {au_index}: {} bytes NAL {types:?}{}{}", au.len(), if repeat { " repeat" } else { "" }, latency.map(|l| format!(" {l:.1} ms")).unwrap_or_default());
                }
                s_aus += 1;
                s_bytes += au.len();
                totals.aus += 1;
                totals.bytes += au.len();
                let strict = gpu_path || h264_encoder.as_ref().is_some_and(|e| e.is_hardware_accelerated());
                if setup.verify && tx_verify.try_send((au_index, au, strict)).is_err() {
                    verify_dropped += 1;
                }
                au_index += 1;
            }
            if let Some((asked, _)) = idr_pending {
                if asked.elapsed() > Duration::from_millis(600) {
                    fails.add(format!("t={t:.2}s: a keyframe asked for 600 ms ago has not arrived"));
                    idr_pending = None;
                }
            }

            // Once a second: what happened, and what it cost.
            if share_start.elapsed().as_secs() > sec {
                sec = share_start.elapsed().as_secs();
                let helper_pids: Vec<u32> = setup.helper_pids.iter().copied().chain(Some(legacy.child_pid.load(Ordering::Relaxed)).filter(|p| *p != 0)).collect();
                let mut helpers = 0.0;
                for pid in helper_pids {
                    if let Some(cpu) = pid_cpu(pid) {
                        helpers += cpu - helper_last.insert(pid, cpu).unwrap_or(cpu);
                    }
                }
                let now = (self_cpu(), shell_pid.and_then(pid_cpu).unwrap_or(0.0));
                let d_self = now.0 - cpu_mark.0;
                let d_shell = now.1 - cpu_mark.1;
                cpu_mark = now;
                let st = gpu::stats().unwrap_or_else(|| last_stats.clone());

                let frames_read = legacy.frames_read.load(Ordering::Relaxed);
                s_lat.sort_by(|a, b| a.total_cmp(b));
                s_enc.sort_by(|a, b| a.total_cmp(b));
                let detail = if gpu::active() {
                    format!(
                        "pictures {:3} (buffers {:3}) encoded new {:3} repeat {:2} over-rate {:3} | capture->AU ms p50 {:4.1} max {:4.1}",
                        st.pictures.saturating_sub(last_stats.pictures),
                        st.buffers.saturating_sub(last_stats.buffers),
                        st.encoded_new.saturating_sub(last_stats.encoded_new),
                        st.encoded_repeats.saturating_sub(last_stats.encoded_repeats),
                        st.skipped_pace.saturating_sub(last_stats.skipped_pace),
                        percentile(&s_lat, 0.5),
                        s_lat.last().copied().unwrap_or(0.0)
                    )
                } else {
                    format!("frames from the pipe {:3} distinct encoded {:3} duplicate encodes {:3} | encode() ms p50 {:4.1} max {:4.1}", frames_read - last_frames_read, distinct, s_dup, percentile(&s_enc, 0.5), s_enc.last().copied().unwrap_or(0.0))
                };
                println!(
                    "  [{sec:3}s {:6} {}] AU {:3} (IDR {} repeat {:2}) {:5.0} kbit | {detail} | turns {:3} FPS label {:3} | cpu self {:.2} helpers {:.2} gnome-shell {:.2}",
                    if gpu::active() { "GPU" } else { path_now },
                    if was_watched { "watched  " } else { "unwatched" },
                    s_aus,
                    s_idr,
                    s_repeat,
                    s_bytes as f64 * 8.0 / 1000.0,
                    s_turns,
                    s_label,
                    d_self,
                    helpers,
                    d_shell
                );
                if was_watched {
                    totals.watched_secs += 1.0;
                }
                totals.cpu_self += d_self;
                totals.cpu_helpers += helpers;
                totals.cpu_shell += d_shell;
                totals.secs += 1.0;
                if gpu::stats().is_some() {
                    last_stats = st;
                }
                last_frames_read = frames_read;
                distinct = 0;
                (s_aus, s_idr, s_bytes, s_repeat, s_turns, s_dup) = (0, 0, 0, 0, 0, 0);
                s_label = 0;
                s_lat.clear();
                s_enc.clear();
            }
        }

        if let (Some((_, Some(before))), Some(now)) = (unwatched_mark.take(), encoded_total()) {
            unwatched_aus += now.saturating_sub(before) as usize;
        }

        // ScreenCaptureManager::stop.
        let stop_started = Instant::now();
        running.store(false, Ordering::SeqCst);
        *legacy.frame.lock().unwrap() = None;
        let pid = legacy.child_pid.load(Ordering::SeqCst);
        if pid != 0 {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        *PORTAL_LOCAL_CB.lock().unwrap() = None;
        gpu::end();
        let stop_ms = stop_started.elapsed().as_secs_f64() * 1000.0;
        drop(h264_encoder);
        let _ = portal.join();
        let portal_ms = stop_started.elapsed().as_secs_f64() * 1000.0;
        let _ = worker.join();
        drop(tx_verify);
        let verdict = verify_thread.join().unwrap_or_default();
        let verify_cpu_last = verdict.cpu;

        // ---- this share's report
        println!("\n== share {cycle}: results ==");
        let previews = preview_count.load(Ordering::Relaxed);
        println!(
            "access units: {} ({} from the GPU capture, {} from the gst-launch path{}), {} IDR, {} repeats, {:.0} kbit/s while watched",
            totals.aus,
            totals.gpu_aus,
            totals.aus - totals.gpu_aus,
            if encoder_name.is_empty() { String::new() } else { format!(" encoded by \"{encoder_name}\"") },
            totals.idrs,
            totals.repeats,
            totals.bytes as f64 * 8.0 / 1000.0 / totals.watched_secs.max(1.0)
        );
        println!("delivered: {:.1} access units per watched second", totals.aus as f64 / totals.watched_secs.max(1.0));
        if let Some(st) = &last_gpu_stats {
            if st.stamped_pictures > 0 {
                println!(
                    "producer's timestamp -> buffer out of pipewiresrc: mean {:.1} ms, max {:.1} ms over {} of {} pictures (Mutter stamps the moment it records the frame; add this to capture -> access unit)",
                    st.source_delay_us as f64 / 1000.0 / st.stamped_pictures as f64, st.source_delay_max_us as f64 / 1000.0, st.stamped_pictures, st.pictures
                );
            } else {
                println!("producer's timestamp -> buffer out of pipewiresrc: the pictures carry no usable timestamp of their producer");
            }
            let per = |us: u64| us as f64 / 1000.0 / st.previews.max(1) as f64;
            println!(
                "preview pictures: {:.1} ms to read back from the GPU, {:.1} ms to allocate the UI's pixel buffer, {:.1} ms to convert to RGBA and call back; {:.1} ms of CPU each on the preview thread",
                per(st.preview_us[0]), per(st.preview_us[1]), per(st.preview_us[2]), per(st.preview_us[3])
            );
            println!(
                "GPU capture: {} buffers, {} distinct pictures; encoded {} new + {} repeats; {} new pictures left out for the frame rate; {} thrown away while a keyframe was awaited; {} previews; {} renegotiations by the source before its first picture",
                st.buffers, st.pictures, st.encoded_new, st.encoded_repeats, st.skipped_pace, st.dropped_before_keyframe, st.previews, st.early_renegotiations
            );
        }
        println!("{} ms", dist("capture -> access unit in hand (GPU path)", &mut all_lat));
        println!("{} ms", dist("encode() per frame (gst-launch path; excludes the capture pipeline before the pipe)", &mut all_enc));
        println!("{} ms", dist("gap between access units while watched", &mut all_gap));
        println!("{} ms", dist("keyframe request -> IDR in hand", &mut idr_latency));
        println!("{} ms", dist("viewer arrives -> first access unit (an IDR)", &mut watch_latency));
        println!("local preview: {previews} callbacks in {:.1} s ({:.1}/s), {} with a wrong size or alpha", totals.secs, previews as f64 / totals.secs.max(1.0), preview_bad.load(Ordering::Relaxed));
        println!(
            "CPU per second: this process {:.3} (of which the verifier's decoding {:.3}), helper processes {:.3}, gnome-shell {:.3}",
            totals.cpu_self / totals.secs.max(1.0),
            verify_cpu_last / totals.secs.max(1.0),
            totals.cpu_helpers / totals.secs.max(1.0),
            totals.cpu_shell / totals.secs.max(1.0)
        );
        println!("stop() returned in {stop_ms:.1} ms; the portal thread ended {portal_ms:.0} ms after it");
        let ended = *gpu_ended.lock().unwrap();
        if let Some(ended) = ended {
            println!("the GPU capture ended as: {ended:?}");
        }
        if setup.end_source.is_some() {
            if ended != Some(gpu::Ended::SourceGone) {
                fails.add(format!("the source was ended but the GPU capture ended as {ended:?}"));
            }
            if totals.aus > totals.gpu_aus {
                fails.add(format!("{} access units came from a gst-launch capture after the source was ended", totals.aus - totals.gpu_aus));
            }
        }
        if portal_ms > 1000.0 {
            fails.add(format!("the portal thread took {portal_ms:.0} ms to end after stop()"));
        }
        if setup.verify {
            println!("receiver: {} of {} access units decoded at {}x{}{}", verdict.decoded, totals.aus - verify_dropped, setup.size.0, setup.size.1, if verify_dropped > 0 { format!(" ({verify_dropped} not checked: verifier behind)") } else { String::new() });
            for line in &verdict.colour_lines {
                println!("{line}");
            }
            if verdict.decoded != totals.aus - verify_dropped {
                fails.add(format!("{} access units were not decoded", totals.aus - verify_dropped - verdict.decoded));
            }
            if setup.colours && verdict.colour_lines.is_empty() {
                fails.add("no colour check was made".into());
            }
        }
        for f in verdict.failures {
            fails.add(f);
        }
        if unwatched_aus > 0 {
            fails.add(format!("{unwatched_aus} pictures were encoded while nobody watched"));
        }
        if preview_bad.load(Ordering::Relaxed) > 0 {
            fails.add("preview pictures with a wrong size or alpha".into());
        }
        if previews == 0 {
            fails.add("no local preview".into());
        }
        if totals.watched_secs >= 1.0 && totals.aus == 0 {
            fails.add("no access units while watched".into());
        }
        if !setup.legacy_only && !gpu_started.load(Ordering::SeqCst) && setup.expect_start && gpu::enabled().is_ok() {
            fails.add("the GPU capture did not start".into());
        }
        totals
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
        // The Mutter session is stopped on every way out: normal return, signal, panic. (A
        // kill -9 leaves it to the compositor, which ends it when the D-Bus name goes.)
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_hook(info);
            stop_mutter();
        }));
        let code = match run_with(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        };
        stop_mutter();
        if STOP.load(Ordering::SeqCst) {
            return 130;
        }
        code
    }

    fn run_with(args: &Args) -> Result<i32, String> {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
        if let Some(mode) = args.0.get("fail") {
            std::env::set_var("LITECORD_VAAPI_CAPTURE_TEST_FAIL", mode);
        }
        if let Some(setting) = args.0.get("encoder-setting") {
            // In memory only: the settings file is not written.
            litecord::utils::video_settings::get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner()).video_encoder = setting.clone();
        }
        let size = parse_size(&args.str("size", "1080"))?;
        let fps = (args.num("fps", 60.0)? as u32).clamp(1, 240);
        let duration = args.num("duration", 10.0)?;
        let watch_spec = args.str("watch", &format!("u1,w{duration}"));
        let watch: Vec<(bool, f64)> = watch_spec
            .split(',')
            .map(|p| {
                let secs = p.get(1..).and_then(|s| s.parse::<f64>().ok()).ok_or(format!("bad --watch part {p}"))?;
                match p.as_bytes().first() {
                    Some(b'w') => Ok((true, secs)),
                    Some(b'u') => Ok((false, secs)),
                    _ => Err(format!("bad --watch part {p}")),
                }
            })
            .collect::<Result<_, String>>()?;
        let bitrate_spec = args.str("bitrate", "8000");
        let bitrates: Vec<(f64, u32)> = bitrate_spec
            .split(',')
            .map(|p| {
                let (at, kbps) = p.split_once(':').unwrap_or(("0", p));
                Ok((at.parse::<f64>().map_err(|_| format!("bad --bitrate {p}"))?, kbps.parse::<u32>().map_err(|_| format!("bad --bitrate {p}"))? * 1000))
            })
            .collect::<Result<_, String>>()?;
        let pli: Vec<f64> = args.str("pli", "").split(',').filter(|s| !s.is_empty()).map(|s| s.parse().map_err(|_| format!("bad --pli {s}"))).collect::<Result<_, _>>()?;
        let memory = match args.str("memory", "auto").as_str() {
            "auto" => gpu::Memory::Auto,
            "dmabuf" => gpu::Memory::DmaBuf,
            "system" => gpu::Memory::System,
            other => return Err(format!("bad --memory {other}")),
        };
        let max_fps = args.0.get("max-fps").map(|v| v.parse::<u32>().map_err(|_| format!("bad --max-fps {v}"))).transpose()?;
        let cycles = args.num("cycles", 1.0)? as usize;
        let cycle_gap = Duration::from_millis(args.num("cycle-gap", 200.0)? as u64);
        let total_secs: f64 = watch.iter().map(|w| w.1).sum::<f64>() * cycles as f64;

        // ---- the source
        let source = args.str("source", "mutter");
        let helper_pids = Vec::new();
        let mut _synthetic = None;
        let end_at = args.0.get("end-source").map(|v| v.parse::<f64>().map_err(|_| format!("bad --end-source {v}"))).transpose()?;
        let mut end_source: Option<(f64, Arc<dyn Fn() + Send + Sync>)> = None;
        let node = if let Some(id) = source.strip_prefix("node:") {
            let id = id.parse::<u32>().map_err(|_| format!("bad node id {id}"))?;
            if !video_node_present(id) {
                return Err(format!("PipeWire has no video node {id}"));
            }
            id
        } else if source == "synthetic" {
            let src_size = parse_size(&args.str("src-size", "2560x1440"))?;
            let src_fps = args.num("src-fps", 60.0)? as u32;
            let props = args.str("src-props", if args.has("colours") { "pattern=smpte100" } else { "pattern=ball motion=sweep" });
            let (synthetic, node) = Synthetic::spawn(&props, src_size, src_fps.max(1), args.0.get("src-colorimetry").map(|s| s.as_str()))?;
            let pid = synthetic.child.id() as i32;
            println!("source: synthetic PipeWire node {node} (videotestsrc {props}, {}x{} at {src_fps} fps), provider pid {pid}", src_size.0, src_size.1);
            if let Some(secs) = args.0.get("freeze-source") {
                let secs = secs.parse::<f64>().map_err(|_| format!("bad --freeze-source {secs}"))?;
                unsafe { libc::kill(pid, libc::SIGSTOP) };
                println!("the provider is stopped for {secs} s");
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs_f64(secs));
                    unsafe { libc::kill(pid, libc::SIGCONT) };
                    println!("  the provider runs again");
                });
            }
            if let Some(at) = end_at {
                end_source = Some((at, Arc::new(move || unsafe {
                    libc::kill(pid, libc::SIGKILL);
                })));
            }
            _synthetic = Some(synthetic);
            node
        } else if source == "mutter" || source.starts_with("mutter:") {
            if total_secs > 180.0 {
                return Err("recordings of the real screen are kept under three minutes".into());
            }
            let connector = source.strip_prefix("mutter:").unwrap_or("");
            let (_, node) = Mutter::open(connector)?;
            println!("source: Mutter ScreenCast RecordMonitor({}) cursor embedded, PipeWire node {node}", if connector.is_empty() { "primary" } else { connector });
            if let Some(at) = end_at {
                end_source = Some((at, Arc::new(stop_mutter)));
            }
            node
        } else {
            return Err(format!("unknown --source {source}"));
        };
        let setup = Setup {
            own_fd: args.has("fd"),
            as_window: args.has("as-window"),
            expect_start: !args.has("fail") && !args.has("freeze-source") && !args.has("as-window"),
            end_source,
            node,
            size,
            fps,
            legacy_only: args.has("legacy"),
            memory,
            max_fps,
            watch,
            bitrates,
            pli,
            verify: !args.has("no-verify"),
            colours: args.has("colours"),
            verbose: args.has("verbose"),
            helper_pids,
        };
        println!(
            "== {}: {}x{} {fps} fps, watch {watch_spec}, bitrate {bitrate_spec} kbps, {cycles} share(s){} ==",
            if setup.legacy_only { "today's gst-launch capture + create_best_encoder()" } else { "in-process GPU capture (linux_gpu_tx)" },
            size.0,
            size.1,
            args.0.get("fail").map(|f| format!(", failure injected: {f}")).unwrap_or_default()
        );

        let mut fails = Failures::default();
        let mut sum = Totals::default();
        for cycle in 1..=cycles {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            if cycle > 1 {
                std::thread::sleep(cycle_gap);
                println!("\n== starting share {cycle} ==");
            }
            let t = run_share(&setup, cycle, &mut fails);
            sum.aus += t.aus;
            sum.gpu_aus += t.gpu_aus;
            sum.secs += t.secs;
        }
        // Pipelines of stopped shares end in the background: none may be left. (The one
        // `hang` blocks ends when its 20 s are over.)
        let waited = Instant::now();
        let patience = Duration::from_secs(if args.0.get("fail").is_some_and(|f| f.starts_with("hang")) { 25 } else { 3 });
        while gpu::live_pipelines() > 0 && waited.elapsed() < patience {
            std::thread::sleep(Duration::from_millis(10));
        }
        let fds = std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
        println!("\nafter the last stop: {} GPU capture pipelines alive ({} ms after stop), {fds} open fds", gpu::live_pipelines(), waited.elapsed().as_millis());
        if gpu::live_pipelines() > 0 {
            fails.add(format!("{} pipelines still alive {} s after stop", gpu::live_pipelines(), patience.as_secs()));
        }
        // Where this process spent its CPU, by thread name (GStreamer names its streaming
        // threads after the pad that drives them).
        let mut by_thread: HashMap<String, f64> = HashMap::new();
        for task in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
            let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else { continue };
            let Some((head, tail)) = stat.rsplit_once(')') else { continue };
            let fields: Vec<&str> = tail.split_whitespace().collect();
            let cpu = (fields.get(11).and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0) + fields.get(12).and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0)) / ticks();
            *by_thread.entry(head.split_once('(').map(|h| h.1).unwrap_or("?").to_string()).or_default() += cpu;
        }
        let mut by_thread: Vec<(String, f64)> = by_thread.into_iter().filter(|t| t.1 >= 0.02).collect();
        by_thread.sort_by(|a, b| b.1.total_cmp(&a.1));
        println!("CPU seconds by thread still alive (threads that ended are not listed; total {:.2}): {}", self_cpu(), by_thread.iter().take(10).map(|(n, c)| format!("{n} {c:.2}")).collect::<Vec<_>>().join(", "));
        println!("\n== summary: {} access units ({} GPU capture) in {:.0} s over {cycles} share(s) ==", sum.aus, sum.gpu_aus, sum.secs);
        if fails.count == 0 {
            println!("RESULT: PASS");
            Ok(0)
        } else {
            println!("RESULT: FAIL ({} problems)", fails.count);
            for f in &fails.list {
                println!("  - {f}");
            }
            Ok(1)
        }
    }
}
