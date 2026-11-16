//! The worker of one Go Live stream: one thread with its own runtime that owns the
//! stream's gateway socket, its UDP socket, its DAVE session and its transport cipher.
//!
//! ```text
//!  Armed --capture has a picture--> Creating --STREAM_CREATE + STREAM_SERVER_UPDATE--> Connecting
//!                                                                                        | op 0 .. op 4
//!                                               Waiting <--DAVE has / has not a key--> Live
//!                                                  | socket lost            ^ op 9
//!                                                  v                        |
//!                                              Reconnecting --resume refused--> Connecting
//! ```
//!
//! Everything from outside comes in through channels (`Handle`), and everything the worker
//! needs from the application comes through `Deps`, so the whole of it runs against a
//! scripted gateway and media server in the tests.
//!
//! What the stream carries is decided by DAVE: while DAVE is in force and the group has no
//! other member (nobody watches), nothing is sent and `Shared::wanted` is false, so the
//! share's encoder is not kept busy for Discord.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde_json::Value;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::crypto::{self, Mode, Transport};
use super::dave::{self, Dave, GroupChoice, Out, Sending};
use super::h264::{Preparer, Skip};
use super::rtp::{self, ExtProfile, Header, History, Sent};
use super::signal::{self, Region, StreamKey};
use super::wire::{self, CloseAction};
use super::{rtcp, udp, Failure, Params, Phase, Status};
use crate::screen_capture::stream_audio::StreamAudioEncoder;

/// Access units waiting for the worker; when full, the newest is dropped and a keyframe asked for.
const FRAME_QUEUE: usize = 32;
/// 10 ms chunks of stream audio waiting for the worker.
const AUDIO_QUEUE: usize = 64;
/// Video packets sent per millisecond at most (about 119 Mbit/s): a keyframe leaves as a
/// stream of packets the path can carry, not as one burst.
const PACE_BURST: u64 = 12;
const PACE_TICK: Duration = Duration::from_millis(1);
/// Packets that may leave at once after a pause (four ticks' worth).
const PACE_BUCKET: u64 = 4 * PACE_BURST * 1000;
/// Video packets kept to answer a NACK: about 2.4 s at 8 Mbit/s.
const HISTORY_PACKETS: usize = 2048;
/// Keyframes asked of the encoder at most this often, however many viewers ask.
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_millis(300);
/// While frames are held back for a keyframe, it is asked for again this often.
const KEYFRAME_RETRY_EVERY: Duration = Duration::from_millis(1000);
const VIDEO_REPORT_EVERY: Duration = Duration::from_secs(1);
const AUDIO_REPORT_EVERY: Duration = Duration::from_secs(5);
const STATS_EVERY: Duration = Duration::from_secs(5);
/// How long a receiver report counts for the share's rate control.
const LOSS_REPORT_AGE: Duration = Duration::from_secs(3);
/// Silence packets sent right after the handshake, 20 ms apart, to open the path.
const OPENING_SILENCE: u32 = 10;
const OPUS_FRAME: Duration = Duration::from_millis(20);
const OPUS_FRAME_SAMPLES: u32 = 960;
/// Stream audio that has been all zeros this long stops being sent...
const QUIET_AFTER: Duration = Duration::from_millis(400);
/// ...after this many silence packets, which tell receivers it paused.
const TRAILING_SILENCE: u32 = 5;
/// An access unit or audio chunk older than this when the worker gets to it is not sent.
const STALE_INPUT: Duration = Duration::from_millis(400);
const NO_LOSS: u64 = u64::MAX;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Discovery = Result<(Arc<UdpSocket>, Option<(String, u16)>), String>;

/// The guesses only a live run can settle, each readable from the environment so that the
/// other answer can be tried without a rebuild. Read once per start and logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switches {
    /// `LITECORD_GOLIVE_EXT`: `full` (default), `min`, `none`.
    pub ext: ExtProfile,
    /// `LITECORD_GOLIVE_GW`: `8` (default) or `9` (adds `channel_id` to Identify and Resume).
    pub gateway_version: u8,
    /// `LITECORD_GOLIVE_PT`: `101` (default) or `105`; RTX is the next number.
    pub payload_type: u8,
    /// `LITECORD_GOLIVE_RTX`: `1` (default) or `0` (no retransmissions declared or sent).
    pub rtx: bool,
    /// `LITECORD_GOLIVE_STREAM_TYPE`: `video` (default) or `screen`, the type in opcode 12.
    pub stream_type: &'static str,
    /// `LITECORD_GOLIVE_REGION`: `null` (default) or `omit`.
    pub region: Region,
    /// `LITECORD_GOLIVE_GROUP`: `auto` (default), `channel`, `server`, or a number.
    pub group: GroupChoice,
    /// `LITECORD_GOLIVE_DAVE_CHECK`: `1` (default) or `0` (anyone the gateway proposes is let in).
    pub dave_check: bool,
    /// `LITECORD_GOLIVE_REQUIRE_DAVE`: `0` (default) or `1` (send nothing when the server
    /// negotiates no end-to-end encryption).
    pub require_dave: bool,
    /// `LITECORD_GOLIVE_DUMP`: a directory for the prepared access units and a packet log.
    pub dump: Option<PathBuf>,
}

impl Default for Switches {
    fn default() -> Self {
        Self {
            ext: ExtProfile::Full,
            gateway_version: 8,
            payload_type: 101,
            rtx: true,
            stream_type: "video",
            region: Region::Null,
            group: GroupChoice::Auto,
            dave_check: true,
            require_dave: false,
            dump: None,
        }
    }
}

impl Switches {
    /// The switches as `get` (the environment) sets them. A value that is not one of a
    /// switch's values leaves its default and is named in the second result.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut unknown = Vec::new();
        let mut take = |name: &str, apply: &mut dyn FnMut(&str) -> bool| {
            if let Some(value) = get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                if !apply(&value) {
                    unknown.push(format!("{name}={value}"));
                }
            }
        };
        let flag = |v: &str| match v {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
        take("LITECORD_GOLIVE_EXT", &mut |v| ExtProfile::from_name(v).map(|x| s.ext = x).is_some());
        take("LITECORD_GOLIVE_GW", &mut |v| matches!(v, "8" | "9").then(|| s.gateway_version = v.parse().unwrap_or(8)).is_some());
        take("LITECORD_GOLIVE_PT", &mut |v| v.parse::<u8>().ok().filter(|pt| (96..=126).contains(pt) && *pt != rtp::OPUS_PAYLOAD_TYPE && *pt + 1 != rtp::OPUS_PAYLOAD_TYPE).map(|pt| s.payload_type = pt).is_some());
        take("LITECORD_GOLIVE_RTX", &mut |v| flag(v).map(|x| s.rtx = x).is_some());
        take("LITECORD_GOLIVE_STREAM_TYPE", &mut |v| match v {
            "video" => {
                s.stream_type = "video";
                true
            }
            "screen" => {
                s.stream_type = "screen";
                true
            }
            _ => false,
        });
        take("LITECORD_GOLIVE_REGION", &mut |v| match v {
            "null" => {
                s.region = Region::Null;
                true
            }
            "omit" => {
                s.region = Region::Omit;
                true
            }
            _ => false,
        });
        take("LITECORD_GOLIVE_GROUP", &mut |v| GroupChoice::from_name(v).map(|x| s.group = x).is_some());
        take("LITECORD_GOLIVE_DAVE_CHECK", &mut |v| flag(v).map(|x| s.dave_check = x).is_some());
        take("LITECORD_GOLIVE_REQUIRE_DAVE", &mut |v| flag(v).map(|x| s.require_dave = x).is_some());
        take("LITECORD_GOLIVE_DUMP", &mut |v| {
            s.dump = Some(PathBuf::from(v));
            true
        });
        (s, unknown)
    }

    pub fn from_env() -> Self {
        let (switches, unknown) = Self::read(|name| std::env::var(name).ok());
        for setting in unknown {
            warn!("[GO LIVE] {setting} is not a value of that switch: the default is used");
        }
        switches
    }

    pub fn describe(&self) -> String {
        format!(
            "EXT={} GW={} PT={} RTX={} STREAM_TYPE={} REGION={} GROUP={} DAVE_CHECK={} REQUIRE_DAVE={} DUMP={}",
            self.ext.name(),
            self.gateway_version,
            self.payload_type,
            self.rtx as u8,
            self.stream_type,
            if self.region == Region::Null { "null" } else { "omit" },
            match self.group {
                GroupChoice::Auto => "auto".to_string(),
                GroupChoice::Channel => "channel".to_string(),
                GroupChoice::Server => "server".to_string(),
                GroupChoice::Id(id) => id.to_string(),
            },
            self.dave_check as u8,
            self.require_dave as u8,
            self.dump.as_ref().map_or("off".to_string(), |d| d.display().to_string())
        )
    }
}

/// Times and limits of the state machine; the tests shorten them.
#[derive(Clone, Debug)]
pub struct Tuning {
    pub switches: Switches,
    /// What opcode 12 announces as the stream's highest bitrate.
    pub max_bitrate: u32,
    /// How long the main gateway may take to answer the stream request.
    pub create_timeout: Duration,
    /// How long a stream whose media server went away waits for another.
    pub unavailable_timeout: Duration,
    /// From opening the stream gateway socket to Session Description (or Resumed).
    pub handshake_timeout: Duration,
    /// Waits before the reconnects after a loss; a failure beyond the last one ends the stream.
    pub retry_delays: Vec<Duration>,
    /// Failed reconnects older than this are forgotten.
    pub retry_window: Duration,
    pub discovery_tries: u32,
    pub discovery_wait: Duration,
    /// With nothing sent for this long, one silence packet keeps the path open.
    pub keepalive: Duration,
    /// A video packet still unsent after this long: the queue is dropped for a new keyframe.
    pub stale_frame: Duration,
    /// How long a stopping worker gives its last gateway messages to leave.
    pub close_linger: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            switches: Switches::default(),
            max_bitrate: super::MAX_BITRATE,
            create_timeout: Duration::from_secs(30),
            unavailable_timeout: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(15),
            retry_delays: [1, 2, 4, 8, 15].into_iter().map(Duration::from_secs).collect(),
            retry_window: Duration::from_secs(120),
            discovery_tries: 5,
            discovery_wait: Duration::from_millis(600),
            keepalive: Duration::from_secs(5),
            stale_frame: Duration::from_millis(400),
            close_linger: Duration::from_millis(300),
        }
    }
}

/// What the worker needs from the application.
#[derive(Clone)]
pub struct Deps {
    /// Sends a payload on the main gateway; false when there is no connection to send it on.
    pub gateway_send: Arc<dyn Fn(Value) -> bool + Send + Sync>,
    /// The stream gateway URL for an endpoint of STREAM_SERVER_UPDATE; `None` refuses it.
    pub url_for: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// Asks the share's encoder for a keyframe.
    pub request_keyframe: Arc<dyn Fn() + Send + Sync>,
    /// Wakes the share's send loop (it idles while nobody watches).
    pub wake_sender: Arc<dyn Fn() + Send + Sync>,
    /// The users of our voice channel: only they (and those the stream server announces)
    /// are let into the DAVE group.
    pub recognised_users: Arc<dyn Fn() -> Vec<u64> + Send + Sync>,
    pub tuning: Tuning,
}

/// The voice call the stream belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub user_id: u64,
    /// The session id of the account's voice state (not logged in full).
    pub session_id: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
}

/// What other threads read of a stream.
pub struct Shared {
    wanted: AtomicBool,
    capture_ready: AtomicBool,
    capture_signal: tokio::sync::Notify,
    /// The last loss a receiver report gave: permille in the low 16 bits, milliseconds
    /// since `born` above them.
    loss: AtomicU64,
    /// Bumped when an access unit could not be queued: the frames after it need a keyframe.
    epoch: AtomicU64,
    status: Mutex<Status>,
    /// A failure the user has not been told about yet.
    notice: Mutex<Option<Failure>>,
    changed: Box<dyn Fn() + Send + Sync>,
    born: Instant,
}

impl Shared {
    /// `changed` is called (from the worker's thread) whenever `status` changed.
    pub fn new(changed: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            wanted: AtomicBool::new(false),
            capture_ready: AtomicBool::new(false),
            capture_signal: tokio::sync::Notify::new(),
            loss: AtomicU64::new(NO_LOSS),
            epoch: AtomicU64::new(0),
            status: Mutex::new(Status { phase: Phase::Armed, e2ee: true, ..Status::default() }),
            notice: Mutex::new(None),
            changed: Box::new(changed),
            born: Instant::now(),
        })
    }

    /// The share should encode for Discord and offer its access units.
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// The share has a picture source (called by its send loop on every turn).
    pub fn capture_ready(&self) {
        if !self.capture_ready.load(Ordering::Relaxed) {
            self.capture_ready.store(true, Ordering::Release);
            self.capture_signal.notify_one();
        }
    }

    /// The loss the media server reported for the video in the last three seconds, in permille.
    pub fn worst_loss(&self) -> Option<u16> {
        let packed = self.loss.load(Ordering::Relaxed);
        if packed == NO_LOSS {
            return None;
        }
        let age = (self.born.elapsed().as_millis() as u64).saturating_sub(packed >> 16);
        (age < LOSS_REPORT_AGE.as_millis() as u64).then_some(packed as u16)
    }

    fn set_loss(&self, permille: u16) {
        self.loss.store((self.born.elapsed().as_millis() as u64) << 16 | permille as u64, Ordering::Relaxed);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The failure that ended the stream, once.
    pub fn take_notice(&self) -> Option<Failure> {
        self.notice.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        let changed = {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            let before = status.clone();
            change(&mut status);
            *status != before
        };
        if changed {
            (self.changed)();
        }
    }
}

enum Control {
    Event(signal::Event),
    /// `deleted`: the stream key the caller already sent opcode 19 for.
    Stop { deleted: Option<String> },
}

struct Frame {
    data: Arc<[u8]>,
    at: Instant,
    epoch: u64,
}

struct Audio {
    pcm: Vec<i16>,
    rate: u32,
    at: Instant,
}

/// The outside's end of a stream.
pub struct Handle {
    pub shared: Arc<Shared>,
    pub context: Context,
    audio: bool,
    control: mpsc::UnboundedSender<Control>,
    frames: mpsc::Sender<Frame>,
    audio_chunks: mpsc::Sender<Audio>,
    gateway_send: Arc<dyn Fn(Value) -> bool + Send + Sync>,
    request_keyframe: Arc<dyn Fn() + Send + Sync>,
    thread: std::thread::JoinHandle<()>,
}

impl Handle {
    /// A STREAM_* event of the main gateway.
    pub fn event(&self, event: signal::Event) {
        let _ = self.control.send(Control::Event(event));
    }

    /// Ends the stream. Opcode 19 is handed to the main gateway here, so that it does not
    /// depend on the worker getting to run; the worker does the rest. Never blocks.
    pub fn stop(&self) {
        let key = self.shared.status().stream_key;
        let deleted = (!key.is_empty() && (self.gateway_send)(signal::delete(&key))).then_some(key);
        self.shared.wanted.store(false, Ordering::Relaxed);
        let _ = self.control.send(Control::Stop { deleted });
    }

    /// The account's voice state changed: a stream does not outlive its voice channel.
    pub fn voice_channel(&self, channel_id: Option<&str>) {
        if channel_id != Some(self.context.channel_id.as_str()) && !self.finished() {
            info!("[GO LIVE] The voice channel changed ({:?}): the stream ends with it", channel_id);
            self.stop();
        }
    }

    /// An access unit of the share (Annex B), for the worker to send if somebody watches.
    pub fn offer_frame(&self, annex_b: &[u8]) {
        if !self.shared.wanted() || !(annex_b.starts_with(&[0, 0, 0, 1]) || annex_b.starts_with(&[0, 0, 1])) {
            return;
        }
        let epoch = self.shared.epoch.load(Ordering::Relaxed);
        if self.frames.try_send(Frame { data: Arc::from(annex_b), at: Instant::now(), epoch }).is_err() {
            self.shared.epoch.fetch_add(1, Ordering::Relaxed);
            (self.request_keyframe)();
        }
    }

    /// 10 ms of the share's audio (mono), when the share has audio.
    pub fn offer_audio(&self, pcm: &[i16], sample_rate: u32) {
        if !self.audio || !self.shared.wanted() {
            return;
        }
        let _ = self.audio_chunks.try_send(Audio { pcm: pcm.to_vec(), rate: sample_rate, at: Instant::now() });
    }

    /// The worker is gone (the stream ended).
    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }
}

/// Starts the worker of a stream for `context`. It waits for the share to have a picture,
/// then asks the main gateway for the stream.
pub fn spawn(deps: Deps, context: Context, params: Params, shared: Arc<Shared>) -> std::io::Result<Handle> {
    let (control, control_rx) = mpsc::unbounded_channel();
    let (frames, frames_rx) = mpsc::channel(FRAME_QUEUE);
    let (audio_chunks, audio_rx) = mpsc::channel(AUDIO_QUEUE);
    let handle_deps = (Arc::clone(&deps.gateway_send), Arc::clone(&deps.request_keyframe));
    let (worker_shared, worker_context) = (Arc::clone(&shared), context.clone());
    let thread = std::thread::Builder::new().name("golive".into()).spawn(move || {
        crate::utils::thread_priority::raise_current_thread();
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime.block_on(run(Worker::new(deps, worker_context, params, worker_shared), control_rx, frames_rx, audio_rx)),
            Err(e) => {
                error!("[GO LIVE] The stream's thread could not start its runtime: {e}");
                worker_shared.wanted.store(false, Ordering::Relaxed);
                *worker_shared.notice.lock().unwrap_or_else(|e| e.into_inner()) = Some(Failure::Lost("runtime".into()));
                worker_shared.update(|s| {
                    s.phase = Phase::Off;
                    s.failure = Some(Failure::Lost("runtime".into()));
                });
            }
        }
    })?;
    Ok(Handle { shared, context, audio: params.audio, control, frames, audio_chunks, gateway_send: handle_deps.0, request_keyframe: handle_deps.1, thread })
}

async fn pending_or<T>(future: &mut Option<Boxed<T>>) -> T {
    match future {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

async fn next_message(reader: &mut Option<SplitStream<Ws>>) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match reader {
        Some(reader) => reader.next().await,
        None => std::future::pending().await,
    }
}

async fn receive(socket: &Option<Arc<UdpSocket>>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match socket {
        Some(socket) => socket.recv(buffer).await,
        None => std::future::pending().await,
    }
}

async fn run(mut w: Worker, mut control: mpsc::UnboundedReceiver<Control>, mut frames: mpsc::Receiver<Frame>, mut audio: mpsc::Receiver<Audio>) {
    w.started();
    let mut datagram = vec![0u8; 2048];
    while !w.finished {
        let wake = w.next_wake(Instant::now());
        tokio::select! {
            biased;
            message = control.recv() => match message {
                Some(Control::Event(event)) => w.on_event(event),
                Some(Control::Stop { deleted }) => w.stop(deleted).await,
                // Nobody holds the stream any more.
                None => w.stop(None).await,
            },
            _ = w.shared.capture_signal.notified(), if w.phase == Phase::Armed => w.create_stream(),
            result = pending_or(&mut w.io.connecting) => w.on_connected(result),
            result = pending_or(&mut w.io.discovering) => w.on_discovered(result),
            message = next_message(&mut w.io.reader) => w.on_message(message),
            received = receive(&w.io.socket, &mut datagram) => {
                if let Ok(len) = received {
                    w.on_datagram(&datagram[..len]);
                }
            },
            Some(frame) = frames.recv() => w.on_frame(frame),
            Some(chunk) = audio.recv() => w.on_audio(chunk),
            _ = tokio::time::sleep_until(wake.into()) => {},
        }
        if !w.finished {
            w.on_time(Instant::now());
        }
    }
}

/// Where the stream gateway socket stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// No socket, and none on its way.
    Down,
    /// A socket is being opened, or will be at `reconnect_at`; `resume`: to resume the session.
    Opening { resume: bool },
    /// Identify or Resume was sent.
    Handshake { resume: bool },
    /// Session Description or Resumed came: the session is in service.
    Up,
}

struct Io {
    connecting: Option<Boxed<Result<Ws, String>>>,
    discovering: Option<Boxed<Discovery>>,
    reader: Option<SplitStream<Ws>>,
    writer: Option<mpsc::UnboundedSender<Message>>,
    writer_task: Option<tokio::task::JoinHandle<()>>,
    socket: Option<Arc<UdpSocket>>,
}

/// Counted between two lines of the log.
#[derive(Default)]
struct Stats {
    frames_in: u32,
    frames_sent: u32,
    dropped: u32,
    keyframes: u32,
    packets: u32,
    bytes: u64,
    audio_packets: u32,
    rtx: u32,
    pli: u32,
    fir: u32,
    nack: u32,
    send_errors: u32,
    loss: Option<u16>,
    round_trip: Option<Duration>,
    estimate: Option<u64>,
}

/// First-of-a-kind log lines.
#[derive(Default)]
struct Seen {
    receiver_report: bool,
    pli: bool,
    fir: bool,
    nack: bool,
    remb: bool,
    other_feedback: bool,
    undecryptable: bool,
    stray: bool,
    keyframe: bool,
    audio: bool,
    bad_unit: bool,
    sink_wants: String,
}

struct Queued {
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    queued_at: Instant,
}

/// Retransmitted bytes may be a quarter of the media bytes of the last two seconds.
struct RtxBudget {
    second: Instant,
    media: [u64; 2],
    rtx: [u64; 2],
}

impl RtxBudget {
    fn new(now: Instant) -> Self {
        Self { second: now, media: [0; 2], rtx: [0; 2] }
    }

    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.second);
        if elapsed >= Duration::from_secs(2) {
            (self.media, self.rtx, self.second) = ([0; 2], [0; 2], now);
        } else if elapsed >= Duration::from_secs(1) {
            (self.media, self.rtx) = ([self.media[1], 0], [self.rtx[1], 0]);
            self.second += Duration::from_secs(1);
        }
    }

    fn sent(&mut self, now: Instant, bytes: usize) {
        self.roll(now);
        self.media[1] += bytes as u64;
    }

    fn allow(&mut self, now: Instant, bytes: usize) -> bool {
        self.roll(now);
        let used = self.rtx[0] + self.rtx[1] + bytes as u64;
        if used * 4 > self.media[0] + self.media[1] {
            return false;
        }
        self.rtx[1] += bytes as u64;
        true
    }
}

struct Dump {
    prepared: std::fs::File,
    packets: std::fs::File,
    started: Instant,
}

impl Dump {
    fn open(dir: &PathBuf) -> Option<Self> {
        let made = std::fs::create_dir_all(dir).and_then(|()| Ok((std::fs::File::create(dir.join("prepared.h264"))?, std::fs::File::create(dir.join("packets.log"))?)));
        match made {
            Ok((prepared, packets)) => {
                info!("[GO LIVE] LITECORD_GOLIVE_DUMP: writing prepared.h264 (access units before DAVE) and packets.log to {}", dir.display());
                Some(Self { prepared, packets, started: Instant::now() })
            }
            Err(e) => {
                warn!("[GO LIVE] LITECORD_GOLIVE_DUMP: {} cannot be written ({e}); nothing is dumped", dir.display());
                None
            }
        }
    }

    fn packet(&mut self, kind: &str, sequence: u16, timestamp: u32, marker: bool, len: usize) {
        let _ = writeln!(self.packets, "{:.3} {kind} seq={sequence} ts={timestamp} m={} len={len}", self.started.elapsed().as_secs_f64(), marker as u8);
    }
}

/// The media side of one stream gateway session: the socket, the SSRCs, the cipher and
/// all counters. Kept across a resume, made anew by every Identify.
struct Media {
    socket: Arc<UdpSocket>,
    transport: Option<Transport>,
    audio_ssrc: u32,
    video_ssrc: u32,
    rtx_ssrc: u32,
    ext: ExtProfile,
    payload_type: u8,
    rtx: bool,
    video_sequence: u16,
    rtx_sequence: u16,
    audio_sequence: u16,
    transport_sequence: u16,
    /// Video timestamps: `ts0` at `t0`, 90 kHz from there.
    t0: Instant,
    ts0: u32,
    last_video_timestamp: Option<u32>,
    audio_timestamp: u32,
    last_audio: Option<Instant>,
    preparer: Preparer,
    /// Frames are dropped until a keyframe comes (after a gap, a new key, a full queue).
    waiting_keyframe: bool,
    frame_epoch: u64,
    queue: VecDeque<Queued>,
    retransmit: VecDeque<Sent>,
    /// A sealed datagram the socket could not take yet.
    blocked: Option<Vec<u8>>,
    history: History,
    budget: RtxBudget,
    /// Pacer: thousandths of a packet that may be sent, and when that was last counted.
    tokens: u64,
    paced_at: Instant,
    video_packets: u32,
    video_octets: u32,
    video_report_at: Option<Instant>,
    audio_packets: u32,
    audio_octets: u32,
    audio_report_at: Option<Instant>,
    encoder: Option<StreamAudioEncoder>,
    /// How long the stream audio has been all zeros, and the silence packets still owed.
    quiet_for: Duration,
    trailing_silence: u32,
    last_sent: Instant,
    opening_silence: u32,
    silence_at: Option<Instant>,
}

enum Pumped {
    Idle,
    /// The queue was older than allowed and was dropped: this many packets.
    Stale(usize),
}

impl Media {
    fn new(socket: Arc<UdpSocket>, ready: &wire::Ready, switches: &Switches, now: Instant) -> Self {
        Self {
            socket,
            transport: None,
            audio_ssrc: ready.audio_ssrc,
            video_ssrc: ready.video_ssrc,
            rtx_ssrc: ready.rtx_ssrc,
            ext: switches.ext,
            payload_type: switches.payload_type,
            rtx: switches.rtx,
            video_sequence: rand::random(),
            rtx_sequence: rand::random(),
            audio_sequence: rand::random(),
            transport_sequence: rand::random(),
            t0: now,
            ts0: rand::random(),
            last_video_timestamp: None,
            audio_timestamp: rand::random(),
            last_audio: None,
            preparer: Preparer::new(),
            waiting_keyframe: true,
            frame_epoch: 0,
            queue: VecDeque::new(),
            retransmit: VecDeque::new(),
            blocked: None,
            history: History::new(HISTORY_PACKETS),
            budget: RtxBudget::new(now),
            tokens: PACE_BUCKET,
            paced_at: now,
            video_packets: 0,
            video_octets: 0,
            video_report_at: None,
            audio_packets: 0,
            audio_octets: 0,
            audio_report_at: None,
            encoder: None,
            quiet_for: Duration::ZERO,
            trailing_silence: 0,
            last_sent: now,
            opening_silence: 0,
            silence_at: None,
        }
    }

    /// The video timestamp of `at`.
    fn video_timestamp(&self, at: Instant) -> u32 {
        self.ts0.wrapping_add((at.saturating_duration_since(self.t0).as_micros() as u64 * 9 / 100) as u32)
    }

    /// Seals and sends at once. False when the socket did not take it.
    fn send_now(&mut self, sealed: Option<Vec<u8>>, now: Instant) -> bool {
        let Some(sealed) = sealed else { return false };
        match self.socket.try_send(&sealed) {
            Ok(_) => {
                self.last_sent = now;
                true
            }
            Err(_) => false,
        }
    }

    /// One Opus packet (already end-to-end encrypted, or the silence packet) on the audio SSRC.
    fn send_opus(&mut self, payload: &[u8], now: Instant, dump: &mut Option<Dump>) -> bool {
        // 20 ms per packet; after a pause, the clock moves by the pause.
        let frames = match self.last_audio {
            Some(last) if now.saturating_duration_since(last) > 5 * OPUS_FRAME => (now.saturating_duration_since(last).as_millis() as u32 + 10) / 20,
            _ => 1,
        };
        self.last_audio = Some(now);
        self.audio_timestamp = self.audio_timestamp.wrapping_add(frames.max(1) * OPUS_FRAME_SAMPLES);
        self.audio_sequence = self.audio_sequence.wrapping_add(1);
        let header = Header { marker: false, payload_type: rtp::OPUS_PAYLOAD_TYPE, sequence: self.audio_sequence, timestamp: self.audio_timestamp, ssrc: self.audio_ssrc };
        let packet = rtp::audio_packet(&header, payload);
        let sealed = self.transport.as_mut().and_then(|t| t.seal_rtp(&packet));
        let sent = self.send_now(sealed, now);
        if sent {
            self.audio_packets = self.audio_packets.wrapping_add(1);
            self.audio_octets = self.audio_octets.wrapping_add(payload.len() as u32);
            if let Some(d) = dump {
                d.packet("audio", header.sequence, header.timestamp, false, packet.len());
            }
        }
        sent
    }

    fn send_report(&mut self, ssrc: u32, timestamp: u32, packets: u32, octets: u32, now: Instant) {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let report = rtcp::sender_report(ssrc, rtcp::ntp(wall), timestamp, packets, octets);
        let sealed = self.transport.as_mut().and_then(|t| t.seal_rtcp(&report));
        self.send_now(sealed, now);
    }

    /// Sends what the pacer allows of the retransmissions and the queued video.
    fn pump(&mut self, now: Instant, stale: Duration, stats: &mut Stats, dump: &mut Option<Dump>) -> Pumped {
        if let Some(sealed) = self.blocked.take() {
            match self.socket.try_send(&sealed) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.blocked = Some(sealed);
                    return Pumped::Idle;
                }
                Err(_) => stats.send_errors += 1,
                Ok(_) => self.last_sent = now,
            }
        }
        self.tokens = (self.tokens + now.saturating_duration_since(self.paced_at).as_micros() as u64 * PACE_BURST).min(PACE_BUCKET);
        self.paced_at = now;
        if self.queue.front().is_some_and(|q| now.saturating_duration_since(q.queued_at) > stale) {
            let dropped = self.queue.len();
            self.queue.clear();
            self.retransmit.clear();
            self.waiting_keyframe = true;
            return Pumped::Stale(dropped);
        }
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let abs_send_time = rtp::abs_send_time(wall);
        while self.tokens >= 1000 && self.blocked.is_none() {
            let (packet, sent, kind) = if let Some(original) = self.retransmit.pop_front() {
                self.rtx_sequence = self.rtx_sequence.wrapping_add(1);
                let header = Header { marker: original.marker, payload_type: self.payload_type + 1, sequence: self.rtx_sequence, timestamp: original.timestamp, ssrc: self.rtx_ssrc };
                self.transport_sequence = self.transport_sequence.wrapping_add(1);
                (rtp::rtx_packet(&header, self.ext, abs_send_time, self.transport_sequence, original.sequence, &original.payload), None, "rtx")
            } else if let Some(q) = self.queue.pop_front() {
                let header = Header { marker: q.marker, payload_type: self.payload_type, sequence: q.sequence, timestamp: q.timestamp, ssrc: self.video_ssrc };
                self.transport_sequence = self.transport_sequence.wrapping_add(1);
                let packet = rtp::video_packet(&header, self.ext, abs_send_time, self.transport_sequence, &q.payload);
                (packet, Some(Sent { sequence: q.sequence, timestamp: q.timestamp, marker: q.marker, payload: q.payload }), "video")
            } else {
                break;
            };
            self.tokens -= 1000;
            let Some(sealed) = self.transport.as_mut().and_then(|t| t.seal_rtp(&packet)) else {
                stats.send_errors += 1;
                continue;
            };
            match self.socket.try_send(&sealed) {
                Ok(_) => self.last_sent = now,
                // It leaves first on the next turn; nothing overtakes it.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.blocked = Some(sealed),
                Err(_) => {
                    stats.send_errors += 1;
                    continue;
                }
            }
            stats.packets += 1;
            stats.bytes += packet.len() as u64 + crypto::OVERHEAD as u64;
            match sent {
                Some(sent) => {
                    if let Some(d) = dump {
                        d.packet(kind, sent.sequence, sent.timestamp, sent.marker, packet.len());
                    }
                    self.video_packets = self.video_packets.wrapping_add(1);
                    self.video_octets = self.video_octets.wrapping_add(sent.payload.len() as u32);
                    self.budget.sent(now, sent.payload.len());
                    let end_of_frame = sent.marker;
                    if self.rtx {
                        self.history.push(sent);
                    }
                    // One wall clock for audio and video reports, so receivers can align them.
                    if end_of_frame && self.video_report_at.map_or(true, |t| now.saturating_duration_since(t) >= VIDEO_REPORT_EVERY) {
                        self.video_report_at = Some(now);
                        self.send_report(self.video_ssrc, self.video_timestamp(now), self.video_packets, self.video_octets, now);
                    }
                }
                None => {
                    stats.rtx += 1;
                    if let Some(d) = dump {
                        d.packet(kind, self.rtx_sequence, 0, false, packet.len());
                    }
                }
            }
        }
        Pumped::Idle
    }

    fn has_queued(&self) -> bool {
        !self.queue.is_empty() || !self.retransmit.is_empty() || self.blocked.is_some()
    }
}

struct Worker {
    deps: Deps,
    context: Context,
    params: Params,
    shared: Arc<Shared>,
    phase: Phase,
    finished: bool,

    // The main gateway's side.
    own_key: StreamKey,
    /// The key STREAM_CREATE gave.
    stream_key: Option<String>,
    /// Opcode 18 was accepted by the main gateway connection.
    create_sent: bool,
    create_retry: Option<Instant>,
    /// Discord deleted the stream itself: no opcode 19 is owed.
    deleted: bool,
    /// The end of the wait for the gateway's answer, or for a new media server.
    deadline: Option<Instant>,
    /// After an outage the stream has to be announced again before it is connected to.
    need_create: bool,
    rtc_server_id: Option<u64>,
    rtc_channel_id: Option<u64>,
    endpoint: Option<String>,
    token: Option<String>,
    viewer_ids: Vec<u64>,
    /// The stream gateway session was in service at least once.
    established: bool,

    // The stream gateway's side.
    io: Io,
    link: Link,
    seq_ack: i64,
    heartbeat_every: Option<Duration>,
    heartbeat_at: Option<Instant>,
    heartbeats_unanswered: u32,
    handshake_deadline: Option<Instant>,
    reconnect_at: Option<Instant>,
    failed_reconnects: VecDeque<Instant>,
    /// The users the stream server says are connected to it (opcodes 11 and 13).
    connected_users: HashSet<u64>,
    ready: Option<wire::Ready>,
    mode: Option<Mode>,

    // Media.
    media: Option<Media>,
    dave: Dave,
    sending: Sending,
    key_generation: u64,
    keyframe_requested_at: Option<Instant>,
    stats: Stats,
    stats_at: Instant,
    seen: Seen,
    dump: Option<Dump>,
}

impl Worker {
    fn new(deps: Deps, context: Context, params: Params, shared: Arc<Shared>) -> Self {
        let own_key = StreamKey::new(context.guild_id.as_deref(), &context.channel_id, &context.user_id.to_string());
        let dave = Dave::new(context.user_id, Vec::new(), deps.tuning.switches.require_dave);
        let dump = deps.tuning.switches.dump.as_ref().and_then(Dump::open);
        Self {
            deps,
            context,
            params,
            shared,
            phase: Phase::Armed,
            finished: false,
            own_key,
            stream_key: None,
            create_sent: false,
            create_retry: None,
            deleted: false,
            deadline: None,
            need_create: true,
            rtc_server_id: None,
            rtc_channel_id: None,
            endpoint: None,
            token: None,
            viewer_ids: Vec::new(),
            established: false,
            io: Io { connecting: None, discovering: None, reader: None, writer: None, writer_task: None, socket: None },
            link: Link::Down,
            seq_ack: -1,
            heartbeat_every: None,
            heartbeat_at: None,
            heartbeats_unanswered: 0,
            handshake_deadline: None,
            reconnect_at: None,
            failed_reconnects: VecDeque::new(),
            connected_users: HashSet::new(),
            ready: None,
            mode: None,
            media: None,
            dave,
            sending: Sending::Nothing,
            key_generation: 0,
            keyframe_requested_at: None,
            stats: Stats::default(),
            stats_at: Instant::now(),
            seen: Seen::default(),
            dump,
        }
    }

    fn tuning(&self) -> &Tuning {
        &self.deps.tuning
    }

    fn started(&mut self) {
        info!(
            "[GO LIVE] Stream armed for {} (channel {}, voice session {}...): {}x{} at {} FPS, audio {}; waiting for the capture to have a picture. Switches: {}",
            self.own_key,
            self.context.channel_id,
            short(&self.context.session_id),
            self.params.width,
            self.params.height,
            self.params.fps,
            if self.params.audio { "on" } else { "off" },
            self.tuning().switches.describe()
        );
        if self.shared.capture_ready.load(Ordering::Acquire) {
            self.create_stream();
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.shared.update(|s| s.phase = phase);
        }
    }

    // ---- The main gateway: asking for the stream ---------------------------------------

    fn create_stream(&mut self) {
        if self.phase != Phase::Armed {
            return;
        }
        info!("[GO LIVE] The capture has a picture");
        self.set_phase(Phase::Creating);
        self.deadline = Some(Instant::now() + self.tuning().create_timeout);
        self.send_create();
    }

    fn send_create(&mut self) {
        let payload = signal::create(&self.own_key, self.tuning().switches.region);
        if (self.deps.gateway_send)(payload.clone()) {
            info!("[GO LIVE] Requesting stream: type={} (op 18 sent: {})", payload["d"]["type"].as_str().unwrap_or(""), payload["d"]);
            self.create_sent = true;
            self.create_retry = None;
            let key = self.own_key.to_string();
            self.shared.update(|s| s.stream_key = key);
        } else {
            warn!("[GO LIVE] The main gateway is not connected right now: the stream request (op 18) will be tried again in a second");
            self.create_retry = Some(Instant::now() + Duration::from_secs(1));
        }
    }

    fn on_event(&mut self, event: signal::Event) {
        let key = event.stream_key().to_string();
        // Other people's streams are for milestone 2 (watching).
        if !signal::is_stream_of(&key, self.context.user_id) {
            return;
        }
        if !self.create_sent {
            info!("[GO LIVE] Event for a stream of this account ({key}) before one was requested here: ignored");
            return;
        }
        if self.stream_key.as_deref().is_some_and(|ours| ours != key) {
            info!("[GO LIVE] Event for another stream of this account ({key}): ignored");
            return;
        }
        match event {
            signal::Event::Create { rtc_server_id, rtc_channel_id, region, viewer_ids, paused, .. } => {
                info!(
                    "[GO LIVE] STREAM_CREATE: key={key} rtc_server_id={rtc_server_id} rtc_channel_id={} region={region} viewers={} paused={paused}",
                    rtc_channel_id.map_or("none".to_string(), |c| c.to_string()),
                    viewer_ids.len()
                );
                if self.stream_key.is_none() && key != self.own_key.to_string() {
                    info!("[GO LIVE] (The gateway's key differs from the one built here, {}; the gateway's is used)", self.own_key);
                }
                let moved = self.rtc_server_id.is_some_and(|old| old != rtc_server_id);
                self.stream_key = Some(key.clone());
                self.rtc_server_id = Some(rtc_server_id);
                self.rtc_channel_id = rtc_channel_id;
                self.need_create = false;
                self.dave.set_group_candidates(dave::group_candidates(self.tuning().switches.group, rtc_server_id, rtc_channel_id.unwrap_or(rtc_server_id)));
                self.shared.update(|s| s.stream_key = key.clone());
                self.set_viewers(viewer_ids);
                if paused {
                    self.unpause(&key);
                }
                if moved && self.link != Link::Down {
                    info!("[GO LIVE] The stream moved to another RTC server: connecting there");
                    self.leave_server();
                    self.set_phase(Phase::Creating);
                }
                self.connect_when_ready();
            }
            signal::Event::ServerUpdate { endpoint: None, .. } => {
                warn!("[GO LIVE] STREAM_SERVER_UPDATE: endpoint null: the stream's media server went away; waiting for another");
                self.endpoint = None;
                self.token = None;
                self.await_server();
            }
            signal::Event::ServerUpdate { endpoint: Some(endpoint), token, .. } => {
                let Some(token) = token else {
                    warn!("[GO LIVE] STREAM_SERVER_UPDATE: endpoint={endpoint} without a token: ignored");
                    return;
                };
                info!("[GO LIVE] STREAM_SERVER_UPDATE: endpoint={endpoint} (token of {} characters)", token.len());
                let same = self.endpoint.as_deref() == Some(endpoint.as_str()) && self.token.as_deref() == Some(token.as_str());
                self.endpoint = Some(endpoint);
                self.token = Some(token);
                if self.link != Link::Down {
                    if same {
                        return;
                    }
                    info!("[GO LIVE] Another media server or token: leaving the old connection");
                    self.leave_server();
                    self.set_phase(Phase::Creating);
                    self.deadline = Some(Instant::now() + self.tuning().unavailable_timeout);
                }
                self.connect_when_ready();
            }
            signal::Event::Update { viewer_ids, paused, region, .. } => {
                info!("[GO LIVE] STREAM_UPDATE: viewers={:?} paused={paused} region={region}", viewer_ids);
                self.set_viewers(viewer_ids);
                if paused {
                    self.unpause(&key);
                }
            }
            signal::Event::Delete { reason, unavailable: true, .. } => {
                warn!("[GO LIVE] STREAM_DELETE: unavailable (reason={reason}): an outage at Discord; waiting for the stream to come back");
                self.endpoint = None;
                self.token = None;
                self.rtc_server_id = None;
                self.need_create = true;
                self.await_server();
            }
            signal::Event::Delete { reason, .. } => {
                info!("[GO LIVE] STREAM_DELETE: reason={reason}");
                self.deleted = true;
                let failure = if self.established { Failure::Ended(reason) } else { Failure::Refused(reason) };
                self.fail(failure);
            }
        }
    }

    fn unpause(&mut self, key: &str) {
        info!("[GO LIVE] The stream is paused: resuming it (op 22, paused=false)");
        (self.deps.gateway_send)(signal::set_paused(key, false));
    }

    fn set_viewers(&mut self, viewer_ids: Vec<u64>) {
        if self.viewer_ids != viewer_ids {
            let count = viewer_ids.len();
            self.viewer_ids = viewer_ids;
            self.shared.update(|s| s.viewers = count);
        }
    }

    /// The media server is gone for now: drop the connection and wait for the next one.
    fn await_server(&mut self) {
        self.leave_server();
        self.set_phase(Phase::Creating);
        // (Before the first connection, the wait for the gateway's answer simply goes on.)
        if self.established || self.deadline.is_none() {
            self.deadline = Some(Instant::now() + self.tuning().unavailable_timeout);
        }
    }

    /// Connects once the stream was announced and its server and token are known.
    fn connect_when_ready(&mut self) {
        if self.link != Link::Down || self.reconnect_at.is_some() || self.need_create || self.rtc_server_id.is_none() || self.endpoint.is_none() || self.token.is_none() {
            return;
        }
        self.deadline = None;
        self.open_socket(false);
    }

    // ---- The stream gateway socket -------------------------------------------------------

    fn open_socket(&mut self, resume: bool) {
        self.reconnect_at = None;
        let Some(endpoint) = self.endpoint.clone() else { return };
        let Some(url) = (self.deps.url_for)(&endpoint) else {
            error!("[GO LIVE] The endpoint {endpoint:?} is not a plain host name: refused");
            self.fail(Failure::Lost("endpoint".into()));
            return;
        };
        info!("[GO LIVE] Connecting to the stream gateway: {url} ({})", if resume { "to resume the session" } else { "new session" });
        self.close_socket();
        self.link = Link::Opening { resume };
        self.handshake_deadline = Some(Instant::now() + self.tuning().handshake_timeout);
        self.io.connecting = Some(Box::pin(async move { tokio_tungstenite::connect_async(url.as_str()).await.map(|(ws, _)| ws).map_err(|e| e.to_string()) }));
        self.set_phase(if resume { Phase::Reconnecting } else { Phase::Connecting });
    }

    fn on_connected(&mut self, result: Result<Ws, String>) {
        self.io.connecting = None;
        let Link::Opening { resume } = self.link else { return };
        let ws = match result {
            Ok(ws) => ws,
            Err(e) => {
                warn!("[GO LIVE] The stream gateway could not be reached: {e}");
                self.socket_lost("unreachable", CloseAction::Resume);
                return;
            }
        };
        let (mut sink, reader) = ws.split();
        let (writer, mut outgoing) = mpsc::unbounded_channel::<Message>();
        self.io.writer_task = Some(tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
        }));
        self.io.reader = Some(reader);
        self.io.writer = Some(writer);
        self.link = Link::Handshake { resume };
        self.heartbeat_every = None;
        self.heartbeat_at = None;
        self.heartbeats_unanswered = 0;
        let (Some(server), Some(token)) = (self.rtc_server_id, self.token.clone()) else { return };
        let channel = (self.tuning().switches.gateway_version >= 9).then_some(self.rtc_channel_id).flatten();
        // Sent at once: the server's Hello need not be waited for.
        let payload = if resume {
            wire::resume(server, &self.context.session_id, &token, self.seq_ack, channel)
        } else {
            self.seq_ack = -1;
            self.connected_users.clear();
            wire::identify(server, self.context.user_id, &self.context.session_id, &token, channel)
        };
        info!("[GO LIVE] Stream gateway connected; {} sent: {}", if resume { "Resume (op 7)" } else { "Identify (op 0)" }, redacted(&payload["d"]));
        self.send_json(payload);
    }

    fn send_json(&mut self, payload: Value) {
        if let Some(writer) = &self.io.writer {
            let _ = writer.send(Message::Text(payload.to_string().into()));
        }
    }

    fn send_all(&mut self, messages: Vec<Out>) {
        let Some(writer) = &self.io.writer else { return };
        for message in messages {
            let _ = writer.send(match message {
                Out::Binary(bytes) => Message::Binary(bytes.into()),
                Out::Json(payload) => Message::Text(payload.to_string().into()),
            });
        }
    }

    /// Drops the socket without a word (it is gone, or is being replaced).
    fn close_socket(&mut self) {
        self.io.connecting = None;
        self.io.discovering = None;
        self.io.reader = None;
        self.io.writer = None;
        if let Some(task) = self.io.writer_task.take() {
            task.abort();
        }
        self.heartbeat_at = None;
        self.handshake_deadline = None;
        self.link = Link::Down;
    }

    /// Forgets the media session: socket, SSRCs, cipher and the DAVE session with them.
    fn drop_media(&mut self) {
        self.io.socket = None;
        self.io.discovering = None;
        self.media = None;
        self.ready = None;
        self.dave.forget_connection();
        self.refresh_sending();
    }

    /// Leaves the media server altogether (it changed, or went away).
    fn leave_server(&mut self) {
        self.close_socket();
        self.reconnect_at = None;
        self.drop_media();
    }

    /// The socket is gone, or an attempt at one failed. `action`: what the close code (or
    /// the kind of failure) allows.
    fn socket_lost(&mut self, why: &str, action: CloseAction) {
        let was_up = self.link == Link::Up;
        self.close_socket();
        if action == CloseAction::Fatal {
            self.fail(Failure::Lost(why.to_string()));
            return;
        }
        let now = Instant::now();
        let window = self.tuning().retry_window;
        while self.failed_reconnects.front().is_some_and(|t| now.duration_since(*t) > window) {
            self.failed_reconnects.pop_front();
        }
        if !was_up {
            self.failed_reconnects.push_back(now);
            if self.failed_reconnects.len() >= self.tuning().retry_delays.len() {
                error!("[GO LIVE] {} reconnects to the stream gateway failed within {} s: giving up", self.failed_reconnects.len(), window.as_secs());
                self.fail(Failure::Lost(why.to_string()));
                return;
            }
        }
        // A session that was in service is resumed; a failed attempt, or a session the
        // server ended, starts over with a new Identify.
        let resume = was_up && action == CloseAction::Resume && self.media.as_ref().is_some_and(|m| m.transport.is_some());
        if !resume {
            self.drop_media();
        }
        let delay = self.tuning().retry_delays.get(self.failed_reconnects.len()).copied().unwrap_or(Duration::from_secs(15));
        info!("[GO LIVE] Stream gateway connection lost ({why}): {} in {:.1} s", if resume { "resuming the session" } else { "starting a new session" }, delay.as_secs_f64());
        self.link = Link::Opening { resume };
        self.reconnect_at = Some(now + delay);
        self.set_phase(if resume { Phase::Reconnecting } else { Phase::Connecting });
    }

    fn on_message(&mut self, message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) {
        match message {
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()),
            Some(Ok(Message::Binary(data))) => self.on_binary(&data),
            Some(Ok(Message::Close(frame))) => {
                let code = frame.as_ref().map_or(1005, |f| u16::from(f.code));
                let reason = frame.as_ref().map(|f| f.reason.to_string()).unwrap_or_default();
                warn!("[GO LIVE] The stream gateway closed the connection: code {code} ({}){}{}", wire::close_name(code), if reason.is_empty() { "" } else { ": " }, reason);
                self.socket_lost(&code.to_string(), wire::close_action(code));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                warn!("[GO LIVE] The stream gateway connection broke: {e}");
                self.socket_lost("connection lost", CloseAction::Resume);
            }
            None => {
                warn!("[GO LIVE] The stream gateway connection ended without a close frame");
                self.socket_lost("connection lost", CloseAction::Resume);
            }
        }
    }

    fn on_text(&mut self, text: &str) {
        let Some(message) = wire::parse_text(text) else {
            warn!("[GO LIVE] Unreadable message from the stream gateway ({} bytes)", text.len());
            return;
        };
        if let Some(seq) = message.seq {
            self.seq_ack = seq;
        }
        let d = &message.d;
        match message.op {
            wire::OP_HELLO => {
                let interval = Duration::from_secs_f64((d["heartbeat_interval"].as_f64().unwrap_or(13_750.0) / 1000.0).clamp(1.0, 120.0));
                info!("[GO LIVE] Hello (op 8): heartbeat every {} ms, protocol version {}", interval.as_millis(), d["v"]);
                self.heartbeat_every = Some(interval);
                // The first one after a random part of the interval.
                self.heartbeat_at = Some(Instant::now() + interval.mul_f64(rand::random::<f64>()));
                self.heartbeats_unanswered = 0;
            }
            wire::OP_READY => self.on_ready(d),
            wire::OP_SESSION_DESCRIPTION => self.on_session_description(d),
            wire::OP_HEARTBEAT_ACK => self.heartbeats_unanswered = 0,
            wire::OP_HEARTBEAT => self.send_heartbeat(),
            wire::OP_RESUMED => self.on_resumed(),
            wire::OP_CLIENTS_CONNECT => {
                let users: Vec<u64> = d["user_ids"].as_array().map(|ids| ids.iter().filter_map(signal::snowflake).collect()).unwrap_or_default();
                let mut new = false;
                for user in users {
                    if self.connected_users.insert(user) && user != self.context.user_id {
                        info!("[GO LIVE] Viewer connected: user {user}");
                        new = true;
                    }
                }
                // (Without DAVE no key change announces a new viewer: it needs a keyframe.)
                if new && self.sending != Sending::Nothing {
                    self.request_keyframe(Instant::now(), false);
                }
            }
            wire::OP_CLIENT_DISCONNECT => {
                if let Some(user) = signal::snowflake(&d["user_id"]) {
                    if self.connected_users.remove(&user) {
                        info!("[GO LIVE] Viewer disconnected: user {user}");
                    }
                }
            }
            wire::OP_SESSION_UPDATE => {
                info!("[GO LIVE] Session Update (op 14): {d}");
                if d["video_codec"].as_str().is_some_and(|codec| !codec.eq_ignore_ascii_case("H264")) {
                    error!("[GO LIVE] The server switched the video codec to {}: this client sends H.264 only", d["video_codec"]);
                    self.fail(Failure::Lost("codec".into()));
                }
            }
            wire::OP_MEDIA_SINK_WANTS => {
                let wants = d.to_string();
                if self.seen.sink_wants != wants {
                    info!("[GO LIVE] Media Sink Wants (op 15): {wants}");
                    self.seen.sink_wants = wants;
                }
            }
            dave::OP_PREPARE_TRANSITION => {
                let out = self.dave.on_prepare_transition(d["transition_id"].as_u64().unwrap_or(0) as u16, d["protocol_version"].as_u64().unwrap_or(0) as u16);
                self.send_all(out);
                self.refresh_sending();
            }
            dave::OP_EXECUTE_TRANSITION => {
                let out = self.dave.on_execute_transition(d["transition_id"].as_u64().unwrap_or(0) as u16);
                self.send_all(out);
                self.refresh_sending();
            }
            dave::OP_PREPARE_EPOCH => {
                let out = self.dave.on_prepare_epoch(d["epoch"].as_u64().unwrap_or(0), d["protocol_version"].as_u64().unwrap_or(0) as u16);
                self.send_all(out);
                self.refresh_sending();
            }
            // Speaking of others, and two opcodes the server sends on streams by mistake.
            wire::OP_SPEAKING | 18 | 20 => {}
            other => info!("[GO LIVE] Stream gateway op {other} not used: {}", clipped(&d.to_string())),
        }
    }

    fn on_binary(&mut self, frame: &[u8]) {
        let Some(message) = wire::parse_binary(frame) else {
            warn!("[GO LIVE] Binary message of {} bytes from the stream gateway: too short", frame.len());
            return;
        };
        self.seq_ack = message.seq as i64;
        let out = match message.op {
            dave::OP_EXTERNAL_SENDER => self.dave.on_external_sender(message.payload),
            dave::OP_PROPOSALS => {
                let recognised = self.recognised();
                self.dave.on_proposals(message.payload, recognised.as_deref())
            }
            dave::OP_ANNOUNCE_COMMIT => match wire::split_transition(message.payload) {
                Some((transition, commit)) => self.dave.on_announce_commit(transition, commit),
                None => Vec::new(),
            },
            dave::OP_WELCOME => match wire::split_transition(message.payload) {
                Some((transition, welcome)) => {
                    let recognised = self.recognised();
                    self.dave.on_welcome(transition, welcome, recognised.as_deref())
                }
                None => Vec::new(),
            },
            other => {
                info!("[GO LIVE] Binary stream gateway op {other} not used ({} bytes)", message.payload.len());
                Vec::new()
            }
        };
        self.send_all(out);
        self.refresh_sending();
    }

    /// Who may be in the stream's DAVE group: this account, the users the stream server
    /// and the main gateway name as viewers, and everybody in our voice channel (a viewer
    /// has to be there, so a real one is never refused).
    fn recognised(&self) -> Option<Vec<u64>> {
        if !self.tuning().switches.dave_check {
            return None;
        }
        let mut users = vec![self.context.user_id];
        users.extend(self.connected_users.iter().copied());
        users.extend(self.viewer_ids.iter().copied());
        users.extend((self.deps.recognised_users)());
        users.sort_unstable();
        users.dedup();
        Some(users)
    }

    fn send_heartbeat(&mut self) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        self.send_json(wire::heartbeat(now, self.seq_ack));
    }

    fn on_ready(&mut self, d: &Value) {
        if self.link != (Link::Handshake { resume: false }) {
            info!("[GO LIVE] Ready (op 2) outside a handshake: ignored");
            return;
        }
        let Some(ready) = wire::parse_ready(d) else {
            error!("[GO LIVE] Ready (op 2) without an SSRC, address or port: {}", clipped(&d.to_string()));
            self.fail(Failure::Lost("ready".into()));
            return;
        };
        let Some(mode) = Mode::pick(ready.modes.iter().map(String::as_str)) else {
            error!("[GO LIVE] The server offers no encryption mode this client knows: {:?}", ready.modes);
            self.fail(Failure::Lost("encryption mode".into()));
            return;
        };
        info!(
            "[GO LIVE] Ready: audio ssrc={} video ssrc={} rtx ssrc={}, media server {}:{}, modes={:?} (using {}), experiments={:?}",
            ready.audio_ssrc,
            ready.video_ssrc,
            ready.rtx_ssrc,
            ready.ip,
            ready.port,
            ready.modes,
            mode.name(),
            ready.experiments
        );
        if !ready.ssrcs_from_streams {
            error!("[GO LIVE] Ready names no video stream: using audio + 1 and audio + 2 as SSRCs, the old convention ({})", clipped(&d["streams"].to_string()));
        }
        let Ok(ip) = ready.ip.parse::<IpAddr>() else {
            error!("[GO LIVE] The media server address {:?} is not an IP address", ready.ip);
            self.fail(Failure::Lost("server address".into()));
            return;
        };
        let server = SocketAddr::new(ip, ready.port);
        let (ssrc, tries, wait) = (ready.audio_ssrc, self.tuning().discovery_tries, self.tuning().discovery_wait);
        self.mode = Some(mode);
        self.ready = Some(ready);
        self.io.discovering = Some(Box::pin(async move {
            let socket = Arc::new(udp::open(server).await.map_err(|e| e.to_string())?);
            let found = udp::discover(&socket, ssrc, tries, wait).await;
            Ok((socket, found))
        }));
    }

    fn on_discovered(&mut self, result: Discovery) {
        self.io.discovering = None;
        let (Link::Handshake { resume: false }, Some(ready), Some(mode)) = (self.link, self.ready.clone(), self.mode) else { return };
        let (socket, found) = match result {
            Ok(found) => found,
            Err(e) => {
                error!("[GO LIVE] No UDP socket for the media server: {e}");
                self.socket_lost("udp", CloseAction::Identify);
                return;
            }
        };
        let (address, port) = match found {
            Some((address, port)) => {
                info!("[GO LIVE] IP discovery: the media server sees this client at {address}:{port}");
                (address, port)
            }
            None => {
                let local = socket.local_addr().map(|a| (a.ip().to_string(), a.port())).unwrap_or(("0.0.0.0".into(), 0));
                warn!(
                    "[GO LIVE] IP discovery: no answer to {} requests; telling the server the local address {}:{} (it only matters for receiving)",
                    self.tuning().discovery_tries,
                    local.0,
                    local.1
                );
                local
            }
        };
        self.io.socket = Some(Arc::clone(&socket));
        self.media = Some(Media::new(socket, &ready, &self.tuning().switches, Instant::now()));
        let switches = &self.tuning().switches;
        let payload = wire::select_protocol(&address, port, mode.name(), switches.payload_type, switches.rtx, &wire::connection_id());
        info!("[GO LIVE] Select Protocol (op 1) sent: {}", payload["d"]);
        self.send_json(payload);
    }

    fn on_session_description(&mut self, d: &Value) {
        if self.link != (Link::Handshake { resume: false }) || self.media.is_none() {
            info!("[GO LIVE] Session Description (op 4) outside a handshake: ignored");
            return;
        }
        let description = match wire::parse_session_description(d) {
            Ok(description) => description,
            Err(e) => {
                error!("[GO LIVE] Session Description (op 4) cannot be used: {e}");
                self.fail(Failure::Lost("session description".into()));
                return;
            }
        };
        info!(
            "[GO LIVE] Session Description: video_codec={} audio_codec={} mode={} dave_protocol_version={} media_session_id={}",
            description.video_codec, description.audio_codec, description.mode, description.dave_protocol_version, description.media_session_id
        );
        if !description.video_codec.eq_ignore_ascii_case("H264") {
            error!("[GO LIVE] The server chose the video codec {:?}: this client sends H.264 only", description.video_codec);
            self.fail(Failure::Lost("codec".into()));
            return;
        }
        let Some(mode) = Mode::from_name(&description.mode) else {
            error!("[GO LIVE] The server chose the encryption mode {:?}, which this client does not know", description.mode);
            self.fail(Failure::Lost("encryption mode".into()));
            return;
        };
        let Some(media) = self.media.as_mut() else { return };
        media.transport = Some(Transport::new(mode, &description.secret_key));
        media.opening_silence = OPENING_SILENCE;
        media.silence_at = Some(Instant::now());
        let (audio_ssrc, video_ssrc, rtx_ssrc) = (media.audio_ssrc, media.video_ssrc, media.rtx_ssrc);
        self.link = Link::Up;
        self.handshake_deadline = None;
        self.established = true;
        // Speaking must be sent once before any media, even with no flag set.
        self.send_json(wire::speaking(0, audio_ssrc));
        let switches = self.tuning().switches.clone();
        let out = self.dave.on_session_description(description.dave_protocol_version);
        self.send_all(out);
        let stream = wire::VideoStream {
            ssrc: video_ssrc,
            rtx_ssrc: if switches.rtx { rtx_ssrc } else { 0 },
            kind: switches.stream_type.to_string(),
            max_bitrate: self.tuning().max_bitrate,
            max_framerate: self.params.fps,
            width: self.params.width,
            height: self.params.height,
        };
        let payload = wire::video(audio_ssrc, Some(&stream));
        info!(
            "[GO LIVE] Video announced (op 12): {}x{}@{}, max {:.1} Mbit/s: {}",
            stream.width,
            stream.height,
            stream.max_framerate,
            stream.max_bitrate as f64 / 1e6,
            payload["d"]
        );
        self.send_json(payload);
        self.set_phase(Phase::Waiting);
        let e2ee = self.dave.end_to_end();
        self.shared.update(|s| s.e2ee = e2ee);
        if self.dave.sending() == Sending::Nothing {
            info!("[GO LIVE] Live, waiting for viewers (nothing is sent until somebody watches)");
        }
        self.refresh_sending();
    }

    fn on_resumed(&mut self) {
        if self.link != (Link::Handshake { resume: true }) {
            return;
        }
        info!("[GO LIVE] Resumed (op 9): the session goes on with the same SSRCs and keys");
        self.link = Link::Up;
        self.handshake_deadline = None;
        self.set_phase(if self.sending == Sending::Nothing { Phase::Waiting } else { Phase::Live });
    }

    // ---- What may be sent ----------------------------------------------------------------

    /// Follows DAVE after anything that may have changed it: starts and stops the flow of
    /// frames, and asks for a keyframe when the key changed.
    fn refresh_sending(&mut self) {
        let ready = self.media.as_ref().is_some_and(|m| m.transport.is_some());
        let now_sending = if ready { self.dave.sending() } else { Sending::Nothing };
        let generation = self.dave.key_generation();
        let now = Instant::now();
        if now_sending != Sending::Nothing && self.sending == Sending::Nothing {
            self.sending = now_sending;
            if let Some(media) = self.media.as_mut() {
                media.waiting_keyframe = true;
            }
            self.shared.wanted.store(true, Ordering::Relaxed);
            info!(
                "[GO LIVE] Sending video from the next keyframe: {}",
                match now_sending {
                    Sending::Plain => "without end-to-end encryption (protocol version 0)".to_string(),
                    _ => format!("end-to-end encrypted, DAVE group of {} at epoch {}", self.dave.members().len(), self.dave.epoch().unwrap_or(0)),
                }
            );
            self.request_keyframe(now, true);
            (self.deps.wake_sender)();
            if self.phase == Phase::Waiting {
                self.set_phase(Phase::Live);
            }
        } else if now_sending == Sending::Nothing && self.sending != Sending::Nothing {
            self.sending = Sending::Nothing;
            self.shared.wanted.store(false, Ordering::Relaxed);
            if let Some(media) = self.media.as_mut() {
                media.queue.clear();
                media.retransmit.clear();
                media.encoder = None;
            }
            if ready {
                info!("[GO LIVE] Live, waiting for viewers (nothing is sent until somebody watches)");
            }
            if self.phase == Phase::Live {
                self.set_phase(Phase::Waiting);
            }
        } else if now_sending != Sending::Nothing && generation != self.key_generation {
            // A new key while sending (somebody joined or left): frames are not held back,
            // those already watching keep decoding; a new viewer starts at the keyframe.
            self.sending = now_sending;
            self.request_keyframe(now, true);
        }
        self.key_generation = generation;
        let e2ee = self.dave.end_to_end();
        self.shared.update(|s| s.e2ee = e2ee);
    }

    fn request_keyframe(&mut self, now: Instant, urgent: bool) {
        if !urgent && self.keyframe_requested_at.is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_REQUEST_EVERY) {
            return;
        }
        self.keyframe_requested_at = Some(now);
        (self.deps.request_keyframe)();
        (self.deps.wake_sender)();
    }

    // ---- Media -----------------------------------------------------------------------------

    fn on_frame(&mut self, frame: Frame) {
        let now = Instant::now();
        self.stats.frames_in += 1;
        let Some(media) = self.media.as_mut() else {
            self.stats.dropped += 1;
            return;
        };
        if self.sending == Sending::Nothing {
            self.stats.dropped += 1;
            return;
        }
        if frame.epoch != media.frame_epoch {
            // Access units were lost on the way here: what follows needs a keyframe.
            media.frame_epoch = frame.epoch;
            media.waiting_keyframe = true;
        }
        if now.saturating_duration_since(frame.at) > STALE_INPUT {
            media.waiting_keyframe = true;
            self.stats.dropped += 1;
            self.request_keyframe(now, false);
            return;
        }
        let prepared = match media.preparer.prepare(&frame.data) {
            Ok(prepared) => prepared,
            Err(Skip::NoPicture) => return,
            Err(skip) => {
                if !self.seen.bad_unit {
                    self.seen.bad_unit = true;
                    warn!(
                        "[GO LIVE] An access unit cannot be sent ({}; NAL types {:?}): dropped, and a keyframe is asked for. Further ones are only counted.",
                        match skip {
                            Skip::NotAnnexB => "no start code in it",
                            Skip::NoParameterSets => "a keyframe without SPS and PPS, and none seen before",
                            Skip::BadSps => "its SPS cannot be read",
                            Skip::NoPicture => "no picture in it",
                        },
                        super::h264::nal_types(&frame.data)
                    );
                }
                media.waiting_keyframe = true;
                self.stats.dropped += 1;
                self.request_keyframe(now, false);
                return;
            }
        };
        if media.waiting_keyframe && !prepared.keyframe {
            self.stats.dropped += 1;
            if self.keyframe_requested_at.map_or(true, |t| now.saturating_duration_since(t) >= KEYFRAME_RETRY_EVERY) {
                self.request_keyframe(now, false);
            }
            return;
        }
        let Some(encrypted) = self.dave.encrypt_video(&prepared.data) else {
            media.waiting_keyframe = true;
            self.stats.dropped += 1;
            self.request_keyframe(now, false);
            return;
        };
        media.waiting_keyframe = false;
        if let Some(dump) = self.dump.as_mut() {
            let _ = dump.prepared.write_all(&prepared.data);
        }
        let mut timestamp = media.video_timestamp(frame.at);
        if let Some(last) = media.last_video_timestamp {
            // Forced to increase, whatever the clock did.
            if (timestamp.wrapping_sub(last) as i32) <= 0 {
                timestamp = last.wrapping_add(1);
            }
        }
        media.last_video_timestamp = Some(timestamp);
        let payloads = rtp::packetize_h264(&encrypted, rtp::MAX_PAYLOAD);
        let count = payloads.len();
        for (i, payload) in payloads.into_iter().enumerate() {
            media.video_sequence = media.video_sequence.wrapping_add(1);
            media.queue.push_back(Queued { sequence: media.video_sequence, timestamp, marker: i + 1 == count, payload, queued_at: now });
        }
        self.stats.frames_sent += 1;
        if prepared.keyframe {
            self.stats.keyframes += 1;
            if !self.seen.keyframe {
                self.seen.keyframe = true;
                info!(
                    "[GO LIVE] First keyframe sent: {} bytes in {count} packets, SPS normalised: {}{}, DAVE: {}",
                    encrypted.len(),
                    match prepared.sps_rewritten {
                        Some(true) => "yes",
                        Some(false) => "not needed",
                        None => "taken from an earlier keyframe",
                    },
                    if prepared.sets_added { " (parameter sets added from an earlier keyframe)" } else { "" },
                    if self.sending == Sending::Encrypted { "encrypted" } else { "not in force" }
                );
            }
        }
        self.pump(now);
    }

    fn pump(&mut self, now: Instant) {
        let stale = self.tuning().stale_frame;
        let Some(media) = self.media.as_mut() else { return };
        if let Pumped::Stale(packets) = media.pump(now, stale, &mut self.stats, &mut self.dump) {
            warn!("[GO LIVE] {packets} video packets waited more than {} ms to be sent: dropped, starting again from a keyframe", stale.as_millis());
            self.stats.dropped += 1;
            self.request_keyframe(now, true);
        }
    }

    fn on_audio(&mut self, chunk: Audio) {
        let now = Instant::now();
        let Some(media) = self.media.as_mut() else { return };
        if self.sending == Sending::Nothing || !self.params.audio {
            media.encoder = None;
            return;
        }
        if now.saturating_duration_since(chunk.at) > STALE_INPUT {
            media.encoder = None;
            return;
        }
        let chunk_time = Duration::from_micros(chunk.pcm.len() as u64 * 1_000_000 / chunk.rate.max(1) as u64);
        if chunk.pcm.iter().all(|s| *s == 0) {
            if media.quiet_for < QUIET_AFTER && media.quiet_for + chunk_time >= QUIET_AFTER {
                media.trailing_silence = TRAILING_SILENCE;
            }
            media.quiet_for += chunk_time;
        } else {
            media.quiet_for = Duration::ZERO;
            media.trailing_silence = 0;
        }
        if media.encoder.is_none() {
            media.encoder = StreamAudioEncoder::new();
        }
        let Some(packets) = media.encoder.as_mut().map(|encoder| encoder.push(&chunk.pcm, chunk.rate)) else { return };
        for packet in packets {
            if media.quiet_for >= QUIET_AFTER {
                // Nothing plays: a few silence packets tell receivers so, then nothing.
                if media.trailing_silence > 0 && self.seen.audio {
                    media.trailing_silence -= 1;
                    media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump);
                }
                continue;
            }
            let Some(encrypted) = self.dave.encrypt_opus(&packet) else { continue };
            if !self.seen.audio {
                self.seen.audio = true;
                // Soundshare: stream audio, without the speaking ring.
                if let Some(writer) = &self.io.writer {
                    let _ = writer.send(Message::Text(wire::speaking(wire::SPEAKING_SOUNDSHARE, media.audio_ssrc).to_string().into()));
                }
                info!("[GO LIVE] Stream audio: speaking=2 sent, first Opus packet ({} bytes, {} Hz capture)", encrypted.len(), chunk.rate);
            }
            if media.send_opus(&encrypted, now, &mut self.dump) {
                self.stats.audio_packets += 1;
                if media.audio_report_at.map_or(true, |t| now.saturating_duration_since(t) >= AUDIO_REPORT_EVERY) {
                    media.audio_report_at = Some(now);
                    media.send_report(media.audio_ssrc, media.audio_timestamp, media.audio_packets, media.audio_octets, now);
                }
            }
        }
    }

    /// A datagram from the media server: RTCP feedback about what this client sends.
    fn on_datagram(&mut self, datagram: &[u8]) {
        let now = Instant::now();
        let Some(media) = self.media.as_mut() else { return };
        let Some(transport) = media.transport.as_ref() else { return };
        if !crypto::is_rtcp(datagram) {
            if !self.seen.stray {
                self.seen.stray = true;
                info!("[GO LIVE] The media server sent something that is not RTCP ({} bytes, first bytes {:02x?}): ignored, as are further ones", datagram.len(), &datagram[..datagram.len().min(2)]);
            }
            return;
        }
        let Some(plain) = transport.open_rtcp(datagram) else {
            if !self.seen.undecryptable {
                self.seen.undecryptable = true;
                warn!("[GO LIVE] RTCP: a packet of {} bytes did not decrypt with the session's key: ignored, as are further ones", datagram.len());
            }
            return;
        };
        let wall = rtcp::ntp(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default());
        let mut keyframe = None;
        for packet in rtcp::parse(&plain) {
            match packet {
                rtcp::Packet::ReceiverReport { blocks, .. } | rtcp::Packet::SenderReport { blocks, .. } => {
                    for block in blocks.iter().filter(|b| b.ssrc == media.video_ssrc) {
                        let loss = block.loss_permille();
                        self.shared.set_loss(loss);
                        self.stats.loss = Some(self.stats.loss.map_or(loss, |l| l.max(loss)));
                        self.stats.round_trip = block.round_trip(wall).or(self.stats.round_trip);
                        if !self.seen.receiver_report {
                            self.seen.receiver_report = true;
                            info!(
                                "[GO LIVE] RTCP: first receiver report (loss {:.1} %, {} packets lost in all, highest sequence {}, round trip {})",
                                loss as f64 / 10.0,
                                block.cumulative_lost,
                                block.highest_sequence & 0xffff,
                                block.round_trip(wall).map_or("unknown".to_string(), |t| format!("{} ms", t.as_millis()))
                            );
                        }
                    }
                }
                rtcp::Packet::Pli { media_ssrc, .. } => {
                    self.stats.pli += 1;
                    keyframe = Some(format!("PLI for ssrc {media_ssrc}"));
                }
                rtcp::Packet::Fir { requests, .. } => {
                    self.stats.fir += 1;
                    keyframe = Some(format!("FIR for {:?}", requests.iter().map(|r| r.0).collect::<Vec<_>>()));
                }
                rtcp::Packet::Nack { media_ssrc, sequences, .. } => {
                    self.stats.nack += 1;
                    let mut resent = 0;
                    if media.rtx && media_ssrc == media.video_ssrc {
                        for sequence in &sequences {
                            let Some(sent) = media.history.get(*sequence) else { continue };
                            if media.retransmit.iter().any(|r| r.sequence == *sequence) || !media.budget.allow(now, sent.payload.len()) {
                                continue;
                            }
                            media.retransmit.push_back(sent.clone());
                            resent += 1;
                        }
                    }
                    if !self.seen.nack {
                        self.seen.nack = true;
                        info!(
                            "[GO LIVE] RTCP: NACK for {} packets of ssrc {media_ssrc} -> {resent} RTX{}",
                            sequences.len(),
                            if media.rtx { "" } else { " (retransmissions are off: LITECORD_GOLIVE_RTX=0)" }
                        );
                    }
                }
                rtcp::Packet::Remb { bitrate, .. } => {
                    self.stats.estimate = Some(bitrate);
                    if !self.seen.remb {
                        self.seen.remb = true;
                        info!("[GO LIVE] RTCP: first REMB, the receiver estimates {:.1} Mbit/s (shown in the 5 s line; not used to set the rate)", bitrate as f64 / 1e6);
                    }
                }
                rtcp::Packet::Other { packet_type, format } => {
                    if !self.seen.other_feedback {
                        self.seen.other_feedback = true;
                        info!("[GO LIVE] RTCP: packet type {packet_type} format {format} is not used (further ones are not logged)");
                    }
                }
            }
        }
        if let Some(what) = keyframe {
            if !(self.seen.pli && self.seen.fir) {
                info!("[GO LIVE] RTCP: {what} -> keyframe");
                self.seen.pli |= what.starts_with("PLI");
                self.seen.fir |= what.starts_with("FIR");
            }
            self.request_keyframe(now, false);
        }
        self.pump(now);
    }

    // ---- Time --------------------------------------------------------------------------------

    fn next_wake(&self, now: Instant) -> Instant {
        let mut wake = now + Duration::from_secs(1);
        let mut soonest = |at: Option<Instant>| {
            if let Some(at) = at {
                wake = wake.min(at);
            }
        };
        soonest(self.deadline);
        soonest(self.create_retry);
        soonest(self.handshake_deadline);
        soonest(self.reconnect_at);
        soonest(self.heartbeat_at);
        if let Some(media) = &self.media {
            soonest(media.silence_at);
            if media.has_queued() {
                soonest(Some(now + PACE_TICK));
            }
            if media.transport.is_some() {
                soonest(Some(media.last_sent + self.tuning().keepalive));
            }
        }
        if self.sending != Sending::Nothing {
            soonest(Some(self.stats_at + STATS_EVERY));
        }
        wake.max(now)
    }

    fn on_time(&mut self, now: Instant) {
        if self.create_retry.is_some_and(|at| now >= at) {
            self.create_retry = None;
            if !self.create_sent {
                self.send_create();
            }
        }
        if self.deadline.is_some_and(|at| now >= at) {
            self.deadline = None;
            if self.established {
                error!("[GO LIVE] No media server was named for the stream within {} s", self.tuning().unavailable_timeout.as_secs());
                self.fail(Failure::Lost("no media server".into()));
            } else {
                error!(
                    "[GO LIVE] The main gateway did not answer the stream request within {} s (STREAM_CREATE: {}, STREAM_SERVER_UPDATE: {})",
                    self.tuning().create_timeout.as_secs(),
                    if self.rtc_server_id.is_some() { "received" } else { "missing" },
                    if self.endpoint.is_some() { "received" } else { "missing" }
                );
                self.fail(Failure::NoAnswer);
            }
            return;
        }
        if self.handshake_deadline.is_some_and(|at| now >= at) {
            warn!("[GO LIVE] The stream gateway handshake did not finish within {} s (stopped at: {:?})", self.tuning().handshake_timeout.as_secs(), self.link);
            self.socket_lost("handshake timeout", CloseAction::Identify);
            if self.finished {
                return;
            }
        }
        if self.reconnect_at.is_some_and(|at| now >= at) {
            let resume = matches!(self.link, Link::Opening { resume: true });
            self.open_socket(resume);
            if self.finished {
                return;
            }
        }
        if self.heartbeat_at.is_some_and(|at| now >= at) {
            if self.heartbeats_unanswered >= 2 {
                warn!("[GO LIVE] Two heartbeats in a row went unanswered: the stream gateway connection is taken for lost");
                self.socket_lost("no heartbeat answer", CloseAction::Resume);
                if self.finished {
                    return;
                }
            } else {
                self.heartbeats_unanswered += 1;
                self.send_heartbeat();
                self.heartbeat_at = self.heartbeat_every.map(|every| now + every);
            }
        }
        let keepalive = self.tuning().keepalive;
        if let Some(media) = self.media.as_mut().filter(|m| m.transport.is_some()) {
            if media.silence_at.is_some_and(|at| now >= at) {
                media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump);
                media.opening_silence = media.opening_silence.saturating_sub(1);
                media.silence_at = (media.opening_silence > 0).then(|| now + OPUS_FRAME);
            } else if now.saturating_duration_since(media.last_sent) >= keepalive {
                // Keeps the path (and the NAT mapping) open while nothing else is sent.
                if !media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump) {
                    media.last_sent = now;
                }
            }
        }
        self.pump(now);
        if self.sending != Sending::Nothing && now.saturating_duration_since(self.stats_at) >= STATS_EVERY {
            self.log_stats(now);
        } else if self.sending == Sending::Nothing {
            self.stats_at = now;
        }
    }

    fn log_stats(&mut self, now: Instant) {
        let seconds = now.saturating_duration_since(self.stats_at).as_secs_f64().max(0.001);
        let s = std::mem::take(&mut self.stats);
        self.stats_at = now;
        info!(
            "[GO LIVE] {:.0} s: {} frames in, {} sent, {} dropped, {} keyframes, {} packets, {:.1} Mbit/s, audio {} packets, RTX {}, PLI {} FIR {} NACK {}, RR loss {}, rtt {}, REMB {}, {} send errors, {} watching, DAVE {}",
            seconds,
            s.frames_in,
            s.frames_sent,
            s.dropped,
            s.keyframes,
            s.packets,
            s.bytes as f64 * 8.0 / seconds / 1e6,
            s.audio_packets,
            s.rtx,
            s.pli,
            s.fir,
            s.nack,
            s.loss.map_or("none reported".to_string(), |l| format!("{:.1} %", l as f64 / 10.0)),
            s.round_trip.map_or("unknown".to_string(), |t| format!("{} ms", t.as_millis())),
            s.estimate.map_or("none".to_string(), |b| format!("{:.1} Mbit/s", b as f64 / 1e6)),
            s.send_errors,
            self.viewer_ids.len(),
            match self.sending {
                Sending::Encrypted => format!("epoch {} ({} in the group)", self.dave.epoch().unwrap_or(0), self.dave.members().len()),
                _ => "not in force".to_string(),
            }
        );
    }

    // ---- Endings -----------------------------------------------------------------------------

    /// `stop()`: the video is taken back, the stream deleted, the socket closed.
    async fn stop(&mut self, deleted: Option<String>) {
        let mut done = Vec::new();
        if let (Some(audio_ssrc), true) = (self.media.as_ref().map(|m| m.audio_ssrc), self.io.writer.is_some()) {
            self.send_json(wire::video(audio_ssrc, None));
            done.push("op 12 cleared");
        }
        if self.create_sent && !self.deleted {
            let key = self.stream_key.clone().unwrap_or_else(|| self.own_key.to_string());
            if deleted.as_deref() == Some(key.as_str()) || (self.deps.gateway_send)(signal::delete(&key)) {
                done.push("STREAM_DELETE sent");
            } else {
                done.push("STREAM_DELETE could not be sent (the main gateway is not connected; Discord ends the stream with the voice state)");
            }
            self.deleted = true;
        }
        if let Some(writer) = self.io.writer.take() {
            let _ = writer.send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })));
            drop(writer);
            if let Some(task) = self.io.writer_task.take() {
                // The last messages get a moment to leave; nobody waits for this thread.
                let _ = tokio::time::timeout(self.tuning().close_linger, task).await;
            }
            done.push("stream gateway closed");
        }
        info!("[GO LIVE] Stopped: {}", if done.is_empty() { "nothing had been set up".to_string() } else { done.join(", ") });
        self.end(None);
    }

    /// The stream is over, by a failure or by Discord's doing; the share itself goes on.
    fn fail(&mut self, failure: Failure) {
        if self.finished {
            return;
        }
        if self.create_sent && !self.deleted {
            let key = self.stream_key.clone().unwrap_or_else(|| self.own_key.to_string());
            (self.deps.gateway_send)(signal::delete(&key));
            self.deleted = true;
        }
        warn!("[GO LIVE] The Discord stream ended: {failure:?}. The share itself goes on.");
        self.end(Some(failure));
    }

    fn end(&mut self, failure: Option<Failure>) {
        self.close_socket();
        self.reconnect_at = None;
        self.io.socket = None;
        self.media = None;
        self.sending = Sending::Nothing;
        self.shared.wanted.store(false, Ordering::Relaxed);
        self.phase = Phase::Off;
        self.finished = true;
        if failure.is_some() {
            *self.shared.notice.lock().unwrap_or_else(|e| e.into_inner()) = failure.clone();
        }
        self.shared.update(|s| {
            s.phase = Phase::Off;
            s.viewers = 0;
            s.stream_key.clear();
            s.failure = failure;
        });
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn clipped(text: &str) -> String {
    if text.chars().count() <= 400 {
        text.to_string()
    } else {
        format!("{}... ({} characters)", text.chars().take(400).collect::<String>(), text.chars().count())
    }
}

/// A payload for the log: no token, and only the start of the session id.
fn redacted(d: &Value) -> Value {
    let mut d = d.clone();
    if let Some(token) = d.get("token").and_then(Value::as_str).map(str::len) {
        d["token"] = Value::String(format!("<{token} characters>"));
    }
    if let Some(session) = d.get("session_id").and_then(Value::as_str).map(short) {
        d["session_id"] = Value::String(format!("{session}..."));
    }
    d
}
