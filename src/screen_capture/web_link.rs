//! "Open in browser": a viewer asks the person streaming for a browser link over the P2P
//! socket instead of watching in the app, where decoding and drawing cost CPU.
//!
//! ```text
//! viewer -> sender  OP_WEB_LINK_REQ    header (25) + target uid (8) + viewer X25519 public key (32)
//!                                      + request id (16, random for every click)
//! sender -> viewer  OP_WEB_LINK_REPLY  header (25) + viewer uid (8) + sender public key (32)
//!                                      + AES-256-GCM under the pair key of:
//!                                        "WLR1" (4) + request id (16) + status (1) + link
//! ```
//!
//! The header and the rules are those of OP_WATCH / OP_WATCH_ACK: the sender answers only
//! someone in its voice channel, under the first public key that user id showed during the
//! stream (`watch::admit`), and the link travels only sealed with the pair key.
//!
//! The viewer accepts an answer only while it waits for one from that user id and only if
//! it carries the id of the request, so an answer nobody asked for, an old one sent again
//! or one from another peer is dropped. Whoever can read the request on its way, or gets
//! a copy of it (it goes to every address known for the sender, and a datagram or a
//! presence in the sender's name is enough to add one), can still forge an answer, as with
//! OP_WATCH_ACK: Discord gives a client no way to prove who it is. That is why the viewer
//! opens nothing but a link of the shape this app makes: https, the watch page of the
//! configured server, a room and a key in hex (`is_watch_link`). The forger's own stream
//! on that page is what remains possible.
//!
//! The link is all a browser needs, so the sharing a request turned on is turned off when
//! nobody uses it, when somebody who got the link leaves the call and when the person
//! streaming switches the setting off; what they turned on by hand is never touched.
//!
//! Clients without this ignore both opcodes; the viewer then gets no answer.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use log::{info, warn};

use super::crypto::{decrypt_signaling_payload, encrypt_signaling_payload, get_or_create_local_ecdh_keypair, pair_key, watch_pair_key, watch_public_key, Identity};
use super::types::{MAGIC, OP_WEB_LINK_REPLY, OP_WEB_LINK_REQ};
use super::watch::Verdict;

const HEADER_LEN: usize = 25;
const REQUEST_LEN: usize = HEADER_LEN + 8 + 32 + 16;
/// What the sealed part of an answer starts with, so nothing else sealed with the pair
/// key (the stream key of OP_WATCH_ACK) can pass for one.
const REPLY_TAG: &[u8; 4] = b"WLR1";
const SEALED_AT: usize = HEADER_LEN + 8 + 32;
/// Nonce (12) + tag, request id, status + GCM tag (16).
const REPLY_MIN_LEN: usize = SEALED_AT + 12 + 4 + 16 + 1 + 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// The link follows.
    Link = 0,
    /// Web sharing is being turned on: ask again in a moment.
    Starting = 1,
    /// The person streaming does not allow it (the setting, or they turned web sharing off).
    Refused = 2,
    /// Every browser slot is taken.
    Full = 3,
    /// Web sharing could not be turned on.
    Unavailable = 4,
}

/// Who speaks on the P2P socket: the ids every packet starts with and the key pair.
pub struct Endpoint<'a> {
    pub instance_id: u32,
    pub cid: u64,
    pub uid: u64,
    identity: Option<&'a Identity>,
}

impl Endpoint<'static> {
    /// The app: the process-wide key pair when streaming, the watch key pair when asking.
    pub fn local(instance_id: u32, cid: u64, uid: u64) -> Self {
        Self { instance_id, cid, uid, identity: None }
    }
}

impl<'a> Endpoint<'a> {
    /// An endpoint with a key pair of its own (several in one process, in tests).
    pub fn with_identity(instance_id: u32, cid: u64, uid: u64, identity: &'a Identity) -> Self {
        Self { instance_id, cid, uid, identity: Some(identity) }
    }

    fn public(&self) -> [u8; 32] {
        self.identity.map_or_else(get_or_create_local_ecdh_keypair, |i| i.public())
    }

    fn pair_key(&self, peer_public: &[u8; 32], peer_uid: u64) -> Option<[u8; 32]> {
        match self.identity {
            Some(i) => Some(i.pair_key(peer_public, self.uid, peer_uid, self.cid)),
            None => pair_key(peer_public, self.uid, peer_uid, self.cid),
        }
    }

    /// The viewer's side asks with the key it watches with: the streamer binds one key per
    /// user for a whole share (`watch::admit`), and OP_WATCH carries the watch key.
    fn viewer_public(&self) -> [u8; 32] {
        self.identity.map_or_else(watch_public_key, |i| i.public())
    }

    fn viewer_pair_key(&self, sender_public: &[u8; 32], sender_uid: u64) -> Option<[u8; 32]> {
        match self.identity {
            Some(i) => Some(i.pair_key(sender_public, self.uid, sender_uid, self.cid)),
            None => Some(watch_pair_key(sender_public, self.uid, sender_uid, self.cid)),
        }
    }

    fn header(&self, op: u8, capacity: usize) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(capacity);
        pkt.extend_from_slice(MAGIC);
        pkt.extend_from_slice(&self.instance_id.to_be_bytes());
        pkt.push(op);
        pkt.extend_from_slice(&self.cid.to_be_bytes());
        pkt.extend_from_slice(&self.uid.to_be_bytes());
        pkt
    }
}

/// The uid a packet of `op` in channel `cid` comes from.
fn sender_of(pkt: &[u8], op: u8, cid: u64) -> Option<u64> {
    if pkt.len() < HEADER_LEN + 8 || &pkt[0..4] != MAGIC || pkt[8] != op {
        return None;
    }
    if cid == 0 || u64::from_be_bytes(pkt[9..17].try_into().unwrap()) != cid {
        return None;
    }
    Some(u64::from_be_bytes(pkt[17..25].try_into().unwrap()))
}

pub fn build_request(viewer: &Endpoint, target_uid: u64, request_id: &[u8; 16]) -> Vec<u8> {
    let mut pkt = viewer.header(OP_WEB_LINK_REQ, REQUEST_LEN);
    pkt.extend_from_slice(&target_uid.to_be_bytes());
    pkt.extend_from_slice(&viewer.viewer_public());
    pkt.extend_from_slice(request_id);
    pkt
}

/// `link` is sent only with `Status::Link`.
pub fn build_reply(sender: &Endpoint, viewer_uid: u64, viewer_public: &[u8; 32], request_id: &[u8; 16], status: Status, link: &str) -> Option<Vec<u8>> {
    let mut plain = Vec::with_capacity(21 + link.len());
    plain.extend_from_slice(REPLY_TAG);
    plain.extend_from_slice(request_id);
    plain.push(status as u8);
    if status == Status::Link {
        plain.extend_from_slice(link.as_bytes());
    }
    let sealed = encrypt_signaling_payload(&sender.pair_key(viewer_public, viewer_uid)?, &plain)?;
    let mut pkt = sender.header(OP_WEB_LINK_REPLY, SEALED_AT + sealed.len());
    pkt.extend_from_slice(&viewer_uid.to_be_bytes());
    pkt.extend_from_slice(&sender.public());
    pkt.extend_from_slice(&sealed);
    Some(pkt)
}

/// A link this app would have made itself, for the server this installation uses: the
/// only thing a viewer hands to the browser and the only thing a sender gives out.
pub fn is_watch_link(link: &str) -> bool {
    is_watch_link_under(link, &super::web_share::link_prefix())
}

fn is_watch_link_under(link: &str, prefix: &str) -> bool {
    let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    prefix.starts_with("https://")
        && link.strip_prefix(prefix).and_then(|rest| rest.split_once("&key=")).is_some_and(|(room, key)| hex(room, 32) && hex(key, 64))
}

// ---- Sender: answering requests --------------------------------------------------------

/// Web sharing as the sender side needs it; the app's is `AppWebShare`.
pub trait WebShare: Send + Sync {
    /// Turns sharing on and returns the link. Blocks (it connects to the server), so it
    /// is never called on the receive thread.
    fn start(&self) -> std::io::Result<String>;
    fn stop(&self);
    /// The link while sharing is on.
    fn link(&self) -> Option<String>;
    /// Browsers connected now.
    fn viewers(&self) -> usize;
}

pub struct AppWebShare;

impl WebShare for AppWebShare {
    fn start(&self) -> std::io::Result<String> {
        super::web_share::start()
    }
    fn stop(&self) {
        super::web_share::stop()
    }
    fn link(&self) -> Option<String> {
        super::web_share::active_url()
    }
    fn viewers(&self) -> usize {
        super::web_share::viewer_count()
    }
}

/// One user id at one address is answered at most this often. A viewer repeats its
/// request (UDP loses packets) and sends each copy to every address it knows for the
/// sender; the extra copies and anything faster get no answer at all. (Per address: a
/// flood under somebody's user id from elsewhere must not take that person's turn.)
const ANSWER_GAP: Duration = Duration::from_millis(200);
/// Answers per second for everybody together...
const ANSWERS_PER_SECOND: usize = 30;
/// ...of which one IP address gets at most this many.
const ANSWERS_PER_ADDRESS: usize = 10;
/// Requests turn web sharing on at most this many times per window during a stream, so
/// asking, letting it stop and asking again cannot restart it in a loop.
const AUTO_STARTS_MAX: usize = 3;
const AUTO_STARTS_WINDOW: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
pub struct HostTiming {
    /// Web sharing that a request turned on goes off after this long with no browser
    /// connected and the link given to nobody new (asked for again in the last third of
    /// it, a new sharing replaces it). Two minutes: a link just handed over has to
    /// survive a browser starting cold, the page loading and the connection being set up
    /// (seconds, tens of seconds on a slow machine), and a viewer who reloads or loses
    /// the network for a moment should find the same link still valid. Leaving it on
    /// costs one idle connection to the server; turning it off too early costs a dead
    /// link. Past two minutes nobody is coming, and a live link nobody uses should not
    /// stay around.
    pub idle_stop: Duration,
    /// Longest sleep between two looks at the browser count.
    pub idle_check: Duration,
    /// After web sharing failed to start, requests are told so for this long instead of
    /// trying the server again.
    pub retry_after_failure: Duration,
    /// Web sharing that a request turned on goes off once somebody who was given its link
    /// has been out of the voice channel for this long: the link is all a browser needs,
    /// and it must not outlive the right to watch. Not at once: a voice state can drop
    /// out for a moment while Discord reconnects. Everybody else asks again and gets a
    /// new link.
    pub left_stop: Duration,
}

impl HostTiming {
    pub const DEFAULT: Self = Self {
        idle_stop: Duration::from_secs(120),
        idle_check: Duration::from_secs(30),
        retry_after_failure: Duration::from_secs(30),
        left_stop: Duration::from_secs(10),
    };
}

/// What the sender's own window should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A request is turning web sharing on.
    Starting,
    /// It is on, with this link.
    Started(String),
    /// It could not be turned on.
    NotStarted,
    /// It went off by itself: no browser for a while.
    StoppedIdle,
}

struct Host {
    /// The stream (its key) the rest belongs to; a new stream starts from nothing.
    stream: Option<[u8; 32]>,
    /// The answers of the last second: who, where to, when (only people in the channel
    /// get here).
    answered: Vec<(u64, SocketAddr, Instant)>,
    starting: bool,
    /// The setting was switched off while a request's start was in progress: what comes
    /// up is turned off again.
    withdrawn: bool,
    /// The person streaming is turning web sharing on themselves right now.
    hand_starting: bool,
    /// Counts what the person streaming did to web sharing by hand.
    hand_actions: u64,
    /// Web sharing is on because of a request, and nobody took it over by hand.
    auto_started: bool,
    /// Counts the times a request turned it on (the watcher of an earlier one ends).
    share_no: u64,
    /// The link that is being given out, and who got it.
    given_link: String,
    given: Vec<u64>,
    /// Since when somebody who got it has been out of the voice channel.
    gone_since: Option<Instant>,
    /// The person streaming turned web sharing off by hand during this stream.
    stopped_by_hand: bool,
    /// A browser was connected, or the link was given to someone for the first time.
    last_use: Option<Instant>,
    failed_at: Option<Instant>,
    auto_starts: VecDeque<Instant>,
}

impl Host {
    const fn new() -> Self {
        Self {
            stream: None,
            answered: Vec::new(),
            starting: false,
            withdrawn: false,
            hand_starting: false,
            hand_actions: 0,
            auto_started: false,
            share_no: 0,
            given_link: String::new(),
            given: Vec::new(),
            gone_since: None,
            stopped_by_hand: false,
            last_use: None,
            failed_at: None,
            auto_starts: VecDeque::new(),
        }
    }

    fn may_answer(&mut self, uid: u64, from: SocketAddr, now: Instant) -> bool {
        self.answered.retain(|(_, _, t)| now.duration_since(*t) < Duration::from_secs(1));
        if self.answered.len() >= ANSWERS_PER_SECOND
            || self.answered.iter().filter(|(_, a, _)| a.ip() == from.ip()).count() >= ANSWERS_PER_ADDRESS
            || self.answered.iter().any(|(u, a, t)| *u == uid && *a == from && now.duration_since(*t) < ANSWER_GAP)
        {
            return false;
        }
        self.answered.push((uid, from, now));
        true
    }

    /// Whether a request may turn web sharing on now.
    fn may_start(&mut self, timing: HostTiming, now: Instant) -> bool {
        if self.starting || self.hand_starting || self.failed_at.is_some_and(|t| now.duration_since(t) < timing.retry_after_failure) {
            return false;
        }
        while self.auto_starts.front().is_some_and(|t| now.duration_since(*t) >= AUTO_STARTS_WINDOW) {
            self.auto_starts.pop_front();
        }
        self.auto_starts.len() < AUTO_STARTS_MAX
    }

    /// Turns off the sharing a request turned on. The caller holds the lock: a request
    /// arriving now must not be given a link that is going away.
    fn stop_ours(&mut self, web: &dyn WebShare) {
        self.auto_started = false;
        self.given_link.clear();
        self.given.clear();
        self.gone_since = None;
        web.stop();
    }

    fn decide(&mut self, viewer: u64, allowed: bool, web: &Arc<dyn WebShare>, stream: [u8; 32], timing: HostTiming, now: Instant) -> (Status, String) {
        if !allowed || self.stopped_by_hand {
            return (Status::Refused, String::new());
        }
        if let Some(link) = web.link() {
            // The manual test mode makes a local file link: nothing a peer could open.
            if !is_watch_link(&link) {
                return (Status::Unavailable, String::new());
            }
            let browsers = web.viewers();
            if browsers >= super::web_share::MAX_WEB_VIEWERS {
                return (Status::Full, String::new());
            }
            if self.given_link != link {
                self.given_link = link.clone();
                self.given.clear();
                self.gone_since = None;
            }
            // Use is a browser connected or the link going to someone for the first time.
            // Asking again is not: the request carries nothing secret, and one datagram
            // every two minutes would keep the sharing on with nobody watching.
            let first = !self.given.contains(&viewer);
            if first || browsers > 0 {
                self.last_use = Some(now);
            }
            let left = self.last_use.map_or(Duration::ZERO, |t| timing.idle_stop.saturating_sub(now.duration_since(t)));
            if !self.auto_started || left >= timing.idle_stop / 3 || !self.may_start(timing, now) {
                if first {
                    self.given.push(viewer);
                }
                return (Status::Link, link);
            }
            // A link about to go off would be dead before a browser opened it: that
            // sharing ends here and a new one starts (within the limit on starts).
            self.stop_ours(web.as_ref());
        }
        // Never two starts at once: the one in progress (ours, or the person streaming
        // pressing the button) brings the link.
        if self.starting || self.hand_starting {
            return (Status::Starting, String::new());
        }
        if !self.may_start(timing, now) {
            return (Status::Unavailable, String::new());
        }
        self.auto_starts.push_back(now);
        self.starting = true;
        self.withdrawn = false;
        let (web, hand_actions) = (Arc::clone(web), self.hand_actions);
        let spawned = std::thread::Builder::new().name("web-link-start".into()).spawn(move || auto_start(web, stream, hand_actions, timing));
        if spawned.is_err() {
            self.starting = false;
            return (Status::Unavailable, String::new());
        }
        (Status::Starting, String::new())
    }
}

static HOST: Mutex<Host> = Mutex::new(Host::new());
static HOST_TIMING: Mutex<HostTiming> = Mutex::new(HostTiming::DEFAULT);
type HostCallback = Arc<dyn Fn(HostEvent) + Send + Sync>;
static HOST_CALLBACK: Mutex<Option<HostCallback>> = Mutex::new(None);

/// The state of the stream being sent now (forgotten when another one starts).
fn host(stream: Option<[u8; 32]>) -> MutexGuard<'static, Host> {
    let mut h = HOST.lock().unwrap_or_else(|e| e.into_inner());
    if h.stream != stream {
        *h = Host::new();
        h.stream = stream;
    }
    h
}

pub fn set_host_timing(timing: HostTiming) {
    *HOST_TIMING.lock().unwrap_or_else(|e| e.into_inner()) = timing;
}

pub fn on_host_event(callback: impl Fn(HostEvent) + Send + Sync + 'static) {
    *HOST_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(callback));
}

fn host_event(event: HostEvent) {
    let callback = HOST_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(callback) = callback {
        callback(event);
    }
}

/// The person streaming pressed the button that turns web sharing on (`done` false), and
/// that start ended, well or not (`done` true). What they turn on is theirs: it is never
/// turned off from here, and no request starts another one meanwhile.
pub fn starting_by_hand(done: bool) {
    let mut h = host(super::watch::session_key());
    h.hand_starting = !done;
    h.hand_actions += 1;
    h.auto_started = false;
    h.stopped_by_hand = false;
}

/// The person streaming copied the link to hand it out themselves: web sharing that a
/// request turned on is theirs from now on.
pub fn taken_over_by_hand() {
    let mut h = host(super::watch::session_key());
    h.hand_actions += 1;
    h.auto_started = false;
}

/// The person streaming turned web sharing off: requests do not turn it back on for the
/// rest of this stream (until they turn it on again themselves).
pub fn stopped_by_hand() {
    let mut h = host(super::watch::session_key());
    h.hand_actions += 1;
    h.auto_started = false;
    h.stopped_by_hand = true;
}

/// Whether web sharing is on only because somebody asked for a link.
pub fn is_auto_started() -> bool {
    host(super::watch::session_key()).auto_started
}

/// The person streaming switched the setting off: what a request turned on (or is turning
/// on) goes off with it. What they turned on themselves stays.
pub fn disallowed(web: &dyn WebShare) {
    let mut h = host(super::watch::session_key());
    h.withdrawn = h.starting;
    if h.auto_started {
        h.stop_ours(web);
        drop(h);
        info!("🌐 [ABRIR NO NAVEGADOR] Compartilhamento web ligado a pedido desligado: a opção foi desativada.");
    }
}

/// The receive loop calls this every couple of seconds while streaming. `in_channel`: the
/// uid is in my voice channel. When somebody who was given the link of a sharing that a
/// request turned on has left for `left_stop`, that sharing is turned off: whoever is
/// still in the call asks again and gets a new link, the one who left has a dead one.
pub fn stop_if_someone_left(web: &dyn WebShare, in_channel: impl Fn(u64) -> bool) {
    let Some(stream) = super::watch::session_key() else { return };
    let given = {
        let h = host(Some(stream));
        if !h.auto_started || h.given.is_empty() {
            return;
        }
        h.given.clone()
    };
    let all_here = given.iter().all(|uid| in_channel(*uid));
    let timing = *HOST_TIMING.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = host(Some(stream));
    if !h.auto_started || h.given != given {
        return;
    }
    if all_here {
        h.gone_since = None;
        return;
    }
    let now = Instant::now();
    if now.duration_since(*h.gone_since.get_or_insert(now)) < timing.left_stop {
        return;
    }
    h.stop_ours(web);
    drop(h);
    info!("🌐 [ABRIR NO NAVEGADOR] Compartilhamento web ligado a pedido desligado: quem recebeu o link saiu da chamada.");
}

/// A datagram with OP_WEB_LINK_REQ arrived from `from` while streaming. `in_channel`: the
/// uid it names is in my voice channel; `allowed`: the setting. Returns the answer to send
/// back to where the request came from, or nothing: outsiders and floods get silence.
pub fn handle_request(pkt: &[u8], from: SocketAddr, me: &Endpoint, in_channel: bool, allowed: bool, web: &Arc<dyn WebShare>) -> Option<Vec<u8>> {
    let viewer_uid = sender_of(pkt, OP_WEB_LINK_REQ, me.cid)?;
    if pkt.len() < REQUEST_LEN || viewer_uid == me.uid || u64::from_be_bytes(pkt[25..33].try_into().unwrap()) != me.uid {
        return None;
    }
    let viewer_public: [u8; 32] = pkt[33..65].try_into().unwrap();
    let request_id: [u8; 16] = pkt[65..81].try_into().unwrap();
    // The rule of a watch request, and the same key binding.
    match super::watch::admit(viewer_uid, viewer_public, in_channel)? {
        Verdict::Accepted { .. } => {}
        refused => {
            static REFUSED: crate::utils::logger::LogThrottle = crate::utils::logger::LogThrottle::new(5000);
            if let Some(n) = REFUSED.hit() {
                warn!("🚫 [ABRIR NO NAVEGADOR] Pedido de link de UID={} recusado: {:?} ({} desde o último aviso)", viewer_uid, refused, n);
            }
            return None;
        }
    }
    let stream = super::watch::session_key()?;
    let timing = *HOST_TIMING.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let (status, link) = {
        let mut h = host(Some(stream));
        if !h.may_answer(viewer_uid, from, now) {
            return None;
        }
        h.decide(viewer_uid, allowed, web, stream, timing, now)
    };
    if status == Status::Link {
        static GIVEN: crate::utils::logger::LogThrottle = crate::utils::logger::LogThrottle::new(5000);
        if let Some(n) = GIVEN.hit() {
            info!("🌐 [ABRIR NO NAVEGADOR] Link da transmissão entregue a UID={} ({} desde o último aviso).", viewer_uid, n);
        }
    }
    build_reply(me, viewer_uid, &viewer_public, &request_id, status, &link)
}

/// Turns web sharing on for a request, then watches over it until it goes off.
fn auto_start(web: Arc<dyn WebShare>, stream: [u8; 32], hand_actions: u64, timing: HostTiming) {
    host_event(HostEvent::Starting);
    let result = web.start();
    let same_stream = super::watch::session_key() == Some(stream);
    let mut h = HOST.lock().unwrap_or_else(|e| e.into_inner());
    let current = same_stream && h.stream == Some(stream);
    if current {
        h.starting = false;
    }
    match result {
        // Ours only if the person streaming did nothing to web sharing meanwhile.
        Ok(link) if current && h.hand_actions == hand_actions && !h.withdrawn => {
            h.auto_started = true;
            h.share_no += 1;
            let share_no = h.share_no;
            h.last_use = Some(Instant::now());
            drop(h);
            info!("🌐 [ABRIR NO NAVEGADOR] Compartilhamento web ligado a pedido de alguém na chamada.");
            host_event(HostEvent::Started(link));
            stop_when_idle(web, stream, share_no, timing);
        }
        Ok(_) => {
            // The stream ended while it connected, or the setting was switched off:
            // nothing may stay on. If instead the person streaming stepped in, what is on
            // is theirs and stays.
            if !current || h.withdrawn {
                web.stop();
            }
            drop(h);
            host_event(HostEvent::NotStarted);
        }
        // Somebody else is turning it on right now: the next request finds it on.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            drop(h);
            host_event(HostEvent::NotStarted);
        }
        Err(e) => {
            if current {
                h.failed_at = Some(Instant::now());
            }
            drop(h);
            warn!("⚠️ [ABRIR NO NAVEGADOR] Não foi possível ligar o compartilhamento web a pedido: {}", e);
            host_event(HostEvent::NotStarted);
        }
    }
}

/// Sleeps until the idle time could be over (or `idle_check`, while a browser is
/// connected) and turns off the sharing a request turned on once nobody used it for
/// `idle_stop`. Ends as soon as that sharing is no longer on or no longer ours.
fn stop_when_idle(web: Arc<dyn WebShare>, stream: [u8; 32], share_no: u64, timing: HostTiming) {
    let mut wait = timing.idle_check.min(timing.idle_stop);
    loop {
        std::thread::sleep(wait);
        let mut h = HOST.lock().unwrap_or_else(|e| e.into_inner());
        if h.stream != Some(stream) || !h.auto_started || h.share_no != share_no {
            return;
        }
        if web.link().is_none() {
            h.auto_started = false;
            return;
        }
        let now = Instant::now();
        if web.viewers() > 0 {
            h.last_use = Some(now);
        }
        let idle = h.last_use.map_or(timing.idle_stop, |t| now.duration_since(t));
        if idle >= timing.idle_stop {
            h.stop_ours(web.as_ref());
            drop(h);
            info!("🌐 [ABRIR NO NAVEGADOR] Compartilhamento web ligado a pedido desligado: nenhum navegador por {} s.", timing.idle_stop.as_secs());
            host_event(HostEvent::StoppedIdle);
            return;
        }
        wait = (timing.idle_stop - idle).min(timing.idle_check).max(Duration::from_millis(20));
    }
}

// ---- Viewer: asking -----------------------------------------------------------------------

/// How a request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A link that passed `is_watch_link`: open it.
    Link(String),
    Refused,
    Full,
    Unavailable,
    /// Nothing came back: an older client (it ignores the request), or no route to the peer.
    NoAnswer,
    /// The answer carried something that is not a watch link of the configured server.
    BadLink,
}

/// When the request goes out, in milliseconds after the click: six copies over 2.5 s,
/// because UDP loses packets and the path to a peer nobody was watching may need a moment.
const SEND_AT_MS: [u64; 6] = [0, 300, 700, 1200, 1800, 2500];
/// Without any answer, the request is given up this long after the click.
const NO_ANSWER: Duration = Duration::from_millis(3500);
/// Once the sender said it is turning web sharing on, the request is repeated this often...
const ASK_WHILE_STARTING: Duration = Duration::from_millis(300);
/// ...for at most this long (web sharing gives up connecting after 20 s), and only while
/// those answers keep coming: `NO_ANSWER` after the last one the request is given up (the
/// stream ended or the sender left meanwhile).
const STARTING_WAIT: Duration = Duration::from_secs(25);

struct Pending {
    peer: u64,
    id: [u8; 16],
    started: Instant,
    sends: usize,
    next_send: Instant,
    deadline: Instant,
    /// The sender answered that web sharing is starting.
    starting: bool,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Read by the receive loop on every turn.
static ASKING: AtomicBool = AtomicBool::new(false);
type OutcomeCallback = Arc<dyn Fn(u64, Outcome) + Send + Sync>;
static OUTCOME_CALLBACK: Mutex<Option<OutcomeCallback>> = Mutex::new(None);

pub fn on_outcome(callback: impl Fn(u64, Outcome) + Send + Sync + 'static) {
    *OUTCOME_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(callback));
}

fn finish(peer: u64, outcome: Outcome) {
    let callback = OUTCOME_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(callback) = callback {
        callback(peer, outcome);
    }
}

pub fn asking() -> bool {
    ASKING.load(Ordering::Relaxed)
}

/// The user asked to open `peer`'s stream in the browser. False while another request is
/// still waiting for its answer. The outcome arrives through `on_outcome`.
pub fn request(peer: u64) -> bool {
    let now = Instant::now();
    let id: [u8; 16] = rand::random();
    {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if peer == 0 || pending.is_some() {
            return false;
        }
        *pending = Some(Pending { peer, id, started: now, sends: 0, next_send: now, deadline: now + NO_ANSWER, starting: false });
        ASKING.store(true, Ordering::Relaxed);
    }
    // The receive thread sends the copies; this one wakes it when one is due (it may be
    // blocked in `recv` for a second) and gives the request up when its time is over.
    let timer = std::thread::Builder::new().name("web-link-request".into()).spawn(move || loop {
        super::types::wake_p2p_receiver();
        let wait = {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            let Some(p) = pending.as_ref().filter(|p| p.id == id) else { return };
            let now = Instant::now();
            if now >= p.deadline {
                let outcome = if p.starting && now >= p.started + STARTING_WAIT { Outcome::Unavailable } else { Outcome::NoAnswer };
                *pending = None;
                ASKING.store(false, Ordering::Relaxed);
                drop(pending);
                finish(peer, outcome);
                return;
            }
            p.next_send.min(p.deadline).saturating_duration_since(now)
        };
        // (Not in a call, the receive thread sends nothing and a copy stays due.)
        std::thread::sleep(wait.max(Duration::from_millis(50)));
    });
    if timer.is_err() {
        cancel();
        return false;
    }
    true
}

/// Left the call: nothing is expected any more, and nothing is reported.
pub fn cancel() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
    ASKING.store(false, Ordering::Relaxed);
}

/// The receive loop asks on every turn while `asking()`: the peer and the datagram to send
/// to every address known for it, when a copy is due.
pub fn request_due(me: &Endpoint) -> Option<(u64, Vec<u8>)> {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let p = pending.as_mut()?;
    let now = Instant::now();
    if now < p.next_send {
        return None;
    }
    p.sends += 1;
    p.next_send = if p.starting {
        now + ASK_WHILE_STARTING
    } else {
        // After the last copy, nothing more until the request is given up.
        // (A copy that went out late does not pull the following ones together.)
        SEND_AT_MS.get(p.sends).map_or(p.deadline + NO_ANSWER, |ms| (p.started + Duration::from_millis(*ms)).max(now + Duration::from_millis(100)))
    };
    Some((p.peer, build_request(me, p.peer, &p.id)))
}

/// A datagram with OP_WEB_LINK_REPLY arrived.
pub fn handle_reply(pkt: &[u8], me: &Endpoint) {
    if !asking() || pkt.len() < REPLY_MIN_LEN {
        return;
    }
    let Some(sender_uid) = sender_of(pkt, OP_WEB_LINK_REPLY, me.cid) else { return };
    if u64::from_be_bytes(pkt[25..33].try_into().unwrap()) != me.uid {
        return;
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    // Nothing asked, or asked of somebody else.
    let Some(p) = pending.as_mut().filter(|p| p.peer == sender_uid) else { return };
    let sender_public: [u8; 32] = pkt[33..65].try_into().unwrap();
    let Some(plain) = me.viewer_pair_key(&sender_public, sender_uid).and_then(|k| decrypt_signaling_payload(&k, &pkt[SEALED_AT..])) else { return };
    // Not an answer, or the answer to an earlier request.
    if plain.len() < 21 || &plain[..4] != REPLY_TAG || plain[4..20] != p.id {
        return;
    }
    let outcome = match plain[20] {
        s if s == Status::Link as u8 => match std::str::from_utf8(&plain[21..]) {
            Ok(link) if is_watch_link(link) => Outcome::Link(link.to_string()),
            _ => Outcome::BadLink,
        },
        s if s == Status::Starting as u8 => {
            let now = Instant::now();
            if !p.starting {
                p.starting = true;
                p.next_send = now + ASK_WHILE_STARTING;
            }
            p.deadline = (now + NO_ANSWER).min(p.started + STARTING_WAIT);
            return;
        }
        s if s == Status::Refused as u8 => Outcome::Refused,
        s if s == Status::Full as u8 => Outcome::Full,
        // Including a status a newer version may add.
        _ => Outcome::Unavailable,
    };
    *pending = None;
    ASKING.store(false, Ordering::Relaxed);
    drop(pending);
    if outcome == Outcome::BadLink {
        warn!("🚫 [ABRIR NO NAVEGADOR] A resposta de UID={} não trazia um link de transmissão válido deste servidor: nada foi aberto.", sender_uid);
    }
    finish(sender_uid, outcome);
}
