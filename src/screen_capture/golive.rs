//! Go Live: the share as a Discord stream, so that people on the official Discord clients
//! can watch it in the voice channel. A third way out for the share's access units, next to
//! the peer-to-peer sender and the web share (`web_share`); those two are untouched by it.
//!
//! What happens, in the order of the log (every line starts with `[GO LIVE]`):
//! 1. the main gateway is asked for a stream (opcode 18) once the capture has a picture,
//!    and answers with STREAM_CREATE and STREAM_SERVER_UPDATE (`signal`);
//! 2. a second voice-gateway connection is made to that server, version 8 (`wire`,
//!    `session`), with its own UDP socket (`udp`) and transport cipher (`crypto`);
//! 3. the stream's own DAVE group is set up (`dave`); nothing is sent, and the share does
//!    not encode for Discord, until somebody watches;
//! 4. each access unit is prepared (`h264`), end-to-end encrypted, split into RTP packets
//!    (`rtp`) and sent, paced; the server's RTCP feedback (`rtcp`) asks for keyframes and
//!    retransmissions and sets how much may be sent (`rate`), which the share's send loop
//!    reads as a cap on what it asks of its encoder.
//!
//! Watching other people's Discord streams is not built yet; `depacketize` and the parsing
//! halves of `rtcp`, `crypto` and `signal` are where it starts.
//!
//! This file is the module's public face. Everything here may be called from any thread and
//! returns at once.

pub mod crypto;
pub mod dave;
pub mod depacketize;
pub mod h264;
pub mod rate;
pub mod rtcp;
pub mod rtp;
pub mod session;
pub mod signal;
pub mod udp;
pub mod wire;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use log::{error, info, warn};

/// The highest bitrate announced to Discord for a share, and so the most its packets may
/// add up to on the wire (what a browser viewer gets too). The media server's own estimate
/// stopped at 9 Mbit/s in the first live runs, and the community documentation shows an
/// official client announcing 9 Mbit/s for a 60 FPS stream at source resolution.
pub const MAX_BITRATE: u32 = 8_000_000;

/// The share being streamed: what opcode 12 announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The share has system or window audio.
    pub audio: bool,
}

impl Params {
    /// The parameters of a share started with the dialog's preset (`res`: 1080, 720 or 480
    /// lines), as `ScreenCaptureManager::start` reads them.
    pub fn for_share(res: i32, fps: i32, audio: bool) -> Self {
        let (width, height) = match res {
            1080 => (1920, 1080),
            480 => (854, 480),
            _ => (1280, 720),
        };
        Self { width, height, fps: fps.clamp(15, 60) as u32, audio }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    /// No Discord stream.
    #[default]
    Off,
    /// Waiting for the capture to have a picture.
    Armed,
    /// The main gateway was asked for the stream.
    Creating,
    /// Connecting to the stream's own gateway and media server.
    Connecting,
    /// Connected and announced; nobody watches, nothing is sent.
    Waiting,
    /// Somebody watches: video flows.
    Live,
    /// The stream gateway connection dropped and is being resumed.
    Reconnecting,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Armed => "armed",
            Phase::Creating => "creating",
            Phase::Connecting => "connecting",
            Phase::Waiting => "waiting",
            Phase::Live => "live",
            Phase::Reconnecting => "reconnecting",
        }
    }

    /// What the UI shows: 0 off, 1 connecting, 2 live on Discord.
    pub fn ui_state(self) -> i32 {
        match self {
            Phase::Off => 0,
            Phase::Waiting | Phase::Live => 2,
            _ => 1,
        }
    }
}

/// Why there is no Discord stream (the share itself goes on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Not in a voice channel: a stream belongs to one.
    NotInVoice,
    /// Discord refused the stream, with its reason.
    Refused(String),
    /// The main gateway did not answer the request.
    NoAnswer,
    /// The connection to the stream was lost for good: a close code, or what failed.
    Lost(String),
    /// Discord ended the stream, with its reason.
    Ended(String),
}

impl Failure {
    /// What the user is told, in the language of the interface. Discord's own words (a
    /// reason, a close code) are shown as they came, cut to what a reason looks like.
    pub fn notice(&self) -> String {
        let text = crate::utils::i18n_ui::ui_text();
        let shown = |words: &str| words.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ')).take(48).collect::<String>();
        match self {
            Failure::NotInVoice => text.golive_err_voice.to_string(),
            Failure::Refused(reason) => text.golive_err_refused.replace("{}", &shown(reason)),
            Failure::NoAnswer => text.golive_err_no_answer.to_string(),
            Failure::Lost(what) => text.golive_err_lost.replace("{}", &shown(what)),
            Failure::Ended(reason) => text.golive_ended.replace("{}", &shown(reason)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub phase: Phase,
    /// How many people Discord says are watching.
    pub viewers: usize,
    /// Media is end-to-end encrypted (DAVE) when it flows.
    pub e2ee: bool,
    /// Why the stream ended, when it did not end by `stop`.
    pub failure: Option<Failure>,
    pub stream_key: String,
}

/// The account's voice state, as the main gateway last reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Voice {
    session_id: String,
    guild_id: Option<String>,
    channel_id: String,
}

// Only start and stop change these. A worker that was retired publishes into its own
// `Shared`, never into a newer stream's.
static HAS_SESSION: AtomicBool = AtomicBool::new(false);
static SESSION: Mutex<Option<session::Handle>> = Mutex::new(None);
static VOICE: Mutex<Option<Voice>> = Mutex::new(None);
/// The running share a Discord stream can be made of (`None`: no share, or a camera's).
static SHARE: Mutex<Option<Params>> = Mutex::new(None);
type StatusCallback = Arc<dyn Fn() + Send + Sync>;
static STATUS_CALLBACK: Mutex<Option<StatusCallback>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `callback` runs (on some thread) whenever `status()` may have changed.
pub fn on_status_change(callback: impl Fn() + Send + Sync + 'static) {
    *lock(&STATUS_CALLBACK) = Some(Arc::new(callback));
}

fn status_changed() {
    let callback = lock(&STATUS_CALLBACK).clone();
    if let Some(callback) = callback {
        callback();
    }
}

/// Starts streaming the running share on Discord. The stream is requested once the capture
/// has a picture; everything after that shows in `status()`.
pub fn start(params: Params) -> Result<(), Failure> {
    let mut session = lock(&SESSION);
    if session.as_ref().is_some_and(|s| !s.finished()) {
        return Ok(());
    }
    let user_id = crate::gateway::get_my_user_id();
    let voice = lock(&VOICE).clone().filter(|_| user_id != 0 && crate::gateway::is_connected_to_voice());
    let Some(voice) = voice else {
        warn!("[GO LIVE] Not started: this account is not in a voice channel (a Discord stream belongs to one)");
        return Err(Failure::NotInVoice);
    };
    let switches = session::Switches::from_env();
    let tuning = session::Tuning {
        max_bitrate: crate::video_settings::get_stream_max_bitrate_bps().min(MAX_BITRATE).min(switches.max_kbps.map_or(u32::MAX, |kbps| kbps * 1000)),
        switches,
        ..session::Tuning::default()
    };
    let version = tuning.switches.gateway_version;
    let channel = voice.channel_id.clone();
    let deps = session::Deps {
        gateway_send: Arc::new(crate::gateway::send_payload),
        url_for: Arc::new(move |endpoint| wire::url(endpoint, version)),
        request_keyframe: Arc::new(super::request_intra_keyframe),
        wake_sender: Arc::new(super::manager::wake_tx),
        recognised_users: Arc::new(move || crate::gateway::get_voice_channel_participants(&channel).into_iter().map(|(user, _)| user).collect()),
        tuning,
    };
    let context = session::Context { user_id, session_id: voice.session_id, guild_id: voice.guild_id, channel_id: voice.channel_id };
    // (A stream that ended and still owes Discord its deletion has the key of the one
    // requested now: that deletion must not be sent any more.)
    if let Some(old) = session.as_ref() {
        old.forget();
    }
    match session::spawn(deps, context, params, session::Shared::new(status_changed)) {
        Ok(handle) => {
            *session = Some(handle);
            HAS_SESSION.store(true, Ordering::Release);
            drop(session);
            status_changed();
            Ok(())
        }
        Err(e) => {
            error!("[GO LIVE] Not started: the stream's thread could not be created ({e})");
            Err(Failure::Lost("thread".into()))
        }
    }
}

/// A screen or window share started: the Go Live button can make a Discord stream of it.
pub fn share_started(share: Params) {
    *lock(&SHARE) = Some(share);
}

/// The share ended (or none is running): its Discord stream ends with it.
pub fn share_ended() {
    *lock(&SHARE) = None;
    stop();
}

/// The Go Live button: ends the Discord stream when there is one (or one is on its way),
/// and makes one of the running share otherwise.
pub fn toggle() -> Result<(), Failure> {
    if status().phase != Phase::Off {
        stop();
        return Ok(());
    }
    let share = *lock(&SHARE);
    match share {
        Some(share) => start(share),
        None => {
            info!("[GO LIVE] Not started: no screen or window share is running");
            Ok(())
        }
    }
}

/// Ends the Discord stream, if there is one. The share is not touched.
pub fn stop() {
    HAS_SESSION.store(false, Ordering::Release);
    let retired = lock(&SESSION).take();
    if let Some(handle) = retired {
        if !handle.finished() {
            handle.stop();
        }
        // (Dropped here: the worker finishes on its own, nobody waits for it.)
    }
    status_changed();
}

pub fn status() -> Status {
    // (With no stream there is nothing unencrypted either.)
    lock(&SESSION).as_ref().map_or_else(|| Status { e2ee: true, ..Status::default() }, |s| s.shared.status())
}

/// The failure that ended the stream, for the user to be told once.
pub fn take_notice() -> Option<Failure> {
    lock(&SESSION).as_ref().and_then(|s| s.shared.take_notice())
}

/// The share's send loop has a picture source (called on its every turn).
pub fn capture_ready() {
    if HAS_SESSION.load(Ordering::Relaxed) {
        if let Some(s) = lock(&SESSION).as_ref() {
            s.shared.capture_ready();
        }
    }
}

/// Somebody watches on Discord: the share encodes for them and offers its access units.
pub fn wanted() -> bool {
    HAS_SESSION.load(Ordering::Relaxed) && lock(&SESSION).as_ref().is_some_and(|s| s.shared.wanted())
}

/// What the share's encoder may be asked for at most while somebody watches on Discord, in
/// bits per second: what was announced to Discord and what its media server estimates it
/// can take (REMB), less what packets add to the encoder's bytes.
pub fn rate_cap() -> u32 {
    if !HAS_SESSION.load(Ordering::Relaxed) {
        return MAX_BITRATE;
    }
    lock(&SESSION).as_ref().map_or(MAX_BITRATE, |s| s.shared.encoder_cap())
}

/// How often the share makes a keyframe by itself while somebody watches on Discord.
/// `None`: its own rule (every 2 s). Official senders make about one a minute and answer
/// requests, which this stream gets too (`LITECORD_GOLIVE_KEYINT`).
pub fn keyframe_interval() -> Option<std::time::Duration> {
    if !HAS_SESSION.load(Ordering::Relaxed) {
        return None;
    }
    lock(&SESSION).as_ref().and_then(|s| s.shared.keyframe_interval())
}

/// One access unit of the share, as the encoder made it (Annex B).
pub fn offer_frame(annex_b: &[u8]) {
    if HAS_SESSION.load(Ordering::Relaxed) {
        if let Some(s) = lock(&SESSION).as_ref() {
            s.offer_frame(annex_b);
        }
    }
}

/// The video loss Discord's media server reported in the last three seconds, in permille.
pub fn worst_loss() -> Option<u16> {
    if !HAS_SESSION.load(Ordering::Relaxed) {
        return None;
    }
    lock(&SESSION).as_ref().and_then(|s| s.shared.worst_loss())
}

/// 10 ms of the share's audio (the mono loopback mix, never the microphone).
pub fn offer_audio(pcm: &[i16], sample_rate: u32) {
    if HAS_SESSION.load(Ordering::Relaxed) {
        if let Some(s) = lock(&SESSION).as_ref() {
            s.offer_audio(pcm, sample_rate);
        }
    }
}

/// A STREAM_* dispatch event of the main gateway (`t`, and its `d`).
pub fn on_gateway_event(t: &str, d: &serde_json::Value) {
    let Some(event) = signal::parse_event(t, d) else {
        if signal::EVENTS.contains(&t) {
            warn!("[GO LIVE] {t} without the fields it should have: ignored");
        }
        return;
    };
    let session = lock(&SESSION);
    match session.as_ref().filter(|s| !s.finished()) {
        Some(s) => s.event(event),
        // (What Discord says about a stream after it ended here, such as its answer to the
        // delete request: for the log only. Never the token of a server update.)
        None => info!(
            "[GO LIVE] {t} for {} with no Discord stream running here{}",
            event.stream_key(),
            match &event {
                signal::Event::Delete { reason, unavailable, .. } => format!(": reason={reason}{}", if *unavailable { ", unavailable" } else { "" }),
                signal::Event::Create { rtc_server_id, viewer_ids, paused, .. } => format!(": rtc_server_id={rtc_server_id} viewers={} paused={paused}", viewer_ids.len()),
                signal::Event::Update { viewer_ids, paused, .. } => format!(": viewers={} paused={paused}", viewer_ids.len()),
                signal::Event::ServerUpdate { endpoint, .. } => format!(": endpoint={}", endpoint.as_deref().unwrap_or("null")),
            }
        ),
    }
}

/// The account's own voice state changed (every VOICE_STATE_UPDATE of this user):
/// `session_id` and the channel it is in; `gateway_session` is the main gateway's session
/// id, to tell in the log whether the voice state is this client's.
pub fn note_voice_state(session_id: Option<&str>, guild_id: Option<&str>, channel_id: Option<&str>, gateway_session: Option<&str>) {
    let state = match (session_id, channel_id) {
        (Some(session), Some(channel)) if !session.is_empty() && !channel.is_empty() => {
            Some(Voice { session_id: session.to_string(), guild_id: guild_id.filter(|g| !g.is_empty()).map(str::to_string), channel_id: channel.to_string() })
        }
        _ => None,
    };
    {
        let mut voice = lock(&VOICE);
        // Leaving a channel of another server than the one we are in is not our leaving.
        if state.is_none() && voice.as_ref().is_some_and(|v| guild_id.is_some() && v.guild_id.as_deref() != guild_id) {
            return;
        }
        if *voice != state {
            match &state {
                Some(v) => info!(
                    "[GO LIVE] Voice context: channel {}, session {}..., matches the gateway session: {}",
                    v.channel_id,
                    v.session_id.chars().take(8).collect::<String>(),
                    gateway_session == Some(v.session_id.as_str())
                ),
                None => info!("[GO LIVE] Voice context: not in a voice channel"),
            }
            *voice = state;
        }
    }
    if let Some(s) = lock(&SESSION).as_ref() {
        s.voice_channel(channel_id);
    }
}
