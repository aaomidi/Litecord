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
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use super::rate::{Ask, AudioClock, Budget, EvenClock, KeyframeGate, Pacer};
use super::rtp::{self, Ext, ExtProfile, Header, History, Playout, Sent};
use super::signal::{self, Region, StreamKey};
use super::wire::{self, CloseAction};
use super::{rtcp, udp, Failure, Params, Phase, Status};
use crate::screen_capture::stream_audio::StreamAudioEncoder;

/// Access units waiting for the worker; when full, the newest is dropped and a keyframe asked for.
const FRAME_QUEUE: usize = 32;
/// 10 ms chunks of stream audio waiting for the worker.
const AUDIO_QUEUE: usize = 64;
/// How often the pacer is given a turn while packets wait (`rate::Pacer` says how many may go).
const PACE_TICK: Duration = Duration::from_millis(1);
/// Video packets kept to answer a NACK: about 2.4 s at 8 Mbit/s.
const HISTORY_PACKETS: usize = 2048;
/// Audio packets kept to answer a NACK: two seconds.
const AUDIO_HISTORY_PACKETS: usize = 100;
/// Audio packets sent again for one NACK at most.
const AUDIO_RESEND_MAX: usize = 16;
/// While frames are held back for a keyframe, it is asked for again this often.
const KEYFRAME_RETRY_EVERY: Duration = Duration::from_millis(1000);
/// A stream deletion (opcode 19) the main gateway could not take is tried again this often.
const DELETE_RETRY_EVERY: Duration = Duration::from_secs(1);
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
/// The audio level written into the audio header extension while sound plays (-dBov), when
/// that extension is on. The real level is not measured: nothing shows it for stream audio.
const AUDIO_LEVEL: u8 = 30;
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
    /// `LITECORD_GOLIVE_AUDIO_EXT`: `none` (default) or `full` (audio packets carry the
    /// header extension native clients send: level, abs-send-time, speaking, mid).
    pub audio_ext: bool,
    /// `LITECORD_GOLIVE_IDLE_SR`: `0` (default) or `1` (while nothing is sent, every
    /// keepalive packet is followed by a sender report, should an idle stream be ended).
    pub idle_report: bool,
    /// `LITECORD_GOLIVE_DUMP`: a directory for the prepared access units and a packet log.
    pub dump: Option<PathBuf>,
    /// `LITECORD_GOLIVE_PLAYOUT`: `5,1000` (default), `none`, `0`, or `<min>,<max>` in units
    /// of 10 ms: what the playout-delay element of the full extension tells receivers.
    pub playout: Playout,
    /// `LITECORD_GOLIVE_EXT_OMIT`: `none` (default) or a list of `toffset`, `orientation`,
    /// `content`, `timing`: elements of the full extension that are left out.
    pub ext_omit: u8,
    /// `LITECORD_GOLIVE_KEYINT`: `60` (default) or another number of seconds between the
    /// share's periodic keyframes while somebody watches on Discord; `0`: none, only the
    /// ones asked for; `share`: the share's own rule (2 s); `server`: what the stream gateway
    /// names, when it does.
    pub keyint: KeyInt,
    /// `LITECORD_GOLIVE_TS`: `even` (default: one frame interval apart while the source
    /// keeps the share's pace but not its rhythm; any other source keeps the clock's) or
    /// `clock` (always the instant the share offered each picture, as in the first live runs).
    pub even_timestamps: bool,
    /// `LITECORD_GOLIVE_HOLD`: `20` (default) or another number of milliseconds up to 100;
    /// `0`: pictures leave as they come. While pictures are put on the even grid each one
    /// leaves this long after its place there, so that pictures also arrive evenly (for a
    /// receiver that shows them as they arrive, whatever their timestamps say).
    pub hold_ms: u32,
    /// `LITECORD_GOLIVE_REMB`: `1` (default) or `0` (the media server's bitrate estimate is
    /// only shown, as in the first version).
    pub follow_remb: bool,
    /// `LITECORD_GOLIVE_PACE`: `2.5` (default) or another multiple of the allowed bitrate
    /// that packets leave at, in tenths here; `burst` (0): the first version's fixed pace.
    pub pace: u32,
    /// `LITECORD_GOLIVE_MAX_KBPS`: unset (default: the share's maximum, at most 8000), or a
    /// lower maximum for Discord, announced in opcode 12 and kept on the wire.
    pub max_kbps: Option<u32>,
}

/// The share's periodic keyframe while somebody watches on Discord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyInt {
    /// Every so many seconds; 0: never, only the ones asked for.
    Seconds(u32),
    /// The share's own rule (one every 2 s), as in the first version.
    Share,
    /// What the stream gateway names (`keyframe_interval`); 60 s when it names none.
    Server,
}

impl KeyInt {
    /// Official senders aim at one keyframe a minute and otherwise answer requests.
    pub const DEFAULT_SECONDS: u32 = 60;

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "share" => Some(Self::Share),
            "server" => Some(Self::Server),
            _ => name.parse::<u32>().ok().filter(|s| *s <= 3600).map(Self::Seconds),
        }
    }

    pub fn name(self) -> String {
        match self {
            Self::Seconds(seconds) => seconds.to_string(),
            Self::Share => "share".to_string(),
            Self::Server => "server".to_string(),
        }
    }

    /// The interval for the send loop, in milliseconds: 0 leaves the share's own rule,
    /// `u64::MAX` is no periodic keyframe. `server_ms`: what the gateway named.
    pub fn millis(self, server_ms: Option<u64>) -> u64 {
        match self {
            Self::Share => 0,
            Self::Seconds(0) => u64::MAX,
            Self::Seconds(seconds) => seconds as u64 * 1000,
            Self::Server => server_ms.filter(|ms| *ms > 0).unwrap_or(Self::DEFAULT_SECONDS as u64 * 1000),
        }
    }
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
            audio_ext: false,
            idle_report: false,
            dump: None,
            playout: Playout::SMOOTH,
            ext_omit: 0,
            keyint: KeyInt::Seconds(KeyInt::DEFAULT_SECONDS),
            even_timestamps: true,
            hold_ms: 20,
            follow_remb: true,
            pace: 25,
            max_kbps: None,
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
        take("LITECORD_GOLIVE_AUDIO_EXT", &mut |v| match v {
            "none" => {
                s.audio_ext = false;
                true
            }
            "full" => {
                s.audio_ext = true;
                true
            }
            _ => false,
        });
        take("LITECORD_GOLIVE_IDLE_SR", &mut |v| flag(v).map(|x| s.idle_report = x).is_some());
        take("LITECORD_GOLIVE_DUMP", &mut |v| {
            s.dump = Some(PathBuf::from(v));
            true
        });
        take("LITECORD_GOLIVE_PLAYOUT", &mut |v| Playout::from_name(v).map(|x| s.playout = x).is_some());
        take("LITECORD_GOLIVE_EXT_OMIT", &mut |v| if v == "none" { Some(0) } else { rtp::omit_from_names(v) }.map(|x| s.ext_omit = x).is_some());
        take("LITECORD_GOLIVE_KEYINT", &mut |v| KeyInt::from_name(v).map(|x| s.keyint = x).is_some());
        take("LITECORD_GOLIVE_TS", &mut |v| match v {
            "clock" => {
                s.even_timestamps = false;
                true
            }
            "even" => {
                s.even_timestamps = true;
                true
            }
            _ => false,
        });
        take("LITECORD_GOLIVE_HOLD", &mut |v| v.parse::<u32>().ok().filter(|ms| *ms <= 100).map(|ms| s.hold_ms = ms).is_some());
        take("LITECORD_GOLIVE_REMB", &mut |v| flag(v).map(|x| s.follow_remb = x).is_some());
        take("LITECORD_GOLIVE_PACE", &mut |v| {
            let tenths = if v == "burst" { Some(0) } else { v.parse::<f64>().ok().filter(|x| (1.0..=20.0).contains(x)).map(|x| (x * 10.0).round() as u32) };
            tenths.map(|x| s.pace = x).is_some()
        });
        take("LITECORD_GOLIVE_MAX_KBPS", &mut |v| v.parse::<u32>().ok().filter(|k| (500..=8000).contains(k)).map(|k| s.max_kbps = Some(k)).is_some());
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
            "EXT={} GW={} PT={} RTX={} STREAM_TYPE={} REGION={} GROUP={} DAVE_CHECK={} REQUIRE_DAVE={} AUDIO_EXT={} IDLE_SR={} DUMP={} PLAYOUT={} EXT_OMIT={} KEYINT={} TS={} HOLD={} REMB={} PACE={} MAX_KBPS={}",
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
            if self.audio_ext { "full" } else { "none" },
            self.idle_report as u8,
            self.dump.as_ref().map_or("off".to_string(), |d| d.display().to_string()),
            self.playout.name(),
            rtp::omit_names(self.ext_omit),
            self.keyint.name(),
            if self.even_timestamps { "even" } else { "clock" },
            self.hold_ms,
            self.follow_remb as u8,
            if self.pace == 0 { "burst".to_string() } else { format!("{}.{}", self.pace / 10, self.pace % 10) },
            self.max_kbps.map_or("share".to_string(), |k| k.to_string())
        )
    }

    /// The header extension of video packets as the switches make it.
    pub fn ext(&self) -> Ext {
        Ext { profile: self.ext, playout: self.playout, omit: self.ext_omit }
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
    /// All viewers together get at most one keyframe per this long (PLI, FIR, a new viewer,
    /// a new key). Only a stream that is holding pictures back for a keyframe asks sooner.
    pub keyframe_every: Duration,
    /// How long after Speaking (opcode 5) the first packet it allows leaves: the server
    /// must have read the one before it gets the other.
    pub speaking_lead: Duration,
    /// A session lost before it was in service this long counts as a failed attempt, as
    /// one that never came up does: a server that takes the handshake and closes again is
    /// not asked every second for as long as the share runs.
    pub settled_after: Duration,
    /// For how long a stream deletion the main gateway could not take is tried again.
    pub delete_retry_for: Duration,
    /// The share's encoder takes a new bitrate only with a keyframe (the fork's VA-API
    /// encoders start a new sequence for it, and otherwise wait up to 3 s): a cut of the
    /// rate asks for that keyframe, so that the cut holds at once.
    pub rate_cut_needs_keyframe: bool,
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
            keyframe_every: Duration::from_secs(1),
            speaking_lead: Duration::from_millis(250),
            settled_after: Duration::from_secs(30),
            delete_retry_for: Duration::from_secs(60),
            rate_cut_needs_keyframe: cfg!(all(target_os = "linux", feature = "vaapi")),
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
    /// Bits per second to ask of the share's encoder at most while this stream is watched.
    encoder_cap: AtomicU32,
    /// The share's periodic keyframe while this stream is watched, in milliseconds
    /// (`KeyInt::millis`: 0 leaves the share's rule, `u64::MAX` is none).
    keyint_ms: AtomicU64,
    /// The stream is over (the thread may still be busy with its last errand).
    ended: AtomicBool,
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
            encoder_cap: AtomicU32::new(super::MAX_BITRATE),
            keyint_ms: AtomicU64::new(0),
            ended: AtomicBool::new(false),
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

    /// What the share's encoder may be asked for while this stream is watched: the wire
    /// rate announced to Discord and the media server's estimate, less what packets add.
    pub fn encoder_cap(&self) -> u32 {
        self.encoder_cap.load(Ordering::Relaxed)
    }

    /// How often the share makes a keyframe by itself while this stream is watched.
    /// `None`: the share's own rule. `Some(Duration::MAX)`: never, only when asked.
    pub fn keyframe_interval(&self) -> Option<Duration> {
        match self.keyint_ms.load(Ordering::Relaxed) {
            0 => None,
            u64::MAX => Some(Duration::MAX),
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// The stream is over.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
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
    /// Another stream is about to be requested: a deletion still owed for this one must
    /// not be sent any more (it would delete the new one, which has the same key).
    Forget,
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

    /// Another stream is about to be requested for this account: whatever this one still
    /// owes Discord (a deletion that could not be sent) is dropped.
    pub fn forget(&self) {
        let _ = self.control.send(Control::Forget);
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
        match self.frames.try_send(Frame { data: Arc::from(annex_b), at: Instant::now(), epoch }) {
            Ok(()) => {}
            // The worker is behind: this picture is lost to it, the next ones need a keyframe.
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.shared.epoch.fetch_add(1, Ordering::Relaxed);
                (self.request_keyframe)();
            }
            // The worker is gone without saying so: nothing wants pictures any more (and
            // nothing is asked of the encoder for it).
            Err(mpsc::error::TrySendError::Closed(_)) => self.shared.wanted.store(false, Ordering::Relaxed),
        }
    }

    /// 10 ms of the share's audio (mono), when the share has audio.
    pub fn offer_audio(&self, pcm: &[i16], sample_rate: u32) {
        if !self.audio || !self.shared.wanted() {
            return;
        }
        let _ = self.audio_chunks.try_send(Audio { pcm: pcm.to_vec(), rate: sample_rate, at: Instant::now() });
    }

    /// The stream ended (its thread may still be trying to tell Discord so).
    pub fn finished(&self) -> bool {
        self.shared.ended() || self.thread.is_finished()
    }

    /// The worker's thread is gone.
    pub fn thread_finished(&self) -> bool {
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
    let gateway_send = Arc::clone(&deps.gateway_send);
    let thread = std::thread::Builder::new().name("golive".into()).spawn(move || {
        crate::utils::thread_priority::raise_current_thread();
        // Whatever goes wrong in here, the share must not go on encoding for a stream
        // nobody serves: the state other threads read is put right on every way out.
        let gave_up = |what: &str| {
            worker_shared.wanted.store(false, Ordering::Relaxed);
            let key = worker_shared.status().stream_key;
            if !key.is_empty() {
                let sent = gateway_send(signal::delete(&key));
                warn!("[GO LIVE] The stream is taken back (op 19 {})", if sent { "sent" } else { "could not be sent: the main gateway is not connected" });
            }
            *worker_shared.notice.lock().unwrap_or_else(|e| e.into_inner()) = Some(Failure::Lost(what.into()));
            worker_shared.ended.store(true, Ordering::Release);
            worker_shared.update(|s| {
                s.phase = Phase::Off;
                s.viewers = 0;
                s.stream_key.clear();
                s.failure = Some(Failure::Lost(what.into()));
            });
        };
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => {
                let shared = Arc::clone(&worker_shared);
                let ran = catch_unwind(AssertUnwindSafe(move || runtime.block_on(run(Worker::new(deps, worker_context, params, shared), control_rx, frames_rx, audio_rx))));
                if let Err(panic) = ran {
                    let what = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".to_string());
                    error!("[GO LIVE] The stream's thread stopped on an internal error ({what}). The share itself goes on.");
                    gave_up("internal");
                }
            }
            Err(e) => {
                error!("[GO LIVE] The stream's thread could not start its runtime: {e}");
                gave_up("runtime");
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
    let mut held = true;
    while !w.finished {
        let wake = w.next_wake(Instant::now());
        tokio::select! {
            biased;
            message = control.recv(), if held => match message {
                Some(Control::Event(event)) => w.on_event(event),
                Some(Control::Stop { deleted }) => w.stop(deleted).await,
                Some(Control::Forget) => w.forget_owed(),
                // Nobody holds the stream any more. (What it still owes Discord is seen to
                // by `on_time`; the closed channel is not asked again.)
                None => {
                    held = false;
                    w.stop(None).await;
                }
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
    /// Everything put on the wire: video, retransmissions, audio and reports.
    bytes: u64,
    audio_packets: u32,
    audio_resent: u32,
    rtx: u32,
    pli: u32,
    fir: u32,
    nack: u32,
    /// What the NACKs asked for: video packets, audio packets, and those no longer kept
    /// (or over the retransmission budget).
    nack_video: u32,
    nack_audio: u32,
    nack_missed: u32,
    /// Keyframe requests that had to wait for the one-per-second rule.
    keyframes_waited: u32,
    send_errors: u32,
    loss: Option<u16>,
    round_trip: Option<Duration>,
    estimates: u32,
    /// Between two pictures offered by the share: shortest and longest.
    gap: Option<(Duration, Duration)>,
    /// Between the last packets of two pictures leaving: when the last left, shortest, longest.
    left: Option<(Instant, Duration, Duration)>,
    /// The longest a picture's last packet left after its timestamp's instant.
    late: Duration,
}

/// First-of-a-kind log lines.
#[derive(Default)]
struct Seen {
    receiver_report: bool,
    pli: bool,
    fir: bool,
    nack: bool,
    remb: bool,
    nack_audio: bool,
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
    /// When the share offered the picture.
    at: Instant,
    /// Not before this instant: its picture's place on the even grid plus the hold.
    release: Instant,
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
        let _ = writeln!(self.packets, "{:.4} {kind} seq={sequence} ts={timestamp} m={} len={len}", self.started.elapsed().as_secs_f64(), marker as u8);
    }

    /// A picture as it was queued: when the share offered it (on the log's clock), its
    /// timestamp, its size and how many packets it became.
    fn frame(&mut self, at: Instant, timestamp: u32, keyframe: bool, bytes: usize, packets: usize) {
        let offered = at.saturating_duration_since(self.started).as_secs_f64();
        let _ = writeln!(self.packets, "{:.4} frame offered={offered:.4} ts={timestamp} key={} bytes={bytes} packets={packets}", self.started.elapsed().as_secs_f64(), keyframe as u8);
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
    ext: Ext,
    payload_type: u8,
    rtx: bool,
    audio_ext: bool,
    /// The speaking flags last sent (opcode 5).
    speaking: u8,
    /// Stream audio waits until the server has read Speaking with the soundshare flag.
    audio_from: Option<Instant>,
    video_sequence: u16,
    rtx_sequence: u16,
    audio_sequence: u16,
    transport_sequence: u16,
    /// Video timestamps: `ts0` at `t0`, 90 kHz from there.
    t0: Instant,
    ts0: u32,
    last_video_timestamp: Option<u32>,
    /// `LITECORD_GOLIVE_TS=even`: the grid, and how far ahead of the clock it is (sender
    /// reports name the grid's time).
    even: Option<EvenClock>,
    grid_offset: u32,
    /// `LITECORD_GOLIVE_HOLD`: how long after its place on the grid a picture leaves, and
    /// how long that is right now (it comes and goes half a millisecond a picture as the
    /// grid is taken up and left, so that nothing jumps).
    hold: Duration,
    hold_now: Duration,
    /// When the share offered the picture before.
    last_frame_at: Option<Instant>,
    audio_timestamp: u32,
    last_audio: Option<Instant>,
    audio_clock: AudioClock,
    preparer: Preparer,
    /// Frames are dropped until a keyframe comes (after a gap, a new key, a full queue).
    waiting_keyframe: bool,
    frame_epoch: u64,
    queue: VecDeque<Queued>,
    retransmit: VecDeque<Sent>,
    /// A sealed datagram the socket could not take yet.
    blocked: Option<Vec<u8>>,
    history: History,
    audio_history: History,
    budget: RtxBudget,
    pacer: Pacer,
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
    fn new(socket: Arc<UdpSocket>, ready: &wire::Ready, switches: &Switches, fps: u32, now: Instant) -> Self {
        Self {
            socket,
            transport: None,
            audio_ssrc: ready.audio_ssrc,
            video_ssrc: ready.video_ssrc,
            rtx_ssrc: ready.rtx_ssrc,
            ext: switches.ext(),
            payload_type: switches.payload_type,
            rtx: switches.rtx,
            audio_ext: switches.audio_ext,
            speaking: 0,
            audio_from: None,
            video_sequence: rand::random(),
            rtx_sequence: rand::random(),
            audio_sequence: rand::random(),
            transport_sequence: rand::random(),
            t0: now,
            ts0: rand::random(),
            last_video_timestamp: None,
            even: switches.even_timestamps.then(|| EvenClock::new(fps)),
            grid_offset: 0,
            hold: Duration::from_millis(switches.hold_ms as u64),
            hold_now: Duration::ZERO,
            last_frame_at: None,
            audio_timestamp: rand::random(),
            last_audio: None,
            audio_clock: AudioClock::new(),
            preparer: Preparer::new(),
            waiting_keyframe: true,
            frame_epoch: 0,
            queue: VecDeque::new(),
            retransmit: VecDeque::new(),
            blocked: None,
            history: History::new(HISTORY_PACKETS),
            audio_history: History::new(AUDIO_HISTORY_PACKETS),
            budget: RtxBudget::new(now),
            pacer: Pacer::new(switches.pace, now),
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

    /// The video timestamp of `at` on the clock.
    fn video_timestamp(&self, at: Instant) -> u32 {
        self.ts0.wrapping_add((at.saturating_duration_since(self.t0).as_micros() as u64 * 9 / 100) as u32)
    }

    /// Seals and sends at once. The bytes put on the wire, or `None` when the socket did
    /// not take it.
    fn send_now(&mut self, sealed: Option<Vec<u8>>, now: Instant) -> Option<usize> {
        let sealed = sealed?;
        match self.socket.try_send(&sealed) {
            Ok(_) => {
                self.last_sent = now;
                Some(sealed.len())
            }
            Err(_) => None,
        }
    }

    /// An Opus packet with this sequence number and timestamp, before transport encryption.
    fn opus_packet(&self, sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
        let header = Header { marker: false, payload_type: rtp::OPUS_PAYLOAD_TYPE, sequence, timestamp, ssrc: self.audio_ssrc };
        if self.audio_ext {
            let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            let (level, speaking) = if payload == rtp::OPUS_SILENCE { (127, 0) } else { (AUDIO_LEVEL, self.speaking) };
            rtp::audio_packet_with_extension(&header, rtp::abs_send_time(wall), level, speaking, payload)
        } else {
            rtp::audio_packet(&header, payload)
        }
    }

    /// One Opus packet (already end-to-end encrypted, or the silence packet) on the audio
    /// SSRC. The bytes put on the wire, or `None` when it did not leave.
    fn send_opus(&mut self, payload: &[u8], now: Instant, dump: &mut Option<Dump>) -> Option<usize> {
        // 20 ms per packet; after a pause, the clock moves by the pause.
        let frames = match self.last_audio {
            Some(last) if now.saturating_duration_since(last) > 5 * OPUS_FRAME => (now.saturating_duration_since(last).as_millis() as u32 + 10) / 20,
            _ => 1,
        };
        self.last_audio = Some(now);
        self.audio_timestamp = self.audio_timestamp.wrapping_add(frames.max(1) * OPUS_FRAME_SAMPLES);
        self.audio_sequence = self.audio_sequence.wrapping_add(1);
        let (sequence, timestamp) = (self.audio_sequence, self.audio_timestamp);
        let packet = self.opus_packet(sequence, timestamp, payload);
        let sealed = self.transport.as_mut().and_then(|t| t.seal_rtp(&packet));
        let sent = self.send_now(sealed, now);
        if sent.is_some() {
            self.audio_packets = self.audio_packets.wrapping_add(1);
            self.audio_octets = self.audio_octets.wrapping_add(payload.len() as u32);
            self.audio_clock.on_packet(timestamp, now);
            // (Kept for a NACK: the media server asks for lost audio packets too.)
            self.audio_history.push(Sent { sequence, timestamp, marker: false, payload: payload.to_vec() });
            if let Some(d) = dump {
                d.packet("audio", sequence, timestamp, false, packet.len());
            }
        }
        sent
    }

    /// An audio packet the media server did not get, once more: the same sequence number,
    /// timestamp and payload on the audio SSRC (audio has no retransmission stream).
    fn resend_opus(&mut self, sequence: u16, now: Instant, dump: &mut Option<Dump>) -> Option<usize> {
        let original = self.audio_history.get(sequence)?.clone();
        let packet = self.opus_packet(original.sequence, original.timestamp, &original.payload);
        let sealed = self.transport.as_mut().and_then(|t| t.seal_rtp(&packet));
        let sent = self.send_now(sealed, now);
        if let (Some(_), Some(d)) = (sent, dump.as_mut()) {
            d.packet("audio-again", original.sequence, original.timestamp, false, packet.len());
        }
        sent
    }

    /// A sender report: `timestamp` is the stream's RTP time at this instant. The bytes put
    /// on the wire.
    fn send_report(&mut self, ssrc: u32, timestamp: u32, packets: u32, octets: u32, now: Instant) -> usize {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let report = rtcp::sender_report(ssrc, rtcp::ntp(wall), timestamp, packets, octets);
        let sealed = self.transport.as_mut().and_then(|t| t.seal_rtcp(&report));
        self.send_now(sealed, now).unwrap_or(0)
    }

    /// The audio sender report: the audio timestamp of this instant by the clock the
    /// packets set (`AudioClock`), not that of whichever packet came last.
    fn send_audio_report(&mut self, now: Instant) -> usize {
        let timestamp = if self.audio_clock.is_set() { self.audio_clock.timestamp_at(now) } else { self.audio_timestamp };
        self.send_report(self.audio_ssrc, timestamp, self.audio_packets, self.audio_octets, now)
    }

    /// Sends what the pacer allows of the retransmissions and the queued video.
    fn pump(&mut self, now: Instant, stale: Duration, stats: &mut Stats, wire: &mut Budget, dump: &mut Option<Dump>) -> Pumped {
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
        // (What a packet adds to its payload: header, extension, tag and nonce.)
        // (Pictures still held for their place on the grid do not wait for the pacer yet.)
        let waiting: usize = self.queue.iter().filter(|q| q.release <= now).map(|q| q.payload.len() + 80).sum::<usize>() + self.retransmit.iter().map(|r| r.payload.len() + 82).sum::<usize>();
        let waited = self.queue.front().map_or(Duration::ZERO, |q| now.saturating_duration_since(q.release));
        self.pacer.set_budget(wire.wire());
        self.pacer.refill(now, waiting, waited);
        if self.queue.front().is_some_and(|q| now.saturating_duration_since(q.release.max(q.queued_at)) > stale) {
            let dropped = self.queue.len();
            self.queue.clear();
            self.retransmit.clear();
            self.waiting_keyframe = true;
            return Pumped::Stale(dropped);
        }
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let abs_send_time = rtp::abs_send_time(wall);
        while self.pacer.ready() && self.blocked.is_none() {
            let (packet, sent, kind) = if let Some(original) = self.retransmit.pop_front() {
                self.rtx_sequence = self.rtx_sequence.wrapping_add(1);
                let header = Header { marker: original.marker, payload_type: self.payload_type + 1, sequence: self.rtx_sequence, timestamp: original.timestamp, ssrc: self.rtx_ssrc };
                self.transport_sequence = self.transport_sequence.wrapping_add(1);
                (rtp::rtx_packet(&header, self.ext, abs_send_time, self.transport_sequence, original.sequence, &original.payload), None, "rtx")
            } else if let Some(q) = if self.queue.front().is_some_and(|q| q.release <= now) { self.queue.pop_front() } else { None } {
                let header = Header { marker: q.marker, payload_type: self.payload_type, sequence: q.sequence, timestamp: q.timestamp, ssrc: self.video_ssrc };
                self.transport_sequence = self.transport_sequence.wrapping_add(1);
                let packet = rtp::video_packet(&header, self.ext, abs_send_time, self.transport_sequence, &q.payload);
                (packet, Some((Sent { sequence: q.sequence, timestamp: q.timestamp, marker: q.marker, payload: q.payload }, q.at)), "video")
            } else {
                break;
            };
            let Some(sealed) = self.transport.as_mut().and_then(|t| t.seal_rtp(&packet)) else {
                stats.send_errors += 1;
                continue;
            };
            let size = sealed.len();
            self.pacer.took(size);
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
            stats.bytes += size as u64;
            wire.on_sent(size);
            match sent {
                Some((sent, offered)) => {
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
                    if end_of_frame {
                        stats.late = stats.late.max(now.saturating_duration_since(offered));
                        stats.left = Some(stats.left.map_or((now, Duration::MAX, Duration::ZERO), |(before, least, most)| {
                            let gap = now.saturating_duration_since(before);
                            (now, least.min(gap), most.max(gap))
                        }));
                        // One wall clock for audio and video reports, so receivers can align them.
                        if self.video_report_at.map_or(true, |t| now.saturating_duration_since(t) >= VIDEO_REPORT_EVERY) {
                            self.video_report_at = Some(now);
                            let timestamp = self.video_timestamp(now).wrapping_add(self.grid_offset);
                            let size = self.send_report(self.video_ssrc, timestamp, self.video_packets, self.video_octets, now);
                            stats.bytes += size as u64;
                            wire.on_sent(size);
                        }
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

    /// When the pacer should be given its next turn, if anything waits.
    fn next_turn(&self, now: Instant) -> Option<Instant> {
        if !self.retransmit.is_empty() || self.blocked.is_some() {
            return Some(now + PACE_TICK);
        }
        // (A picture held for its place on the grid: when that comes.)
        self.queue.front().map(|q| q.release.max(now + PACE_TICK))
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
    /// A stream deletion the main gateway could not take: the key, when it is tried next
    /// and when trying ends. The stream itself is over by then.
    owed_delete: Option<(String, Instant, Instant)>,

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
    /// Since when the session is in service (Session Description or Resumed).
    up_since: Option<Instant>,
    /// What the gateway names as the keyframe interval, in milliseconds.
    server_keyint: Option<u64>,
    /// The users the stream server says are connected to it (opcodes 11 and 13).
    connected_users: HashSet<u64>,
    ready: Option<wire::Ready>,
    mode: Option<Mode>,

    // Media.
    media: Option<Media>,
    dave: Dave,
    sending: Sending,
    key_generation: u64,
    keyframes: KeyframeGate,
    /// What may go on the wire, and what the share's encoder is asked for.
    wire: Budget,
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
        let now = Instant::now();
        let keyframes = KeyframeGate::new(deps.tuning.keyframe_every);
        let wire = Budget::new(deps.tuning.max_bitrate, deps.tuning.switches.follow_remb, params.audio, now);
        shared.encoder_cap.store(wire.asked(), Ordering::Relaxed);
        shared.keyint_ms.store(deps.tuning.switches.keyint.millis(None), Ordering::Relaxed);
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
            owed_delete: None,
            io: Io { connecting: None, discovering: None, reader: None, writer: None, writer_task: None, socket: None },
            link: Link::Down,
            seq_ack: -1,
            heartbeat_every: None,
            heartbeat_at: None,
            heartbeats_unanswered: 0,
            handshake_deadline: None,
            reconnect_at: None,
            failed_reconnects: VecDeque::new(),
            up_since: None,
            server_keyint: None,
            connected_users: HashSet::new(),
            ready: None,
            mode: None,
            media: None,
            dave,
            sending: Sending::Nothing,
            key_generation: 0,
            keyframes,
            wire,
            stats: Stats::default(),
            stats_at: now,
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
        let switches = &self.tuning().switches;
        info!(
            "[GO LIVE] Receivers are told: {}. Periodic keyframe while somebody watches on Discord: {}. At most {:.1} Mbit/s on the wire{}, packets leave at {}.",
            format!(
                "{}; timestamps {}",
                if switches.ext == ExtProfile::Full { switches.playout.describe() } else { "nothing about the playout delay (LITECORD_GOLIVE_EXT is not full)".to_string() },
                match (switches.even_timestamps, switches.hold_ms) {
                    (false, _) => "of the instant each picture was offered".to_string(),
                    (true, 0) => "one frame interval apart while the source keeps the share's pace, pictures sent as they come".to_string(),
                    (true, hold) => format!("one frame interval apart while the source keeps the share's pace, each picture then sent {hold} ms after its place on that grid"),
                }
            ),
            match switches.keyint {
                KeyInt::Seconds(0) => "none, only the ones asked for".to_string(),
                KeyInt::Seconds(seconds) => format!("every {seconds} s"),
                KeyInt::Share => "the share's own, every 2 s".to_string(),
                KeyInt::Server => format!("what the stream gateway names, else every {} s", KeyInt::DEFAULT_SECONDS),
            },
            self.tuning().max_bitrate as f64 / 1e6,
            if switches.follow_remb { ", less when the media server's estimate (REMB) is lower" } else { " (the media server's estimate is not followed: LITECORD_GOLIVE_REMB=0)" },
            if switches.pace == 0 { "up to 12 a millisecond".to_string() } else { format!("{}.{} times that rate", switches.pace / 10, switches.pace % 10) }
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
        // (The stream is over; only a deletion is still owed.)
        if self.phase == Phase::Off {
            return;
        }
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
            // The gateway's answer to an earlier stop (the button turned off and on again
            // quickly), arriving after this stream's request: not a refusal of this one. This
            // worker asks for no deletion before it ends, so until its session was in
            // service a "user_requested" is always about the stream of the request before.
            signal::Event::Delete { reason, .. } if !self.established && reason == "user_requested" => {
                if self.stream_key.is_none() {
                    info!("[GO LIVE] STREAM_DELETE: reason={reason}, before this stream was announced: the answer to an earlier stop, ignored");
                } else {
                    // That earlier stream was announced to this worker too (its STREAM_CREATE
                    // came after this one's request). It is gone now; ours is still to come.
                    info!("[GO LIVE] STREAM_DELETE: reason={reason}, for the stream an earlier request made: its server is left, waiting for this request's own STREAM_CREATE");
                    self.stream_key = None;
                    self.rtc_server_id = None;
                    self.rtc_channel_id = None;
                    self.endpoint = None;
                    self.token = None;
                    self.need_create = true;
                    self.leave_server();
                    self.set_phase(Phase::Creating);
                    self.deadline = Some(Instant::now() + self.tuning().create_timeout);
                }
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
            // (Without end-to-end encryption this list is one of the two that say whether
            // anybody watches.)
            self.refresh_sending();
        }
    }

    /// Somebody else is connected to the stream, by the stream server's list (opcodes 11
    /// and 13) or the main gateway's (STREAM_UPDATE).
    fn somebody_connected(&self) -> bool {
        !self.viewer_ids.is_empty() || self.connected_users.iter().any(|user| *user != self.context.user_id)
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
        self.up_since = None;
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
        let now = Instant::now();
        let was_up = self.link == Link::Up;
        // A session that worked for a while and then dropped is one thing; a server that
        // takes the handshake (or the resume) and closes again is a failed attempt like one
        // that never answered, or it would be asked again every second for as long as the
        // share runs.
        let settled = was_up && self.up_since.is_some_and(|since| now.saturating_duration_since(since) >= self.tuning().settled_after);
        self.close_socket();
        if action == CloseAction::Fatal {
            self.fail(Failure::Lost(why.to_string()));
            return;
        }
        let window = self.tuning().retry_window;
        while self.failed_reconnects.front().is_some_and(|t| now.duration_since(*t) > window) {
            self.failed_reconnects.pop_front();
        }
        if !settled {
            self.failed_reconnects.push_back(now);
            if self.failed_reconnects.len() >= self.tuning().retry_delays.len() {
                error!(
                    "[GO LIVE] The stream gateway connection failed, or was closed again within {} s, {} times in {} s: giving up",
                    self.tuning().settled_after.as_secs(),
                    self.failed_reconnects.len(),
                    window.as_secs()
                );
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
                // (Without DAVE no key change announces a new viewer: it starts the stream
                // when it is the first, and needs a keyframe when it is not.)
                let was_sending = self.sending;
                self.refresh_sending();
                if new && was_sending != Sending::Nothing && self.sending == Sending::Plain {
                    self.request_keyframe(Instant::now(), false);
                }
            }
            wire::OP_CLIENT_DISCONNECT => {
                if let Some(user) = signal::snowflake(&d["user_id"]) {
                    if self.connected_users.remove(&user) {
                        info!("[GO LIVE] Viewer disconnected: user {user}");
                        self.refresh_sending();
                    }
                }
            }
            wire::OP_SESSION_UPDATE => {
                info!("[GO LIVE] Session Update (op 14): {d}");
                if d["video_codec"].as_str().is_some_and(|codec| !codec.eq_ignore_ascii_case("H264")) {
                    error!("[GO LIVE] The server switched the video codec to {}: this client sends H.264 only", d["video_codec"]);
                    self.fail(Failure::Lost("codec".into()));
                    return;
                }
                if let Some(ms) = d["keyframe_interval"].as_u64() {
                    self.note_server_keyint(ms);
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
                // Version 0 here is a downgrade to no end-to-end encryption, and transition
                // 0 takes effect at once: neither is read out of a field that is missing.
                let number = |name: &str| d[name].as_u64().and_then(|n| u16::try_from(n).ok());
                let (Some(transition), Some(version)) = (number("transition_id"), number("protocol_version")) else {
                    warn!("[GO LIVE] DAVE: prepare transition (op 21) without a transition id or a protocol version: ignored ({})", clipped(&d.to_string()));
                    return;
                };
                let out = self.dave.on_prepare_transition(transition, version);
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
        self.media = Some(Media::new(socket, &ready, &self.tuning().switches, self.params.fps, Instant::now()));
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
        let lead = self.tuning().speaking_lead;
        let Some(media) = self.media.as_mut() else { return };
        media.transport = Some(Transport::new(mode, &description.secret_key));
        media.opening_silence = OPENING_SILENCE;
        // Speaking must reach the server before any packet does (it closes the connection
        // over an SSRC it was not told about), and it is only queued for the socket here:
        // the first packet waits a moment.
        media.silence_at = Some(Instant::now() + lead);
        let (audio_ssrc, video_ssrc, rtx_ssrc) = (media.audio_ssrc, media.video_ssrc, media.rtx_ssrc);
        self.link = Link::Up;
        self.up_since = Some(Instant::now());
        self.handshake_deadline = None;
        self.established = true;
        // Speaking must be sent once before any media, even with no flag set.
        self.send_json(wire::speaking(0, audio_ssrc));
        if let Some(ms) = d["keyframe_interval"].as_u64() {
            self.note_server_keyint(ms);
        }
        if let Some(experiment) = d["bandwidth_estimation_experiment"].as_str() {
            info!("[GO LIVE] Session Description names the bandwidth estimation experiment {experiment:?}");
        }
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
        self.up_since = Some(Instant::now());
        self.handshake_deadline = None;
        self.set_phase(if self.sending == Sending::Nothing { Phase::Waiting } else { Phase::Live });
    }

    /// The stream gateway named a keyframe interval (Session Description or Session
    /// Update): logged, and followed with `LITECORD_GOLIVE_KEYINT=server`.
    fn note_server_keyint(&mut self, ms: u64) {
        if self.server_keyint == Some(ms) {
            return;
        }
        self.server_keyint = Some(ms);
        let keyint = self.tuning().switches.keyint;
        info!(
            "[GO LIVE] The stream gateway names a keyframe interval of {ms} ms: {}",
            if keyint == KeyInt::Server { "followed (LITECORD_GOLIVE_KEYINT=server)".to_string() } else { format!("not followed (LITECORD_GOLIVE_KEYINT={}; `server` follows it)", keyint.name()) }
        );
        self.shared.keyint_ms.store(keyint.millis(self.server_keyint), Ordering::Relaxed);
    }

    // ---- What may be sent ----------------------------------------------------------------

    /// Follows DAVE after anything that may have changed it: starts and stops the flow of
    /// frames, and asks for a keyframe when the key changed.
    fn refresh_sending(&mut self) {
        let ready = self.media.as_ref().is_some_and(|m| m.transport.is_some());
        let mut now_sending = if ready { self.dave.sending() } else { Sending::Nothing };
        // With DAVE the group says whether anybody watches (no other member, no key).
        // Without it nothing does, so the lists of who is connected decide: an unwatched
        // stream costs no encoding and puts nothing on the wire either way.
        if now_sending == Sending::Plain && !self.somebody_connected() {
            now_sending = Sending::Nothing;
        }
        let generation = self.dave.key_generation();
        let now = Instant::now();
        if now_sending != Sending::Nothing && self.sending == Sending::Nothing {
            self.sending = now_sending;
            let lead = self.tuning().speaking_lead;
            let mut speaking = None;
            if let Some(media) = self.media.as_mut() {
                media.waiting_keyframe = true;
                media.last_frame_at = None;
                // The soundshare flag (stream audio, no speaking ring), once per session and
                // before the stream's media, with or without sound: what both working senders
                // do. Stream audio waits until the server has read it.
                if media.speaking != wire::SPEAKING_SOUNDSHARE {
                    media.speaking = wire::SPEAKING_SOUNDSHARE;
                    media.audio_from = Some(now + lead);
                    speaking = Some(wire::speaking(wire::SPEAKING_SOUNDSHARE, media.audio_ssrc));
                }
            }
            if let Some(payload) = speaking {
                info!("[GO LIVE] Speaking (op 5) sent with the soundshare flag: {}", payload["d"]);
                self.send_json(payload);
            }
            self.wire.idle(now);
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
                media.last_frame_at = None;
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
            info!("[GO LIVE] The key changed while sending ({} in the DAVE group now): a keyframe is asked for, the stream goes on", self.dave.members().len());
            self.request_keyframe(now, false);
        }
        self.key_generation = generation;
        let e2ee = self.dave.end_to_end();
        self.shared.update(|s| s.e2ee = e2ee);
    }

    /// Asks the share's encoder for a keyframe, within the one-per-second rule for all
    /// viewers together (`KeyframeGate`). `urgent`: nothing is being sent until it comes.
    /// A request that has to wait is granted by `on_time`.
    fn request_keyframe(&mut self, now: Instant, urgent: bool) {
        if self.keyframes.request(now, urgent) {
            (self.deps.request_keyframe)();
            (self.deps.wake_sender)();
        } else {
            self.stats.keyframes_waited += 1;
        }
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
            self.request_keyframe(now, true);
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
                self.request_keyframe(now, true);
                return;
            }
        };
        if media.waiting_keyframe && !prepared.keyframe {
            self.stats.dropped += 1;
            if self.keyframes.last().map_or(true, |t| now.saturating_duration_since(t) >= KEYFRAME_RETRY_EVERY) {
                self.request_keyframe(now, true);
            }
            return;
        }
        let Some(encrypted) = self.dave.encrypt_video(&prepared.data) else {
            media.waiting_keyframe = true;
            self.stats.dropped += 1;
            self.request_keyframe(now, true);
            return;
        };
        media.waiting_keyframe = false;
        if let Some(dump) = self.dump.as_mut() {
            let _ = dump.prepared.write_all(&prepared.data);
        }
        // The timestamp: the instant the share offered the picture, or with
        // LITECORD_GOLIVE_TS=even the place on an even grid nearest to it.
        let clock = media.video_timestamp(frame.at);
        let mut timestamp = match media.even.as_mut() {
            Some(even) => even.stamp(clock),
            None => clock,
        };
        if let Some(last) = media.last_video_timestamp {
            // Forced to increase, whatever the clock did.
            if (timestamp.wrapping_sub(last) as i32) <= 0 {
                timestamp = last.wrapping_add(1);
            }
        }
        media.grid_offset = timestamp.wrapping_sub(clock);
        media.last_video_timestamp = Some(timestamp);
        // On the grid the picture also leaves evenly: at its place there plus the hold (the
        // grid may be before or after the instant the picture was offered).
        let on_grid = media.even.as_ref().is_some_and(EvenClock::engaged);
        let step = Duration::from_micros(500);
        media.hold_now = if on_grid { (media.hold_now + step).min(media.hold) } else { media.hold_now.saturating_sub(step) };
        let release = if media.hold_now.is_zero() {
            now
        } else {
            let ahead = media.grid_offset as i32 as i64 * 100 / 9;
            let place = if ahead >= 0 { frame.at + Duration::from_micros(ahead as u64) } else { frame.at.checked_sub(Duration::from_micros((-ahead) as u64)).unwrap_or(frame.at) };
            (place + media.hold_now).max(now)
        };
        if let Some(before) = media.last_frame_at.replace(frame.at) {
            let gap = frame.at.saturating_duration_since(before);
            self.stats.gap = Some(self.stats.gap.map_or((gap, gap), |(least, most)| (least.min(gap), most.max(gap))));
        }
        let payloads = rtp::packetize_h264(&encrypted, rtp::MAX_PAYLOAD);
        let count = payloads.len();
        if let Some(dump) = self.dump.as_mut() {
            dump.frame(frame.at, timestamp, prepared.keyframe, encrypted.len(), count);
        }
        for (i, payload) in payloads.into_iter().enumerate() {
            media.video_sequence = media.video_sequence.wrapping_add(1);
            media.queue.push_back(Queued { sequence: media.video_sequence, timestamp, marker: i + 1 == count, payload, queued_at: now, at: frame.at, release });
        }
        self.stats.frames_sent += 1;
        if prepared.keyframe {
            self.stats.keyframes += 1;
            // (Whoever asked, and the share's own periodic ones: the next for the viewers is
            // a second away.)
            self.keyframes.sent(now);
            if !self.seen.keyframe {
                self.seen.keyframe = true;
                let (profile, flags, level) = media.preparer.profile_level().unwrap_or_default();
                info!(
                    "[GO LIVE] First keyframe sent: {} bytes in {count} packets, H.264 profile {profile} (constraint flags {flags:#04x}) level {}.{}, SPS normalised: {}{}, DAVE: {}",
                    encrypted.len(),
                    level / 10,
                    level % 10,
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
        if let Pumped::Stale(packets) = media.pump(now, stale, &mut self.stats, &mut self.wire, &mut self.dump) {
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
        // (Until the server has read Speaking with the soundshare flag.)
        if media.audio_from.is_some_and(|from| now < from) {
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
                    if let Some(size) = media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump) {
                        self.stats.bytes += size as u64;
                        self.wire.on_sent(size);
                    }
                }
                continue;
            }
            let Some(encrypted) = self.dave.encrypt_opus(&packet) else { continue };
            if !self.seen.audio {
                self.seen.audio = true;
                info!("[GO LIVE] Stream audio: first Opus packet ({} bytes, {} Hz capture)", encrypted.len(), chunk.rate);
            }
            if let Some(size) = media.send_opus(&encrypted, now, &mut self.dump) {
                self.stats.audio_packets += 1;
                self.stats.bytes += size as u64;
                self.wire.on_sent(size);
                if media.audio_report_at.map_or(true, |t| now.saturating_duration_since(t) >= AUDIO_REPORT_EVERY) {
                    media.audio_report_at = Some(now);
                    let size = media.send_audio_report(now);
                    self.stats.bytes += size as u64;
                    self.wire.on_sent(size);
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
                    let what;
                    if media_ssrc == media.audio_ssrc {
                        // Audio has no retransmission stream: the packet goes out again as
                        // it was, on the audio SSRC with its own sequence number.
                        what = "the audio";
                        self.stats.nack_audio += sequences.len() as u32;
                        for sequence in sequences.iter().take(AUDIO_RESEND_MAX) {
                            if let Some(size) = media.resend_opus(*sequence, now, &mut self.dump) {
                                resent += 1;
                                self.stats.audio_resent += 1;
                                self.stats.bytes += size as u64;
                                self.wire.on_sent(size);
                            }
                        }
                        self.stats.nack_missed += (sequences.len() - resent) as u32;
                    } else if media_ssrc == media.video_ssrc {
                        what = "the video";
                        self.stats.nack_video += sequences.len() as u32;
                        if media.rtx {
                            for sequence in &sequences {
                                // (Asked for twice while it waits to leave: sent once.)
                                if media.retransmit.iter().any(|r| r.sequence == *sequence) {
                                    resent += 1;
                                    continue;
                                }
                                let Some(sent) = media.history.get(*sequence) else { continue };
                                if !media.budget.allow(now, sent.payload.len()) {
                                    continue;
                                }
                                media.retransmit.push_back(sent.clone());
                                resent += 1;
                            }
                        }
                        self.stats.nack_missed += (sequences.len() - resent) as u32;
                    } else {
                        what = "another stream";
                        self.stats.nack_missed += sequences.len() as u32;
                    }
                    // The first of each kind is logged; the 5 s line counts them all.
                    let seen = if media_ssrc == media.audio_ssrc { &mut self.seen.nack_audio } else { &mut self.seen.nack };
                    if !*seen {
                        *seen = true;
                        info!(
                            "[GO LIVE] RTCP: NACK for {} packets of {what} (ssrc {media_ssrc}) -> {resent} sent again{}",
                            sequences.len(),
                            match (media_ssrc == media.audio_ssrc, media.rtx) {
                                (true, _) => " as they were (audio has no RTX stream)",
                                (false, true) => " on the RTX stream",
                                (false, false) => " (retransmissions are off: LITECORD_GOLIVE_RTX=0)",
                            }
                        );
                    }
                }
                rtcp::Packet::Remb { bitrate, .. } => {
                    self.stats.estimates += 1;
                    self.wire.on_remb(bitrate, now);
                    if !self.seen.remb {
                        self.seen.remb = true;
                        info!(
                            "[GO LIVE] RTCP: first REMB, the media server estimates {:.1} Mbit/s: {}",
                            bitrate as f64 / 1e6,
                            if self.deps.tuning.switches.follow_remb { "the stream stays below it (and below what it announced)" } else { "shown in the 5 s line only (LITECORD_GOLIVE_REMB=0)" }
                        );
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
            let seen = if what.starts_with("PLI") { &mut self.seen.pli } else { &mut self.seen.fir };
            if !*seen {
                *seen = true;
                info!("[GO LIVE] RTCP: {what} -> keyframe, at most one a second for all viewers together (further ones are counted in the 5 s line)");
            }
            self.request_keyframe(now, false);
        }
        self.publish_rate(now);
        self.pump(now);
    }

    /// Moves the wire budget to where it belongs now and tells the share's send loop what
    /// to ask of its encoder (`Shared::encoder_cap`). That moves in steps (`Budget::ask`).
    fn publish_rate(&mut self, now: Instant) {
        if self.sending == Sending::Nothing {
            self.wire.idle(now);
        } else {
            self.wire.update(now);
        }
        let before = self.wire.asked();
        let (asked, change) = self.wire.ask(now);
        if change == Ask::Same {
            return;
        }
        self.shared.encoder_cap.store(asked, Ordering::Relaxed);
        if self.sending == Sending::Nothing {
            return;
        }
        info!(
            "[GO LIVE] Rate: {:.1} Mbit/s allowed on the wire ({}), the most the encoder may use goes from {:.1} to {:.1} Mbit/s",
            self.wire.wire() as f64 / 1e6,
            match self.wire.estimate(now).filter(|_| self.deps.tuning.switches.follow_remb) {
                Some(estimate) => format!("REMB {:.1}, announced {:.1}", estimate as f64 / 1e6, self.deps.tuning.max_bitrate as f64 / 1e6),
                None => format!("announced {:.1}", self.deps.tuning.max_bitrate as f64 / 1e6),
            },
            before as f64 / 1e6,
            asked as f64 / 1e6
        );
        // A cut has to hold now, and the share's GPU encoder takes a new rate only with a
        // keyframe (it would wait up to three seconds for one otherwise).
        if change == Ask::Cut && self.tuning().rate_cut_needs_keyframe {
            self.request_keyframe(now, false);
        }
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
        soonest(self.keyframes.due());
        soonest(self.owed_delete.as_ref().map(|owed| owed.1));
        if let Some(media) = &self.media {
            soonest(media.silence_at);
            soonest(media.next_turn(now));
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
        // The stream is over and a deletion the main gateway could not take is still owed:
        // nothing else is left to do.
        if self.phase == Phase::Off {
            let Some((key, next, until)) = self.owed_delete.take() else {
                self.finished = true;
                return;
            };
            if now < next {
                self.owed_delete = Some((key, next, until));
            } else if (self.deps.gateway_send)(signal::delete(&key)) {
                info!("[GO LIVE] STREAM_DELETE sent, now that the main gateway is connected again");
                self.finished = true;
            } else if now >= until {
                warn!(
                    "[GO LIVE] STREAM_DELETE could not be sent for {} s (the main gateway is not connected): given up. Discord ends the stream when this account leaves the voice channel.",
                    self.tuning().delete_retry_for.as_secs()
                );
                self.finished = true;
            } else {
                self.owed_delete = Some((key, now + DELETE_RETRY_EVERY, until));
            }
            return;
        }
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
        if self.dave.commit_unanswered(now) {
            warn!(
                "[GO LIVE] DAVE: five seconds after this client's commit (op 28) the gateway has neither announced a commit (op 29) nor sent a welcome (op 30). \
                 If no picture reaches the viewer, the commit was not accepted: try LITECORD_GOLIVE_GROUP=channel, then LITECORD_GOLIVE_DAVE_CHECK=0."
            );
        }
        // A keyframe request that had to wait for the one-per-second rule.
        if self.keyframes.take_due(now) && self.sending != Sending::Nothing {
            (self.deps.request_keyframe)();
            (self.deps.wake_sender)();
        }
        let (keepalive, idle_report) = (self.tuning().keepalive, self.tuning().switches.idle_report);
        if let Some(media) = self.media.as_mut().filter(|m| m.transport.is_some()) {
            if media.silence_at.is_some_and(|at| now >= at) {
                if let Some(size) = media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump) {
                    self.stats.bytes += size as u64;
                    self.wire.on_sent(size);
                }
                media.opening_silence = media.opening_silence.saturating_sub(1);
                media.silence_at = (media.opening_silence > 0).then(|| now + OPUS_FRAME);
            } else if media.silence_at.is_none() && now.saturating_duration_since(media.last_sent) >= keepalive {
                // Keeps the path (and the NAT mapping) open while nothing else is sent.
                if media.send_opus(&rtp::OPUS_SILENCE, now, &mut self.dump).is_none() {
                    media.last_sent = now;
                } else if idle_report {
                    media.send_audio_report(now);
                }
            }
        }
        self.publish_rate(now);
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
        let estimate = self.wire.estimate(now);
        info!(
            "[GO LIVE] {:.0} s: {} frames in, {} sent, {} dropped, {} keyframes, {} packets, {:.1} Mbit/s on the wire of {:.1} allowed (encoder at most {:.1}), audio {} packets, RTX {}, PLI {} FIR {} NACK {}{}, RR loss {}, rtt {}, REMB {}, frames {}, {} send errors, {} watching, DAVE {}",
            seconds,
            s.frames_in,
            s.frames_sent,
            s.dropped,
            s.keyframes,
            s.packets,
            s.bytes as f64 * 8.0 / seconds / 1e6,
            self.wire.wire() as f64 / 1e6,
            self.wire.asked() as f64 / 1e6,
            s.audio_packets,
            s.rtx,
            s.pli,
            s.fir,
            s.nack,
            if s.nack == 0 && s.keyframes_waited == 0 {
                String::new()
            } else {
                // What was asked for and what was done about it.
                format!(
                    " ({} video packets and {} audio asked again, {} audio resent, {} not kept any more; {} keyframe requests waited for the one-a-second rule)",
                    s.nack_video, s.nack_audio, s.audio_resent, s.nack_missed, s.keyframes_waited
                )
            },
            s.loss.map_or("none reported".to_string(), |l| format!("{:.1} %", l as f64 / 10.0)),
            s.round_trip.map_or("unknown".to_string(), |t| format!("{} ms", t.as_millis())),
            match estimate {
                Some(bitrate) => format!("{:.1} Mbit/s ({} reports)", bitrate as f64 / 1e6, s.estimates),
                None => "none".to_string(),
            },
            match s.gap {
                // How evenly the share hands pictures over, and how long the pacer keeps
                // the last packet of one back.
                Some((least, most)) => format!(
                    "offered {:.1} to {:.1} ms apart ({}) and sent {} apart, last packet up to {:.1} ms after its frame was offered",
                    least.as_secs_f64() * 1e3,
                    most.as_secs_f64() * 1e3,
                    match self.media.as_ref().and_then(|m| m.even.as_ref()) {
                        Some(even) if even.engaged() => "on the even grid",
                        Some(_) => "not the share's pace: stamped as they come",
                        None => "stamped as they come",
                    },
                    match s.left {
                        Some((_, least, most)) if least <= most => format!("{:.1} to {:.1} ms", least.as_secs_f64() * 1e3, most.as_secs_f64() * 1e3),
                        _ => "(too few)".to_string(),
                    },
                    s.late.as_secs_f64() * 1e3
                ),
                None => "too few to space".to_string(),
            },
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
        // (Over already; a deletion still owed goes on being tried.)
        if self.phase == Phase::Off {
            return;
        }
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
                done.push("STREAM_DELETE could not be sent yet (the main gateway is not connected): it is tried again until it is");
                self.owe_delete(key);
            }
            self.deleted = true;
        }
        if let Some(writer) = self.io.writer.take() {
            let _ = writer.send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })));
            drop(writer);
            // The last messages get a moment to leave, and the server one to say goodbye. (A
            // socket dropped with something unread in it is reset, and a reset can cost the
            // other side what it had not read yet: the cleared video, the close.) Nobody
            // waits for this thread.
            let (writer_task, mut reader) = (self.io.writer_task.take(), self.io.reader.take());
            let _ = tokio::time::timeout(self.tuning().close_linger, async {
                if let Some(task) = writer_task {
                    let _ = task.await;
                }
                if let Some(reader) = reader.as_mut() {
                    while let Some(Ok(message)) = reader.next().await {
                        if message.is_close() {
                            break;
                        }
                    }
                }
            })
            .await;
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
            if !(self.deps.gateway_send)(signal::delete(&key)) {
                warn!("[GO LIVE] STREAM_DELETE could not be sent yet (the main gateway is not connected): it is tried again until it is");
                self.owe_delete(key);
            }
            self.deleted = true;
        }
        warn!("[GO LIVE] The Discord stream ended: {failure:?}. The share itself goes on.");
        self.end(Some(failure));
    }

    /// Discord was not told that the stream is over: until it is, the stream would stay
    /// announced in the voice channel with nothing behind it.
    fn owe_delete(&mut self, key: String) {
        let now = Instant::now();
        self.owed_delete = Some((key, now + DELETE_RETRY_EVERY, now + self.tuning().delete_retry_for));
    }

    /// Another stream is requested for this account (it has the same key): a deletion
    /// still owed for this one would delete that one.
    fn forget_owed(&mut self) {
        if self.owed_delete.take().is_some() {
            info!("[GO LIVE] A new stream is requested: the deletion still owed for the old one is dropped");
        }
    }

    fn end(&mut self, failure: Option<Failure>) {
        self.close_socket();
        self.reconnect_at = None;
        self.io.socket = None;
        self.media = None;
        self.sending = Sending::Nothing;
        self.shared.wanted.store(false, Ordering::Relaxed);
        self.phase = Phase::Off;
        // (With a deletion still owed the thread stays for it; `on_time` ends it.)
        self.finished = self.owed_delete.is_none();
        self.shared.ended.store(true, Ordering::Release);
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
