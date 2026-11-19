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
    use litecord::screen_capture::live_controls;
    use litecord::screen_capture::sources::linux_portal::{gst_launch_capture_args, WindowCapture, WindowRead, PORTAL_LOCAL_CB};
    use litecord::screen_capture::video_pipeline::{decode_video_frame, Decoded, StreamDecoders};
    use litecord::screen_capture::watch::{ProfileChoice, RateControl, Verdict as WatchVerdict, Viewers, CAP_H264_MAIN};
    use litecord::utils::video_settings::StreamProfile;
    use openh264::formats::YUVSource;
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    use std::collections::HashMap;
    use std::io::Read;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const HELP: &str = "\
test_vaapi_capture [options]
  --source S       mutter (default: Mutter ScreenCast RecordMonitor of the primary monitor, cursor
                   embedded, no dialog) | mutter:CONNECTOR | mutter-area:WxH[+X+Y] (RecordArea: a
                   rectangle of the desktop, for a compositor source of any shape) |
                   mutter-window[:ID] (RecordWindow, what the portal does for a shared window:
                   the window with that Mutter id, or the focused one) | node:ID |
                   synthetic (a `videotestsrc ! pipewiresink` this spawns)
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
  --profile P      the 'stream profile' setting: compatible (default) | efficient | auto
  --viewers SPEC   SEC:LIST,... who watches from those seconds on, by the send loop's own rules
                   (watch::Viewers + watch::ProfileChoice), instead of --watch; the share then
                   lasts --duration seconds in all. LIST: viewers joined by '+': m (native,
                   decodes Main), c (native, does not), w (a browser), or - for nobody. A
                   position keeps its identity: 0:m,5:c is one viewer changing its capability
                   byte. With --profile auto, 0:-,1:m,4:m+c,7:m goes Main -> Constrained Baseline
                   -> Main (the last step 5 s after the viewer that cannot decode Main left).
                   Without it, the viewers of --watch are one native viewer that decodes Main.
  --live SPEC      SEC:CHANGE,SEC:CHANGE,...: the stream controls of the running share
                   (live_controls, the button on the streamer's own stream), by the send loop's
                   own rules. CHANGE: bitrate=MBPS (the share's maximum bitrate, slider released),
                   floor=QP (the quality floor, released), floor~A..B (the floor slider dragged
                   from A to B, a step every 50 ms, never released: it applies once it has been
                   still for 400 ms). The stream's rate is then watch::RateControl's under the
                   share's maximum and the browser viewer's 8 Mbit/s (--bitrate is ignored).
                   Checked: a released change is in effect within 1.5 s (the target rate; for the
                   floor, an encoder that showed a keyframe with it), the first access unit of
                   a new floor is a keyframe, no slice has a QP below the floor in effect, and a
                   drag replaces the encoder once. Checked in the bitstream, so that the exit
                   status does not rest on what the app believes: slice QPs can be read with
                   every floor; after a floor went DOWN the lowest slice QP is below the floor
                   before it (the default synthetic pattern drives the QP to the floor; a
                   source that does not fails this); as many floors took effect as the engine
                   was asked for, and the encoder ends on the script's last one; the measured
                   rate 0.5 to 2.5 s after the target moved is under 1.3 times a lowered
                   target, and above 1.15 times the old one after a raise when the source was
                   using its rate (--src-props pattern=snow). Printed: when each change took
                   effect, the gap between access units around it, the slice QPs seen with each
                   floor and the measured rates.
                   LITECORD_VAAPI_TEST_FAIL=floor in the environment (with --legacy) makes the
                   memory-fed encoder's pipeline for a new floor fail: the floor before it must
                   stay, on VA-API.
  --live-start M:Q the saved settings the share starts from, in memory only (default 8:30)
  --pli SECS       extra keyframe requests at these times, e.g. 3.3,5.1 (besides one every 2 s)
  --memory M       auto (default) | dmabuf | system
  --max-fps N      ask the compositor for at most N pictures a second (experiment)
  --fail MODE      start | first | after:N | stall:N | nosrc:N | hang:N
                   (LITECORD_VAAPI_CAPTURE_TEST_FAIL): break the GPU path at its start or from
                   access unit N on (hang: its streaming thread blocks for 20 s, like a driver
                   that hangs); the share must go on
                   nomain | mainrefuse | mainprofile | mainbreak | mainlate:N: H.264 Main goes
                   wrong (the driver has no such profile, the encoder refuses the format, it
                   produces another profile, it fails on its first picture, its first picture
                   is withheld N times): the share must stay on the GPU capture in Constrained
                   Baseline (mainlate:1: go to Main)
                   prepstuck:N: making a new encoder blocks from the Nth replacement on (the
                   driver hangs while the VA context is created): the send loop must not wait
                   for it (0: Main is refused and the share stays as it is; 1 with a script
                   that goes to Main and back: the share goes on over gst-launch)
                   nobars: the bars around a picture of another shape cannot be painted: no
                   such picture may be sent, and the share goes on over gst-launch
                   floorrefuse | floormute | floorbreak (with --live): the encoder made for a
                   new quality floor refuses the pictures' format, puts out nothing, or fails
                   on its first picture: the share must stay on the GPU capture with the floor
                   it had, and the controls show that floor
  --freeze-source S  synthetic: the provider is stopped (SIGSTOP) before the capture connects
                   and continued after S seconds: the capture's start must stay bounded
  --end-source S   end the source S seconds into the share (Mutter: Session.Stop, as GNOME's
                   indicator does; synthetic: the provider is killed): the capture must report
                   the source gone and no other source may be taken
  --as-window      tell the capture the source is a shared window (LITECORD_VAAPI_CAPTURE_WINDOWS=0
                   then keeps it on the gst-launch capture)
  --window-pipe P  with --window-script: the FIFO a test window reads its commands from
  --window-script S  SEC:COMMAND;SEC:COMMAND;...: lines written to --window-pipe at those seconds
                   of the share (size WxH, drag ..., maximize, minimize, close, ...)
  --au-log FILE    one line per access unit: seconds into the share, its number, bytes, I or P,
                   the path, and the GPU capture's picture rectangle at that moment
  --source-must-end  the share is expected to end with its source gone (a closed window)
  --cycles N       stop the share and start it again N times in all (default 1). Not with the
                   synthetic source: its provider stops delivering to anybody once a consumer
                   left mid-stream (plain gst-launch consumers do the same to it)
  --cycle-gap MS   pause between stop and start (default 200)
  --colours        synthetic colour bars: check decoded colours against BT.601 limited
  --geometry       check where the picture lies in every decoded keyframe and in the previews:
                   the source's shape kept, centred, black bars around it, the same place on
                   the GPU capture and on the gst-launch path. The synthetic source then shows
                   colour bars (pattern=smpte100) and the picture's rectangle is measured from
                   the decoded pixels; with a compositor source only the bars are checked
  --png DIR        with --geometry: write the first and the last checked keyframe of each path
                   and one preview there as PNG
  --no-verify      do not decode (CPU measurements)
  --dump-aus FILE  write every access unit sent to FILE, one after the other (an H.264 byte
                   stream for other tools), and one line per keyframe to FILE.idr: its number
                   in the stream, the path, and the word new-encoder for the first of a new encoder
  --verbose        one line per access unit
Exit status 1 when a check fails, 2 when the source cannot be opened.";

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    struct Args(HashMap<String, String>);

    impl Args {
        fn parse() -> Result<Self, String> {
            let flags = ["fd", "legacy", "colours", "no-verify", "verbose", "help", "as-window", "geometry", "source-must-end"];
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
        /// `area`: (x, y, width, height) of the desktop to record instead of a monitor.
        fn open(connector: &str, area: Option<(i32, i32, i32, i32)>, window: Option<Option<u64>>) -> Result<(Arc<Self>, u32), String> {
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
                let reply = match (window, area) {
                    (Some(id), _) => {
                        // No id: Mutter takes the focused window.
                        if let Some(id) = id {
                            props.insert("window-id", zbus::zvariant::Value::U64(id));
                        }
                        conn.call_method(Some(MUTTER_BUS), mutter.session.as_str(), Some(session_iface), "RecordWindow", &(props,)).await.map_err(e)?
                    }
                    (None, Some((x, y, w, h))) => conn.call_method(Some(MUTTER_BUS), mutter.session.as_str(), Some(session_iface), "RecordArea", &(x, y, w, h, props)).await.map_err(e)?,
                    (None, None) => conn.call_method(Some(MUTTER_BUS), mutter.session.as_str(), Some(session_iface), "RecordMonitor", &(connector, props)).await.map_err(e)?,
                };
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
        /// New window captures started after a change of format or a silence.
        window_restarts: AtomicU64,
    }

    /// linux_portal.rs's gst-launch pipeline and reader loop, on `node`.
    fn legacy_capture(shared: &LegacyShared, fd: Option<RawFd>, node: u32, size: (u32, u32), fps: u32, monitor: Option<(u32, u32)>, window: bool, running: &AtomicBool) {
        let (dst_w, dst_h) = size;
        let frame_size = (dst_w * dst_h * 4) as usize;
        // (pipewiresrc inside this process closes the connection it is given: a copy.)
        let window_fd = fd.filter(|_| window).map(|fd| unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) });
        let mut source: Vec<String> = window_fd.or(fd).map(|fd| format!("fd={fd}")).into_iter().collect();
        // Not in the app's arguments: on the session's own connection (the portal's shows
        // only its node) a node that is gone must not be replaced by another video source.
        source.extend(["stream-properties=props,node.dont-fallback=(string)true".to_string(), format!("path={node}")]);
        // What linux_portal.rs does for a shared window: the pipeline inside this process.
        let mut window_capture = if window {
            match WindowCapture::start(&source, dst_w, dst_h, fps as u64) {
                Ok(capture) => {
                    println!("  the window is captured inside this process (videocrop by each buffer's rectangle), no gst-launch");
                    Some(capture)
                }
                Err(e) => return eprintln!("legacy: the in-process window capture did not start: {e}"),
            }
        } else {
            None
        };
        let mut child = None;
        let mut stdout = None;
        if window_capture.is_none() {
            let gst_args = gst_launch_capture_args(&source, dst_w, dst_h, fps as u64, monitor);
            let spawned = dies_with_us(Command::new("gst-launch-1.0").env_remove("PIPEWIRE_NODE").args(&gst_args).stdout(Stdio::piped()).stderr(Stdio::inherit())).spawn();
            let mut spawned = match spawned {
                Ok(c) => c,
                Err(e) => return eprintln!("legacy: gst-launch-1.0 did not start: {e}"),
            };
            shared.child_pid.store(spawned.id(), Ordering::SeqCst);
            stdout = spawned.stdout.take();
            child = Some(spawned);
            if stdout.is_none() {
                return;
            }
        }
        // What linux_portal.rs does with the window capture: a new one when the source
        // changed its format or sent nothing for a while (here on a new duplicate of the
        // connection, or the default remote).
        let mut silence = Duration::from_secs(3);
        let (mut restarted_silent, mut restarted_for_format) = (false, false);
        let mut next_frame = |frame: &mut [u8]| -> bool {
            loop {
                let read = window_capture.as_ref().map(|capture| capture.read(frame, &|| running.load(Ordering::Relaxed), silence));
                match read {
                    None => return stdout.as_mut().is_some_and(|s| s.read_exact(frame).is_ok()),
                    Some(WindowRead::Frame) => {
                        // (linux_portal.rs: the first picture after a restart keeps the wait.)
                        if !restarted_silent {
                            silence = Duration::from_secs(3);
                        }
                        restarted_silent = false;
                        return true;
                    }
                    Some(WindowRead::Ended) => return false,
                    // (linux_portal.rs: only after a change of format does silence mean gone.)
                    Some(WindowRead::Silent(_)) if restarted_silent && restarted_for_format => {
                        println!("  window capture: started again and silent since: the source is gone");
                        return false;
                    }
                    Some(why) => {
                        println!("  RESTART: window capture: {why:?}; a new capture is started");
                        shared.window_restarts.fetch_add(1, Ordering::SeqCst);
                        if matches!(why, WindowRead::Silent(_)) {
                            silence = (silence * 2).min(Duration::from_secs(60));
                        }
                        window_capture = None;
                        let mut source = source.clone();
                        if let Some(fd) = fd {
                            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                            source.retain(|arg| !arg.starts_with("fd="));
                            source.insert(0, format!("fd={dup}"));
                        }
                        restarted_silent = true;
                        restarted_for_format = why == WindowRead::Changed;
                        match WindowCapture::start(&source, dst_w, dst_h, fps as u64) {
                            Ok(capture) => window_capture = Some(capture),
                            Err(e) => {
                                eprintln!("legacy: the new window capture did not start: {e}");
                                return false;
                            }
                        }
                    }
                }
            }
        };
        let mut raw_buf = vec![0u8; frame_size];
        let mut last_preview = Instant::now() - Duration::from_secs(1);
        while running.load(Ordering::Relaxed) && next_frame(&mut raw_buf) {
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
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
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

    /// `--geometry`: what the checks need to know, and where pictures are written.
    #[derive(Clone)]
    struct Geometry {
        /// The source's picture size, once known (a compositor's is learnt from the capture).
        source: Arc<Mutex<Option<(u32, u32)>>>,
        /// The gst-launch capture is told the source's size (a monitor's): a fit one pixel
        /// short fills the frame there too.
        told: bool,
        /// The source shows colour bars: the picture's rectangle can be measured.
        measure: bool,
        png: Option<std::path::PathBuf>,
        name: String,
    }

    /// One picture's geometry: the rectangle measured (rows and columns with bright pixels in
    /// them), and the brightest channel value found in the bars, with how many bar pixels are
    /// brighter than black may be after encoding.
    struct Measured {
        rect: Option<(u32, u32, u32, u32)>,
        bar_max: u8,
        bar_bad: usize,
    }

    /// Bars are black: nothing above this in any channel, 4 pixels away from the picture
    /// (the encoder rings that far; the green of an unwritten surface is 73).
    const BAR_MAX: u8 = 40;

    fn measure(rgba: &[u8], (w, h): (u32, u32), expected: (u32, u32, u32, u32), find_rect: bool) -> Measured {
        let (w, h) = (w as usize, h as usize);
        let px = |x: usize, y: usize| {
            let o = (y * w + x) * 4;
            rgba[o].max(rgba[o + 1]).max(rgba[o + 2])
        };
        let rect = find_rect.then(|| {
            let (mut rows, mut cols) = (vec![0usize; h], vec![0usize; w]);
            for y in 0..h {
                for x in 0..w {
                    if px(x, y) > 96 {
                        rows[y] += 1;
                        cols[x] += 1;
                    }
                }
            }
            // The colour bars reach every edge of the picture: a row or column of it has
            // bright pixels along a good part of its length.
            let (x0, x1) = (cols.iter().position(|c| *c * 50 > h), cols.iter().rposition(|c| *c * 50 > h));
            let (y0, y1) = (rows.iter().position(|c| *c * 50 > w), rows.iter().rposition(|c| *c * 50 > w));
            match (x0, x1, y0, y1) {
                (Some(x0), Some(x1), Some(y0), Some(y1)) => Some((x0 as u32, y0 as u32, (x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32)),
                _ => None,
            }
        }).flatten();
        let (ex, ey, ew, eh) = (expected.0 as usize, expected.1 as usize, expected.2 as usize, expected.3 as usize);
        let (mut bar_max, mut bar_bad) = (0u8, 0usize);
        for y in 0..h {
            for x in 0..w {
                let in_bars = x + 4 < ex || x >= ex + ew + 4 || y + 4 < ey || y >= ey + eh + 4;
                if in_bars {
                    let v = px(x, y);
                    bar_max = bar_max.max(v);
                    bar_bad += (v > BAR_MAX) as usize;
                }
            }
        }
        Measured { rect, bar_max, bar_bad }
    }

    /// What is wrong with a measured picture, if anything. `source` -> `frame`.
    fn geometry_faults(m: &Measured, source: (u32, u32), frame: (u32, u32), expected: (u32, u32, u32, u32)) -> Vec<String> {
        let mut faults = Vec::new();
        if m.bar_bad > 0 {
            faults.push(format!("{} bar pixels are not black (brightest channel value {})", m.bar_bad, m.bar_max));
        }
        // What keeping the shape means, without the implementation's own arithmetic.
        let scale = (frame.0 as f64 / source.0 as f64).min(frame.1 as f64 / source.1 as f64);
        let ideal = (source.0 as f64 * scale, source.1 as f64 * scale);
        if let Some((x, y, w, h)) = m.rect {
            let near = |a: u32, b: u32| a.abs_diff(b) <= 1;
            if !(near(x, expected.0) && near(y, expected.1) && near(w, expected.2) && near(h, expected.3)) {
                faults.push(format!("the picture is at {x},{y} {w}x{h}, not at {},{} {}x{} (letterbox())", expected.0, expected.1, expected.2, expected.3));
            }
            if (w as f64 - ideal.0).abs() > 2.0 || (h as f64 - ideal.1).abs() > 2.0 {
                faults.push(format!("the picture is {w}x{h}; the source's shape in this frame is {:.1}x{:.1}", ideal.0, ideal.1));
            }
            if (x as f64 - (frame.0 - w) as f64 / 2.0).abs() > 1.0 || (y as f64 - (frame.1 - h) as f64 / 2.0).abs() > 1.0 {
                faults.push(format!("the picture at {x},{y} {w}x{h} is not centred"));
            }
        }
        faults
    }

    /// One access unit on its way to the verifier.
    struct Unit {
        n: usize,
        au: Vec<u8>,
        /// From the GPU (capture or encoder): one slice, no filler, colours declared.
        strict: bool,
        /// Every viewer of this access unit decodes H.264 Main: the stream may be Main
        /// then, and must be Constrained Baseline otherwise.
        main_ok: bool,
        gpu_path: bool,
    }

    /// What the verifier thread found; it decodes every access unit like a receiver.
    #[derive(Default)]
    struct Verdict {
        decoded: usize,
        failures: Vec<String>,
        sps: String,
        colour_lines: Vec<String>,
        cpu: f64,
        /// Access units of a stream whose last SPS said Main.
        main_units: usize,
        /// --geometry: keyframes checked on each path (GPU capture, gst-launch), the
        /// rectangles measured on each, and the brightest bar pixel seen.
        geometry_checked: [usize; 2],
        geometry_rects: [Vec<(u32, u32, u32, u32)>; 2],
        geometry_bar_max: u8,
        geometry_lines: Vec<String>,
    }

    fn verifier(rx: std::sync::mpsc::Receiver<Unit>, size: (u32, u32), colours: bool, geometry: Option<Geometry>) -> Verdict {
        let mut v = Verdict::default();
        let mut stream_main = false;
        let mut decoders = StreamDecoders::default();
        let mut raw = if colours { openh264::decoder::Decoder::new().ok() } else { None };
        let mut colours_done = 0;
        let fail = |v: &mut Verdict, n: usize, what: String| {
            if v.failures.len() < 30 {
                v.failures.push(format!("access unit {n}: {what}"));
            }
        };
        while let Ok(Unit { n, au, strict, main_ok, gpu_path }) = rx.recv() {
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
                let constrained_baseline = nal[1] == 66 && nal[2] & 0x40 != 0;
                stream_main = nal[1] == 77;
                if !constrained_baseline && !stream_main {
                    fail(&mut v, n, format!("neither Constrained Baseline nor Main: profile_idc={} flags={:#04x}", nal[1], nal[2]));
                }
                if stream_main && sps.vui_parameters.as_ref().and_then(|v| v.bitstream_restrictions.as_ref()).map(|r| r.max_num_reorder_frames) != Some(0) {
                    fail(&mut v, n, "Main SPS without max_num_reorder_frames=0: decoders may hold pictures back".into());
                }
                if strict && colour != Some((false, Some((1, 1, 6)))) {
                    fail(&mut v, n, format!("SPS colours {colour:?}, expected limited range 1/1/6"));
                }
            }
            if stream_main {
                v.main_units += 1;
                if !main_ok {
                    fail(&mut v, n, "a Main access unit while a viewer that cannot decode Main is watching (or the profile setting is 'compatible')".into());
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
                    } else if let Some((g, source)) = geometry.as_ref().filter(|_| idr).and_then(|g| g.source.lock().unwrap().map(|s| (g, s))) {
                        let path = if gpu_path { 0 } else { 1 };
                        let expected = if gpu_path || g.told { gpu::picture_rect(source, size) } else { gpu::letterbox(source, size) };
                        let m = measure(buf.as_bytes(), size, expected, g.measure);
                        let faults = geometry_faults(&m, source, size, expected);
                        v.geometry_bar_max = v.geometry_bar_max.max(m.bar_max);
                        if let Some(rect) = m.rect.filter(|r| !v.geometry_rects[path].contains(r)) {
                            v.geometry_rects[path].push(rect);
                        }
                        let first = v.geometry_checked[path] == 0;
                        v.geometry_checked[path] += 1;
                        let path_name = if gpu_path { "gpu" } else { "gstlaunch" };
                        if first || !faults.is_empty() {
                            let measured = m.rect.map(|(x, y, w, h)| format!("measured at {x},{y} {w}x{h}")).unwrap_or_else(|| "not measured".into());
                            v.geometry_lines.push(format!(
                                "  GEOMETRY {path_name} access unit {n}: source {}x{} in {}x{}: picture expected at {},{} {}x{}, {measured}; brightest bar pixel {}{}",
                                source.0, source.1, size.0, size.1, expected.0, expected.1, expected.2, expected.3, m.bar_max,
                                if faults.is_empty() { "  ok".to_string() } else { format!("  WRONG: {}", faults.join("; ")) }
                            ));
                        }
                        for fault in faults {
                            fail(&mut v, n, format!("geometry ({path_name}): {fault}"));
                        }
                        if let Some(dir) = &g.png {
                            let file = dir.join(format!("{}_{path_name}_{}.png", g.name, if first { "first" } else { "last" }));
                            if let Err(e) = image::save_buffer(&file, buf.as_bytes(), size.0, size.1, image::ExtendedColorType::Rgba8) {
                                fail(&mut v, n, format!("{}: {e}", file.display()));
                            }
                        }
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
        /// What the portal tells the gst-launch capture: the size of the source when it is
        /// known and not a window's.
        monitor: Option<(u32, u32)>,
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
        /// Who watches from which second on (m, c, w as in --viewers), and how long the
        /// share lasts.
        viewers: Vec<(f64, Vec<char>)>,
        total_secs: f64,
        profile: StreamProfile,
        /// A --fail mode about Main: whether Main must turn out unavailable (or be reached
        /// all the same).
        main_must_fail: Option<bool>,
        main_late: bool,
        /// (second, bps).
        bitrates: Vec<(f64, u32)>,
        /// --live: (second, the floor rather than the bitrate, value, released), and how many
        /// times the floor comes to rest in it (released values and drags).
        live: Option<Vec<(f64, bool, u32, bool)>>,
        live_floor_rests: usize,
        /// --fail floorrefuse | floormute: no new floor may take effect.
        floor_must_fail: bool,
        pli: Vec<f64>,
        verify: bool,
        colours: bool,
        geometry: Option<Geometry>,
        verbose: bool,
        dump_aus: Option<String>,
        au_log: Option<String>,
        /// (second, line) written to the test window's command FIFO.
        window_script: Vec<(f64, String)>,
        window_pipe: Option<String>,
        source_must_end: bool,
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

    /// --live: what the slice QPs seen with one quality floor prove. `before`: the floor in
    /// effect before it (0: none); `aus`: access units a VA-API encoder sent with it.
    fn live_floor_proof(fails: &mut Failures, floor: u32, before: u32, qps: &[i32], aus: usize) {
        if floor == 0 || aus < 30 {
            return;
        }
        let Some(lo) = qps.iter().min().copied() else {
            fails.add(format!("no slice QP could be read from the {aus} access units sent with quality floor QP {floor}: nothing in the bitstream proves that floor"));
            return;
        };
        if before > floor && lo >= before as i32 {
            fails.add(format!(
                "the quality floor went down from QP {before} to QP {floor}, and the lowest slice QP of the {} pictures after it is {lo}: the encoder still has the floor before it (or this source does not drive the QP to the floor: the default synthetic pattern does)",
                qps.len()
            ));
        }
    }

    fn run_share(setup: &Setup, cycle: usize, fails: &mut Failures) -> Totals {
        // --live: this share's values start from the settings, as in the app; the capture's
        // encoder is made with them.
        let live_start = setup.live.as_ref().map(|_| live_controls::begin_share());
        let live_start_qp = live_controls::status().min_qp;
        let running = Arc::new(AtomicBool::new(true));
        let legacy = Arc::new(LegacyShared { frame: Mutex::new(None), frames_read: AtomicU64::new(0), first_frame_at: Mutex::new(None), child_pid: AtomicU32::new(0), previews: AtomicU64::new(0), window_restarts: AtomicU64::new(0) });
        let preview_count = Arc::new(AtomicU64::new(0));
        let preview_bad = Arc::new(AtomicU64::new(0));
        let preview_geometry_bad = Arc::new(AtomicU64::new(0));
        let preview_geometry: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            // What ScreenCaptureManager::start does with the UI's callback.
            let (count, bad, size) = (preview_count.clone(), preview_bad.clone(), setup.size);
            let (geometry, geometry_bad, geometry_seen) = (setup.geometry.clone(), preview_geometry_bad.clone(), preview_geometry.clone());
            *PORTAL_LOCAL_CB.lock().unwrap() = Some(Arc::new(move |p: SharedPixelBuffer<Rgba8Pixel>| {
                let seen = count.fetch_add(1, Ordering::Relaxed);
                let opaque = p.as_bytes().chunks_exact(4).step_by(997).all(|px| px[3] == 255);
                if (p.width(), p.height()) != size || !opaque {
                    bad.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                // The preview shows the stream's frame: the same picture rectangle and bars.
                let Some((g, source)) = geometry.as_ref().and_then(|g| g.source.lock().unwrap().map(|s| (g, s))) else { return };
                // (The GPU capture fills the frame where the gst-launch path, not told the
                // source's size, leaves a bar one pixel thin.)
                let expected = if gpu::active() || g.told { gpu::picture_rect(source, size) } else { gpu::letterbox(source, size) };
                let m = measure(p.as_bytes(), size, expected, g.measure);
                let faults = geometry_faults(&m, source, size, expected);
                let mut lines = geometry_seen.lock().unwrap();
                if !faults.is_empty() {
                    geometry_bad.fetch_add(1, Ordering::Relaxed);
                }
                if lines.is_empty() || (!faults.is_empty() && lines.len() < 4) {
                    lines.push(format!("preview {seen}: {}; brightest bar pixel {}{}", m.rect.map(|(x, y, w, h)| format!("picture measured at {x},{y} {w}x{h}")).unwrap_or_else(|| "picture not measured".into()), m.bar_max, if faults.is_empty() { "  ok".to_string() } else { format!("  WRONG: {}", faults.join("; ")) }));
                }
                if let Some(dir) = g.png.as_ref().filter(|_| seen == 3) {
                    let _ = image::save_buffer(dir.join(format!("{}_preview.png", g.name)), p.as_bytes(), size.0, size.1, image::ExtendedColorType::Rgba8);
                }
            }));
        }

        // The portal thread of linux_portal.rs from the point where it has the fd and node.
        let gpu_gave_up_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let gpu_started = Arc::new(AtomicBool::new(false));
        let gpu_restarts = Arc::new(AtomicU64::new(0));
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
            let gpu_restarts = gpu_restarts.clone();
            let (node, size, fps, legacy_only, memory, max_fps, as_window, monitor) = (setup.node, setup.size, setup.fps, setup.legacy_only, setup.memory, setup.max_fps, setup.as_window, setup.monitor);
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
                        gpu::begin_with(&gpu::Config { fd: raw(&fd), node, width: size.0, height: size.1, fps, memory, max_fps, min_qp: 30 }, &wanted).map_err(|e| println!("  GPU capture did not start: {e}")).ok()
                    };
                    println!("  begin() returned after {} ms: {}", began.elapsed().as_millis(), if begun.is_some() { "started" } else { "not started" });
                    let mut begun = begun;
                    // What linux_portal.rs does when the source renegotiates (a window resized on
                    // Hyprland): a new GPU capture on a new connection.
                    let ended = loop {
                        let ended = match begun {
                            Some(id) => {
                                gpu_started.store(true, Ordering::SeqCst);
                                let ended = gpu::run_until_end(id, wanted);
                                *gpu_ended.lock().unwrap() = Some(ended);
                                ended
                            }
                            None => gpu::Ended::Failed,
                        };
                        if ended != gpu::Ended::SourceChanged || !wanted() {
                            break ended;
                        }
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while gpu::live_pipelines() > 0 && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        println!("  RESTART: the old GPU capture was taken down ({} pipelines left)", gpu::live_pipelines());
                        if let Some(old) = fd.take() {
                            match open_remote() {
                                Ok(new) => {
                                    unsafe { libc::shutdown(old.as_raw_fd(), libc::SHUT_RDWR) };
                                    fd = Some(new);
                                }
                                Err(_) => fd = Some(old),
                            }
                        }
                        let again = Instant::now();
                        begun = gpu::begin(raw(&fd), node, size.0, size.1, fps, &wanted);
                        // (linux_portal.rs: up to 3 tries, 250 ms apart.)
                        for _ in 1..3 {
                            if begun.is_some() || !wanted() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(250));
                            begun = gpu::begin(raw(&fd), node, size.0, size.1, fps, &wanted);
                        }
                        println!("  RESTART: the source changed its format; a new GPU capture {} after {} ms", if begun.is_some() { "started" } else { "did not start" }, again.elapsed().as_millis());
                        gpu_restarts.fetch_add(1, Ordering::SeqCst);
                    };
                    let ended = if ended == gpu::Ended::SourceChanged { gpu::Ended::Stopped } else { ended };
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
                    legacy_capture(&legacy, raw(&fd), node, size, fps, monitor, as_window, &running);
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
        let (tx_verify, rx_verify) = std::sync::mpsc::sync_channel::<Unit>(600);
        let verify_thread = {
            let (size, colours, geometry) = (setup.size, setup.colours, setup.geometry.clone());
            std::thread::Builder::new().name("verifier".into()).spawn(move || verifier(rx_verify, size, colours, geometry)).expect("verifier thread")
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

        // Who watches, by the send loop's own rules (watch.rs).
        let mut viewers = Viewers::new();
        let mut choice = ProfileChoice::new(setup.profile);
        let mut tokens: Vec<char> = Vec::new();
        let mut viewer_step: Option<usize> = None;
        let mut last_renewal = Instant::now();
        // The profile of the stream by its last SPS, and what its changes cost.
        let mut stream_main: Option<bool> = None;
        let mut profile_changes: Vec<String> = Vec::new();
        let mut switch_gaps: Vec<f64> = Vec::new();
        let mut idr_asked_efficient = false;
        // Keyframes of the GPU capture: the bytes of the first one of each new encoder and of
        // all the others (a new encoder's rate control starts anew: --dump-aus and a
        // decoder's QP output tell how coarse), and those nobody asked for.
        let mut new_encoder_idrs: Vec<usize> = Vec::new();
        let mut other_idrs: Vec<usize> = Vec::new();
        let mut unasked_idrs: Vec<String> = Vec::new();
        let mut last_idr_request: Option<Instant> = None;
        let mut starved = false;
        let mut dump = setup.dump_aus.as_ref().and_then(|path| Some((std::fs::File::create(path).ok()?, std::fs::File::create(format!("{path}.idr")).ok()?)));

        // --live: the send loop's rate controller, the script, and what was seen of it.
        let mut live_rate = RateControl::new(live_start.unwrap_or(8_000_000));
        let mut live_script: std::collections::VecDeque<(f64, bool, u32, bool)> = setup.live.clone().unwrap_or_default().into();
        let mut live_target = live_start.unwrap_or(0);
        let mut live_floor = 0u32;
        let mut live_floor_changes = 0usize;
        let mut live_switches = 0u64;
        let mut live_qps: Vec<i32> = Vec::new();
        let mut live_qp_parser = litecord::screen_capture::hw_decode::h264_state::H264State::new();
        let mut live_low_qp_reported = false;
        // Released changes still to be found in effect: (asked at, deadline, floor, value,
        // the floor in effect before it).
        let mut live_expected: Vec<(Instant, Instant, bool, u32, u32)> = Vec::new();
        let mut live_last_ask: Option<Instant> = None;
        // For the exit status: the floor before the one in effect and the access units sent
        // with the one in effect; the floor the engine was asked for last and how many
        // changes it was asked for; the moves of the target still to be measured (when, from,
        // to, kbit/s in the 1.5 s before) and the access units of the last seconds.
        let mut live_floor_before = 0u32;
        let mut live_floor_aus = 0usize;
        let mut live_asked_qp = 0u32;
        let mut live_floors_asked = 0usize;
        let mut live_asked_counts = false;
        let mut live_rate_steps: Vec<(Instant, u32, u32, f64)> = Vec::new();
        let mut live_sent: std::collections::VecDeque<(Instant, usize)> = std::collections::VecDeque::new();
        let mut live_unwatched_at: Option<Instant> = None;

        let share_start = Instant::now();
        let total_secs = setup.total_secs;
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
        let mut window_script: std::collections::VecDeque<(f64, String)> = setup.window_script.iter().cloned().collect();
        let mut au_log = setup.au_log.as_ref().and_then(|path| std::fs::File::create(path).ok());
        let mut rect_seen: Option<(u32, u32, u32, u32)> = None;

        while !STOP.load(Ordering::Relaxed) {
            let t = share_start.elapsed().as_secs_f64();
            if t >= total_secs {
                break;
            }
            while window_script.front().is_some_and(|(at, _)| t >= *at) {
                if let Some((_, line)) = window_script.pop_front() {
                    use std::io::Write;
                    let sent = setup.window_pipe.as_ref().ok_or("no --window-pipe".to_string()).and_then(|p| std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(p).map_err(|e| e.to_string())).and_then(|mut f| writeln!(f, "{line}").map_err(|e| e.to_string()));
                    println!("  t={t:.3}s WINDOW: {line}{}", sent.err().map(|e| format!(" (not sent: {e})")).unwrap_or_default());
                }
            }
            if let Some(st) = gpu::stats().filter(|st| rect_seen != Some(st.picture_rect)) {
                if rect_seen.is_some() || setup.as_window {
                    println!("  t={t:.3}s the picture is now at {:?} of the frame (bars painted on {} surfaces so far)", st.picture_rect, st.bars_painted);
                }
                rect_seen = Some(st.picture_rect);
            }
            if end_source.as_ref().is_some_and(|(at, _)| t >= *at) {
                if let Some((_, end)) = end_source.take() {
                    println!("  t={t:.2}s the source is ended");
                    end();
                }
            }
            // Watch requests: a change in the list, and every viewer's renewal once a second.
            let now = Instant::now();
            let step = setup.viewers.iter().rposition(|(at, _)| *at <= t);
            let mut new_viewer = false;
            if step != viewer_step {
                viewer_step = step;
                let list = step.map(|i| setup.viewers[i].1.clone()).unwrap_or_default();
                // A browser that connects asks for a keyframe (web_share).
                new_viewer |= list.iter().filter(|t| **t == 'w').count() > tokens.iter().filter(|t| **t == 'w').count();
                for i in 0..tokens.len() {
                    if list.get(i).is_none_or(|new| *new == 'w') {
                        viewers.unwatch(i as u64 + 1);
                    }
                }
                tokens = list;
                last_renewal = now - Duration::from_secs(2);
                println!("  t={t:.2}s viewers are now {}", if tokens.is_empty() { "-".to_string() } else { tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>().join("+") });
            }
            if last_renewal.elapsed() >= Duration::from_secs(1) {
                last_renewal = now;
                for (i, t) in tokens.iter().enumerate().filter(|(_, t)| **t != 'w') {
                    let uid = i as u64 + 1;
                    // What watch::on_watch does with a request.
                    if let WatchVerdict::Accepted { new } = viewers.watch(uid, [uid as u8; 32], std::net::SocketAddr::from(([10, 0, 0, uid as u8], 50005)), true, now) {
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
            let main_ok = setup.profile == StreamProfile::Efficient || (setup.profile == StreamProfile::Auto && all_decode_main);
            if new_viewer {
                keyframe_requested = true;
            }
            // Viewers drop a stream after 2.5 s without pictures, whichever path they come from
            // (--end-source takes the pictures away on purpose).
            let fed_at = last_au_at.max(watch_started);
            if watched && was_watched && !starved && setup.end_source.is_none() && !setup.source_must_end && fed_at.is_some_and(|at| at.elapsed() > Duration::from_millis(2500)) {
                starved = true;
                fails.add(format!("t={t:.2}s: the viewers have been without pictures for 2.5 s"));
            }
            let mut bitrate = setup.bitrates.iter().rev().find(|(s, _)| *s <= t).map(|b| b.1).unwrap_or(8_000_000);
            let mut live_min_qp = None;
            if setup.live.is_some() {
                while live_script.front().is_some_and(|step| t >= step.0) {
                    let Some((_, floor, value, released)) = live_script.pop_front() else { break };
                    if floor {
                        live_controls::set_min_qp(value, released);
                    } else {
                        live_controls::set_max_bitrate_mbps(value, released);
                    }
                    live_last_ask = Some(Instant::now());
                    if released {
                        println!("  t={t:.3}s LIVE: asked for {} {value}{}", if floor { "quality floor QP" } else { "maximum bitrate" }, if floor { "" } else { " Mbit/s" });
                        // A value asked for before this one and not in effect yet is superseded:
                        // the engine replaces one encoder at a time and may go straight to the
                        // newest value, so only that one has to take effect.
                        live_expected.retain(|expected| expected.2 != floor);
                        live_expected.push((Instant::now(), Instant::now() + Duration::from_millis(1500), floor, value, live_controls::status().encoder_min_qp));
                    } else if setup.verbose {
                        println!("  t={t:.3}s LIVE: the floor slider is at QP {value} (held)");
                    }
                }
                let cap = if web_viewers > 0 { 8_000_000 } else { u32::MAX };
                let qp = live_controls::turn(&mut live_rate, cap, efficient, now);
                live_controls::set_watched(watched);
                if qp != live_asked_qp {
                    // What has to take effect: not a value that was replaced by another before
                    // any encoder showed it (one replacement at a time: the engine may skip
                    // it), and not a return to the floor that is in effect.
                    let in_effect = live_controls::status().encoder_min_qp;
                    if live_asked_counts && in_effect != live_asked_qp {
                        live_floors_asked = live_floors_asked.saturating_sub(1);
                    }
                    live_asked_counts = live_asked_qp != 0 && qp != in_effect;
                    live_floors_asked += usize::from(live_asked_counts);
                    live_asked_qp = qp;
                }
                bitrate = live_rate.current().min(cap);
                // (`turn` asks for a keyframe when the target moved: vah264enc takes the rate with it.)
                if litecord::screen_capture::types::KEYFRAME_REQUESTED.swap(false, Ordering::Relaxed) {
                    keyframe_requested = true;
                }
                if bitrate != live_target {
                    println!(
                        "  t={t:.3}s LIVE: target bitrate {:.1} -> {:.1} Mbit/s (the share's maximum is {} Mbit/s{}){}",
                        live_target as f64 / 1e6,
                        bitrate as f64 / 1e6,
                        live_rate.max() / 1_000_000,
                        if live_rate.current() > cap { ", a browser viewer holds it at 8" } else { "" },
                        live_last_ask.map(|at| format!(", {:.0} ms after the last change was asked", at.elapsed().as_secs_f64() * 1000.0)).unwrap_or_default()
                    );
                    if watched {
                        let from = now.checked_sub(Duration::from_millis(1500)).unwrap_or(now);
                        let before = live_sent.iter().filter(|(at, _)| *at >= from).map(|sent| sent.1).sum::<usize>() as f64 * 8.0 / 1.5 / 1000.0;
                        live_rate_steps.push((now, live_target, bitrate, before));
                    }
                    live_target = bitrate;
                }
                // The rate the encoder really makes, 0.5 to 2.5 s after the target moved.
                if !watched {
                    live_unwatched_at = Some(now);
                }
                while let Some(&(at, from, to, before)) = live_rate_steps.first() {
                    let end = at + Duration::from_millis(2500);
                    let disturbed = live_rate_steps.get(1).is_some_and(|next| next.0 < end) || live_unwatched_at.is_some_and(|gone| gone >= at);
                    if now < end && !disturbed {
                        break;
                    }
                    live_rate_steps.remove(0);
                    let (from_k, to_k) = (from as f64 / 1000.0, to as f64 / 1000.0);
                    if disturbed {
                        println!("  t={t:.3}s LIVE: the rate after the target went {from_k:.0} -> {to_k:.0} kbit/s is not measured (another change, or no viewer, within 2.5 s of it)");
                        continue;
                    }
                    let begin = at + Duration::from_millis(500);
                    let measured = live_sent.iter().filter(|(when, _)| *when >= begin && *when < end).map(|sent| sent.1).sum::<usize>() as f64 * 8.0 / 2.0 / 1000.0;
                    println!("  t={t:.3}s LIVE: measured {measured:.0} kbit/s from 0.5 to 2.5 s after the target went {from_k:.0} -> {to_k:.0} kbit/s ({before:.0} kbit/s in the 1.5 s before it)");
                    // (OpenH264 after a failure has its own ceiling.)
                    if !(gpu::active() || h264_encoder.as_ref().is_some_and(|e| e.is_hardware_accelerated())) {
                        continue;
                    }
                    if to < from && measured > to_k * 1.3 {
                        fails.add(format!("t={t:.2}s: the target went down to {to_k:.0} kbit/s and the stream still measured {measured:.0} kbit/s 0.5 to 2.5 s later: the encoder did not take the new rate in time"));
                    }
                    // Up: only a source that was using its rate can show it.
                    if to > from && before >= from_k * 0.7 {
                        if measured < from_k * 1.15 {
                            fails.add(format!("t={t:.2}s: the target went up from {from_k:.0} to {to_k:.0} kbit/s and the stream measured {measured:.0} kbit/s 0.5 to 2.5 s later: the encoder did not take the new rate in time"));
                        } else if measured < from_k + (to_k - from_k) * 0.5 {
                            println!("  t={t:.3}s LIVE: NOTE less than half way to the new target: see whether the quality floor is what holds the rate (slice QPs at the floor)");
                        }
                    } else if to > from {
                        println!("  t={t:.3}s LIVE: (the source was not using its rate before the raise: nothing to prove; --src-props pattern=snow does)");
                    }
                }
                gpu::set_min_qp(qp);
                live_min_qp = Some(qp);
                // Released changes whose time is up (the time only runs while somebody watches).
                let st = live_controls::status();
                let mut i = 0;
                while i < live_expected.len() {
                    let (asked, deadline, floor, value, floor_before) = live_expected[i];
                    if !watched && floor {
                        live_expected[i].1 = Instant::now() + Duration::from_millis(1500);
                    }
                    let met = if !floor {
                        bitrate == (value * 1_000_000).min(cap)
                    } else if setup.floor_must_fail {
                        false
                    } else {
                        st.encoder_min_qp == value
                    };
                    if met {
                        live_expected.remove(i);
                        continue;
                    }
                    if now >= deadline && (watched || !floor) {
                        if floor && setup.floor_must_fail {
                            // (--legacy: the memory-fed encoder, still on VA-API and not on OpenH264.)
                            let (in_service, path) = if setup.legacy_only { (h264_encoder.as_ref().is_some_and(|e| e.is_hardware_accelerated()), "memory-fed VA-API encoder") } else { (gpu::active(), "GPU capture") };
                            let stayed = st.encoder_min_qp == floor_before && st.min_qp == floor_before && in_service;
                            println!(
                                "  t={t:.3}s LIVE: the encoder for QP {value} was made to fail: the floor in effect is QP {}, the controls show QP {}, {path} {}",
                                st.encoder_min_qp,
                                st.min_qp,
                                if in_service { "still in service" } else { "given up" }
                            );
                            if !stayed {
                                fails.add(format!("t={t:.2}s: after the encoder for floor QP {value} failed, the share is not back on QP {floor_before} on the {path}"));
                            }
                        } else if floor {
                            fails.add(format!("t={t:.2}s: quality floor QP {value} was asked {:.0} ms ago and no encoder has shown a keyframe with it (in effect: QP {})", asked.elapsed().as_secs_f64() * 1000.0, st.encoder_min_qp));
                        } else {
                            fails.add(format!("t={t:.2}s: maximum bitrate {value} Mbit/s was asked {:.0} ms ago and the target is {:.1} Mbit/s", asked.elapsed().as_secs_f64() * 1000.0, bitrate as f64 / 1e6));
                        }
                        live_expected.remove(i);
                        continue;
                    }
                    i += 1;
                }
            }
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
                    if let Some(g) = &setup.geometry {
                        let mut source = g.source.lock().unwrap();
                        if source.is_some_and(|s| s != st.source_size) {
                            fails.add(format!("the source delivers {}x{}, not the {:?} asked for", st.source_size.0, st.source_size.1, *source));
                        }
                        *source = Some(st.source_size);
                    }
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
                    last_idr_request = Some(Instant::now());
                    idr_asked_efficient = efficient;
                }
                gpu::next_picture(bitrate, want_idr, efficient).map(|p| {
                    repeat = p.repeat;
                    latency = Some(p.latency.as_secs_f64() * 1000.0);
                    p.data
                })
            } else if let Some((frame, seq)) = latest.as_ref().filter(|_| encode_now) {
                let enc = h264_encoder.get_or_insert_with(|| create_best_encoder(setup.fps, true));
                if enc.get_bitrate_bps() != bitrate {
                    enc.set_bitrate_bps(bitrate);
                }
                enc.set_efficient_profile(efficient);
                if let Some(qp) = live_min_qp {
                    enc.set_quality_floor(qp);
                }
                if std::mem::take(&mut keyframe_requested) || last_idr.elapsed() >= Duration::from_millis(2000) {
                    last_idr = Instant::now();
                    enc.force_intra_frame();
                    idr_pending.get_or_insert((Instant::now(), "request"));
                    idr_asked_efficient = efficient;
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
                // The stream's profile, by its SPS.
                let sps_main = nal_ranges(&au).iter().map(|r| &au[r.clone()]).find(|nal| nal[0] & 0x1f == 7 && nal.len() > 3).map(|nal| nal[1] == 77);
                let (mut profile_changed, viewer_first) = (false, first_after_watch);
                if let Some(main) = sps_main.filter(|main| stream_main != Some(*main)) {
                    let name = if main { "Main" } else { "Constrained Baseline" };
                    if stream_main.is_some() {
                        profile_changed = true;
                        let replaced = gpu::stats().filter(|_| gpu_path).map(|st| format!(", encoder replaced in {:.1} ms (switch {})", st.last_switch_us as f64 / 1000.0, st.profile_switches)).unwrap_or_default();
                        match last_au_at.filter(|_| !first_after_watch) {
                            Some(prev) => {
                                let gap = now.duration_since(prev).as_secs_f64() * 1000.0;
                                println!("  t={t:.2}s PROFILE: the stream is now {name} ({path_now} path), {gap:.1} ms after the last access unit of the other profile{replaced}; {} bytes, NAL types {types:?}", au.len());
                                switch_gaps.push(gap);
                                if gap > 500.0 {
                                    fails.add(format!("t={t:.2}s: {gap:.0} ms without pictures at the change to {name}"));
                                }
                            }
                            None => println!("  t={t:.2}s PROFILE: the stream is now {name} ({path_now} path), with the first picture for the new viewers{replaced}"),
                        }
                        profile_changes.push(format!("{t:.2}s -> {name}"));
                        if main && idr_pending.is_none() && !first_after_watch {
                            fails.add(format!("t={t:.2}s: the stream went to Main without a keyframe being asked for"));
                        }
                    }
                    stream_main = Some(main);
                }
                if idr && gpu_path && idr_pending.is_some() && idr_asked_efficient && efficient && sps_main == Some(false) && setup.main_must_fail.is_none() {
                    fails.add(format!("t={t:.2}s: a keyframe was asked for while every viewer decodes Main, and it is not Main"));
                }
                // --live: the access unit before this one, and whether this is the first one of
                // an encoder the stream controls had replaced (for a new quality floor, or to
                // put the floor before it back): its keyframe comes unasked, like a profile's.
                let live_prev_au = last_au_at.filter(|_| !first_after_watch);
                let live_replaced = setup.live.is_some() && gpu_path && gpu::stats().is_some_and(|g| std::mem::replace(&mut live_switches, g.profile_switches) != g.profile_switches);
                if let Some(prev) = last_au_at {
                    all_gap.push(now.duration_since(prev).as_secs_f64() * 1000.0);
                }
                last_au_at = Some(now);
                starved = false;
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
                    if gpu_path {
                        if profile_changed || live_replaced { &mut new_encoder_idrs } else { &mut other_idrs }.push(au.len());
                        // (A change to Constrained Baseline brings its keyframe unasked.)
                        // (Two requests a few milliseconds apart may each get theirs.)
                        if idr_pending.is_none() && !profile_changed && !live_replaced && !viewer_first && last_idr_request.is_none_or(|at| at.elapsed() > Duration::from_millis(150)) {
                            unasked_idrs.push(format!("{t:.2}s"));
                        }
                    }
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
                if setup.live.is_some() {
                    // The picture's QP, from its first slice header; the floor of the encoder
                    // it came from (reported by that encoder with its first keyframe).
                    let slice_qp = live_qp_parser.parse(&au).ok().flatten().map(|p| 26 + p.pps.pic_init_qp_minus26 + p.header.slice_qp_delta);
                    let st = live_controls::status();
                    live_sent.push_back((now, au.len()));
                    while live_sent.front().is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(6)) {
                        live_sent.pop_front();
                    }
                    if st.encoder_min_qp != live_floor {
                        let seen = |qps: &[i32]| match (qps.iter().min(), qps.iter().max()) {
                            (Some(lo), Some(hi)) => format!("{lo}..{hi} over {} pictures", qps.len()),
                            _ => "none".to_string(),
                        };
                        if live_floor == 0 {
                            println!("  t={t:.3}s LIVE: the {} starts with quality floor QP {}", st.encoder.name(), st.encoder_min_qp);
                            if st.encoder_min_qp != 0 && st.encoder_min_qp != live_start_qp {
                                // Asked for while nobody watched: the first encoder to send a picture has it.
                                println!("  t={t:.3}s LIVE: (the share started at QP {live_start_qp}: a floor asked for before anybody watched took effect with the first picture)");
                                live_floor_changes += 1;
                                if !idr {
                                    fails.add(format!("t={t:.2}s: the first access unit with quality floor QP {} is not a keyframe (NAL types {types:?})", st.encoder_min_qp));
                                }
                            }
                        } else {
                            let gap = live_prev_au.map(|prev| now.duration_since(prev).as_secs_f64() * 1000.0);
                            let replaced = gpu::stats().filter(|_| gpu_path).map(|g| format!("; encoder replacement {} kept the send loop {:.1} ms", g.profile_switches, g.last_switch_us as f64 / 1000.0)).unwrap_or_default();
                            println!(
                                "  t={t:.3}s LIVE: quality floor QP {live_floor} -> {} in effect on the {} ({path_now} path): {} without pictures{}{replaced}; {} bytes, NAL types {types:?}, slice QP {slice_qp:?}; slice QPs with floor {live_floor}: {}",
                                st.encoder_min_qp,
                                st.encoder.name(),
                                gap.map(|g| format!("{g:.0} ms")).unwrap_or_else(|| "the first picture for these viewers,".into()),
                                live_last_ask.map(|at| format!(", {:.0} ms after the last change was asked", at.elapsed().as_secs_f64() * 1000.0)).unwrap_or_default(),
                                au.len(),
                                seen(&live_qps)
                            );
                            live_floor_changes += 1;
                            if !idr {
                                fails.add(format!("t={t:.2}s: the first access unit with quality floor QP {} is not a keyframe (NAL types {types:?})", st.encoder_min_qp));
                            }
                            if gap.is_some_and(|g| g > 1000.0) {
                                fails.add(format!("t={t:.2}s: the change of quality floor left the viewers {:.0} ms without pictures", gap.unwrap_or_default()));
                            }
                            if setup.floor_must_fail {
                                fails.add(format!("t={t:.2}s: a new quality floor took effect although its encoder was made to fail"));
                            }
                        }
                        live_floor_proof(fails, live_floor, live_floor_before, &live_qps, live_floor_aus);
                        live_floor_before = live_floor;
                        live_floor_aus = 0;
                        live_floor = st.encoder_min_qp;
                        live_qps.clear();
                    }
                    // (Only the VA-API encoders have the floor; OpenH264 after a failure does not.)
                    let strict = gpu_path || h264_encoder.as_ref().is_some_and(|e| e.is_hardware_accelerated());
                    live_floor_aus += usize::from(strict);
                    if let Some(qp) = slice_qp {
                        live_qps.push(qp);
                        if strict && live_floor != 0 && qp < live_floor as i32 && !std::mem::replace(&mut live_low_qp_reported, true) {
                            fails.add(format!("t={t:.2}s: access unit {au_index} has slice QP {qp}, below the quality floor in effect (QP {live_floor})"));
                        }
                    }
                }
                if setup.verbose {
                    println!("  t={t:.3}s {path_now} au {au_index}: {} bytes NAL {types:?}{}{}", au.len(), if repeat { " repeat" } else { "" }, latency.map(|l| format!(" {l:.1} ms")).unwrap_or_default());
                }
                s_aus += 1;
                s_bytes += au.len();
                totals.aus += 1;
                totals.bytes += au.len();
                if let Some(log) = au_log.as_mut() {
                    use std::io::Write;
                    let rect = gpu::stats().filter(|_| gpu_path).map(|st| st.picture_rect).unwrap_or_default();
                    let _ = writeln!(log, "{:.4} {au_index} {} {} {path_now} {} {} {} {}", share_start.elapsed().as_secs_f64(), au.len(), if idr { "I" } else { "P" }, rect.0, rect.1, rect.2, rect.3);
                }
                if let Some((stream, index)) = dump.as_mut() {
                    use std::io::Write;
                    let _ = stream.write_all(&au);
                    if idr {
                        let _ = writeln!(index, "{au_index} {path_now}{}", if (profile_changed || live_replaced) && gpu_path { " new-encoder" } else { "" });
                    }
                }
                let strict = gpu_path || h264_encoder.as_ref().is_some_and(|e| e.is_hardware_accelerated());
                if setup.verify && tx_verify.try_send(Unit { n: au_index, au, strict, main_ok, gpu_path }).is_err() {
                    verify_dropped += 1;
                }
                au_index += 1;
            }
            if let Some((asked, _)) = idr_pending {
                // (A Main encoder whose first picture is withheld twice keeps the stream
                // without pictures for twice 400 ms before it is replaced.)
                let patience = Duration::from_millis(if setup.main_late { 1000 } else { 600 });
                if asked.elapsed() > patience {
                    fails.add(format!("t={t:.2}s: a keyframe asked for {} ms ago has not arrived", patience.as_millis()));
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
                        "pictures {:3} (buffers {:3}) encoded new {:3} repeat {:2} over-rate {:3} | capture->AU ms p50 {:4.1} max {:4.1} | gate {}",
                        st.pictures.saturating_sub(last_stats.pictures),
                        st.buffers.saturating_sub(last_stats.buffers),
                        st.encoded_new.saturating_sub(last_stats.encoded_new),
                        st.encoded_repeats.saturating_sub(last_stats.encoded_repeats),
                        st.skipped_pace.saturating_sub(last_stats.skipped_pace),
                        percentile(&s_lat, 0.5),
                        s_lat.last().copied().unwrap_or(0.0),
                        // (The spacing of the pictures the gate passed, over its last 300.)
                        st.cadence.as_ref().map_or("-".to_string(), |c| format!(
                            "{:4.1}/s step p5 {:4.1} p50 {:4.1} p95 {:4.1} ms, short {} long {} of {}",
                            c.passed_per_s, c.p5_ms, c.p50_ms, c.p95_ms, c.short, c.long, c.steps
                        ))
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
        if setup.live.is_some() {
            let st = live_controls::status();
            println!(
                "stream controls: {} change(s) of quality floor took effect ({} asked for: released values and drags); at the end the share's values are {} Mbit/s and QP {}, in effect {:.1} Mbit/s and floor QP {} ({}); slice QPs with the last floor: {}; encoder replacements on the GPU capture: {}",
                live_floor_changes,
                setup.live_floor_rests,
                st.max_bitrate_mbps,
                st.min_qp,
                st.target_bps as f64 / 1e6,
                st.encoder_min_qp,
                st.encoder.name(),
                match (live_qps.iter().min(), live_qps.iter().max()) {
                    (Some(lo), Some(hi)) => format!("{lo}..{hi} over {} pictures", live_qps.len()),
                    _ => "none".to_string(),
                },
                last_gpu_stats.as_ref().map(|g| g.profile_switches.to_string()).unwrap_or_else(|| "-".into())
            );
            if live_floor_changes > setup.live_floor_rests {
                fails.add(format!("the quality floor changed {} times for {} values asked for (a dragged slider must replace the encoder once, when it rests)", live_floor_changes, setup.live_floor_rests));
            }
            live_floor_proof(fails, live_floor, live_floor_before, &live_qps, live_floor_aus);
            if !setup.floor_must_fail {
                println!("stream controls: the engine was asked for {live_floors_asked} change(s) of quality floor (values that were released or came to rest)");
                if live_floor_changes < live_floors_asked {
                    fails.add(format!("the engine was asked for {live_floors_asked} changes of quality floor and {live_floor_changes} took effect"));
                }
                // The script's last floor, when somebody watches at the end (an encoder is only made for viewers).
                let last = setup.live.as_ref().and_then(|steps| steps.iter().rev().find(|step| step.1).map(|step| step.2));
                if let Some(last) = last.filter(|last| st.watched && st.encoder_min_qp != *last) {
                    fails.add(format!("the script's last quality floor is QP {last} and the encoder in use at the end has QP {} ({})", st.encoder_min_qp, st.encoder.name()));
                }
            }
            if !live_rate_steps.is_empty() {
                println!("stream controls: {} move(s) of the target came too close to the end to be measured", live_rate_steps.len());
            }
            live_controls::end_share();
        }
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
            if let Some(cadence) = &st.cadence {
                println!("GPU capture cadence at the end: {cadence}");
            }
        }
        if let Some(st) = last_gpu_stats.as_ref().filter(|st| (st.picture_rect.2, st.picture_rect.3) != setup.size || st.bars_painted + st.bars_failed > 0) {
            println!(
                "GPU capture: the picture lies at {},{} {}x{} in the {}x{} frame; bars painted on {} surfaces in {:.1} ms each ({} could not be painted)",
                st.picture_rect.0, st.picture_rect.1, st.picture_rect.2, st.picture_rect.3, setup.size.0, setup.size.1, st.bars_painted, st.bars_us as f64 / 1000.0 / st.bars_painted.max(1) as f64, st.bars_failed
            );
            if st.bars_failed > 0 {
                fails.add(format!("the bars of {} surfaces could not be painted", st.bars_failed));
            }
            if st.shape_changes > 0 {
                println!("GPU capture: vapostproc was told a new shape of the picture {} times, {:.2} ms each", st.shape_changes, st.shape_us as f64 / 1000.0 / st.shape_changes as f64);
            }
        }
        println!("{} ms", dist("capture -> access unit in hand (GPU path)", &mut all_lat));
        println!("{} ms", dist("encode() per frame (gst-launch path; excludes the capture pipeline before the pipe)", &mut all_enc));
        println!("{} ms", dist("gap between access units while watched", &mut all_gap));
        println!("{} ms", dist("keyframe request -> IDR in hand", &mut idr_latency));
        println!("{} ms", dist("viewer arrives -> first access unit (an IDR)", &mut watch_latency));
        if !profile_changes.is_empty() || setup.profile != StreamProfile::Compatible {
            println!("profile changes: {}", if profile_changes.is_empty() { "none".to_string() } else { profile_changes.join(", ") });
            println!("{} ms", dist("profile change mid-stream: last access unit of the old profile -> first IDR of the new", &mut switch_gaps));
            if let Some(st) = &last_gpu_stats {
                println!("GPU capture at its end: {} encoder, replaced {} times{}", if st.main { "Main" } else { "Constrained Baseline" }, st.profile_switches, if st.main_unavailable { ", Main found unavailable" } else { "" });
            }
        }
        if totals.gpu_aus > 0 {
            let show = |bytes: &mut Vec<usize>| {
                bytes.sort();
                match bytes.len() {
                    0 => "none".to_string(),
                    n => format!("n={n} {}..{} bytes (median {})", bytes[0], bytes[n - 1], bytes[n / 2]),
                }
            };
            println!("GPU keyframes: first of a new encoder (a change of profile, or of quality floor with --live): {}; all others: {}", show(&mut new_encoder_idrs), show(&mut other_idrs));
            if !unasked_idrs.is_empty() {
                fails.add(format!("{} keyframes of the GPU capture that nobody asked for (at {})", unasked_idrs.len(), unasked_idrs.join(", ")));
            }
        }
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
            println!("restarts after a change of format or a silence: GPU capture {}, window capture {}", gpu_restarts.load(Ordering::SeqCst), legacy.window_restarts.load(Ordering::SeqCst));
        }
        if setup.end_source.is_some() || setup.source_must_end {
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
            if let Some(g) = &setup.geometry {
                for line in &verdict.geometry_lines {
                    println!("{line}");
                }
                let rects = |path: usize| verdict.geometry_rects[path].iter().map(|(x, y, w, h)| format!("{x},{y} {w}x{h}")).collect::<Vec<_>>().join(" | ");
                println!(
                    "geometry: {} keyframes of the GPU capture checked (picture measured at: {}), {} of the gst-launch path (picture measured at: {}); brightest bar pixel in any of them {}",
                    verdict.geometry_checked[0], rects(0), verdict.geometry_checked[1], rects(1), verdict.geometry_bar_max
                );
                for line in preview_geometry.lock().unwrap().iter() {
                    println!("geometry: {line}");
                }
                if verdict.geometry_checked[0] + verdict.geometry_checked[1] == 0 {
                    fails.add("no keyframe's geometry was checked".into());
                }
                // A share that changed path keeps its picture where it was.
                if g.measure && verdict.geometry_checked[0] > 0 && verdict.geometry_checked[1] > 0 {
                    let all: Vec<_> = verdict.geometry_rects.iter().flatten().collect();
                    let spread = |f: fn(&(u32, u32, u32, u32)) -> u32| all.iter().map(|r| f(r)).max().unwrap_or(0) - all.iter().map(|r| f(r)).min().unwrap_or(0);
                    let worst = spread(|r| r.0).max(spread(|r| r.1)).max(spread(|r| r.2)).max(spread(|r| r.3));
                    println!("geometry: across the handover the picture's rectangle moves by at most {worst} px");
                    if worst > 1 {
                        fails.add(format!("the picture's rectangle differs by {worst} px between the GPU capture and the gst-launch path"));
                    }
                }
                let bad = preview_geometry_bad.load(Ordering::Relaxed);
                if bad > 0 {
                    fails.add(format!("{bad} preview pictures with the wrong geometry or bars that are not black"));
                }
            }
        }
        if setup.verify && (verdict.main_units > 0 || setup.profile != StreamProfile::Compatible) {
            println!("receiver: {} of the access units were H.264 Main", verdict.main_units);
        }
        if let Some(must_fail) = setup.main_must_fail {
            let unavailable = last_gpu_stats.as_ref().is_some_and(|st| st.main_unavailable);
            if totals.aus > totals.gpu_aus {
                fails.add(format!("Main going wrong sent {} access units to the gst-launch path", totals.aus - totals.gpu_aus));
            }
            if must_fail && (!unavailable || verdict.main_units > 0) {
                fails.add(format!("Main was to be found unavailable: unavailable={unavailable}, {} Main access units", verdict.main_units));
            }
            if !must_fail && (unavailable || (setup.verify && verdict.main_units == 0)) {
                fails.add(format!("one late first picture was to be forgiven: unavailable={unavailable}, {} Main access units", verdict.main_units));
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
        let profile_spec = args.str("profile", "compatible");
        let profile = match profile_spec.as_str() {
            "compatible" => StreamProfile::Compatible,
            "efficient" => StreamProfile::Efficient,
            "auto" => StreamProfile::Auto,
            _ => return Err(format!("--profile: compatible, efficient or auto, not {profile_spec}")),
        };
        // --watch as a viewer script: one native viewer that decodes Main.
        let mut viewers: Vec<(f64, Vec<char>)> = Vec::new();
        let mut watch_secs = 0.0;
        for (watched, secs) in &watch {
            viewers.push((watch_secs, if *watched { vec!['m'] } else { Vec::new() }));
            watch_secs += secs;
        }
        let mut share_secs = watch_secs;
        if let Some(spec) = args.0.get("viewers") {
            viewers.clear();
            for part in spec.split(',') {
                let (at, list) = part.split_once(':').ok_or(format!("bad --viewers part {part}"))?;
                let at = at.parse::<f64>().map_err(|_| format!("bad --viewers part {part}"))?;
                let tokens: Vec<char> = if list == "-" { Vec::new() } else { list.split('+').filter_map(|t| t.chars().next()).collect() };
                if let Some(t) = tokens.iter().find(|t| !matches!(t, 'm' | 'c' | 'w')) {
                    return Err(format!("--viewers: unknown viewer '{t}' (m, c, w or -)"));
                }
                viewers.push((at, tokens));
            }
            share_secs = duration;
        }
        let fail_mode = args.str("fail", "");
        let main_must_fail = match fail_mode.as_str() {
            "nomain" | "mainrefuse" | "mainprofile" | "mainbreak" | "prepstuck:0" => Some(true),
            "mainlate:1" => Some(false),
            mode if mode.starts_with("mainlate:") => Some(true),
            _ => None,
        };
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
        // --live: the script of the stream controls, and the settings the share starts from.
        let mut live_floor_rests = 0;
        let live = match args.0.get("live") {
            None => None,
            Some(spec) => {
                let mut steps: Vec<(f64, bool, u32, bool)> = Vec::new();
                let bad = |part: &str| format!("bad --live part {part} (SEC:bitrate=MBPS, SEC:floor=QP or SEC:floor~A..B)");
                for part in spec.split(',') {
                    let (at, change) = part.split_once(':').ok_or_else(|| bad(part))?;
                    let at = at.parse::<f64>().map_err(|_| bad(part))?;
                    if let Some(v) = change.strip_prefix("bitrate=") {
                        steps.push((at, false, v.parse().map_err(|_| bad(part))?, true));
                    } else if let Some(v) = change.strip_prefix("floor=") {
                        steps.push((at, true, v.parse().map_err(|_| bad(part))?, true));
                        live_floor_rests += 1;
                    } else if let Some((a, b)) = change.strip_prefix("floor~").and_then(|v| v.split_once("..")) {
                        let (a, b) = (a.parse::<i64>().map_err(|_| bad(part))?, b.parse::<i64>().map_err(|_| bad(part))?);
                        let n = (b - a).abs();
                        for i in 0..=n {
                            steps.push((at + i as f64 * 0.05, true, (a + (b - a).signum() * i) as u32, false));
                        }
                        live_floor_rests += 1;
                    } else {
                        return Err(bad(part));
                    }
                }
                steps.sort_by(|a, b| a.0.total_cmp(&b.0));
                let start = args.str("live-start", "8:30");
                let (mbps, qp) = start.split_once(':').and_then(|(m, q)| Some((m.parse::<u32>().ok()?, q.parse::<u32>().ok()?))).ok_or(format!("bad --live-start {start} (MBPS:QP)"))?;
                // In memory only: the settings file is not written.
                let mut store = litecord::utils::video_settings::get_video_settings_store().lock().unwrap_or_else(|e| e.into_inner());
                store.stream_max_bitrate_mbps = Some(mbps);
                store.stream_min_qp = Some(qp);
                Some(steps)
            }
        };
        let max_fps = args.0.get("max-fps").map(|v| v.parse::<u32>().map_err(|_| format!("bad --max-fps {v}"))).transpose()?;
        let cycles = args.num("cycles", 1.0)? as usize;
        let cycle_gap = Duration::from_millis(args.num("cycle-gap", 200.0)? as u64);
        let total_secs: f64 = share_secs * cycles as f64;

        // ---- the source
        let source = args.str("source", "mutter");
        let helper_pids = Vec::new();
        let mut _synthetic = None;
        let end_at = args.0.get("end-source").map(|v| v.parse::<f64>().map_err(|_| format!("bad --end-source {v}"))).transpose()?;
        let mut end_source: Option<(f64, Arc<dyn Fn() + Send + Sync>)> = None;
        // --geometry: the source's picture size when it is known beforehand, and whether its
        // pictures are the colour bars.
        let mut source_size: Option<(u32, u32)> = None;
        let mut measurable = false;
        let node = if let Some(id) = source.strip_prefix("node:") {
            let id = id.parse::<u32>().map_err(|_| format!("bad node id {id}"))?;
            if !video_node_present(id) {
                return Err(format!("PipeWire has no video node {id}"));
            }
            id
        } else if source == "synthetic" {
            let src_size = parse_size(&args.str("src-size", "2560x1440"))?;
            let src_fps = args.num("src-fps", 60.0)? as u32;
            let props = args.str("src-props", if args.has("colours") || args.has("geometry") { "pattern=smpte100" } else { "pattern=ball motion=sweep" });
            source_size = Some(src_size);
            // (Other patterns have dark edges: only their bars are checked.)
            measurable = props.contains("smpte100") || props.contains("pattern=white");
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
        } else if source == "mutter-window" || source.starts_with("mutter-window:") {
            if total_secs > 180.0 {
                return Err("recordings of the real screen are kept under three minutes".into());
            }
            let id = source.strip_prefix("mutter-window:").map(|id| id.parse::<u64>().map_err(|_| format!("bad window id {id}"))).transpose()?;
            let (_, node) = Mutter::open("", None, Some(id))?;
            println!("source: Mutter ScreenCast RecordWindow({}) cursor embedded, PipeWire node {node}", id.map(|id| id.to_string()).unwrap_or_else(|| "the focused window".into()));
            if let Some(at) = end_at {
                end_source = Some((at, Arc::new(stop_mutter)));
            }
            node
        } else if source == "mutter" || source.starts_with("mutter:") || source.starts_with("mutter-area:") {
            if total_secs > 180.0 {
                return Err("recordings of the real screen are kept under three minutes".into());
            }
            let connector = source.strip_prefix("mutter:").unwrap_or("");
            let area = match source.strip_prefix("mutter-area:") {
                Some(spec) => {
                    let mut parts = spec.split('+');
                    let (w, h) = parse_size(parts.next().unwrap_or(""))?;
                    let mut offset = || parts.next().map(|v| v.parse::<i32>().map_err(|_| format!("bad area {spec}"))).transpose().map(|v| v.unwrap_or(0));
                    let (x, y) = (offset()?, offset()?);
                    source_size = Some((w, h));
                    Some((x, y, w as i32, h as i32))
                }
                None => None,
            };
            if area.is_none() && args.has("src-size") {
                // (--legacy has no capture to learn the monitor's size from.)
                source_size = Some(parse_size(&args.str("src-size", ""))?);
            }
            let (_, node) = Mutter::open(connector, area, None)?;
            match area {
                Some((x, y, w, h)) => println!("source: Mutter ScreenCast RecordArea({x}, {y}, {w}, {h}) cursor embedded, PipeWire node {node}"),
                None => println!("source: Mutter ScreenCast RecordMonitor({}) cursor embedded, PipeWire node {node}", if connector.is_empty() { "primary" } else { connector }),
            }
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
            monitor: source_size.filter(|_| !args.has("as-window")),
            expect_start: (!args.has("fail") || main_must_fail.is_some()) && !args.has("freeze-source") && !(args.has("as-window") && std::env::var("LITECORD_VAAPI_CAPTURE_WINDOWS").as_deref() == Ok("0")),
            end_source,
            node,
            size,
            fps,
            legacy_only: args.has("legacy"),
            memory,
            max_fps,
            viewers,
            total_secs: share_secs,
            profile,
            main_must_fail,
            main_late: fail_mode.starts_with("mainlate"),
            bitrates,
            live,
            live_floor_rests,
            floor_must_fail: matches!(args.0.get("fail").map(|f| f.as_str()), Some("floorrefuse" | "floormute" | "floorbreak")) || std::env::var("LITECORD_VAAPI_TEST_FAIL").as_deref() == Ok("floor"),
            pli,
            verify: !args.has("no-verify"),
            colours: args.has("colours"),
            geometry: args.has("geometry").then(|| Geometry {
                source: Arc::new(Mutex::new(source_size)),
                told: source_size.is_some() && !args.has("as-window"),
                measure: measurable,
                png: args.0.get("png").map(std::path::PathBuf::from),
                name: format!("{}_{}_in_{}x{}", source.split(':').next().unwrap_or("source"), source_size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_else(|| "screen".into()), size.0, size.1),
            }),
            verbose: args.has("verbose"),
            dump_aus: args.0.get("dump-aus").cloned(),
            au_log: args.0.get("au-log").cloned(),
            window_script: args
                .str("window-script", "")
                .split(';')
                .filter(|p| !p.trim().is_empty())
                .map(|p| {
                    let (at, line) = p.trim().split_once(':').ok_or(format!("bad --window-script part {p}"))?;
                    Ok((at.parse::<f64>().map_err(|_| format!("bad --window-script part {p}"))?, line.to_string()))
                })
                .collect::<Result<_, String>>()?,
            window_pipe: args.0.get("window-pipe").cloned(),
            source_must_end: args.has("source-must-end"),
            helper_pids,
        };
        println!(
            "== {}: {}x{} {fps} fps, {}, profile {profile_spec}, bitrate {bitrate_spec} kbps, {cycles} share(s){} ==",
            if setup.legacy_only { "today's gst-launch capture + create_best_encoder()" } else { "in-process GPU capture (linux_gpu_tx)" },
            size.0,
            size.1,
            args.0.get("viewers").map(|v| format!("viewers {v} for {duration} s")).unwrap_or_else(|| format!("watch {watch_spec}")),
            args.0.get("fail").map(|f| format!(", failure injected: {f}")).unwrap_or_default()
        );

        if let Some(dir) = setup.geometry.as_ref().and_then(|g| g.png.as_ref()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
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
