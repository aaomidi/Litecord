//! Stand-ins for the other side of a stream: the voice gateway's MLS half (an external
//! sender that proposes members, as Discord's does), a viewer (a DAVE session of its own),
//! libwebrtc's receiver, and, for the state machine, a scripted stream gateway and media
//! server on 127.0.0.1.

use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openmls::prelude::{tls_codec::Serialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::super::crypto::{self, Mode, Transport};
use super::super::depacketize::Depacketizer;
use super::super::h264::{find_nalus, normalize_sps, Sps, NAL_SPS, START_CODE};
use super::super::session::{self, Context, Deps, Handle, Shared, Tuning};
use super::super::signal::Event;
use super::super::{rtcp, udp, Params, Status};
use super::h264::webrtc_leaves_it_alone;

/// The MLS half of the voice gateway: it holds the external sender's key and turns key
/// packages into add proposals for a group.
pub struct MlsGateway {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    external_sender: ExternalSender,
}

impl MlsGateway {
    pub fn new() -> Self {
        let suite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer = SignatureKeyPair::new(suite.signature_algorithm()).unwrap();
        let external_sender = ExternalSender::new(signer.public().into(), BasicCredential::new(vec![0, 1, 1, 0]).into());
        Self { provider: OpenMlsRustCrypto::default(), signer, external_sender }
    }

    /// The payload of opcode 25.
    pub fn external_sender(&self) -> Vec<u8> {
        self.external_sender.tls_serialize_detached().unwrap()
    }

    /// The payload of opcode 27 (operation byte 0, then the vector of proposals) that adds
    /// the owners of `key_packages` to `group` at `epoch`.
    pub fn propose_add(&self, group: u64, epoch: u64, key_packages: &[&[u8]]) -> Vec<u8> {
        let mut messages = Vec::new();
        for key_package in key_packages {
            let key_package = KeyPackageIn::tls_deserialize_exact_bytes(key_package).unwrap().validate(self.provider.crypto(), ProtocolVersion::Mls10).unwrap();
            let proposal =
                ExternalProposal::new_add::<OpenMlsRustCrypto>(key_package, GroupId::from_slice(&group.to_be_bytes()), GroupEpoch::from(epoch), &self.signer, SenderExtensionIndex::new(0)).unwrap();
            messages.extend_from_slice(&proposal.tls_serialize_detached().unwrap());
        }
        [vec![0u8], VLBytes::new(messages).tls_serialize_detached().unwrap()].concat()
    }

    /// The payload of opcode 27 that removes the member at `leaf`.
    pub fn propose_remove(&self, group: u64, epoch: u64, leaf: u32) -> Vec<u8> {
        let proposal =
            ExternalProposal::new_remove::<OpenMlsRustCrypto>(LeafNodeIndex::new(leaf), GroupId::from_slice(&group.to_be_bytes()), GroupEpoch::from(epoch), &self.signer, SenderExtensionIndex::new(0)).unwrap();
        [vec![0u8], VLBytes::new(proposal.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()].concat()
    }
}

/// The key package in an opcode 26 message (opcode byte first), checked to be one.
pub fn key_package_of(message: &[u8]) -> Vec<u8> {
    assert_eq!(message[0], 26, "a key package message");
    KeyPackageIn::tls_deserialize_exact_bytes(&message[1..]).expect("a key package");
    message[1..].to_vec()
}

/// The commit and the welcome of an opcode 28 message (opcode byte first): the welcome,
/// when there is one, follows the commit directly.
pub fn split_commit_welcome(message: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    assert_eq!(message[0], 28, "a commit/welcome message");
    let body = &message[1..];
    let (commit, rest) = MlsMessageIn::tls_deserialize_bytes(body).expect("opcode 28 starts with an MLS message");
    assert!(matches!(commit.wire_format(), WireFormat::PublicMessage), "the commit is a public message");
    let commit_len = body.len() - rest.len();
    if rest.is_empty() {
        return (body.to_vec(), None);
    }
    Welcome::tls_deserialize_exact_bytes(rest).expect("what follows the commit is a welcome and nothing else");
    (body[..commit_len].to_vec(), Some(rest.to_vec()))
}

/// Somebody watching: a DAVE session of their own. `Old` is davey 0.1.4, the version that
/// talks to official clients in voice calls; it has no transitions, its keys change when it
/// processes a commit or welcome.
pub enum Viewer {
    New(davey02::DaveSession),
    Old(davey::DaveSession),
}

impl Viewer {
    /// A viewer waiting to be added to `group`, and its key package.
    pub fn new(old: bool, user: u64, group: u64, gateway: &MlsGateway) -> (Self, Vec<u8>) {
        let version = NonZeroU16::new(1).unwrap();
        if old {
            let mut session = davey::DaveSession::new(version, user, group, None).unwrap();
            session.set_external_sender(&gateway.external_sender()).unwrap();
            let key_package = session.create_key_package().unwrap();
            (Viewer::Old(session), key_package)
        } else {
            let mut session = davey02::DaveSession::new(version, user, group, None).unwrap();
            session.set_external_sender(&gateway.external_sender()).unwrap();
            let key_package = session.create_key_package().unwrap();
            (Viewer::New(session), key_package)
        }
    }

    /// Opcode 30 at the viewer.
    pub fn welcomed(&mut self, welcome: &[u8], transition: u16) {
        match self {
            Viewer::New(s) => {
                s.process_welcome(welcome, None).unwrap();
                s.prepare_transition(transition, 1).unwrap();
            }
            Viewer::Old(s) => s.process_welcome(welcome).unwrap(),
        }
    }

    /// Opcode 29 at the viewer.
    pub fn committed(&mut self, commit: &[u8], transition: u16) {
        match self {
            Viewer::New(s) => {
                assert!(s.process_commit(commit).unwrap());
                s.prepare_transition(transition, 1).unwrap();
            }
            Viewer::Old(s) => s.process_commit(commit).unwrap(),
        }
    }

    /// Opcode 22 at the viewer.
    pub fn execute(&mut self, transition: u16) {
        if let Viewer::New(s) = self {
            s.execute_transition(transition).unwrap();
        }
    }

    /// Opcode 27 at the viewer: its own commit and welcome for the proposals.
    pub fn proposals(&mut self, payload: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
        match self {
            Viewer::New(s) => {
                let made = s.process_proposals(davey02::ProposalsOperationType::APPEND, &payload[1..], None).unwrap().expect("a commit");
                (made.commit, made.welcome)
            }
            Viewer::Old(s) => {
                let made = s.process_proposals(davey::ProposalsOperationType::APPEND, &payload[1..], None).unwrap().expect("a commit");
                (made.commit, made.welcome)
            }
        }
    }

    pub fn decrypt_video(&mut self, from: u64, frame: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Viewer::New(s) => s.decrypt(from, davey02::MediaType::VIDEO, frame).map_err(|e| format!("{e:?}")),
            Viewer::Old(s) => s.decrypt(from, davey::MediaType::VIDEO, frame).map_err(|e| format!("{e:?}")),
        }
    }

    pub fn decrypt_audio(&mut self, from: u64, packet: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Viewer::New(s) => s.decrypt(from, davey02::MediaType::AUDIO, packet).map_err(|e| format!("{e:?}")),
            Viewer::Old(s) => s.decrypt(from, davey::MediaType::AUDIO, packet).map_err(|e| format!("{e:?}")),
        }
    }

    pub fn leaf(&self) -> u32 {
        match self {
            Viewer::New(s) => s.own_leaf_index().unwrap().u32(),
            Viewer::Old(s) => s.own_leaf_index().unwrap().u32(),
        }
    }

    pub fn epoch(&self) -> u64 {
        match self {
            Viewer::New(s) => s.epoch().unwrap().as_u64(),
            Viewer::Old(s) => s.epoch().unwrap().as_u64(),
        }
    }

    /// The 30-digit code of the call, as official clients show it.
    pub fn privacy_code(&self) -> Option<String> {
        match self {
            Viewer::New(s) => s.voice_privacy_code().map(str::to_string),
            Viewer::Old(s) => s.voice_privacy_code().map(str::to_string),
        }
    }
}

/// What libwebrtc's receiver does to a frame between the network and the DAVE decryptor:
/// every NAL unit behind a 4-byte start code, and an SPS it does not like rewritten.
/// Whether it likes one is read by `h264-reader`, not by the normaliser under test.
pub fn through_webrtc_receiver(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 8);
    for n in find_nalus(frame) {
        let nal = &frame[n.payload..n.payload + n.len];
        out.extend_from_slice(&START_CODE);
        if nal[0] & 0x1f == NAL_SPS && !webrtc_leaves_it_alone(nal) {
            match normalize_sps(nal) {
                Some(Sps::Rewritten(rewritten)) => out.extend_from_slice(&rewritten),
                other => panic!("an SPS libwebrtc would rewrite, and the rewriter says {other:?}"),
            }
        } else {
            out.extend_from_slice(nal);
        }
    }
    out
}

// ---- The scripted other side of the state machine --------------------------------------

/// How long a step of a script waits for the client before the test fails.
pub const WAIT: Duration = Duration::from_secs(10);
pub const USER: u64 = 852_892_297_661_906_993;
pub const GUILD: &str = "839502008108580904";
pub const VOICE_CHANNEL: &str = "850360749460553769";
pub const RTC_SERVER: u64 = 1_278_201_813_102_755_892;
pub const RTC_CHANNEL: u64 = 1_278_201_813_102_755_893;
/// The MLS group of the stream as the default guess has it.
pub const GROUP: u64 = RTC_SERVER - 1;
pub const VOICE_SESSION: &str = "9a8b7c6d5e4f3a2b1c0d";
pub const TOKEN: &str = "91f8016f34a5cd17";
pub const AUDIO_SSRC: u32 = 1001;
pub const VIDEO_SSRC: u32 = 1002;
pub const RTX_SSRC: u32 = 1003;

/// What the worker asked of the application.
#[derive(Default)]
pub struct Calls {
    pub keyframes: AtomicUsize,
    pub wakes: AtomicUsize,
    pub status_changes: AtomicUsize,
}

/// One stream worker with everything around it scripted: the main gateway is a channel,
/// the stream gateway a websocket server and the media server a UDP socket, all on 127.0.0.1.
pub struct Rig {
    pub handle: Handle,
    /// What the worker sent on the main gateway.
    pub main: mpsc::UnboundedReceiver<Value>,
    /// Whether the main gateway takes payloads (a connection is there).
    pub main_up: Arc<AtomicBool>,
    pub listener: TcpListener,
    pub sfu: Sfu,
    pub calls: Arc<Calls>,
    /// The members of the voice channel, for the DAVE check of proposed users.
    pub voice_users: Arc<Mutex<Vec<u64>>>,
}

impl Rig {
    pub async fn new(params: Params, tune: impl FnOnce(&mut Tuning)) -> Self {
        super::log_init();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (main_tx, main) = mpsc::unbounded_channel();
        let main_up = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(Calls::default());
        let voice_users = Arc::new(Mutex::new(Vec::new()));
        let mut tuning = Tuning {
            create_timeout: Duration::from_secs(8),
            unavailable_timeout: Duration::from_secs(8),
            handshake_timeout: Duration::from_secs(8),
            retry_delays: vec![Duration::from_millis(40); 5],
            discovery_wait: Duration::from_millis(400),
            close_linger: Duration::from_millis(300),
            // (The real ones are 1 s, 250 ms and 60 s: tests that are about them set them.)
            keyframe_every: Duration::from_millis(300),
            speaking_lead: Duration::from_millis(30),
            delete_retry_for: Duration::from_secs(5),
            // (As on Linux with the GPU encoders, whatever this test build is for.)
            rate_cut_needs_keyframe: true,
            ..Tuning::default()
        };
        // (Pictures are not held for their place on the even grid: most tests feed them one
        // at a time and wait for each. The tests about the grid turn the hold on.)
        tuning.switches.hold_ms = 0;
        tune(&mut tuning);
        let (up, c1, c2, c3, users) = (Arc::clone(&main_up), Arc::clone(&calls), Arc::clone(&calls), Arc::clone(&calls), Arc::clone(&voice_users));
        let deps = Deps {
            gateway_send: Arc::new(move |payload| up.load(Ordering::SeqCst) && main_tx.send(payload).is_ok()),
            // (The real one makes wss:// URLs only; see the wire tests.)
            url_for: Arc::new(|endpoint| Some(format!("ws://{endpoint}/?v=8"))),
            request_keyframe: Arc::new(move || {
                c1.keyframes.fetch_add(1, Ordering::SeqCst);
            }),
            wake_sender: Arc::new(move || {
                c2.wakes.fetch_add(1, Ordering::SeqCst);
            }),
            recognised_users: Arc::new(move || users.lock().unwrap().clone()),
            tuning,
        };
        let context = Context { user_id: USER, session_id: VOICE_SESSION.into(), guild_id: Some(GUILD.into()), channel_id: VOICE_CHANNEL.into() };
        let shared = Shared::new(move || {
            c3.status_changes.fetch_add(1, Ordering::SeqCst);
        });
        let handle = session::spawn(deps, context, params, shared).unwrap();
        Self { handle, main, main_up, listener, sfu: Sfu::new().await, calls, voice_users }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.handle.shared
    }

    pub fn status(&self) -> Status {
        self.handle.shared.status()
    }

    /// The stream key the gateway gives this stream.
    pub fn key(&self) -> String {
        format!("guild:{GUILD}:{VOICE_CHANNEL}:{USER}")
    }

    pub fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.listener.local_addr().unwrap().port())
    }

    pub fn create_event(&self) -> Event {
        Event::Create { stream_key: self.key(), rtc_server_id: RTC_SERVER, rtc_channel_id: Some(RTC_CHANNEL), region: "test".into(), viewer_ids: Vec::new(), paused: false }
    }

    pub fn server_event(&self) -> Event {
        Event::ServerUpdate { stream_key: self.key(), endpoint: Some(self.endpoint()), token: Some(TOKEN.into()) }
    }

    /// The next payload the worker sent on the main gateway; its opcode must be `op`.
    pub async fn main_op(&mut self, op: u64) -> Value {
        let payload = timeout(WAIT, self.main.recv()).await.expect("nothing was sent on the main gateway").expect("the main gateway channel closed");
        assert_eq!(payload["op"], op, "main gateway payload: {payload}");
        payload["d"].clone()
    }

    /// Nothing is waiting on the main gateway.
    pub fn main_idle(&mut self) -> bool {
        self.main.try_recv().is_err()
    }

    /// The capture has a picture, the main gateway is asked (opcode 18) and answers with
    /// both events: what every scenario starts with.
    pub async fn announce(&mut self) {
        self.shared().capture_ready();
        let create = self.main_op(18).await;
        assert_eq!(create, json!({ "type": "guild", "guild_id": GUILD, "channel_id": VOICE_CHANNEL, "preferred_region": null }));
        self.handle.event(self.create_event());
        self.handle.event(self.server_event());
    }

    /// The worker's connection to the stream gateway.
    pub async fn accept(&self) -> Conn {
        Conn::accept(&self.listener).await
    }

    /// Waits until the status is as `test` wants it.
    pub async fn status_when(&self, what: &str, test: impl Fn(&Status) -> bool) -> Status {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let status = self.status();
            if test(&status) {
                return status;
            }
            assert!(tokio::time::Instant::now() < deadline, "the status never became: {what} (it is {status:?})");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until `test` holds (for counters and flags the worker sets).
    pub async fn when(&self, what: &str, test: impl Fn(&Rig) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !test(self) {
            assert!(tokio::time::Instant::now() < deadline, "never happened: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub fn keyframes(&self) -> usize {
        self.calls.keyframes.load(Ordering::SeqCst)
    }

    /// The stream gateway's side of a handshake on `conn`, from Identify to the announced
    /// video (opcode 12). `external_sender`: the MLS gateway's package, sent before
    /// Session Description as the real gateway may. Returns the client's key package when
    /// `dave_version` is not 0.
    pub async fn handshake(&mut self, conn: &mut Conn, dave_version: u16, external_sender: Option<&[u8]>) -> Option<Vec<u8>> {
        let identify = conn.expect_json(0).await;
        assert_eq!(
            identify,
            json!({ "server_id": RTC_SERVER.to_string(), "user_id": USER.to_string(), "session_id": VOICE_SESSION, "token": conn.token, "video": true,
                "streams": [{ "type": "screen", "rid": "100", "quality": 100 }], "max_dave_protocol_version": 1 })
        );
        self.handshake_after_identify(conn, dave_version, external_sender, [AUDIO_SSRC, VIDEO_SSRC, RTX_SSRC]).await
    }

    pub async fn handshake_after_identify(&mut self, conn: &mut Conn, dave_version: u16, external_sender: Option<&[u8]>, ssrcs: [u32; 3]) -> Option<Vec<u8>> {
        conn.send_plain(8, json!({ "v": 8, "heartbeat_interval": conn.heartbeat_ms })).await;
        conn.send_json(
            2,
            json!({ "ssrc": ssrcs[0], "ip": "127.0.0.1", "port": self.sfu.port(), "modes": ["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize", "xsalsa20_poly1305_lite_rtpsize"],
                "experiments": ["fixed_keyframe_interval"], "streams": [{ "type": "video", "rid": "100", "quality": 100, "ssrc": ssrcs[1], "rtx_ssrc": ssrcs[2], "active": false }] }),
        )
        .await;
        let discovered = self.sfu.answer_discovery().await;
        assert_eq!(discovered, ssrcs[0], "IP discovery names the audio SSRC");
        let select = conn.expect_json(1).await;
        assert_eq!(select["protocol"], "udp");
        assert_eq!(select["data"], json!({ "address": "127.0.0.1", "port": self.sfu.client.unwrap().port(), "mode": "aead_aes256_gcm_rtpsize" }), "the discovered address, and AES-GCM picked by name");
        assert_eq!(select["codecs"][0], json!({ "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 }));
        assert_eq!(select["codecs"].as_array().unwrap().len(), 2, "only Opus and H264 are offered");
        assert_eq!(select["rtc_connection_id"].as_str().unwrap().len(), 36);
        conn.select = select;
        if let Some(package) = external_sender {
            conn.send_binary(25, package).await;
        }
        let key: [u8; 32] = rand::random();
        self.sfu.session(Mode::Aes256Gcm, &key, ssrcs);
        conn.send_json(
            4,
            json!({ "audio_codec": "opus", "video_codec": "H264", "media_session_id": "0123abcd", "mode": "aead_aes256_gcm_rtpsize", "secret_key": key.to_vec(), "dave_protocol_version": dave_version }),
        )
        .await;
        assert_eq!(conn.expect_json(5).await, json!({ "speaking": 0, "delay": 0, "ssrc": ssrcs[0] }), "Speaking with no flag comes before any media");
        let key_package = if dave_version > 0 { Some(key_package_of(&conn.expect_binary(26).await)) } else { None };
        let video = conn.expect_json(12).await;
        assert_eq!((video["audio_ssrc"].as_u64(), video["video_ssrc"].as_u64()), (Some(ssrcs[0] as u64), Some(ssrcs[1] as u64)));
        assert_eq!(video["streams"][0]["active"], true);
        conn.video = video;
        key_package
    }
}

/// What the client sent on the stream gateway.
#[derive(Debug)]
pub enum Got {
    Json(Value),
    Binary(Vec<u8>),
    /// The socket closed, with the close code when there was a close frame.
    Closed(Option<u16>),
}

/// The stream gateway's end of one websocket connection.
pub struct Conn {
    ws: WebSocketStream<TcpStream>,
    /// The sequence number of the last numbered message sent to the client.
    pub seq: u16,
    pub heartbeat_ms: u64,
    /// Heartbeats are answered (opcode 6); turned off to play a dead connection.
    pub answer_heartbeats: bool,
    /// Heartbeats received, and what the last one acknowledged.
    pub heartbeats: u32,
    pub last_ack: Option<i64>,
    /// The token Identify must carry.
    pub token: String,
    /// The `d` of the last opcodes 1 and 12 read by `handshake`.
    pub select: Value,
    pub video: Value,
}

impl Conn {
    pub async fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = timeout(WAIT, listener.accept()).await.expect("the client never connected to the stream gateway").unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.expect("a websocket handshake");
        // (Heartbeats are only answered while a test reads from the connection, and the
        // worker gives a connection up after two unanswered ones: the interval leaves a
        // test ten seconds to be busy elsewhere. Tests about heartbeats set their own.)
        Self { ws, seq: 0, heartbeat_ms: 5000, answer_heartbeats: true, heartbeats: 0, last_ack: None, token: TOKEN.into(), select: Value::Null, video: Value::Null }
    }

    /// The next message that is not a heartbeat.
    pub async fn next(&mut self) -> Got {
        loop {
            let message = timeout(WAIT, self.ws.next()).await.expect("the client sent nothing on the stream gateway");
            match message {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(text.as_str()).expect("JSON in a text frame");
                    if v["op"] == 3 {
                        self.heartbeats += 1;
                        self.last_ack = v["d"]["seq_ack"].as_i64();
                        assert!(v["d"]["t"].as_u64().is_some_and(|t| t > 1_600_000_000_000), "a heartbeat carries the time in milliseconds: {v}");
                        if self.answer_heartbeats {
                            let _ = self.ws.send(Message::Text(json!({ "op": 6, "d": { "t": v["d"]["t"] } }).to_string().into())).await;
                        }
                        continue;
                    }
                    return Got::Json(v);
                }
                Some(Ok(Message::Binary(data))) => return Got::Binary(data.to_vec()),
                Some(Ok(Message::Close(frame))) => {
                    // (The answer to the client's goodbye, as a server gives it.)
                    let _ = self.ws.flush().await;
                    return Got::Closed(Some(frame.map_or(1005, |f| u16::from(f.code))));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Got::Closed(None),
            }
        }
    }

    /// The next message must be JSON with opcode `op`; returns its `d`.
    pub async fn expect_json(&mut self, op: u64) -> Value {
        match self.next().await {
            Got::Json(v) if v["op"] == op => v["d"].clone(),
            other => panic!("expected opcode {op} from the client, got {other:?}"),
        }
    }

    /// The next message must be binary with opcode `op`; returns it whole (opcode first).
    pub async fn expect_binary(&mut self, op: u8) -> Vec<u8> {
        match self.next().await {
            Got::Binary(data) if data.first() == Some(&op) => data,
            other => panic!("expected binary opcode {op} from the client, got {other:?}"),
        }
    }

    /// The client closes the socket (with this code, when it says goodbye).
    pub async fn expect_close(&mut self) -> Option<u16> {
        match self.next().await {
            Got::Closed(code) => code,
            other => panic!("expected the client to close the stream gateway socket, got {other:?}"),
        }
    }

    /// Nothing but heartbeats comes for `time`.
    pub async fn expect_quiet(&mut self, time: Duration) {
        if let Ok(got) = timeout(time, self.next()).await {
            panic!("expected nothing from the client, got {got:?}");
        }
    }

    /// A numbered JSON message, as the version 8 gateway sends them.
    pub async fn send_json(&mut self, op: u8, d: Value) {
        self.seq += 1;
        self.ws.send(Message::Text(json!({ "op": op, "d": d, "seq": self.seq }).to_string().into())).await.unwrap();
    }

    /// A JSON message without a sequence number (Hello, heartbeat answers, Resumed).
    pub async fn send_plain(&mut self, op: u8, d: Value) {
        self.ws.send(Message::Text(json!({ "op": op, "d": d }).to_string().into())).await.unwrap();
    }

    /// A binary message: `[sequence: u16][opcode][payload]`.
    pub async fn send_binary(&mut self, op: u8, payload: &[u8]) {
        self.seq += 1;
        let mut frame = self.seq.to_be_bytes().to_vec();
        frame.push(op);
        frame.extend_from_slice(payload);
        self.ws.send(Message::Binary(frame.into())).await.unwrap();
    }

    /// Opcode 29 or 30: a transition id, then the commit or welcome.
    pub async fn send_transition(&mut self, op: u8, transition: u16, body: &[u8]) {
        self.send_binary(op, &[&transition.to_be_bytes()[..], body].concat()).await;
    }

    /// Closes the socket with a close code.
    pub async fn close(mut self, code: u16) {
        let _ = self.ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: "".into() })).await;
        // (Until the client has taken it.)
        let _ = timeout(Duration::from_millis(500), async { while let Some(Ok(_)) = self.ws.next().await {} }).await;
    }

    /// The connection breaks: no close frame.
    pub fn drop_tcp(self) {
        drop(self.ws);
    }
}

/// An RTP packet as the media server reads it after transport decryption.
#[derive(Clone, Debug)]
pub struct Rtp {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// The header extension elements: (id, data).
    pub extension: Vec<(u8, Vec<u8>)>,
    pub payload: Vec<u8>,
}

impl Rtp {
    /// The data of the extension element `id`.
    pub fn element(&self, id: u8) -> Option<&[u8]> {
        self.extension.iter().find(|e| e.0 == id).map(|e| e.1.as_slice())
    }
}

pub fn parse_rtp(packet: &[u8]) -> Rtp {
    assert!(packet.len() >= 12 && packet[0] >> 6 == 2, "an RTP packet");
    let mut at = 12 + 4 * (packet[0] & 0x0f) as usize;
    let mut extension = Vec::new();
    if packet[0] & 0x10 != 0 {
        assert_eq!(&packet[at..at + 2], &[0xbe, 0xde], "one-byte header extensions");
        let len = 4 * u16::from_be_bytes([packet[at + 2], packet[at + 3]]) as usize;
        let mut elements = &packet[at + 4..at + 4 + len];
        while let Some((&first, rest)) = elements.split_first() {
            if first == 0 {
                elements = rest;
                continue;
            }
            let size = (first & 0x0f) as usize + 1;
            extension.push((first >> 4, rest[..size].to_vec()));
            elements = &rest[size..];
        }
        at += 4 + len;
    }
    Rtp {
        marker: packet[1] & 0x80 != 0,
        payload_type: packet[1] & 0x7f,
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        extension,
        payload: packet[at..].to_vec(),
    }
}

/// A video frame the media server put back together.
pub struct VideoFrame {
    /// Annex B, as libwebrtc's receiver rebuilds it (still end-to-end encrypted, if it was).
    pub data: Vec<u8>,
    pub timestamp: u32,
    pub packets: Vec<Rtp>,
}

/// The media server: answers IP discovery, opens what the client sends with the key of
/// Session Description, puts video frames back together, and can send RTCP feedback.
pub struct Sfu {
    socket: UdpSocket,
    /// Where the client sends from (known after IP discovery).
    pub client: Option<SocketAddr>,
    opener: Option<Transport>,
    sealer: Option<Transport>,
    ssrcs: [u32; 3],
    depacketizer: Depacketizer,
    pending: Vec<Rtp>,
    /// Everything received, by kind.
    pub audio: Vec<Rtp>,
    pub rtx: Vec<Rtp>,
    pub video: Vec<Rtp>,
    /// When each packet of `video` and of `audio` was taken off the socket.
    pub video_at: Vec<std::time::Instant>,
    pub audio_at: Vec<std::time::Instant>,
    pub reports: Vec<rtcp::Packet>,
    /// The transport counter of every datagram, in order of arrival.
    pub counters: Vec<u32>,
    /// Datagrams that did not open with the key, or were neither RTP of this stream nor RTCP.
    pub strays: usize,
}

impl Sfu {
    pub async fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // (Room for a keyframe's burst while the test is busy with something else.)
        let _ = socket2::SockRef::from(&socket).set_recv_buffer_size(4 << 20);
        Self {
            socket,
            client: None,
            opener: None,
            sealer: None,
            ssrcs: [0; 3],
            depacketizer: Depacketizer::new(),
            pending: Vec::new(),
            audio: Vec::new(),
            rtx: Vec::new(),
            video: Vec::new(),
            video_at: Vec::new(),
            audio_at: Vec::new(),
            reports: Vec::new(),
            counters: Vec::new(),
            strays: 0,
        }
    }

    pub fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    /// A new media session: the key of its Session Description and the SSRCs of its Ready.
    pub fn session(&mut self, mode: Mode, key: &[u8; 32], ssrcs: [u32; 3]) {
        self.opener = Some(Transport::new(mode, key));
        self.sealer = Some(Transport::new(mode, key));
        self.ssrcs = ssrcs;
        self.depacketizer = Depacketizer::new();
        self.pending.clear();
        self.counters.clear();
    }

    /// Waits for the 74-byte discovery request and answers it; returns the SSRC it named.
    pub async fn answer_discovery(&mut self) -> u32 {
        let mut buffer = [0u8; 2048];
        loop {
            let (len, from) = timeout(WAIT, self.socket.recv_from(&mut buffer)).await.expect("no IP discovery request came").unwrap();
            if len != udp::DISCOVERY_LEN || buffer[..4] != [0, 1, 0, 70] {
                continue;
            }
            assert!(buffer[8..74].iter().all(|b| *b == 0), "the rest of a discovery request is zero");
            let mut answer = [0u8; udp::DISCOVERY_LEN];
            answer[..2].copy_from_slice(&2u16.to_be_bytes());
            answer[2..4].copy_from_slice(&70u16.to_be_bytes());
            answer[4..8].copy_from_slice(&buffer[4..8]);
            let address = from.ip().to_string();
            answer[8..8 + address.len()].copy_from_slice(address.as_bytes());
            answer[72..].copy_from_slice(&from.port().to_be_bytes());
            self.socket.send_to(&answer, from).await.unwrap();
            self.client = Some(from);
            return u32::from_be_bytes(buffer[4..8].try_into().unwrap());
        }
    }

    /// Takes one datagram (within `wait`) and files it. Returns the video frame it completed.
    async fn take(&mut self, wait: Duration) -> Result<Option<VideoFrame>, ()> {
        let mut buffer = [0u8; 2048];
        let Ok(received) = timeout(wait, self.socket.recv_from(&mut buffer)).await else { return Err(()) };
        let (len, _) = received.unwrap();
        let datagram = &buffer[..len];
        let Some(opener) = self.opener.as_ref() else {
            self.strays += 1;
            return Ok(None);
        };
        if crypto::is_rtcp(datagram) {
            match opener.open_rtcp(datagram) {
                Some(plain) => {
                    self.counters.push(u32::from_be_bytes(datagram[len - 4..].try_into().unwrap()));
                    self.reports.extend(rtcp::parse(&plain));
                }
                None => self.strays += 1,
            }
            return Ok(None);
        }
        let Some(plain) = opener.open_rtp(datagram) else {
            self.strays += 1;
            return Ok(None);
        };
        self.counters.push(u32::from_be_bytes(datagram[len - 4..].try_into().unwrap()));
        let packet = parse_rtp(&plain);
        if packet.ssrc == self.ssrcs[0] {
            self.audio.push(packet);
            self.audio_at.push(std::time::Instant::now());
        } else if packet.ssrc == self.ssrcs[2] {
            self.rtx.push(packet);
        } else if packet.ssrc == self.ssrcs[1] {
            self.video.push(packet.clone());
            self.video_at.push(std::time::Instant::now());
            let frame = self.depacketizer.push(packet.sequence, packet.timestamp, packet.marker, &packet.payload);
            let (timestamp, marker) = (packet.timestamp, packet.marker);
            self.pending.push(packet);
            if marker {
                let packets = std::mem::take(&mut self.pending);
                return Ok(frame.map(|data| VideoFrame { data, timestamp, packets }));
            }
        } else {
            self.strays += 1;
        }
        Ok(None)
    }

    /// The next whole video frame, within `wait`.
    pub async fn next_frame(&mut self, wait: Duration) -> Option<VideoFrame> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
            match self.take(left).await {
                Ok(Some(frame)) => return Some(frame),
                Ok(None) => {}
                Err(()) => return None,
            }
        }
    }

    /// Receives for `time` and files what comes; returns how many video packets came.
    pub async fn listen(&mut self, time: Duration) -> usize {
        let before = self.video.len();
        let deadline = tokio::time::Instant::now() + time;
        while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
            if self.take(left).await.is_err() {
                break;
            }
        }
        self.video.len() - before
    }

    /// Receives until `test` holds for what was filed (within the usual wait).
    pub async fn until(&mut self, what: &str, test: impl Fn(&Sfu) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !test(self) {
            let left = deadline.checked_duration_since(tokio::time::Instant::now()).unwrap_or_else(|| panic!("the media server never saw: {what}"));
            let _ = self.take(left.min(Duration::from_millis(50))).await;
        }
    }

    /// Sends RTCP feedback (a compound packet), sealed with the session's key.
    pub async fn send_rtcp(&mut self, compound: &[u8]) {
        let sealed = self.sealer.as_mut().unwrap().seal_rtcp(compound).unwrap();
        self.socket.send_to(&sealed, self.client.unwrap()).await.unwrap();
    }

    /// Sends a datagram as it is (something the client must not choke on).
    pub async fn send_raw(&mut self, datagram: &[u8]) {
        self.socket.send_to(datagram, self.client.unwrap()).await.unwrap();
    }
}
