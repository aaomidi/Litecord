// "Open in browser" (screen_capture::web_link), end to end in one process.
//
// A "streamer" and a "viewer" endpoint talk over loopback UDP through the code the app's
// receive loop calls (`handle_request`, `request_due`, `handle_reply`), with the real
// packet layout, pair-key derivation, AES-GCM sealing, key binding (`watch::admit`) and
// rate limits. What the loop takes from the rest of the app is injected: whether the
// requester is in the voice channel, the setting, and web sharing itself (`FakeWebShare`
// makes links of the real shape without contacting any server).
//
// Usage: test_web_link_request [-v] [--real-web-share]
//   -v                 also prints the app's log lines
//   --real-web-share   afterwards, the same request against the app's own web sharing in
//                      its manual offer/answer mode (LITECORD_WEB_MANUAL=1: no signalling
//                      server; it asks public STUN servers for its address, as the app
//                      does), with a WebRTC peer in this process standing in for the browser

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use litecord::screen_capture::crypto::{encrypt_signaling_payload, Identity};
use litecord::screen_capture::types::{set_my_rx_port, MAGIC, OP_WATCH_ACK, OP_WEB_LINK_REPLY, OP_WEB_LINK_REQ};
use litecord::screen_capture::web_link::{self, Endpoint, HostEvent, HostTiming, Outcome, Status, WebShare};
use litecord::screen_capture::{watch, web_share};

const CID: u64 = 777_000_000_000_000_001;
const STREAMER: u64 = 100_000_000_000_000_001;
const VIEWER: u64 = 200_000_000_000_000_002;
const OTHER: u64 = 300_000_000_000_000_003;

const IDLE_STOP: Duration = Duration::from_millis(1500);
const RETRY_AFTER_FAILURE: Duration = Duration::from_millis(1500);
const LEFT_STOP: Duration = Duration::from_millis(300);

// ---- Log capture: nothing the app logs may contain a link ------------------------------

static LOG_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static VERBOSE: AtomicBool = AtomicBool::new(false);

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let line = format!("[{}] {}", record.level(), record.args());
        if VERBOSE.load(Ordering::Relaxed) {
            println!("      log: {line}");
        }
        LOG_LINES.lock().unwrap().push(line);
    }
    fn flush(&self) {}
}

// ---- Web sharing stand-in --------------------------------------------------------------

#[derive(Default)]
struct FakeWebShare {
    on: Mutex<Option<String>>,
    viewers: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    fail: AtomicBool,
    start_takes: Mutex<Duration>,
    /// What the next start returns instead of a real-shaped link.
    next_link: Mutex<Option<String>>,
    /// Every link ever made, to look for them in the logs and on the wire.
    made: Mutex<Vec<String>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn real_shaped_link() -> String {
    format!("{}{}&key={}", web_share::link_prefix(), hex(&rand::random::<[u8; 16]>()), hex(&rand::random::<[u8; 32]>()))
}

impl FakeWebShare {
    fn is_on(&self) -> bool {
        self.on.lock().unwrap().is_some()
    }
    /// As if the person streaming had pressed "Share on web" (main.rs brackets its start
    /// with these two calls).
    fn start_by_hand(&self) -> String {
        web_link::starting_by_hand(false);
        let link = real_shaped_link();
        self.made.lock().unwrap().push(link.clone());
        *self.on.lock().unwrap() = Some(link.clone());
        web_link::starting_by_hand(true);
        link
    }
    fn reset(&self) {
        *self.on.lock().unwrap() = None;
        self.viewers.store(0, Ordering::Relaxed);
        self.starts.store(0, Ordering::Relaxed);
        self.stops.store(0, Ordering::Relaxed);
        self.fail.store(false, Ordering::Relaxed);
        *self.start_takes.lock().unwrap() = Duration::ZERO;
        *self.next_link.lock().unwrap() = None;
    }
}

impl WebShare for FakeWebShare {
    fn start(&self) -> std::io::Result<String> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(*self.start_takes.lock().unwrap());
        if self.fail.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("servidor de teste indisponível"));
        }
        let link = self.next_link.lock().unwrap().take().unwrap_or_else(real_shaped_link);
        self.made.lock().unwrap().push(link.clone());
        *self.on.lock().unwrap() = Some(link.clone());
        Ok(link)
    }
    fn stop(&self) {
        if self.on.lock().unwrap().take().is_some() {
            self.stops.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn link(&self) -> Option<String> {
        self.on.lock().unwrap().clone()
    }
    fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Relaxed)
    }
}

// ---- The two endpoints and the wire between them ---------------------------------------

/// What the wire does to a datagram on its way in.
#[derive(Default)]
struct Wire {
    /// Requests the streamer never sees (lost on the way), counted down.
    lose_requests: usize,
    /// Answers the viewer never sees, counted down.
    lose_replies: usize,
    /// The streamer hears nothing at all (unreachable).
    streamer_deaf: bool,
    /// The streamer is an upstream client: it reads the packet and has no arm for the opcode.
    streamer_is_upstream: bool,
    /// Applied to each answer before the viewer gets it (an attacker on the path).
    tamper: Option<fn(&mut Vec<u8>)>,
}

struct World {
    streamer_sock: UdpSocket,
    viewer_sock: UdpSocket,
    streamer_addr: SocketAddr,
    viewer_addr: SocketAddr,
    streamer_id: Identity,
    viewer_id: Identity,
    web: Arc<FakeWebShare>,
    web_dyn: Arc<dyn WebShare>,
    members: HashSet<u64>,
    allowed: bool,
    wire: Wire,
    outcomes: mpsc::Receiver<(u64, Outcome)>,
    host_events: mpsc::Receiver<HostEvent>,
    /// Every datagram that crossed the wire, both ways.
    seen: Vec<Vec<u8>>,
    requests_sent: usize,
    requests_heard: usize,
    replies_sent: usize,
    /// What the viewer's app would have handed to the browser.
    opened: Vec<String>,
    passed: usize,
    failed: Vec<String>,
}

fn op_of(pkt: &[u8]) -> u8 {
    if pkt.len() >= 25 && &pkt[0..4] == MAGIC { pkt[8] } else { 0 }
}

impl World {
    fn streamer(&self) -> Endpoint<'_> {
        Endpoint::with_identity(1001, CID, STREAMER, &self.streamer_id)
    }
    fn viewer(&self) -> Endpoint<'_> {
        Endpoint::with_identity(2002, CID, VIEWER, &self.viewer_id)
    }

    fn check(&mut self, what: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("   ok    {what}");
        } else {
            self.failed.push(what.to_string());
            println!("   FAIL  {what}");
        }
    }

    /// A new stream: new key, no key bound, sender-side state forgotten.
    fn new_stream(&mut self) {
        web_link::cancel();
        watch::end_session();
        self.web.reset();
        watch::start_session();
        self.members = [STREAMER, VIEWER, OTHER].into_iter().collect();
        self.allowed = true;
        self.wire = Wire::default();
        self.requests_sent = 0;
        self.requests_heard = 0;
        self.replies_sent = 0;
        while self.outcomes.try_recv().is_ok() {}
        while self.host_events.try_recv().is_ok() {}
        self.drain_sockets();
    }

    fn drain_sockets(&mut self) {
        let mut buf = [0u8; 2048];
        while self.streamer_sock.recv_from(&mut buf).is_ok() {}
        while self.viewer_sock.recv_from(&mut buf).is_ok() {}
    }

    /// One turn of both receive loops.
    fn turn(&mut self) {
        let mut buf = [0u8; 2048];
        // Streamer, top of its loop (every 2 s in the app): did somebody with the link leave?
        web_link::stop_if_someone_left(self.web_dyn.as_ref(), |uid| self.members.contains(&uid));
        // Viewer, top of its loop: a copy of the request when one is due.
        if web_link::asking() {
            if let Some((peer, pkt)) = web_link::request_due(&self.viewer()) {
                assert_eq!(peer, STREAMER);
                self.requests_sent += 1;
                self.seen.push(pkt.clone());
                let _ = self.viewer_sock.send_to(&pkt, self.streamer_addr);
            }
        }
        // Streamer.
        while let Ok((len, src)) = self.streamer_sock.recv_from(&mut buf) {
            let pkt = buf[..len].to_vec();
            if len < 25 || self.wire.streamer_deaf {
                continue;
            }
            if op_of(&pkt) == OP_WEB_LINK_REQ && self.wire.lose_requests > 0 {
                self.wire.lose_requests -= 1;
                continue;
            }
            self.requests_heard += 1;
            if self.wire.streamer_is_upstream {
                // An upstream receive loop: opcodes 1 to 11 have an arm, the rest fall
                // into `_ => {}`.
                continue;
            }
            let uid = u64::from_be_bytes(pkt[17..25].try_into().unwrap());
            let in_channel = self.members.contains(&uid);
            if let Some(reply) = web_link::handle_request(&pkt, src, &self.streamer(), in_channel, self.allowed, &self.web_dyn) {
                self.replies_sent += 1;
                self.seen.push(reply.clone());
                let _ = self.streamer_sock.send_to(&reply, src);
            }
        }
        // Viewer.
        while let Ok((len, _)) = self.viewer_sock.recv_from(&mut buf) {
            // (The 1-byte datagram that wakes the receive thread lands here too.)
            if len < 25 {
                continue;
            }
            let mut pkt = buf[..len].to_vec();
            if op_of(&pkt) == OP_WEB_LINK_REPLY && self.wire.lose_replies > 0 {
                self.wire.lose_replies -= 1;
                continue;
            }
            if let Some(tamper) = self.wire.tamper {
                tamper(&mut pkt);
            }
            web_link::handle_reply(&pkt, &self.viewer());
        }
    }

    /// Runs both loops until the viewer's request ends, or `limit`.
    fn outcome_within(&mut self, limit: Duration) -> Option<Outcome> {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            self.turn();
            if let Ok((peer, outcome)) = self.outcomes.try_recv() {
                assert_eq!(peer, STREAMER);
                // What main.rs does with it.
                if let Outcome::Link(link) = &outcome {
                    self.opened.push(link.clone());
                }
                return Some(outcome);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }

    fn run_for(&mut self, time: Duration) {
        let until = Instant::now() + time;
        while Instant::now() < until {
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The viewer clicks "Open in browser" and waits for how it ends.
    fn ask(&mut self) -> Option<Outcome> {
        assert!(web_link::request(STREAMER), "a request was still pending");
        self.outcome_within(Duration::from_secs(30))
    }

    /// The last request the viewer put on the wire, as somebody reading the wire has it.
    fn last_request(&self) -> (Vec<u8>, [u8; 32], [u8; 16]) {
        let pkt = self.seen.iter().rev().find(|p| op_of(p) == OP_WEB_LINK_REQ).expect("no request on the wire").clone();
        (pkt.clone(), pkt[33..65].try_into().unwrap(), pkt[65..81].try_into().unwrap())
    }

    fn inject_to_viewer(&self, pkt: &[u8]) {
        let _ = self.streamer_sock.send_to(pkt, self.viewer_addr);
    }

    fn host_events(&self) -> Vec<HostEvent> {
        self.host_events.try_iter().collect()
    }
}

fn is_link(outcome: &Option<Outcome>) -> bool {
    matches!(outcome, Some(Outcome::Link(_)))
}

/// The app's own web sharing behind the same request: turned on by it, serving video to a
/// WebRTC peer, kept on while that peer is connected and turned off when nobody comes.
fn real_web_share(w: &mut World) {
    use openh264::encoder::{Encoder, EncoderConfig};
    use openh264::formats::{YUVSlices, YUVSource};
    use str0m::media::{Direction, MediaKind};
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, IceConnectionState, Input, Output, RtcConfig};

    println!("\n18. The app's own web sharing, turned on by a request (manual offer/answer mode, no server)");
    std::env::set_var("LITECORD_WEB_MANUAL", "1");
    w.new_stream();
    w.web_dyn = Arc::new(web_link::AppWebShare);
    let outcome = w.ask();
    let events = w.host_events();
    let page = events.iter().find_map(|e| match e {
        HostEvent::Started(link) => Some(link.clone()),
        _ => None,
    });
    w.check("the request turned it on, and it is remembered as started by a request",
        web_share::is_active() && web_link::is_auto_started() && events.first() == Some(&HostEvent::Starting) && page.is_some());
    w.check("this mode's link is a page on the streamer's own disk: the viewer is told \"unavailable\", not sent it",
        page.as_deref().is_some_and(|l| l.starts_with("file:///")) && outcome == Some(Outcome::Unavailable));

    // The browser's part: an offer for one video track, answered through `answer_manual`
    // (what the test page and scripts/perf/web_share_manual.ps1 do by copy and paste).
    let ip = litecord::screen_capture::types::get_local_lan_addresses(0).into_iter().map(|a| a.ip())
        .find(|ip| ip.is_ipv4() && !ip.is_loopback()).unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    let local = SocketAddr::new(ip, sock.local_addr().unwrap().port());
    let mut rtc = RtcConfig::new().clear_codecs().enable_h264(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    let (offer, pending) = change.apply().unwrap();
    let answered = web_share::answer_manual(offer).map(|answer| rtc.sdp_api().accept_answer(pending, answer).is_ok());
    w.check("the session that was turned on answers a browser's offer", answered == Ok(true));
    if answered != Ok(true) {
        web_share::stop();
        return;
    }

    // What the capture thread does: a picture every 33 ms while a browser is connected,
    // a keyframe whenever one is asked for.
    let (width, height) = (640usize, 360usize);
    let mut encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new()).unwrap();
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    let (mut connected, mut sent, mut received, mut decoded, mut size) = (false, 0u32, 0u32, 0u32, (0usize, 0usize));
    let mut buf = vec![0u8; 65536];
    let t = Instant::now();
    let mut next_frame = t;
    let mut on_while_watched = true;
    while t.elapsed() < Duration::from_secs(20) && !(decoded >= 60 && t.elapsed() > IDLE_STOP * 3) {
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(out)) => {
                    let _ = sock.send_to(&out.contents, out.destination);
                }
                Ok(Output::Event(Event::IceConnectionStateChange(IceConnectionState::Connected | IceConnectionState::Completed))) => connected = true,
                Ok(Output::Event(Event::MediaData(data))) => {
                    received += 1;
                    if let Ok(Some(picture)) = decoder.decode(&data.data) {
                        decoded += 1;
                        size = picture.dimensions();
                    }
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }
        let _ = sock.set_read_timeout(Some(Duration::from_millis(4)));
        match sock.recv_from(&mut buf) {
            Ok((len, source)) => {
                if let Ok(contents) = buf[..len].try_into() {
                    let _ = rtc.handle_input(Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }));
                }
            }
            Err(_) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
        if Instant::now() >= next_frame {
            next_frame += Duration::from_millis(33);
            if web_share::viewer_count() > 0 {
                if litecord::screen_capture::types::KEYFRAME_REQUESTED.swap(false, Ordering::Relaxed) {
                    encoder.force_intra_frame();
                }
                let shade = (sent * 3 % 200) as u8 + 16;
                let y: Vec<u8> = (0..width * height).map(|i| if (i % width) / 40 % 2 == 0 { shade } else { 255 - shade }).collect();
                let (u, v) = (vec![128u8; width * height / 4], vec![128u8; width * height / 4]);
                if let Ok(bits) = encoder.encode(&YUVSlices::new((&y, &u, &v), (width, height), (width, width / 2, width / 2))) {
                    web_share::offer_frame(&bits.to_vec());
                    sent += 1;
                }
            }
        }
        w.turn();
        on_while_watched &= !connected || web_share::is_active();
    }
    w.check(&format!("the stand-in browser connected (ICE + DTLS) and counts as a viewer: {}", web_share::viewer_count()), connected && web_share::viewer_count() == 1);
    w.check(&format!("it received and decoded the stream: {sent} pictures offered, {received} received, {decoded} decoded at {}x{}", size.0, size.1),
        decoded >= 60 && size == (width, height));
    w.check(&format!("with it connected, web sharing stayed on past the idle time ({:.1} s)", t.elapsed().as_secs_f64()),
        on_while_watched && web_share::is_active() && t.elapsed() > IDLE_STOP * 3 && web_link::is_auto_started());
    // The stream ends (what ScreenCaptureManager::stop does to web sharing).
    web_share::stop();
    watch::end_session();

    w.new_stream();
    let outcome = w.ask();
    w.check("a new stream, a new request: on again", web_share::is_active() && web_link::is_auto_started() && outcome == Some(Outcome::Unavailable));
    w.run_for(IDLE_STOP + Duration::from_millis(700));
    w.check("no browser came: the app's web sharing is off again after the idle time", !web_share::is_active() && !web_link::is_auto_started() && w.host_events().contains(&HostEvent::StoppedIdle));
    std::env::remove_var("LITECORD_WEB_MANUAL");
    w.web_dyn = w.web.clone();
}

fn main() {
    VERBOSE.store(std::env::args().any(|a| a == "-v"), Ordering::Relaxed);
    log::set_logger(&Capture).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let default_prefix = web_share::link_prefix();
    // The server every link must point at here. Nothing is ever sent to it.
    std::env::set_var("LITECORD_WEB_SIGNALING_URL", "wss://weblink-harness.invalid");
    let prefix = web_share::link_prefix();

    let streamer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let viewer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    streamer_sock.set_nonblocking(true).unwrap();
    viewer_sock.set_nonblocking(true).unwrap();
    // The viewer's wake-up datagram goes to its own socket here, not to an app that may
    // be running on this machine (port 50005).
    set_my_rx_port(viewer_sock.local_addr().unwrap().port());

    let (outcome_tx, outcomes) = mpsc::channel();
    let outcome_tx = Mutex::new(outcome_tx);
    web_link::on_outcome(move |peer, outcome| {
        let _ = outcome_tx.lock().unwrap().send((peer, outcome));
    });
    let (event_tx, host_events) = mpsc::channel();
    let event_tx = Mutex::new(event_tx);
    web_link::on_host_event(move |event| {
        let _ = event_tx.lock().unwrap().send(event);
    });
    web_link::set_host_timing(HostTiming { idle_stop: IDLE_STOP, idle_check: Duration::from_millis(100), retry_after_failure: RETRY_AFTER_FAILURE, left_stop: LEFT_STOP });

    let web = Arc::new(FakeWebShare::default());
    let mut w = World {
        streamer_addr: streamer_sock.local_addr().unwrap(),
        viewer_addr: viewer_sock.local_addr().unwrap(),
        streamer_sock,
        viewer_sock,
        streamer_id: Identity::generate(),
        viewer_id: Identity::generate(),
        web_dyn: web.clone(),
        web,
        members: HashSet::new(),
        allowed: true,
        wire: Wire::default(),
        outcomes,
        host_events,
        seen: Vec::new(),
        requests_sent: 0,
        requests_heard: 0,
        replies_sent: 0,
        opened: Vec::new(),
        passed: 0,
        failed: Vec::new(),
    };
    let other_id = Identity::generate();
    let started = Instant::now();
    println!("web_link harness: streamer {} <-> viewer {} over loopback UDP; links must start with {prefix}", w.streamer_addr, w.viewer_addr);

    // ------------------------------------------------------------------------------------
    println!("\n1. Happy path: web sharing off, the request turns it on");
    w.new_stream();
    let t = Instant::now();
    let outcome = w.ask();
    let took = t.elapsed();
    let link = w.web.link().unwrap_or_default();
    w.check("the viewer gets the link web sharing made", outcome == Some(Outcome::Link(link.clone())) && !link.is_empty());
    w.check(&format!("it arrives quickly ({} ms)", took.as_millis()), took < Duration::from_millis(1500));
    w.check("web sharing was started exactly once", w.web.starts.load(Ordering::Relaxed) == 1);
    w.check("it is remembered as started by a request", web_link::is_auto_started());
    let events = w.host_events();
    w.check("the streamer's window is told: starting, then on with the link", events == vec![HostEvent::Starting, HostEvent::Started(link.clone())]);
    let (request, _, _) = w.last_request();
    w.check("request layout: 81 bytes, LTPV, opcode 12, cid, viewer uid, target uid, key, id", {
        request.len() == 81 && &request[0..4] == MAGIC && request[8] == 12
            && request[9..17] == CID.to_be_bytes() && request[17..25] == VIEWER.to_be_bytes()
            && request[25..33] == STREAMER.to_be_bytes() && request[33..65] == w.viewer_id.public()
    });
    let reply = w.seen.iter().rev().find(|p| op_of(p) == OP_WEB_LINK_REPLY).unwrap().clone();
    w.check("answer layout: LTPV, opcode 13, cid, streamer uid, viewer uid, streamer key, sealed part", {
        &reply[0..4] == MAGIC && reply[8] == 13 && reply[9..17] == CID.to_be_bytes() && reply[17..25] == STREAMER.to_be_bytes()
            && reply[25..33] == VIEWER.to_be_bytes() && reply[33..65] == w.streamer_id.public()
            && reply.len() == 65 + 12 + 4 + 16 + 1 + link.len() + 16
    });
    let link_answer = reply.clone();

    println!("\n2. Asking again while it is on: same link, no restart");
    let outcome = w.ask();
    w.check("same link", outcome == Some(Outcome::Link(link.clone())));
    w.check("web sharing was not started again", w.web.starts.load(Ordering::Relaxed) == 1 && w.web.stops.load(Ordering::Relaxed) == 0);

    println!("\n3. Old answers and answers nobody asked for");
    w.inject_to_viewer(&link_answer);
    w.run_for(Duration::from_millis(100));
    w.check("an answer with no request pending is dropped", w.outcomes.try_recv().is_err());
    // A new request that the streamer never hears; meanwhile the earlier, genuine answer is sent again.
    w.wire.streamer_deaf = true;
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(200));
    for _ in 0..5 {
        w.inject_to_viewer(&link_answer);
    }
    let fresh = web_link::build_reply(&w.streamer(), VIEWER, &w.viewer_id.public(), &rand::random(), Status::Link, &real_shaped_link()).unwrap();
    w.inject_to_viewer(&fresh);
    let opened_before = w.opened.len();
    let outcome = w.outcome_within(Duration::from_secs(10));
    w.check("a replayed answer (the id of an earlier request) is not accepted", outcome == Some(Outcome::NoAnswer));
    w.check("nor a genuine-looking one with an id the viewer never sent; nothing was opened", w.opened.len() == opened_before);
    w.wire.streamer_deaf = false;

    println!("\n4. An answer from somebody the viewer did not ask");
    w.new_stream();
    w.wire.streamer_deaf = true;
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(200));
    let (_, viewer_public, id) = w.last_request();
    let other = Endpoint::with_identity(3003, CID, OTHER, &other_id);
    // Another peer in the channel answers under its own uid, even knowing the request id.
    w.inject_to_viewer(&web_link::build_reply(&other, VIEWER, &viewer_public, &id, Status::Link, &real_shaped_link()).unwrap());
    // It claims the streamer's uid without having seen the request (it has to guess the id).
    let as_streamer = Endpoint::with_identity(3003, CID, STREAMER, &other_id);
    w.inject_to_viewer(&web_link::build_reply(&as_streamer, VIEWER, &viewer_public, &rand::random(), Status::Link, &real_shaped_link()).unwrap());
    // Addressed to another viewer, and in another channel.
    w.inject_to_viewer(&web_link::build_reply(&w.streamer(), OTHER, &viewer_public, &id, Status::Link, &real_shaped_link()).unwrap());
    let elsewhere = Endpoint::with_identity(1001, CID + 1, STREAMER, &w.streamer_id);
    w.inject_to_viewer(&web_link::build_reply(&elsewhere, VIEWER, &viewer_public, &id, Status::Link, &real_shaped_link()).unwrap());
    let opened_before = w.opened.len();
    let outcome = w.outcome_within(Duration::from_secs(10));
    w.check("none of them is taken for the streamer's answer", outcome == Some(Outcome::NoAnswer) && w.opened.len() == opened_before);

    println!("\n5. An answer changed on the way");
    w.new_stream();
    w.wire.tamper = Some(|pkt| {
        let last = pkt.len() - 20;
        pkt[last] ^= 0x01;
    });
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(600));
    let answers = w.replies_sent;
    w.check("one flipped bit in the sealed part: not accepted", answers >= 1 && w.outcomes.try_recv().is_err() && web_link::asking());
    w.wire.tamper = Some(|pkt| pkt[40] ^= 0x80);
    w.run_for(Duration::from_millis(600));
    w.check("streamer key replaced: not accepted", w.replies_sent > answers && w.outcomes.try_recv().is_err() && web_link::asking());
    let answers = w.replies_sent;
    w.wire.tamper = Some(|pkt| pkt.truncate(pkt.len() - 9));
    w.run_for(Duration::from_millis(600));
    w.check("cut short: not accepted", w.replies_sent > answers && w.outcomes.try_recv().is_err() && web_link::asking());
    // Something else sealed with the same pair key: the stream key of an OP_WATCH_ACK.
    let (_, viewer_public, _) = w.last_request();
    let pair = w.streamer_id.pair_key(&viewer_public, STREAMER, VIEWER, CID);
    let mut ack_as_reply = reply[..65].to_vec();
    ack_as_reply.extend_from_slice(&encrypt_signaling_payload(&pair, &[7u8; 32]).unwrap());
    let mut padded = ack_as_reply.clone();
    padded.extend_from_slice(&[0u8; 40]);
    w.wire.tamper = None;
    w.wire.streamer_deaf = true;
    w.inject_to_viewer(&ack_as_reply);
    w.inject_to_viewer(&padded);
    let mut real_ack = ack_as_reply.clone();
    real_ack[8] = OP_WATCH_ACK;
    w.inject_to_viewer(&real_ack);
    w.run_for(Duration::from_millis(100));
    w.check("a stream key sealed with the same pair key does not pass for an answer", w.outcomes.try_recv().is_err() && web_link::asking());
    w.wire.streamer_deaf = false;
    let outcome = w.outcome_within(Duration::from_secs(10));
    w.check("the untouched answer that follows is accepted", outcome == Some(Outcome::Link(w.web.link().unwrap_or_default())));

    println!("\n6. A requester who is not in the voice channel, or under another key");
    w.new_stream();
    w.members.remove(&VIEWER);
    let outcome = w.ask();
    w.check("outside the channel: the streamer stays silent", outcome == Some(Outcome::NoAnswer) && w.replies_sent == 0 && w.requests_heard >= 6);
    w.check("and web sharing stays off", w.web.starts.load(Ordering::Relaxed) == 0 && !w.web.is_on());
    w.members.insert(VIEWER);
    let outcome = w.ask();
    w.check("back in the channel: served", is_link(&outcome));
    // Somebody else uses the viewer's uid with a key of their own.
    let impostor = Endpoint::with_identity(4004, CID, VIEWER, &other_id);
    w.run_for(Duration::from_millis(400)); // past the per-user pause, so silence is not the rate limit
    let (replies_before, heard_before) = (w.replies_sent, w.requests_heard);
    let _ = w.viewer_sock.send_to(&web_link::build_request(&impostor, STREAMER, &rand::random()), w.streamer_addr);
    w.run_for(Duration::from_millis(300));
    w.check("the same uid under another key gets nothing (key bound by the first request)", w.replies_sent == replies_before && w.requests_heard == heard_before + 1);
    // Control: the silence was the key, not the pause: the viewer's own key is answered right after.
    let _ = w.viewer_sock.send_to(&web_link::build_request(&w.viewer(), STREAMER, &rand::random()), w.streamer_addr);
    w.run_for(Duration::from_millis(100));
    w.check("while the viewer's own key is answered in the same moment", w.replies_sent == replies_before + 1);
    w.drain_sockets();
    // The binding is the one watch requests use.
    let bound = watch::on_watch(VIEWER, other_id.public(), w.viewer_addr, 0, true);
    w.check("and that key cannot watch either: the binding is shared with OP_WATCH", bound == Some(watch::Verdict::KeyMismatch));
    w.check("a link request did not put the viewer on the watch list (no video is sent to it)", watch::viewer_addrs().is_empty());

    println!("\n7. The setting is off");
    w.new_stream();
    w.allowed = false;
    let outcome = w.ask();
    w.check("refused, and web sharing is not started", outcome == Some(Outcome::Refused) && w.web.starts.load(Ordering::Relaxed) == 0);
    w.web.start_by_hand();
    let outcome = w.ask();
    w.check("refused even while web sharing is on by hand", outcome == Some(Outcome::Refused));

    println!("\n8. All three browser places taken");
    w.new_stream();
    w.web.start_by_hand();
    w.web.viewers.store(3, Ordering::Relaxed);
    let outcome = w.ask();
    w.check("the viewer is told it is full", outcome == Some(Outcome::Full));
    w.web.viewers.store(2, Ordering::Relaxed);
    let outcome = w.ask();
    w.check("with a place free again: the link", is_link(&outcome));

    println!("\n9. Lossy delivery");
    w.new_stream();
    w.wire.lose_requests = 3;
    w.wire.lose_replies = 2;
    let t = Instant::now();
    let outcome = w.ask();
    w.check(&format!("3 requests and 2 answers lost: the link still arrives ({} ms, {} copies sent)", t.elapsed().as_millis(), w.requests_sent), is_link(&outcome));
    w.check("web sharing was started once", w.web.starts.load(Ordering::Relaxed) == 1);

    println!("\n10. A link that is not a watch link of the configured server");
    w.new_stream();
    *w.web.next_link.lock().unwrap() = Some(format!("https://evil.example/watch#room={}&key={}", "a".repeat(32), "b".repeat(64)));
    let outcome = w.ask();
    w.check("streamer side: such a link is never given out", outcome == Some(Outcome::Unavailable) && w.opened.iter().all(|l| l.starts_with(&prefix)));
    // Viewer side: somebody who read the request (its id and the viewer's key are in the
    // clear) answers in the streamer's name with a link of their choice.
    let good_tail = format!("{}&key={}", "a".repeat(32), "b".repeat(64));
    let bad_links = [
        format!("https://evil.example/watch#room={good_tail}"),
        format!("http://weblink-harness.invalid/watch#room={good_tail}"),
        format!("https://weblink-harness.invalid.evil.example/watch#room={good_tail}"),
        format!("https://weblink-harness.invalid@evil.example/watch#room={good_tail}"),
        format!("https://evil.example/?{prefix}{good_tail}"),
        format!(" {prefix}{good_tail}"),
        format!("{prefix}{good_tail}&next=https://evil.example"),
        format!("{prefix}{}&key={}", "A".repeat(32), "b".repeat(64)),
        format!("{prefix}../../x?{}&key={}", "a".repeat(26), "b".repeat(64)),
        format!("{}/watch?x=1#room={good_tail}", prefix.trim_end_matches("/watch#room=")),
        "file:///etc/passwd".to_string(),
        "javascript:alert(1)".to_string(),
        String::new(),
    ];
    let mut refused = 0;
    let opened_before = w.opened.len();
    for bad in &bad_links {
        w.new_stream();
        w.wire.streamer_deaf = true;
        assert!(web_link::request(STREAMER));
        w.run_for(Duration::from_millis(60));
        let (_, viewer_public, id) = w.last_request();
        let forger = Endpoint::with_identity(5005, CID, STREAMER, &other_id);
        w.inject_to_viewer(&web_link::build_reply(&forger, VIEWER, &viewer_public, &id, Status::Link, bad).unwrap());
        if w.outcome_within(Duration::from_secs(10)) == Some(Outcome::BadLink) {
            refused += 1;
        } else {
            println!("         accepted or unanswered: {bad:?}");
        }
    }
    w.check(&format!("viewer side: {refused} of {} forged answers with a foreign link end as \"not opened\"", bad_links.len()), refused == bad_links.len() && w.opened.len() == opened_before);
    w.check("the same links, straight through the check", bad_links.iter().all(|l| !web_link::is_watch_link(l)) && web_link::is_watch_link(&format!("{prefix}{good_tail}")));
    w.check(&format!("a link of the default server ({default_prefix}...) is foreign here, and https"),
        default_prefix.starts_with("https://") && default_prefix.ends_with("/watch#room=") && !web_link::is_watch_link(&format!("{default_prefix}{good_tail}")));
    // The limit that stays (as for OP_WATCH_ACK): the same forger with a link of the right shape is believed.
    w.new_stream();
    w.wire.streamer_deaf = true;
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(60));
    let (_, viewer_public, id) = w.last_request();
    let forger = Endpoint::with_identity(5005, CID, STREAMER, &other_id);
    w.inject_to_viewer(&web_link::build_reply(&forger, VIEWER, &viewer_public, &id, Status::Link, &format!("{prefix}{good_tail}")).unwrap());
    let outcome = w.outcome_within(Duration::from_secs(10));
    println!("   note  known limit: a forger who read the request and answers with a watch link of the configured server is believed ({})",
        if is_link(&outcome) { "confirmed: the link can only be a watch page of that server" } else { "NOT reproduced" });

    println!("\n11. Rate limits");
    w.new_stream();
    w.web.start_by_hand();
    let me = Endpoint::with_identity(2002, CID, VIEWER, &w.viewer_id);
    let flood: Vec<Vec<u8>> = (0..300).map(|_| web_link::build_request(&me, STREAMER, &rand::random())).collect();
    let t = Instant::now();
    for (i, pkt) in flood.iter().enumerate() {
        let _ = w.viewer_sock.send_to(pkt, w.streamer_addr);
        if i % 10 == 9 {
            w.turn();
        }
    }
    w.run_for(Duration::from_millis(150));
    w.check(&format!("300 requests from one user in {} ms ({} heard): {} answered (one per 200 ms)", t.elapsed().as_millis(), w.requests_heard, w.replies_sent), w.requests_heard >= 290 && w.replies_sent >= 1 && w.replies_sent <= 3);
    // Many users at once: everybody together gets 30 answers a second.
    w.new_stream();
    w.web.start_by_hand();
    let crowd: Vec<(u64, Identity)> = (0..80u64).map(|i| (400_000_000_000_000_000 + i, Identity::generate())).collect();
    w.members.extend(crowd.iter().map(|(uid, _)| *uid));
    // (From eight addresses: one address alone gets 10 of the 30.)
    let homes: Vec<UdpSocket> = (2..10).map(|n| UdpSocket::bind(format!("127.0.0.{n}:0")).unwrap()).collect();
    let t = Instant::now();
    for (i, (uid, identity)) in crowd.iter().enumerate() {
        let _ = homes[i % homes.len()].send_to(&web_link::build_request(&Endpoint::with_identity(6006, CID, *uid, identity), STREAMER, &rand::random()), w.streamer_addr);
    }
    w.run_for(Duration::from_millis(200));
    w.check(&format!("80 users asking at once from 8 addresses ({} ms, {} heard): {} answered", t.elapsed().as_millis(), w.requests_heard, w.replies_sent), w.requests_heard == 80 && w.replies_sent == 30);
    w.run_for(Duration::from_millis(1000));
    let (heard_before, replies_before) = (w.requests_heard, w.replies_sent);
    for (uid, identity) in &crowd {
        let _ = homes[0].send_to(&web_link::build_request(&Endpoint::with_identity(6006, CID, *uid, identity), STREAMER, &rand::random()), w.streamer_addr);
    }
    w.run_for(Duration::from_millis(200));
    w.check(&format!("the same 80 from one address: {} answered", w.replies_sent - replies_before), w.requests_heard == heard_before + 80 && w.replies_sent == replies_before + 10);
    w.drain_sockets();

    // Asking, letting it stop, asking again: three starts per ten minutes of a stream.
    w.new_stream();
    let mut links = Vec::new();
    for round in 1..=3 {
        let outcome = w.ask();
        if let Some(Outcome::Link(l)) = outcome {
            links.push(l);
        }
        w.run_for(IDLE_STOP + Duration::from_millis(500));
        w.check(&format!("round {round}: served, then off by itself after the idle time"), links.len() == round && !w.web.is_on());
    }
    let outcome = w.ask();
    w.check("the fourth request in the window does not turn it on again", outcome == Some(Outcome::Unavailable) && w.web.starts.load(Ordering::Relaxed) == 3);
    w.check("each start had its own link", links.iter().collect::<HashSet<_>>().len() == 3);

    // A start that fails is not tried again for a while.
    w.new_stream();
    w.web.fail.store(true, Ordering::Relaxed);
    let outcome = w.ask();
    w.check("web sharing cannot start: the viewer is told", outcome == Some(Outcome::Unavailable) && w.web.starts.load(Ordering::Relaxed) == 1);
    w.check("the streamer's window: starting, then not started", w.host_events() == vec![HostEvent::Starting, HostEvent::NotStarted]);
    w.web.fail.store(false, Ordering::Relaxed);
    let outcome = w.ask();
    w.check("asked again at once: told the same without trying the server again", outcome == Some(Outcome::Unavailable) && w.web.starts.load(Ordering::Relaxed) == 1);
    w.run_for(RETRY_AFTER_FAILURE);
    let outcome = w.ask();
    w.check("after the pause it is tried again", is_link(&outcome) && w.web.starts.load(Ordering::Relaxed) == 2);

    println!("\n12. Turning itself off, and never what was turned on by hand");
    w.new_stream();
    let outcome = w.ask();
    w.check("started by a request", is_link(&outcome) && web_link::is_auto_started());
    let first_link = w.web.link();
    w.run_for(IDLE_STOP - Duration::from_millis(1100));
    let outcome = w.ask();
    w.check("asked again early: the same link, nothing restarted", outcome == first_link.clone().map(Outcome::Link) && w.web.starts.load(Ordering::Relaxed) == 1);
    w.run_for(IDLE_STOP - Duration::from_millis(900));
    w.check("still on before the idle time is over", w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 0);
    w.run_for(Duration::from_millis(900));
    w.check("asking again did not count as use: off one idle time after the link was first given", !w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 1);
    w.new_stream();
    let outcome = w.ask();
    let first_link = w.web.link();
    w.run_for(IDLE_STOP - Duration::from_millis(350));
    let again = w.ask();
    w.check("asked again just before it would go off: a new sharing with a new link, not a dying one",
        is_link(&outcome) && is_link(&again) && again != outcome && w.web.link() != first_link && w.web.starts.load(Ordering::Relaxed) == 2 && w.web.stops.load(Ordering::Relaxed) == 1);
    w.run_for(IDLE_STOP - Duration::from_millis(500));
    w.check("which has its whole idle time ahead", w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 1);
    w.web.viewers.store(1, Ordering::Relaxed);
    w.run_for(IDLE_STOP * 2);
    w.check("with a browser connected it stays on however long", w.web.is_on());
    w.web.viewers.store(0, Ordering::Relaxed);
    w.run_for(IDLE_STOP - Duration::from_millis(500));
    w.check("the browser left: not off at once", w.web.is_on());
    w.run_for(Duration::from_millis(1000));
    w.check("off after the idle time with no browser", !w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 2 && !web_link::is_auto_started());
    w.check("the streamer's window is told", w.host_events().contains(&HostEvent::StoppedIdle));

    w.new_stream();
    w.web.start_by_hand();
    let outcome = w.ask();
    w.check("turned on by hand: a request gets its link", is_link(&outcome) && !web_link::is_auto_started() && w.web.starts.load(Ordering::Relaxed) == 0);
    w.run_for(IDLE_STOP * 2 + Duration::from_millis(500));
    w.check("and it is never turned off from here", w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 0);

    w.new_stream();
    let outcome = w.ask();
    web_link::taken_over_by_hand();
    w.run_for(IDLE_STOP * 2 + Duration::from_millis(500));
    w.check("started by a request, then the streamer copied the link: theirs, stays on", is_link(&outcome) && w.web.is_on());

    w.new_stream();
    let outcome = w.ask();
    w.web.stop();
    web_link::stopped_by_hand();
    w.check("started by a request...", is_link(&outcome));
    let outcome = w.ask();
    w.check("...then turned off by hand: requests do not turn it back on", outcome == Some(Outcome::Refused) && !w.web.is_on() && w.web.starts.load(Ordering::Relaxed) == 1);
    w.web.start_by_hand();
    let outcome = w.ask();
    w.check("until the streamer turns it on again", is_link(&outcome));

    // The streamer presses the button while a request arrives: one start, theirs.
    w.new_stream();
    web_link::starting_by_hand(false);
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(700));
    w.check("while the streamer's own start is in progress no second one is made", w.web.starts.load(Ordering::Relaxed) == 0 && web_link::asking());
    let by_hand = w.web.start_by_hand();
    let outcome = w.outcome_within(Duration::from_secs(10));
    w.check("the request gets that link, and web sharing is not ours to turn off", outcome == Some(Outcome::Link(by_hand)) && !web_link::is_auto_started());

    println!("\n13. A slow start, and a streamer that never answers");
    w.new_stream();
    *w.web.start_takes.lock().unwrap() = Duration::from_millis(5000);
    let t = Instant::now();
    let outcome = w.ask();
    w.check(&format!("web sharing takes 5 s to connect: the viewer waits and gets the link ({} ms)", t.elapsed().as_millis()), is_link(&outcome) && t.elapsed() >= Duration::from_millis(5000));
    w.check("one start only, while the viewer kept asking", w.web.starts.load(Ordering::Relaxed) == 1 && w.requests_sent > 6);

    w.new_stream();
    w.wire.streamer_is_upstream = true;
    let t = Instant::now();
    let outcome = w.ask();
    let took = t.elapsed();
    w.check(&format!("an upstream client (no arm for opcode 12): \"no answer\" after {} ms", took.as_millis()), outcome == Some(Outcome::NoAnswer) && took >= Duration::from_millis(3400) && took < Duration::from_millis(4500));
    w.check(&format!("six copies of the request went out ({}), all heard, none answered", w.requests_sent), w.requests_sent == 6 && w.requests_heard == 6 && w.replies_sent == 0);
    w.check("a second click while one is pending is not a second request", {
        assert!(web_link::request(STREAMER));
        let second = web_link::request(STREAMER);
        web_link::cancel();
        !second
    });


    println!("\n14. Requests that carry nothing secret: keeping it on, and taking somebody's turn");
    // Somebody who saw the viewer's uid and public key (both travel in the clear) sends
    // requests under them from elsewhere; the answers are sealed for the viewer's key.
    let elsewhere = UdpSocket::bind("127.0.0.2:0").unwrap();
    w.new_stream();
    let outcome = w.ask();
    let t = Instant::now();
    let mut on_for = Duration::ZERO;
    while t.elapsed() < IDLE_STOP * 6 {
        let _ = elsewhere.send_to(&web_link::build_request(&w.viewer(), STREAMER, &rand::random()), w.streamer_addr);
        w.run_for(Duration::from_millis(250));
        if w.web.is_on() {
            on_for = t.elapsed();
        }
    }
    w.check(&format!("one request every 250 ms for {:.1} s with no browser: web sharing went off after {:.1} s and stayed off ({} starts, the limit)",
        t.elapsed().as_secs_f64(), on_for.as_secs_f64(), w.web.starts.load(Ordering::Relaxed)),
        is_link(&outcome) && !w.web.is_on() && on_for < IDLE_STOP * 4 && w.web.starts.load(Ordering::Relaxed) == 3);

    // The same from twenty ports of another address, a datagram every millisecond: the
    // viewer's own requests are still answered.
    w.new_stream();
    w.web.start_by_hand();
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flood_request = web_link::build_request(&w.viewer(), STREAMER, &rand::random());
    let flood = {
        let (stop, to) = (stop_flood.clone(), w.streamer_addr);
        std::thread::spawn(move || {
            let socks: Vec<UdpSocket> = (0..20).map(|_| UdpSocket::bind("127.0.0.3:0").unwrap()).collect();
            let mut sent = 0usize;
            while !stop.load(Ordering::Relaxed) {
                for sock in &socks {
                    let _ = sock.send_to(&flood_request, to);
                    sent += 1;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            sent
        })
    };
    w.run_for(Duration::from_millis(300));
    let mut served = 0;
    for _ in 0..8 {
        if is_link(&w.ask()) {
            served += 1;
        }
        w.run_for(Duration::from_millis(250));
    }
    stop_flood.store(true, Ordering::Relaxed);
    let flooded = flood.join().unwrap();
    w.check(&format!("under a flood in the viewer's name from another address ({flooded} datagrams, {} answered): {served} of 8 clicks got the link", w.replies_sent), served == 8);
    w.run_for(Duration::from_millis(300));
    w.drain_sockets();

    println!("\n15. A link must not outlive the right to watch");
    w.new_stream();
    let outcome = w.ask();
    let first_link = w.web.link();
    let t = Instant::now();
    w.members.remove(&VIEWER);
    w.run_for(LEFT_STOP / 2);
    w.members.insert(VIEWER);
    w.run_for(LEFT_STOP);
    w.check("the viewer got the link, dropped out of the voice channel for a moment and came back: still on", is_link(&outcome) && w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 0);
    w.members.remove(&VIEWER);
    w.run_for(LEFT_STOP + Duration::from_millis(200));
    w.check(&format!("the viewer left the call: the sharing a request turned on goes off, the link is dead ({} ms after it was given, well before the idle time)", t.elapsed().as_millis()),
        !w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 1 && !web_link::is_auto_started() && t.elapsed() < IDLE_STOP - Duration::from_millis(300));
    let outcome = w.ask();
    w.check("the one who left gets no new link", outcome == Some(Outcome::NoAnswer) && !w.web.is_on());
    let _ = elsewhere.send_to(&web_link::build_request(&Endpoint::with_identity(3003, CID, OTHER, &other_id), STREAMER, &rand::random()), w.streamer_addr);
    w.run_for(Duration::from_millis(300));
    w.check("somebody still in the call asks: a new sharing with a new link", w.web.is_on() && w.web.link() != first_link && w.web.starts.load(Ordering::Relaxed) == 2);
    w.new_stream();
    w.web.start_by_hand();
    let outcome = w.ask();
    w.members.remove(&VIEWER);
    w.run_for(LEFT_STOP * 3);
    w.check("turned on by hand: never turned off from here, whoever leaves", is_link(&outcome) && w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 0);

    println!("\n16. The setting switched off during the stream");
    w.new_stream();
    let outcome = w.ask();
    w.web.viewers.store(1, Ordering::Relaxed);
    w.allowed = false;
    web_link::disallowed(w.web_dyn.as_ref());
    w.check("what a request turned on goes off at once, browser connected or not", is_link(&outcome) && !w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 1 && !web_link::is_auto_started());
    let outcome = w.ask();
    w.check("and requests are refused", outcome == Some(Outcome::Refused) && w.web.starts.load(Ordering::Relaxed) == 1);
    w.new_stream();
    w.web.start_by_hand();
    web_link::disallowed(w.web_dyn.as_ref());
    w.check("what the streamer turned on by hand stays on", w.web.is_on() && w.web.stops.load(Ordering::Relaxed) == 0);
    w.new_stream();
    *w.web.start_takes.lock().unwrap() = Duration::from_millis(800);
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(300));
    w.allowed = false;
    web_link::disallowed(w.web_dyn.as_ref());
    let outcome = w.outcome_within(Duration::from_secs(10));
    w.run_for(Duration::from_millis(900));
    w.check("switched off while a request's start was still connecting: it does not stay on", outcome == Some(Outcome::Refused) && !w.web.is_on() && w.web.starts.load(Ordering::Relaxed) == 1 && !web_link::is_auto_started());

    println!("\n17. \"Starting\", then silence");
    w.new_stream();
    *w.web.start_takes.lock().unwrap() = Duration::from_millis(4300);
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(600));
    let told = w.replies_sent;
    // The stream ends or the streamer leaves before the link exists.
    w.wire.streamer_deaf = true;
    let t = Instant::now();
    let outcome = w.outcome_within(Duration::from_secs(30));
    w.check(&format!("the streamer said \"starting\" and went silent: given up {} ms after its last answer, as \"no answer\"", t.elapsed().as_millis()),
        told >= 1 && outcome == Some(Outcome::NoAnswer) && t.elapsed() < Duration::from_millis(4200));
    w.run_for(Duration::from_millis(900)); // that start ends before the next stream
    w.new_stream();
    w.wire.streamer_deaf = true;
    assert!(web_link::request(STREAMER));
    w.run_for(Duration::from_millis(60));
    let (_, viewer_public, id) = w.last_request();
    let forger = Endpoint::with_identity(5005, CID, STREAMER, &other_id);
    w.inject_to_viewer(&web_link::build_reply(&forger, VIEWER, &viewer_public, &id, Status::Starting, "").unwrap());
    let t = Instant::now();
    let outcome = w.outcome_within(Duration::from_secs(30));
    w.check(&format!("one forged \"starting\" holds the button for {} ms, not 25 s", t.elapsed().as_millis()), outcome == Some(Outcome::NoAnswer) && t.elapsed() < Duration::from_millis(4200));
    w.wire.streamer_deaf = false;

    if std::env::args().any(|a| a == "--real-web-share") {
        real_web_share(&mut w);
    }

    println!("\n19. Secrets");
    watch::end_session();
    let made = w.web.made.lock().unwrap().clone();
    let mut secrets: Vec<String> = Vec::new();
    for link in made.iter().chain(w.opened.iter()) {
        if let Some((room, key)) = link.strip_prefix(&prefix).and_then(|rest| rest.split_once("&key=")) {
            secrets.push(room.to_string());
            secrets.push(key.to_string());
        }
    }
    let logs = LOG_LINES.lock().unwrap().clone();
    let leaked = logs.iter().filter(|line| secrets.iter().any(|s| line.contains(s.as_str())) || line.contains("/watch#")).count();
    w.check(&format!("none of the {} log lines contains a link, a room or a key ({} links made)", logs.len(), made.len()), leaked == 0 && !logs.is_empty() && made.len() > 10);
    let on_wire = w.seen.iter().filter(|pkt| {
        let text = String::from_utf8_lossy(pkt);
        secrets.iter().any(|s| text.contains(s.as_str())) || text.contains("/watch") || text.contains("https:")
    }).count();
    w.check(&format!("none of the {} datagrams carries a link in the clear", w.seen.len()), on_wire == 0 && w.seen.len() > 50);
    w.check("everything the viewer would have opened is a watch link of the configured server", !w.opened.is_empty() && w.opened.iter().all(|l| web_link::is_watch_link(l)));

    println!("\n{} checks passed, {} failed, in {:.1} s", w.passed, w.failed.len(), started.elapsed().as_secs_f64());
    for what in &w.failed {
        println!("   FAILED: {what}");
    }
    std::process::exit(if w.failed.is_empty() { 0 } else { 1 });
}
