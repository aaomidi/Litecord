//! T8 and T9: the stream worker (`session`) from start to stop against a scripted main
//! gateway, stream gateway and media server (`fake`), with viewers that are DAVE sessions of
//! their own. What the viewers decrypt is decoded in software and compared with what went in.
//!
//! Nothing here uses the module's globals or the gateway client: each test spawns its own
//! worker with scripted dependencies.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

use super::super::dave::GroupChoice;
use super::super::h264::{nal_units, Preparer, Skip};
use super::super::rtcp::{self, ReportBlock};
use super::super::rtp::{abs_send_time, ExtProfile, Playout, MAX_PAYLOAD, OMIT_CONTENT, OMIT_ORIENTATION, OMIT_TIMING, OMIT_TOFFSET, OPUS_SILENCE};
use super::super::session::{KeyInt, Switches, Tuning};
use super::super::signal::{Event, Region};
use super::super::{Failure, Params, Phase};
use super::fake::*;
use super::rate::{on_a_60_hz_screen, RUN4_STEPS};
use super::{access_units, framemd5, logged, run, samples, synth, tool, WorkDir};

const PARAMS: Params = Params { width: 1920, height: 1080, fps: 60, audio: false };
const V1: u64 = 193_696_591_125_807_105;
const V2: u64 = 193_696_591_125_807_106;

/// The stream's MLS group as the gateway keeps it, and the people watching.
struct Room {
    mls: MlsGateway,
    group: u64,
    epoch: u64,
    transition: u16,
    viewers: Vec<Viewer>,
}

impl Room {
    fn new(group: u64) -> Self {
        Self { mls: MlsGateway::new(), group, epoch: 0, transition: 0, viewers: Vec::new() }
    }

    /// A viewer arrives, up to the client's "ready": Clients Connect (11), the proposals
    /// (27), the client's commit and welcome (28), the announcement of that commit (29)
    /// and the client's answer (23). The new key is not in force yet.
    async fn join_prepare(&mut self, conn: &mut Conn, user: u64, old: bool) -> u16 {
        let (mut viewer, key_package) = Viewer::new(old, user, self.group, &self.mls);
        conn.send_json(11, json!({ "user_ids": [user.to_string()] })).await;
        let proposals = self.mls.propose_add(self.group, self.epoch, &[&key_package]);
        conn.send_binary(27, &proposals).await;
        let (commit, welcome) = split_commit_welcome(&conn.expect_binary(28).await);
        self.transition += 1;
        let transition = self.transition;
        // (The members get the proposals too; their own commits lose.)
        for member in &mut self.viewers {
            member.proposals(&proposals);
        }
        conn.send_transition(29, transition, &commit).await;
        assert_eq!(conn.expect_json(23).await, json!({ "transition_id": transition }));
        for member in &mut self.viewers {
            member.committed(&commit, transition);
        }
        viewer.welcomed(&welcome.expect("a welcome for the new viewer"), transition);
        self.viewers.push(viewer);
        transition
    }

    /// The gateway executes the transition (22): from here the new key is used.
    async fn execute(&mut self, conn: &mut Conn, transition: u16) {
        conn.send_json(22, json!({ "transition_id": transition })).await;
        for viewer in &mut self.viewers {
            viewer.execute(transition);
        }
        self.epoch += 1;
    }

    /// The first viewer of a session is in: the stream starts sending, and says so first
    /// with Speaking and the soundshare flag (once per session, with or without sound).
    async fn started(conn: &mut Conn, audio_ssrc: u32) {
        assert_eq!(conn.expect_json(5).await, json!({ "speaking": 2, "delay": 0, "ssrc": audio_ssrc }), "Speaking with the soundshare flag when the stream starts sending");
    }

    async fn join(&mut self, conn: &mut Conn, user: u64, old: bool) {
        let transition = self.join_prepare(conn, user, old).await;
        self.execute(conn, transition).await;
    }

    /// The viewers `who` decrypt a frame as it arrived, to exactly `prepared`.
    fn decrypt(&mut self, frame: &VideoFrame, prepared: &[u8], who: std::ops::Range<usize>) {
        assert_eq!(&frame.data[frame.data.len() - 2..], &[0xfa, 0xfa], "an end-to-end encrypted frame ends with the DAVE marker");
        let received = through_webrtc_receiver(&frame.data);
        assert_eq!(received, frame.data, "libwebrtc's receiver leaves the frame as it is");
        for n in who {
            assert_eq!(self.viewers[n].decrypt_video(USER, &received).unwrap_or_else(|e| panic!("viewer {n} cannot decrypt: {e}")), prepared, "viewer {n}");
        }
    }
}

/// A stream up to "live": requested, connected, one viewer (davey 0.2.0) watching.
async fn live(params: Params, tune: impl FnOnce(&mut Tuning)) -> (Rig, Conn, Room) {
    let mut rig = Rig::new(params, tune).await;
    rig.announce().await;
    let mut conn = rig.accept().await;
    let mut room = Room::new(GROUP);
    rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.expect("a key package");
    rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
    room.join(&mut conn, V1, false).await;
    Room::started(&mut conn, AUDIO_SSRC).await;
    rig.status_when("live", |s| s.phase == Phase::Live).await;
    assert!(rig.shared().wanted());
    (rig, conn, room)
}

/// A further viewer joins, and the worker has switched to the new key (it asks for a
/// keyframe when it does): from here every frame is for the new group.
async fn join_later(rig: &Rig, room: &mut Room, conn: &mut Conn, user: u64, old: bool) {
    let before = rig.keyframes();
    room.join(conn, user, old).await;
    rig.when("the new key is in force", |r| r.keyframes() > before).await;
}

/// Plays 10 ms chunks into the worker at a pace it keeps up with.
async fn play(rig: &Rig, chunks: impl Iterator<Item = Vec<i16>>) {
    for chunk in chunks {
        rig.handle.offer_audio(&chunk, 48_000);
        sleep(Duration::from_millis(1)).await;
    }
}

/// Offers one access unit to the worker; returns the frame the media server rebuilt of it.
async fn send(rig: &mut Rig, unit: &[u8]) -> VideoFrame {
    rig.handle.offer_frame(unit);
    rig.sfu.next_frame(WAIT).await.expect("the access unit arrives at the media server as one frame")
}

fn prepared(unit: &[u8]) -> Vec<u8> {
    Preparer::new().prepare(unit).expect("a unit that can be sent").data
}

/// The shape of what is sent, across frames.
#[derive(Default)]
struct Wire {
    sequence: Option<u16>,
    transport_sequence: Option<u16>,
    timestamp: Option<u32>,
}

impl Wire {
    fn check(&mut self, frame: &VideoFrame, payload_type: u8, ext: ExtProfile) {
        let last = frame.packets.len() - 1;
        for (i, p) in frame.packets.iter().enumerate() {
            assert_eq!(p.marker, i == last, "the marker bit is on the last packet of an access unit only");
            assert_eq!(p.timestamp, frame.timestamp, "one timestamp for all packets of an access unit");
            assert_eq!(p.payload_type, payload_type);
            assert!(!p.payload.is_empty() && p.payload.len() <= MAX_PAYLOAD);
            if let Some(before) = self.sequence {
                assert_eq!(p.sequence, before.wrapping_add(1), "video sequence numbers go up by one");
            }
            self.sequence = Some(p.sequence);
            match ext {
                ExtProfile::Full => {
                    check_full_extension(p, false);
                    self.transport(p);
                }
                ExtProfile::Min => assert_eq!(p.extension, vec![(5u8, vec![0u8, 0])]),
                ExtProfile::None => assert!(p.extension.is_empty()),
            }
        }
        if let Some(before) = self.timestamp {
            assert!((frame.timestamp.wrapping_sub(before) as i32) > 0, "timestamps increase from unit to unit");
        }
        self.timestamp = Some(frame.timestamp);
    }

    /// The transport-wide sequence number goes up by one per video or retransmission packet.
    fn transport(&mut self, p: &Rtp) {
        let e = p.element(5).unwrap();
        let number = u16::from_be_bytes([e[0], e[1]]);
        if let Some(before) = self.transport_sequence {
            assert_eq!(number, before.wrapping_add(1), "transport-wide sequence numbers go up by one");
        }
        self.transport_sequence = Some(number);
    }
}

fn check_full_extension(p: &Rtp, retransmission: bool) {
    assert_eq!(p.element(2), Some(&[0u8, 0, 0][..]), "transmission offset 0");
    let sent = p.element(3).expect("abs-send-time");
    let sent = u32::from_be_bytes([0, sent[0], sent[1], sent[2]]);
    let now = abs_send_time(SystemTime::now().duration_since(UNIX_EPOCH).unwrap());
    assert!((now.wrapping_sub(sent) & 0xff_ffff) < 5 << 18, "abs-send-time is the wall clock: {sent:#x} against {now:#x}");
    assert_eq!(p.element(4), Some(&[0u8][..]), "video orientation 0");
    assert_eq!(p.element(6), Some(&[0x00u8, 0x53, 0xe8][..]), "playout delay: at least 50 ms (5 units), at most 10 s (1000 units)");
    assert_eq!(p.element(7), Some(&[1u8][..]), "content type: screen");
    assert_eq!(p.element(8), Some(&[0u8; 13][..]), "video timing");
    assert_eq!(p.element(10), Some(&b"screen"[..]), "mid");
    assert_eq!(p.element(if retransmission { 12 } else { 11 }), Some(&b"100"[..]), "rid, or repaired rid on a retransmission");
    assert_eq!(p.element(if retransmission { 11 } else { 12 }), None);
    assert_eq!(p.extension.len(), 9);
}

fn sender_reports(sfu: &Sfu, of: u32) -> Vec<(u32, u32, u32)> {
    sfu.reports.iter().filter_map(|r| if let rtcp::Packet::SenderReport { ssrc, rtp_timestamp, packets, octets, .. } = r { (*ssrc == of).then_some((*rtp_timestamp, *packets, *octets)) } else { None }).collect()
}

async fn stopped(rig: &mut Rig, conn: &mut Conn) {
    assert_eq!(conn.expect_json(12).await, json!({ "audio_ssrc": AUDIO_SSRC, "video_ssrc": 0, "rtx_ssrc": 0, "streams": [] }), "the video is taken back");
    assert_eq!(conn.expect_close().await, Some(1000));
    assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
    let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
    assert_eq!((status.failure, status.viewers, status.stream_key.as_str()), (None, 0, ""));
    assert!(!rig.shared().wanted());
    rig.when("the worker's thread ends", |r| r.handle.finished()).await;
    assert!(rig.main_idle(), "one opcode 19, not two");
}

#[test]
fn golive_e2e_happy_path() {
    run(120, async {
        // (Behind an SPS without a VUI: one the sink has to rewrite before DAVE.)
        let units = synth::without_vui(&synth::stream(70));
        let mut rig = Rig::new(PARAMS, |t| t.keepalive = Duration::from_millis(400)).await;

        // Armed: Discord is asked for nothing before the capture has a picture.
        assert_eq!(rig.status().phase, Phase::Armed);
        sleep(Duration::from_millis(150)).await;
        assert!(rig.main_idle(), "no stream is requested before the capture has a picture");
        rig.announce().await;
        assert_eq!(rig.status().stream_key, rig.key());

        // The stream gateway: Identify, Ready, IP discovery, Select Protocol, Session
        // Description, Speaking, the key package, Video.
        let mut conn = rig.accept().await;
        conn.heartbeat_ms = 1000;
        let mut room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.expect("a key package");
        assert_eq!(conn.select["codecs"][1], json!({ "name": "H264", "type": "video", "priority": 1000, "payload_type": 101, "rtx_payload_type": 102, "encode": true, "decode": true }));
        assert_eq!(
            conn.video,
            json!({ "audio_ssrc": AUDIO_SSRC, "video_ssrc": VIDEO_SSRC, "rtx_ssrc": RTX_SSRC, "streams": [{ "type": "video", "rid": "100", "ssrc": VIDEO_SSRC, "active": true, "quality": 100,
                "rtx_ssrc": RTX_SSRC, "max_bitrate": 8_000_000, "max_framerate": 60, "max_resolution": { "type": "fixed", "width": 1920, "height": 1080 } }] })
        );
        let status = rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        assert!(status.e2ee && status.failure.is_none());
        assert!(!rig.shared().wanted(), "nobody watches: the share does not encode for Discord");

        // Ten silence packets open the path, 20 ms apart.
        rig.sfu.until("ten silence packets", |s| s.audio.len() >= 10).await;
        for pair in rig.sfu.audio.windows(2) {
            assert_eq!((pair[1].sequence, pair[1].timestamp), (pair[0].sequence.wrapping_add(1), pair[0].timestamp.wrapping_add(960)));
        }
        assert!(rig.sfu.audio.iter().all(|p| p.payload == OPUS_SILENCE && p.payload_type == 120 && p.extension.is_empty() && !p.marker));
        // A picture offered while nobody watches goes nowhere.
        rig.handle.offer_frame(&units[0]);
        assert_eq!(rig.sfu.listen(Duration::from_millis(150)).await, 0);

        // A viewer starts watching. Its key is in force when the gateway executes the transition.
        let (keyframes, wakes) = (rig.keyframes(), rig.calls.wakes.load(Ordering::SeqCst));
        let transition = room.join_prepare(&mut conn, V1, false).await;
        sleep(Duration::from_millis(100)).await;
        assert!(!rig.shared().wanted(), "prepared is not executed: still nothing to send with");
        room.execute(&mut conn, transition).await;
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        assert!(rig.shared().wanted());
        assert!(rig.keyframes() > keyframes && rig.calls.wakes.load(Ordering::SeqCst) > wakes, "the share is woken and asked for a keyframe");
        rig.handle.event(Event::Update { stream_key: rig.key(), region: "test".into(), viewer_ids: vec![V1], paused: false });
        rig.status_when("one viewer", |s| s.viewers == 1).await;

        // Until a keyframe comes, pictures are not sent (nobody could decode them).
        rig.handle.offer_frame(&units[1]);
        rig.handle.offer_frame(&units[2]);
        assert_eq!(rig.sfu.listen(Duration::from_millis(150)).await, 0);

        // Video: every unit arrives as one frame, the viewer decrypts it to the prepared unit.
        let mut wire = Wire::default();
        let mut watched = Vec::new();
        for unit in &units[10..30] {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            let expected = prepared(unit);
            room.decrypt(&frame, &expected, 0..1);
            watched.push(expected);
        }
        // T9: what the viewer got decodes to the pictures of what the encoder made.
        let (mut ours, mut theirs) = (synth::SoftwareDecoder::new(), synth::SoftwareDecoder::new());
        let mut pictures = 0;
        for (unit, got) in units[10..30].iter().zip(&watched) {
            let (a, b) = (ours.decode(unit).unwrap(), theirs.decode(got).unwrap());
            assert!(a == b, "a picture differs after the trip");
            pictures += a.is_some() as usize;
        }
        assert!(pictures >= 18, "{pictures} pictures decoded");

        // A sender report follows the first frame; its counts are those of the video SSRC.
        rig.sfu.until("a sender report for the video", |s| !sender_reports(s, VIDEO_SSRC).is_empty()).await;
        let (report_timestamp, packets, octets) = sender_reports(&rig.sfu, VIDEO_SSRC)[0];
        assert!(packets >= 1 && packets as usize <= rig.sfu.video.len() && octets > 0);
        assert!(report_timestamp.wrapping_sub(rig.sfu.video[0].timestamp) < 90_000 * 10, "the report's RTP time is on the video's clock");

        // A NACK is answered with the packet again, on the RTX stream.
        let lost = rig.sfu.video[rig.sfu.video.len() - 2].clone();
        rig.sfu.send_rtcp(&rtcp::nack(7, VIDEO_SSRC, &[lost.sequence, lost.sequence.wrapping_add(9000)])).await;
        rig.sfu.until("the retransmission", |s| !s.rtx.is_empty()).await;
        let again = rig.sfu.rtx[0].clone();
        assert_eq!((again.payload_type, again.timestamp, again.marker), (102, lost.timestamp, lost.marker));
        assert_eq!(&again.payload[..2], &lost.sequence.to_be_bytes(), "the original sequence number comes first");
        assert_eq!(&again.payload[2..], &lost.payload[..]);
        check_full_extension(&again, true);
        wire.transport(&again);
        rig.sfu.listen(Duration::from_millis(100)).await;
        assert_eq!(rig.sfu.rtx.len(), 1, "a packet that was never sent is not made up");

        // A receiver report's loss reaches the share's rate control, and the estimate that
        // came with it (REMB) what the share's encoder is asked for: nine tenths of it for
        // the wire, nine tenths of that for the encoder.
        assert_eq!(rig.shared().worst_loss(), None);
        assert_eq!(rig.shared().encoder_cap(), 7_040_000, "until an estimate comes: what was announced, less what packets add");
        rig.sfu.send_rtcp(&[rtcp::receiver_report(7, &[ReportBlock { ssrc: VIDEO_SSRC, fraction_lost: 26, ..Default::default() }]), rtcp::remb(7, 6_000_000, &[VIDEO_SSRC])].concat()).await;
        rig.when("the loss is known", |r| r.shared().worst_loss() == Some(101)).await;
        // (Once the estimate has stayed down for a second. The gateway has nothing to say
        // meanwhile, and answers the heartbeats.)
        conn.expect_quiet(Duration::from_millis(1100)).await;
        rig.when("the estimate is followed", |r| r.shared().encoder_cap() == 4_752_000).await;

        // PLI and FIR ask the encoder for a keyframe. Things that are not feedback do no harm.
        rig.sfu.send_raw(&[1, 2, 3]).await;
        rig.sfu.send_raw(&[0x80, 201, 0, 1, 9, 9, 9, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 0, 0, 0, 1]).await;
        sleep(Duration::from_millis(350)).await;
        let before = rig.keyframes();
        rig.sfu.send_rtcp(&rtcp::pli(7, VIDEO_SSRC)).await;
        rig.when("a keyframe is asked for after a PLI", |r| r.keyframes() > before).await;
        sleep(Duration::from_millis(350)).await;
        let before = rig.keyframes();
        rig.sfu.send_rtcp(&rtcp::fir(7, VIDEO_SSRC, 1)).await;
        rig.when("a keyframe is asked for after a FIR", |r| r.keyframes() > before).await;

        // A second viewer (davey 0.1.4). Until the gateway executes the transition the
        // stream goes on with the old key, so the first viewer keeps its picture.
        let transition = room.join_prepare(&mut conn, V2, true).await;
        for unit in &units[30..34] {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            room.decrypt(&frame, &prepared(unit), 0..1);
        }
        let before = rig.keyframes();
        room.execute(&mut conn, transition).await;
        rig.when("a keyframe is asked for the new viewer", |r| r.keyframes() > before).await;
        assert_eq!(rig.status().phase, Phase::Live);
        assert_eq!((room.viewers[0].epoch(), room.viewers[1].epoch()), (2, 2));
        for unit in &units[34..50] {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            room.decrypt(&frame, &prepared(unit), 0..2);
        }

        // Both stop watching: the gateway starts a new group (24), which takes effect at
        // once (21 with transition 0). Sending stops.
        conn.send_json(13, json!({ "user_id": V1.to_string() })).await;
        conn.send_json(13, json!({ "user_id": V2.to_string() })).await;
        conn.send_json(24, json!({ "protocol_version": 1, "epoch": 1 })).await;
        let new_package = key_package_of(&conn.expect_binary(26).await);
        conn.send_json(21, json!({ "transition_id": 0, "protocol_version": 1 })).await;
        rig.status_when("waiting for viewers again", |s| s.phase == Phase::Waiting).await;
        assert!(!rig.shared().wanted());
        rig.handle.event(Event::Update { stream_key: rig.key(), region: "test".into(), viewer_ids: Vec::new(), paused: false });
        rig.status_when("no viewer", |s| s.viewers == 0).await;
        rig.handle.offer_frame(&units[50]);
        let silent_from = rig.sfu.audio.len();
        assert_eq!(rig.sfu.listen(Duration::from_millis(150)).await, 0, "nothing is sent while nobody can decrypt it");
        // The path is kept open with a silence packet now and then.
        rig.sfu.until("a keepalive silence packet", |s| s.audio.len() > silent_from).await;
        assert_eq!(rig.sfu.audio.last().unwrap().payload, OPUS_SILENCE);

        // Somebody watches again, and this time their commit is the one announced: the
        // client is welcomed (30) with transition 0, in force at once.
        room.viewers.clear();
        let (mut viewer, key_package) = Viewer::new(false, V1, GROUP, &room.mls);
        conn.send_json(11, json!({ "user_ids": [V1.to_string()] })).await;
        conn.send_binary(27, &room.mls.propose_add(GROUP, 0, &[&key_package])).await;
        split_commit_welcome(&conn.expect_binary(28).await);
        let (their_commit, their_welcome) = viewer.proposals(&room.mls.propose_add(GROUP, 0, &[&new_package]));
        conn.send_transition(30, 0, &their_welcome.unwrap()).await;
        viewer.committed(&their_commit, 0);
        room.viewers.push(viewer);
        rig.status_when("live again", |s| s.phase == Phase::Live).await;
        for unit in &units[50..60] {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            room.decrypt(&frame, &prepared(unit), 0..1);
        }

        // The whole time: heartbeats that acknowledge the gateway's sequence numbers, and
        // one transport counter for audio, video, retransmissions and reports.
        assert!(conn.heartbeats >= 1, "heartbeats were sent");
        assert!(conn.last_ack.is_some_and(|ack| ack >= 1 && ack <= conn.seq as i64), "seq_ack {:?} of {}", conn.last_ack, conn.seq);
        assert_eq!(rig.sfu.counters[0], 1, "the counter starts at 1");
        assert!(rig.sfu.counters.windows(2).all(|w| w[1] == w[0] + 1), "the counter goes up by one per packet of any kind");
        assert_eq!(rig.sfu.strays, 0);

        // Stop: the video is taken back (12), the stream deleted (19), the socket closed.
        let begun = Instant::now();
        rig.handle.stop();
        assert!(begun.elapsed() < Duration::from_millis(100), "stop() does not wait");
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_refused_or_unanswered_request_ends_with_a_notice() {
    run(40, async {
        // STREAM_DELETE instead of STREAM_CREATE and STREAM_SERVER_UPDATE.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        // (Somebody else's stream is not ours to act on.)
        rig.handle.event(Event::Delete { stream_key: format!("guild:{GUILD}:{VOICE_CHANNEL}:{}", USER + 1), reason: "stream_ended".into(), unavailable: false });
        // (Nor is the answer to an earlier stop, when the button was turned off and on again.)
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "user_requested".into(), unavailable: false });
        sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.status().phase, Phase::Creating);
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "unauthorized".into(), unavailable: false });
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Refused("unauthorized".into())));
        assert_eq!(rig.shared().take_notice(), Some(Failure::Refused("unauthorized".into())));
        assert_eq!(rig.shared().take_notice(), None, "the user is told once");
        rig.when("the worker's thread ends", |r| r.handle.finished()).await;
        assert!(rig.main_idle(), "Discord deleted the stream itself: no opcode 19");

        // No answer at all: the request is taken back with the key built here.
        let mut rig = Rig::new(PARAMS, |t| t.create_timeout = Duration::from_millis(300)).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::NoAnswer));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));

        // The main gateway is between two connections: the request waits for it.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.main_up.store(false, Ordering::SeqCst);
        rig.shared().capture_ready();
        rig.status_when("creating", |s| s.phase == Phase::Creating).await;
        sleep(Duration::from_millis(200)).await;
        assert!(rig.main_idle());
        rig.main_up.store(true, Ordering::SeqCst);
        rig.main_op(18).await;
        rig.handle.stop();
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
    });
}

#[test]
fn golive_e2e_server_update_before_stream_create() {
    run(40, async {
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        // The events in the other order, and a server that went away in between.
        rig.handle.event(rig.server_event());
        rig.handle.event(Event::ServerUpdate { stream_key: rig.key(), endpoint: None, token: Some(TOKEN.into()) });
        let mut create = rig.create_event();
        if let Event::Create { paused, .. } = &mut create {
            *paused = true;
        }
        rig.handle.event(create);
        // A paused stream is resumed (opcode 22).
        assert_eq!(rig.main_op(22).await, json!({ "stream_key": rig.key(), "paused": false }));
        assert!(timeout(Duration::from_millis(200), rig.listener.accept()).await.is_err(), "no media server is known: nothing to connect to");
        assert_eq!(rig.status().phase, Phase::Creating);
        rig.handle.event(rig.server_event());
        let mut conn = rig.accept().await;
        let room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_fatal_close_codes_end_the_stream() {
    run(60, async {
        for code in [4014u16, 4017] {
            let (mut rig, conn, _room) = live(PARAMS, |_| {}).await;
            conn.close(code).await;
            let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
            assert_eq!(status.failure, Some(Failure::Lost(code.to_string())), "close {code}");
            assert_eq!(rig.shared().take_notice(), Some(Failure::Lost(code.to_string())));
            assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
            assert!(!rig.shared().wanted());
            assert!(timeout(Duration::from_millis(250), rig.listener.accept()).await.is_err(), "close {code}: no reconnect");
        }
    });
}

#[test]
fn golive_e2e_a_dropped_connection_is_resumed() {
    run(60, async {
        let units = synth::stream(40);
        let (mut rig, conn, mut room) = live(PARAMS, |_| {}).await;
        let mut wire = Wire::default();
        let frame = send(&mut rig, &units[0]).await;
        wire.check(&frame, 101, ExtProfile::Full);
        room.decrypt(&frame, &prepared(&units[0]), 0..1);
        let seq = conn.seq;
        conn.drop_tcp();
        // A new socket, and Resume (7) with the last sequence number seen.
        let mut conn = rig.accept().await;
        conn.seq = seq;
        assert_eq!(conn.expect_json(7).await, json!({ "server_id": RTC_SERVER.to_string(), "session_id": VOICE_SESSION, "token": TOKEN, "seq_ack": seq }));
        assert_eq!(rig.status().phase, Phase::Reconnecting);
        // Meanwhile the media goes on: same socket, same SSRCs, same keys.
        for unit in &units[1..6] {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            room.decrypt(&frame, &prepared(unit), 0..1);
        }
        conn.send_plain(8, json!({ "v": 8, "heartbeat_interval": 1000 })).await;
        conn.send_plain(9, Value::Null).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        // No Identify, no second Select Protocol, no new key package.
        conn.expect_quiet(Duration::from_millis(300)).await;
        // The session goes on over the new socket: another viewer gets in.
        join_later(&rig, &mut room, &mut conn, V2, true).await;
        let frame = send(&mut rig, &units[10]).await;
        wire.check(&frame, 101, ExtProfile::Full);
        room.decrypt(&frame, &prepared(&units[10]), 0..2);
        assert_eq!(rig.sfu.strays, 0);
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_a_refused_resume_starts_a_new_session() {
    run(60, async {
        let units = synth::stream(20);
        let (mut rig, conn, _room) = live(PARAMS, |_| {}).await;
        conn.drop_tcp();
        let mut conn = rig.accept().await;
        conn.expect_json(7).await;
        // The session is no longer valid.
        conn.close(4006).await;
        // A new socket and Identify: new SSRCs, a new key, a new DAVE session.
        let mut conn = rig.accept().await;
        let mut room = Room::new(GROUP);
        assert_eq!(conn.expect_json(0).await["token"], TOKEN);
        rig.handshake_after_identify(&mut conn, 1, Some(&room.mls.external_sender()), [2001, 2002, 2003]).await.expect("a new key package");
        assert_eq!(conn.video["streams"][0]["ssrc"], 2002);
        rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        assert!(!rig.shared().wanted(), "the new group has no viewer in it yet");
        room.join(&mut conn, V1, false).await;
        // (A new session: the soundshare flag again, with the new audio SSRC.)
        Room::started(&mut conn, 2001).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        let frame = send(&mut rig, &units[0]).await;
        assert!(frame.packets.iter().all(|p| p.ssrc == 2002));
        room.decrypt(&frame, &prepared(&units[0]), 0..1);
        rig.handle.stop();
        assert_eq!(conn.expect_json(12).await["audio_ssrc"], 2001);
    });
}

#[test]
fn golive_e2e_unanswered_heartbeats_resume_on_a_new_socket() {
    run(60, async {
        let (rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        // (A Hello with a short interval: the worker beats by the last one it was given.)
        conn.send_plain(8, json!({ "v": 8, "heartbeat_interval": 300 })).await;
        conn.answer_heartbeats = false;
        // Two heartbeats without an answer, and at the third beat the socket is given up.
        loop {
            if let Got::Closed(_) = conn.next().await {
                break;
            }
        }
        assert!(conn.heartbeats >= 2, "{} heartbeats", conn.heartbeats);
        let mut conn = rig.accept().await;
        assert_eq!(conn.expect_json(7).await["session_id"], VOICE_SESSION);
        assert!(rig.shared().wanted(), "the media went on meanwhile");
    });
}

#[test]
fn golive_e2e_another_media_server_moves_the_connection() {
    run(60, async {
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        // The same server and token again: nothing to do.
        rig.handle.event(rig.server_event());
        conn.expect_quiet(Duration::from_millis(200)).await;
        // Another endpoint: the old socket is left, the new one identified with the new token.
        let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
        rig.handle.event(Event::ServerUpdate { stream_key: rig.key(), endpoint: Some(format!("127.0.0.1:{}", other.local_addr().unwrap().port())), token: Some("another-token".into()) });
        assert!(matches!(conn.next().await, Got::Closed(_)), "the old stream gateway socket is left");
        let mut conn = Conn::accept(&other).await;
        conn.token = "another-token".into();
        let mut room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        room.join(&mut conn, V1, false).await;
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        let unit = &synth::stream(1)[0];
        let frame = send(&mut rig, unit).await;
        room.decrypt(&frame, &prepared(unit), 0..1);
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_an_outage_waits_for_the_stream_to_come_back() {
    run(60, async {
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "stream_ended".into(), unavailable: true });
        assert!(matches!(conn.next().await, Got::Closed(_)));
        let status = rig.status_when("waiting for the stream", |s| s.phase == Phase::Creating).await;
        assert!(status.failure.is_none());
        assert!(!rig.shared().wanted());
        assert!(rig.main_idle(), "the stream is not deleted, and not requested twice");
        // It is back: announced again, on a server of its own.
        rig.handle.event(rig.create_event());
        rig.handle.event(rig.server_event());
        let mut conn = rig.accept().await;
        let room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;

        // It does not come back: the stream ends with a notice.
        let (mut rig, _conn, _room) = live(PARAMS, |t| t.unavailable_timeout = Duration::from_millis(300)).await;
        rig.handle.event(Event::ServerUpdate { stream_key: rig.key(), endpoint: None, token: None });
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("no media server".into())));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));

        // Discord ends a running stream: a notice, and nothing to delete.
        let (mut rig, _conn, _room) = live(PARAMS, |_| {}).await;
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "stream_ended".into(), unavailable: false });
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Ended("stream_ended".into())));
        rig.when("the worker's thread ends", |r| r.handle.finished()).await;
        assert!(rig.main_idle());
    });
}

#[test]
fn golive_e2e_stop_while_creating_and_while_connecting() {
    run(60, async {
        // Armed: nothing was asked, nothing is taken back.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.handle.stop();
        rig.status_when("off", |s| s.phase == Phase::Off).await;
        rig.when("the worker's thread ends", |r| r.handle.finished()).await;
        assert!(rig.main_idle());

        // Creating: the request is out, no answer yet.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        let begun = Instant::now();
        rig.handle.stop();
        assert!(begun.elapsed() < Duration::from_millis(100));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert!(status.failure.is_none());
        rig.when("the worker's thread ends", |r| r.handle.finished()).await;
        assert!(rig.main_idle(), "one opcode 19, not two");

        // Connecting: the stream gateway has the socket and says nothing, not even goodbye.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        conn.expect_json(0).await;
        assert_eq!(rig.status().phase, Phase::Connecting);
        let begun = Instant::now();
        rig.handle.stop();
        assert!(begun.elapsed() < Duration::from_millis(100));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        rig.when("the worker's thread ends without the gateway's goodbye", |r| r.handle.finished()).await;
        assert_eq!(rig.status().phase, Phase::Off);
        assert_eq!(conn.expect_close().await, Some(1000));
    });
}

#[test]
fn golive_e2e_leaving_the_voice_channel_ends_the_stream() {
    run(60, async {
        // A voice state for the same channel (mute, deafen) changes nothing.
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        rig.handle.voice_channel(Some(VOICE_CHANNEL));
        conn.expect_quiet(Duration::from_millis(200)).await;
        assert_eq!(rig.status().phase, Phase::Live);
        // No channel any more: as stop().
        rig.handle.voice_channel(None);
        stopped(&mut rig, &mut conn).await;
        // Moved to another channel: the same.
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        rig.handle.voice_channel(Some("850360749460553770"));
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_the_group_id_is_learned_from_the_gateway() {
    run(60, async {
        // The gateway's group is rtc_channel_id here, not the rtc_server_id - 1 assumed.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        let mls = MlsGateway::new();
        rig.handshake(&mut conn, 1, Some(&mls.external_sender())).await.unwrap();
        let (mut viewer, key_package) = Viewer::new(false, V1, RTC_CHANNEL, &mls);
        conn.send_json(11, json!({ "user_ids": [V1.to_string()] })).await;
        conn.send_binary(27, &mls.propose_add(RTC_CHANNEL, 0, &[&key_package])).await;
        // The session is made again with the id the proposals name, then they are committed.
        key_package_of(&conn.expect_binary(26).await);
        let (commit, welcome) = split_commit_welcome(&conn.expect_binary(28).await);
        conn.send_transition(29, 1, &commit).await;
        assert_eq!(conn.expect_json(23).await, json!({ "transition_id": 1 }));
        viewer.welcomed(&welcome.unwrap(), 1);
        conn.send_json(22, json!({ "transition_id": 1 })).await;
        viewer.execute(1);
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        let unit = &synth::stream(1)[0];
        let frame = send(&mut rig, unit).await;
        assert_eq!(viewer.decrypt_video(USER, &frame.data).unwrap(), prepared(unit));
        if let Some(said) = logged("the gateway names group") {
            assert!(said, "the log says which group id the gateway uses");
        }
    });
}

#[test]
fn golive_e2e_only_expected_users_are_let_into_the_group() {
    run(60, async {
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        let mut room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        // Somebody the stream server has not announced and who is not in the voice channel
        // is proposed: no commit, the stream goes on waiting.
        let (_stranger, key_package) = Viewer::new(false, V2, GROUP, &room.mls);
        conn.send_binary(27, &room.mls.propose_add(GROUP, 0, &[&key_package])).await;
        conn.expect_quiet(Duration::from_millis(300)).await;
        assert_eq!(rig.status().phase, Phase::Waiting);
        if let Some(said) = logged("neither in the voice channel nor announced") {
            assert!(said, "the log names who was refused");
        }
        // A member of the voice channel is let in even before the stream server names them.
        rig.voice_users.lock().unwrap().push(V1);
        let (mut viewer, key_package) = Viewer::new(false, V1, GROUP, &room.mls);
        conn.send_binary(27, &room.mls.propose_add(GROUP, 0, &[&key_package])).await;
        let (commit, welcome) = split_commit_welcome(&conn.expect_binary(28).await);
        conn.send_transition(29, 1, &commit).await;
        assert_eq!(conn.expect_json(23).await, json!({ "transition_id": 1 }));
        viewer.welcomed(&welcome.unwrap(), 1);
        room.viewers.push(viewer);
        room.execute(&mut conn, 1).await;
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        let unit = &synth::stream(1)[0];
        let frame = send(&mut rig, unit).await;
        room.decrypt(&frame, &prepared(unit), 0..1);
    });
}

#[test]
fn golive_e2e_an_unreadable_commit_is_reported_three_times() {
    run(60, async {
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        let mls = MlsGateway::new();
        rig.handshake(&mut conn, 1, Some(&mls.external_sender())).await.unwrap();
        conn.send_json(11, json!({ "user_ids": [V1.to_string()] })).await;
        for round in 0..3u16 {
            let (_viewer, key_package) = Viewer::new(false, V1, GROUP, &mls);
            conn.send_binary(27, &mls.propose_add(GROUP, 0, &[&key_package])).await;
            conn.expect_binary(28).await;
            conn.send_transition(29, 10 + round, &[0xde, 0xad, 0xbe, 0xef]).await;
            // Opcode 31, and a new key package.
            assert_eq!(conn.expect_json(31).await, json!({ "transition_id": 10 + round }));
            key_package_of(&conn.expect_binary(26).await);
        }
        // The fourth failure within thirty seconds: nothing more is sent about it.
        let (_viewer, key_package) = Viewer::new(false, V1, GROUP, &mls);
        conn.send_binary(27, &mls.propose_add(GROUP, 0, &[&key_package])).await;
        conn.expect_binary(28).await;
        conn.send_transition(29, 13, &[0xde, 0xad, 0xbe, 0xef]).await;
        conn.expect_quiet(Duration::from_millis(400)).await;
        // The stream stays connected and sends nothing.
        assert_eq!(rig.status().phase, Phase::Waiting);
        assert!(!rig.shared().wanted());
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;
    });
}

#[test]
fn golive_e2e_without_dave_frames_are_sent_as_they_are() {
    run(60, async {
        let units = synth::stream(12);
        // (With LITECORD_GOLIVE_DUMP: what is sent is also written to a directory.)
        let dir = WorkDir::new("e2e-dump");
        let dump = dir.0.clone();
        let mut rig = Rig::new(PARAMS, move |t| t.switches.dump = Some(dump)).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        // dave_protocol_version 0 (a stage channel): no key package.
        assert!(rig.handshake(&mut conn, 0, None).await.is_none());
        let status = rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        assert!(!status.e2ee, "the status says the stream is not end-to-end encrypted");
        // No group says whether anybody watches, so the lists of who is connected do: until
        // somebody is, nothing is encoded for Discord and nothing is sent.
        assert!(!rig.shared().wanted(), "nobody is connected: the share does not encode for Discord");
        rig.handle.offer_frame(&units[0]);
        assert_eq!(rig.sfu.listen(Duration::from_millis(150)).await, 0);
        // Our own arrival is not a viewer.
        conn.send_json(11, json!({ "user_ids": [USER.to_string()] })).await;
        conn.expect_quiet(Duration::from_millis(150)).await;
        assert!(!rig.shared().wanted());
        // A viewer connects: video at once, from a keyframe.
        let before = rig.keyframes();
        conn.send_json(11, json!({ "user_ids": [V1.to_string()] })).await;
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        assert!(rig.shared().wanted() && rig.keyframes() > before);
        let mut wire = Wire::default();
        for unit in &units {
            let frame = send(&mut rig, unit).await;
            wire.check(&frame, 101, ExtProfile::Full);
            assert_eq!(frame.data, prepared(unit), "no DAVE marker, no ciphertext: the prepared unit itself");
        }
        // No key change announces a further viewer here: Clients Connect asks for the keyframe.
        sleep(Duration::from_millis(350)).await;
        let before = rig.keyframes();
        conn.send_json(11, json!({ "user_ids": [V2.to_string()] })).await;
        rig.when("a keyframe is asked for the viewer", |r| r.keyframes() > before).await;
        if let Some(said) = logged("negotiated protocol version 0") {
            assert!(said, "the log says the stream is not end-to-end encrypted");
        }
        // The last viewer leaves: the stream stops costing anything again.
        conn.send_json(13, json!({ "user_id": V1.to_string() })).await;
        sleep(Duration::from_millis(100)).await;
        assert!(rig.shared().wanted(), "one viewer is still there");
        conn.send_json(13, json!({ "user_id": V2.to_string() })).await;
        rig.status_when("waiting for viewers again", |s| s.phase == Phase::Waiting).await;
        assert!(!rig.shared().wanted());
        // (The main gateway's list counts too.)
        rig.handle.event(Event::Update { stream_key: rig.key(), region: "test".into(), viewer_ids: vec![V1], paused: false });
        rig.status_when("live again", |s| s.phase == Phase::Live).await;
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;
        // The dump: the prepared access units one after the other, a line per packet and one
        // per picture.
        let expected: Vec<u8> = units.iter().flat_map(|unit| prepared(unit)).collect();
        assert!(std::fs::read(dir.0.join("prepared.h264")).unwrap() == expected, "prepared.h264 holds what was given to the packetizer");
        let log = std::fs::read_to_string(dir.0.join("packets.log")).unwrap();
        assert_eq!(log.lines().filter(|l| l.contains(" video seq=")).count(), rig.sfu.video.len());
        assert_eq!(log.lines().filter(|l| l.contains(" frame offered=")).count(), units.len());
        assert!(log.lines().any(|l| l.contains(" audio seq=")));
    });
}

#[test]
fn golive_e2e_switches_change_what_goes_on_the_wire() {
    run(90, async {
        let units = synth::stream(4);
        for ext in [ExtProfile::Min, ExtProfile::None] {
            let mut rig = Rig::new(PARAMS, |t| {
                t.switches = Switches {
                    ext,
                    gateway_version: 9,
                    payload_type: 105,
                    rtx: false,
                    stream_type: "screen",
                    region: Region::Omit,
                    group: GroupChoice::Channel,
                    dave_check: false,
                    audio_ext: true,
                    idle_report: true,
                    ..Switches::default()
                };
                t.max_bitrate = 5_000_000;
                t.keepalive = Duration::from_millis(300);
            })
            .await;
            rig.shared().capture_ready();
            // LITECORD_GOLIVE_REGION=omit
            assert_eq!(rig.main_op(18).await, json!({ "type": "guild", "guild_id": GUILD, "channel_id": VOICE_CHANNEL }));
            rig.handle.event(rig.create_event());
            rig.handle.event(rig.server_event());
            let mut conn = rig.accept().await;
            // LITECORD_GOLIVE_GW=9: Identify names the stream's RTC channel.
            let identify = conn.expect_json(0).await;
            assert_eq!(identify["channel_id"], RTC_CHANNEL.to_string());
            // LITECORD_GOLIVE_GROUP=channel: the MLS group is rtc_channel_id from the start.
            let mut room = Room::new(RTC_CHANNEL);
            rig.handshake_after_identify(&mut conn, 1, Some(&room.mls.external_sender()), [AUDIO_SSRC, VIDEO_SSRC, RTX_SSRC]).await.unwrap();
            // LITECORD_GOLIVE_PT=105 and LITECORD_GOLIVE_RTX=0: no retransmissions declared.
            assert_eq!(conn.select["codecs"][1], json!({ "name": "H264", "type": "video", "priority": 1000, "payload_type": 105, "encode": true, "decode": true }));
            // LITECORD_GOLIVE_STREAM_TYPE=screen
            assert_eq!((&conn.video["rtx_ssrc"], &conn.video["streams"][0]["type"], &conn.video["streams"][0]["rtx_ssrc"], &conn.video["streams"][0]["max_bitrate"]), (&json!(0), &json!("screen"), &json!(0), &json!(5_000_000)));
            // LITECORD_GOLIVE_AUDIO_EXT=full: audio packets (here the opening silence) carry
            // the native clients' elements. LITECORD_GOLIVE_IDLE_SR=1: a sender report
            // follows each keepalive packet while nothing else is sent.
            rig.sfu.until("the opening silence and a keepalive", |s| s.audio.len() >= 11 && !sender_reports(s, AUDIO_SSRC).is_empty()).await;
            for p in &rig.sfu.audio {
                assert_eq!(p.extension.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 3, 9, 10]);
                assert_eq!((p.element(1), p.element(9), p.element(10)), (Some(&[127u8][..]), Some(&[0u8][..]), Some(&b"audio"[..])));
                assert_eq!(p.payload, OPUS_SILENCE);
            }
            // (LITECORD_GOLIVE_DAVE_CHECK=0: no list of expected users is given to the MLS library.)
            let transition = room.join_prepare(&mut conn, V1, false).await;
            room.execute(&mut conn, transition).await;
            Room::started(&mut conn, AUDIO_SSRC).await;
            rig.status_when("live", |s| s.phase == Phase::Live).await;
            // LITECORD_GOLIVE_EXT: one element, or no extension at all.
            let mut wire = Wire::default();
            for unit in &units {
                let frame = send(&mut rig, unit).await;
                wire.check(&frame, 105, ext);
                room.decrypt(&frame, &prepared(unit), 0..1);
            }
            let lost = rig.sfu.video[0].sequence;
            rig.sfu.send_rtcp(&rtcp::nack(7, VIDEO_SSRC, &[lost])).await;
            rig.sfu.listen(Duration::from_millis(200)).await;
            assert!(rig.sfu.rtx.is_empty(), "retransmissions are off");
            rig.handle.stop();
            assert_eq!(conn.expect_json(12).await["video_ssrc"], 0);
        }
    });
}

#[test]
fn golive_e2e_a_handshake_that_never_finishes_gives_up_after_five_tries() {
    run(60, async {
        let mut rig = Rig::new(PARAMS, |t| {
            t.handshake_timeout = Duration::from_millis(150);
            t.retry_delays = vec![Duration::from_millis(20); 5];
        })
        .await;
        rig.announce().await;
        // The stream gateway takes Identify and never answers.
        let mut held = Vec::new();
        for _ in 0..5 {
            let mut conn = rig.accept().await;
            conn.expect_json(0).await;
            held.push(conn);
        }
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("handshake timeout".into())));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        assert!(timeout(Duration::from_millis(250), rig.listener.accept()).await.is_err(), "no sixth try");
    });
}

#[test]
fn golive_e2e_another_codec_ends_the_stream() {
    run(60, async {
        // The server changes the codec of a running stream (opcode 14).
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        conn.send_json(14, json!({ "video_codec": "VP8" })).await;
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("codec".into())));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        // (A session update that keeps H264 is only noted.)
        let (rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        conn.send_json(14, json!({ "video_codec": "H264", "keyframe_interval": 0 })).await;
        conn.send_json(15, json!({ "any": 100 })).await;
        conn.send_json(18, json!({ "user_id": V1.to_string(), "flags": 2 })).await;
        conn.send_json(20, json!({ "user_id": V1.to_string(), "platform": 0 })).await;
        conn.expect_quiet(Duration::from_millis(200)).await;
        assert_eq!(rig.status().phase, Phase::Live);
    });
}

#[test]
fn golive_e2e_stream_audio() {
    run(60, async {
        let (mut rig, _conn, mut room) = live(Params { audio: true, ..PARAMS }, |_| {}).await;
        rig.sfu.until("the opening silence", |s| s.audio.len() >= 10).await;
        let opening = rig.sfu.audio.len();
        // 600 ms of a tone, in the loopback's 10 ms chunks.
        let tone = |n: usize| -> Vec<i16> { (0..480).map(|i| (((n * 480 + i) as f32 * 0.06).sin() * 9000.0) as i16).collect() };
        // (Speaking with the soundshare flag went out when the stream started: `live`.)
        play(&rig, (0..60).map(tone)).await;
        rig.sfu.until("the Opus packets of the tone", |s| s.audio.len() >= opening + 28).await;
        rig.sfu.listen(Duration::from_millis(100)).await;
        let packets = rig.sfu.audio[opening..].to_vec();
        assert!((28..=30).contains(&packets.len()), "{} packets for 600 ms", packets.len());
        for (i, p) in packets.iter().enumerate() {
            assert_eq!((p.payload_type, p.marker, p.extension.len(), p.ssrc), (120, false, 0, AUDIO_SSRC));
            assert_eq!(&p.payload[p.payload.len() - 2..], &[0xfa, 0xfa], "stream audio is end-to-end encrypted too");
            let opus = room.viewers[0].decrypt_audio(USER, &p.payload).unwrap();
            assert!(opus.len() > 3 && opus.len() < p.payload.len());
            if i > 0 {
                let step = p.timestamp.wrapping_sub(packets[i - 1].timestamp);
                assert_eq!(p.sequence, packets[i - 1].sequence.wrapping_add(1));
                assert!(step >= 960 && step % 960 == 0, "20 ms per packet: {step}");
            }
        }
        rig.sfu.until("a sender report for the audio", |s| !sender_reports(s, AUDIO_SSRC).is_empty()).await;
        // Nothing plays any more: after 400 ms of zeros, five silence packets and then nothing.
        let sounding = rig.sfu.audio.len();
        play(&rig, (0..120).map(|_| vec![0i16; 480])).await;
        rig.sfu.listen(Duration::from_millis(300)).await;
        let tail = rig.sfu.audio[sounding..].to_vec();
        let silent: Vec<&Rtp> = tail.iter().filter(|p| p.payload == OPUS_SILENCE).collect();
        assert_eq!(silent.len(), 5, "five silence packets tell receivers the sound paused");
        assert!(tail.iter().rev().take(5).all(|p| p.payload == OPUS_SILENCE), "and they come last");
        assert!(tail.len() <= 5 + 21, "{} packets after the sound ended", tail.len());
        // Sound again: packets again.
        let paused = rig.sfu.audio.len();
        play(&rig, (0..20).map(tone)).await;
        rig.sfu.until("Opus packets after the pause", |s| s.audio.len() >= paused + 8).await;
        assert!(room.viewers[0].decrypt_audio(USER, &rig.sfu.audio.last().unwrap().payload).is_ok());
        // Video and audio share the transport counter.
        let unit = &synth::stream(1)[0];
        let frame = send(&mut rig, unit).await;
        room.decrypt(&frame, &prepared(unit), 0..1);
        assert!(rig.sfu.counters.windows(2).all(|w| w[1] == w[0] + 1));
        assert_eq!(rig.sfu.strays, 0);
    });
}

#[test]
fn golive_e2e_sample_streams_decode_to_the_same_pictures() {
    let samples = samples();
    if samples.is_empty() || !tool("ffmpeg", "-version") {
        return;
    }
    run(900, async {
        let dir = WorkDir::new("e2e-samples");
        for sample in samples {
            let (mut rig, _conn, mut room) = live(PARAMS, |_| {}).await;
            let mut preparer = Preparer::new();
            let (mut source, mut watched) = (Vec::new(), Vec::new());
            let (mut sent, mut packets) = (0usize, 0usize);
            for (n, au) in access_units(&sample.data).iter().enumerate() {
                let expected = match preparer.prepare(au) {
                    Ok(prepared) => prepared,
                    Err(Skip::NoPicture) => continue,
                    Err(e) => panic!("{} unit {n}: {e:?}", sample.name),
                };
                // From the first keyframe, as the worker sends.
                if sent == 0 && !expected.keyframe {
                    continue;
                }
                let frame = send(&mut rig, au).await;
                assert_eq!(&frame.data[frame.data.len() - 2..], &[0xfa, 0xfa]);
                let plain = room.viewers[0].decrypt_video(USER, &through_webrtc_receiver(&frame.data)).unwrap_or_else(|e| panic!("{} unit {n}: {e}", sample.name));
                assert!(plain == expected.data, "{} unit {n}: the viewer's unit is not the prepared one", sample.name);
                source.extend_from_slice(au);
                watched.extend_from_slice(&plain);
                sent += 1;
                packets += frame.packets.len();
                if sent == 400 {
                    break;
                }
            }
            assert!(sent > 0, "{}: nothing was sent", sample.name);
            assert_eq!(rig.sfu.strays, 0);
            let stem = sample.name.replace('.', "_");
            let (source_file, watched_file) = (dir.0.join(format!("{stem}.encoder.h264")), dir.0.join(format!("{stem}.viewer.h264")));
            std::fs::write(&source_file, &source).unwrap();
            std::fs::write(&watched_file, &watched).unwrap();
            let (expected, got) = (framemd5(&source_file), framemd5(&watched_file));
            assert!(!expected.is_empty(), "{}: ffmpeg decoded nothing", sample.name);
            assert_eq!(got.len(), expected.len(), "{}: pictures", sample.name);
            assert!(got == expected, "{}: a picture differs at the viewer", sample.name);
            println!("{}: {sent} units in {packets} packets through the worker, the scripted media server and a DAVE viewer -> ffmpeg: {} of {} pictures identical to the encoder's", sample.name, got.len(), expected.len());
            rig.handle.stop();
        }
    });
}

// ---- What the first live runs showed, and what the review found -----------------------------

/// A stream without DAVE (protocol version 0) up to "live": one viewer is connected.
async fn live_plain(params: Params, tune: impl FnOnce(&mut Tuning)) -> (Rig, Conn) {
    let mut rig = Rig::new(params, tune).await;
    rig.announce().await;
    let mut conn = rig.accept().await;
    assert!(rig.handshake(&mut conn, 0, None).await.is_none());
    rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
    conn.send_json(11, json!({ "user_ids": [V1.to_string()] })).await;
    Room::started(&mut conn, AUDIO_SSRC).await;
    rig.status_when("live", |s| s.phase == Phase::Live).await;
    (rig, conn)
}

/// A keyframe of about `bytes`: the parameter sets of a real one and a made-up IDR slice
/// (nothing reads a slice when DAVE is off).
fn big_keyframe(bytes: usize) -> Vec<u8> {
    let real = &synth::stream(1)[0];
    let mut out = Vec::new();
    for nal in nal_units(real).filter(|nal| matches!(nal[0] & 0x1f, 7 | 8)) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out.extend_from_slice(&[0, 0, 0, 1, 0x65]);
    out.extend(std::iter::repeat(0xab).take(bytes));
    out
}

/// The most packets that arrived within any `window`.
fn densest(arrivals: &[Instant], window: Duration) -> usize {
    (0..arrivals.len()).map(|first| arrivals[first..].iter().take_while(|at| at.duration_since(arrivals[first]) <= window).count()).max().unwrap_or(0)
}

#[test]
fn golive_e2e_the_rate_follows_the_media_servers_estimate() {
    run(60, async {
        // 8 Mbit/s are announced. With stream audio 0.1 of it is the audio's; the encoder is
        // asked for 88 % of the rest, because packets add to its bytes.
        let units = synth::stream(1);
        let (mut rig, _conn, _room) = live(Params { audio: true, ..PARAMS }, |_| {}).await;
        assert_eq!(rig.shared().encoder_cap(), 6_952_000);
        send(&mut rig, &units[0]).await;
        sleep(Duration::from_millis(350)).await;
        // An estimate below what is sent: followed at once, a tenth below it. The share's
        // GPU encoder takes a new rate only with a keyframe, so one is asked for.
        let before = rig.keyframes();
        rig.sfu.send_rtcp(&rtcp::remb(7, 3_000_000, &[VIDEO_SSRC])).await;
        rig.when("the rate is down at once", |r| r.shared().encoder_cap() == 2_288_000).await;
        rig.when("with a keyframe, so that the encoder takes it now", |r| r.keyframes() == before + 1).await;
        // The estimate recovers: what is asked of the encoder does not follow every move
        // (each is a keyframe); it steps up after eight seconds (the rate tests show how).
        rig.sfu.send_rtcp(&rtcp::remb(7, 9_000_000, &[VIDEO_SSRC])).await;
        sleep(Duration::from_millis(1000)).await;
        rig.sfu.send_rtcp(&rtcp::remb(7, 9_000_000, &[VIDEO_SSRC])).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!((rig.shared().encoder_cap(), rig.keyframes()), (2_288_000, before + 1));
        // An estimate of next to nothing does not stop the stream: the floor.
        rig.sfu.send_rtcp(&rtcp::remb(7, 20_000, &[VIDEO_SSRC])).await;
        rig.when("the floor", |r| r.shared().encoder_cap() == 352_000).await;
        if let Some(said) = logged("the most the encoder may use goes from") {
            assert!(said, "the log says when the rate asked of the encoder changes");
        }

        // LITECORD_GOLIVE_REMB=0: the estimate is only shown, as in the first version.
        let (mut rig, _conn, _room) = live(PARAMS, |t| t.switches.follow_remb = false).await;
        assert_eq!(rig.shared().encoder_cap(), 7_040_000);
        rig.sfu.send_rtcp(&rtcp::remb(7, 1_000_000, &[VIDEO_SSRC])).await;
        sleep(Duration::from_millis(200)).await;
        assert_eq!(rig.shared().encoder_cap(), 7_040_000);

        // LITECORD_GOLIVE_MAX_KBPS (through the tuning): a lower maximum is what opcode 12
        // announces and what the wire is held to.
        let (rig, conn, _room) = live(PARAMS, |t| t.max_bitrate = 5_000_000).await;
        assert_eq!(conn.video["streams"][0]["max_bitrate"], 5_000_000);
        assert_eq!(rig.shared().encoder_cap(), 4_400_000);
    });
}

#[test]
fn golive_e2e_all_viewers_together_get_one_keyframe_a_second() {
    run(60, async {
        let units = synth::stream(1);
        let (mut rig, mut conn, mut room) = live(PARAMS, |t| t.keyframe_every = Duration::from_secs(1)).await;
        // The keyframe the stream starts with.
        send(&mut rig, &units[0]).await;
        let (before, begun) = (rig.keyframes(), Instant::now());
        // Several viewers stall and ask, again and again, for 400 ms.
        for request in 0..10u8 {
            rig.sfu.send_rtcp(&rtcp::pli(7, VIDEO_SSRC)).await;
            rig.sfu.send_rtcp(&rtcp::fir(8, VIDEO_SSRC, request)).await;
            sleep(Duration::from_millis(40)).await;
        }
        assert_eq!(rig.keyframes(), before, "within a second of a keyframe nobody is given another");
        // The request is kept, not dropped: whoever asked still needs one.
        rig.when("the kept request is granted", |r| r.keyframes() == before + 1).await;
        assert!(begun.elapsed() >= Duration::from_millis(800), "granted after {:?}", begun.elapsed());
        sleep(Duration::from_millis(400)).await;
        assert_eq!(rig.keyframes(), before + 1, "one keyframe for twenty requests");
        // A key change while sending (a viewer joins) waits its turn too; the others go on
        // watching meanwhile.
        send(&mut rig, &units[0]).await;
        let (before, begun) = (rig.keyframes(), Instant::now());
        room.join(&mut conn, V2, false).await;
        rig.when("a keyframe for the new viewer", |r| r.keyframes() == before + 1).await;
        assert!(begun.elapsed() >= Duration::from_millis(800), "granted after {:?}", begun.elapsed());
        let frame = send(&mut rig, &units[0]).await;
        room.decrypt(&frame, &prepared(&units[0]), 0..2);
    });
}

#[test]
fn golive_e2e_a_nack_for_audio_is_answered_with_the_packet_again() {
    run(60, async {
        // (What the first live run showed: the media server asks for lost audio packets
        // too, and audio has no retransmission stream.)
        let (mut rig, _conn, mut room) = live(Params { audio: true, ..PARAMS }, |_| {}).await;
        rig.sfu.until("the opening silence", |s| s.audio.len() >= 10).await;
        let opening = rig.sfu.audio.len();
        let tone = |n: usize| -> Vec<i16> { (0..480).map(|i| (((n * 480 + i) as f32 * 0.06).sin() * 9000.0) as i16).collect() };
        play(&rig, (0..30).map(tone)).await;
        rig.sfu.until("the Opus packets of the tone", |s| s.audio.len() >= opening + 12).await;
        rig.sfu.listen(Duration::from_millis(100)).await;
        let count = rig.sfu.audio.len();
        let lost = rig.sfu.audio[count - 3].clone();
        rig.sfu.send_rtcp(&rtcp::nack(7, AUDIO_SSRC, &[lost.sequence, lost.sequence.wrapping_sub(5000)])).await;
        rig.sfu.until("the audio packet again", |s| s.audio.len() > count).await;
        let again = rig.sfu.audio.last().unwrap().clone();
        assert_eq!((again.sequence, again.timestamp, again.payload_type, again.ssrc), (lost.sequence, lost.timestamp, 120, AUDIO_SSRC), "the same packet, on the audio SSRC");
        assert_eq!(again.payload, lost.payload);
        assert!(room.viewers[0].decrypt_audio(USER, &again.payload).is_ok());
        rig.sfu.listen(Duration::from_millis(150)).await;
        assert_eq!(rig.sfu.audio.len(), count + 1, "a packet that was never sent is not made up");
        assert!(rig.sfu.rtx.is_empty(), "nothing on the video's retransmission stream");
        assert!(rig.sfu.counters.windows(2).all(|w| w[1] == w[0] + 1), "sealed anew, with the next transport counter");
        if let Some(said) = logged("packets of the audio") {
            assert!(said, "the log says which stream a NACK was for");
        }
    });
}

#[test]
fn golive_e2e_a_keyframe_leaves_as_a_stream_of_packets() {
    run(90, async {
        let key = big_keyframe(120_000);
        // 8 Mbit/s allowed, packets leave at two and a half times that: 130 kB take about 50 ms.
        let (mut rig, _conn) = live_plain(PARAMS, |_| {}).await;
        rig.handle.offer_frame(&key);
        let frame = rig.sfu.next_frame(WAIT).await.expect("the keyframe");
        assert_eq!(frame.data, prepared(&key));
        let packets = frame.packets.len();
        assert!(packets >= 100, "{packets} packets");
        let arrivals = rig.sfu.video_at[rig.sfu.video_at.len() - packets..].to_vec();
        let spread = arrivals[packets - 1].duration_since(arrivals[0]);
        assert!(spread >= Duration::from_millis(30) && spread <= Duration::from_millis(150), "the keyframe took {spread:?} to arrive");
        let burst = densest(&arrivals, Duration::from_millis(5));
        assert!(burst <= 45, "{burst} packets within 5 ms");
        // The pictures behind it follow in order, and nothing is dropped as too old.
        let units = synth::stream(11);
        for unit in &units[1..5] {
            let frame = send(&mut rig, unit).await;
            assert_eq!(frame.data, prepared(unit));
        }

        // LITECORD_GOLIVE_PACE=burst: as the first version sent it, 48 packets at once and
        // 12 more every millisecond.
        let (mut rig, _conn) = live_plain(PARAMS, |t| t.switches.pace = 0).await;
        rig.handle.offer_frame(&key);
        let frame = rig.sfu.next_frame(WAIT).await.expect("the keyframe");
        let packets = frame.packets.len();
        let arrivals = rig.sfu.video_at[rig.sfu.video_at.len() - packets..].to_vec();
        let spread = arrivals[packets - 1].duration_since(arrivals[0]);
        assert!(spread <= Duration::from_millis(25), "the keyframe took {spread:?} to arrive");
        assert!(densest(&arrivals, Duration::from_millis(5)) >= 48);

        // A low estimate does not hold a keyframe back until it is too old to send: whatever
        // is queued leaves within about 150 ms.
        let (mut rig, _conn) = live_plain(PARAMS, |_| {}).await;
        rig.sfu.send_rtcp(&rtcp::remb(7, 600_000, &[VIDEO_SSRC])).await;
        rig.when("the low estimate is followed", |r| r.shared().encoder_cap() < 600_000).await;
        rig.handle.offer_frame(&key);
        let frame = rig.sfu.next_frame(WAIT).await.expect("the keyframe, whole");
        assert_eq!(frame.data, prepared(&key));
        let packets = frame.packets.len();
        let arrivals = rig.sfu.video_at[rig.sfu.video_at.len() - packets..].to_vec();
        let spread = arrivals[packets - 1].duration_since(arrivals[0]);
        assert!(spread >= Duration::from_millis(90) && spread <= Duration::from_millis(220), "the keyframe took {spread:?} to arrive");
    });
}

/// The wall-clock time a sender report names, in seconds since 1970.
fn report_time(ntp: (u32, u32)) -> f64 {
    ntp.0 as f64 - 2_208_988_800.0 + ntp.1 as f64 / 4_294_967_296.0
}

#[test]
fn golive_e2e_pictures_of_an_uneven_capture_are_stamped_and_sent_evenly() {
    run(240, async {
        let units = synth::stream(331);
        // The rhythm of the fourth live run: a 144 Hz monitor shared at 60 pictures a
        // second, as the capture let its first pictures pass (7 to 35 ms apart, 60 a second
        // on average). And the same monitor once the capture's own rhythm is fixed: one
        // picture per three refreshes, a steady 48 a second.
        let run4: Vec<Duration> = RUN4_STEPS[..330].iter().map(|ticks| Duration::from_micros(*ticks as u64 * 100 / 9)).collect();
        let steady48 = vec![Duration::from_micros(20_833); 330];
        // (clock: as the first live runs sent it. even: the grid. even and held: the default.)
        for (name, rhythm, even, hold) in [("run 4", &run4, false, 0u32), ("run 4", &run4, true, 0), ("run 4", &run4, true, 20), ("48 a second", &steady48, true, 20)] {
            let dir = WorkDir::new(&format!("e2e-cadence-{}-{}-{hold}", name.replace(' ', "-"), if even { "even" } else { "clock" }));
            let dump = dir.0.clone();
            let (mut rig, _conn, _room) = live(PARAMS, move |t| {
                t.switches.even_timestamps = even;
                t.switches.hold_ms = hold;
                t.switches.dump = Some(dump);
            })
            .await;
            let mut due = Instant::now() + Duration::from_millis(20);
            let mut offered = Vec::new();
            for (n, unit) in units.iter().enumerate() {
                // (The media server reads while it waits, so arrivals are timed as they come.)
                rig.sfu.listen(due.saturating_duration_since(Instant::now())).await;
                offered.push((Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()));
                rig.handle.offer_frame(unit);
                due += rhythm.get(n).copied().unwrap_or_default();
            }
            rig.sfu.until("every picture", |s| s.video.iter().filter(|p| p.marker).count() >= 331).await;
            let last_packets: Vec<(u32, Instant)> = rig.sfu.video.iter().zip(&rig.sfu.video_at).filter(|(p, _)| p.marker).map(|(p, at)| (p.timestamp, *at)).collect();
            assert_eq!(last_packets.len(), units.len(), "no picture was dropped");
            let stamps: Vec<u32> = last_packets.iter().map(|p| p.0).collect();
            let steps: Vec<i64> = stamps.windows(2).map(|w| w[1].wrapping_sub(w[0]) as i32 as i64).collect();
            let gaps: Vec<f64> = offered.windows(2).map(|w| w[1].0.duration_since(w[0].0).as_secs_f64() * 90_000.0).collect();
            let arrivals: Vec<f64> = last_packets.windows(2).map(|w| w[1].1.duration_since(w[0].1).as_secs_f64() * 1e3).collect();
            let after: Vec<f64> = last_packets.iter().zip(&offered).map(|(packet, offered)| packet.1.saturating_duration_since(offered.0).as_secs_f64() * 1e3).collect();
            // (The grid is taken up once the source's pace is known, after 24 pictures, and
            // the hold comes in over the 40 after: what is judged starts behind that.)
            let (settled, held) = (30, 80);
            let (never_shown, repeated) = on_a_60_hz_screen(&stamps[settled..]);
            println!(
                "{name}, TS={} HOLD={hold}: timestamp steps {} to {} ticks; on a 60 Hz screen by timestamp {never_shown} of {} pictures never shown, {repeated} refreshes repeat one; arrivals {:.1} to {:.1} ms apart, {:.1} to {:.1} ms after the picture was offered",
                if even { "even" } else { "clock" },
                steps[settled..].iter().min().unwrap(),
                steps[settled..].iter().max().unwrap(),
                stamps.len() - settled,
                arrivals[held..].iter().cloned().fold(f64::MAX, f64::min),
                arrivals[held..].iter().cloned().fold(0.0, f64::max),
                after[held..].iter().cloned().fold(f64::MAX, f64::min),
                after[held..].iter().cloned().fold(0.0, f64::max)
            );
            let follows_the_source = |from: usize| {
                let off = steps[from..].iter().zip(&gaps[from..]).filter(|(step, gap)| (**step as f64 - **gap).abs() > 180.0).count();
                assert!(off <= steps.len() / 20, "{name}: {off} of {} steps are more than 2 ms off the source's rhythm", steps.len());
            };
            let on_the_grid = even && name == "run 4";
            if on_the_grid {
                // One frame interval (1500 ticks of 90 kHz) apart, whatever the source's
                // rhythm: every picture has a refresh of its own.
                let (least, most) = (steps[settled..].iter().min().unwrap(), steps[settled..].iter().max().unwrap());
                assert!(*least >= 1400 && *most <= 1600, "steps from {least} to {most}");
                // (What a 60 Hz screen makes of them is printed above and not judged here:
                // with pictures fed in real time the count moves by a few with where the
                // refreshes fall. `rate`'s tests count it on the exact instants of the live
                // run: 28 of 901 pictures never shown as sent, none on the grid.)
            } else if name == "run 4" {
                // The instants the pictures were offered at, as in the live runs: steps of
                // half a frame interval and of one and a half and more. A 60 Hz screen that
                // goes by them cannot show them all (how many it loses depends on where its
                // refreshes fall: `rate`'s tests count 28 of 901 on the exact instants).
                follows_the_source(0);
                assert!(steps.iter().filter(|s| **s < 1_000).count() >= 8 && steps.iter().filter(|s| **s > 2_100).count() >= 8);
            } else {
                // Not the share's pace: an even source is left as it is, timestamps and
                // sending both.
                follows_the_source(0);
                assert!(steps.iter().filter(|s| (1_700..=2_050).contains(*s)).count() >= steps.len() * 19 / 20, "20.8 ms apart, as offered");
                assert!(after.iter().cloned().fold(0.0, f64::max) <= 12.0, "nothing is held back");
            }
            if on_the_grid && hold > 0 {
                // Held for its place on the grid, each picture also leaves a frame interval
                // after the one before.
                let uneven = arrivals[held..].iter().filter(|gap| (**gap - 16.67).abs() > 4.0).count();
                assert!(uneven <= arrivals.len() / 20, "{uneven} of {} pictures arrived more than 4 ms off the frame interval", arrivals.len());
                let latest = after.iter().cloned().fold(0.0, f64::max);
                assert!(latest <= 55.0, "a picture left {latest:.1} ms after it was offered");
            } else if name == "run 4" {
                // Sent as they come: the arrivals have the capture's rhythm.
                assert!(arrivals.iter().filter(|gap| **gap < 10.0).count() >= 8 && arrivals.iter().filter(|gap| **gap > 24.0).count() >= 8);
            }
            // Either way the timestamps stay on the wall clock the sender reports name: a
            // report's (time, RTP time) pair lies on the line through the pictures, within
            // what the grid may be off the clock.
            rig.sfu.listen(Duration::from_millis(50)).await;
            let reports: Vec<(f64, u32)> = rig.sfu.reports.iter().filter_map(|r| if let rtcp::Packet::SenderReport { ssrc, ntp, rtp_timestamp, .. } = r { (*ssrc == VIDEO_SSRC).then_some((report_time(*ntp), *rtp_timestamp)) } else { None }).collect();
            assert!(reports.len() >= 4, "{} sender reports in five seconds", reports.len());
            let (report_at, report_stamp) = reports[reports.len() - 1];
            let nearest = (0..offered.len()).min_by(|a, b| (offered[*a].1 - report_at).abs().total_cmp(&(offered[*b].1 - report_at).abs())).unwrap();
            let expected = stamps[nearest] as f64 + (report_at - offered[nearest].1) * 90_000.0;
            let apart = (report_stamp.wrapping_sub(expected as u32) as i32).abs();
            assert!(apart <= if on_the_grid { 1_500 } else { 270 }, "the report is {apart} ticks off the pictures' clock");
            // The 5 s line shows both rhythms and whether the grid is in use, and the dump
            // has a line per picture.
            if let Some(said) = logged(if on_the_grid { "(on the even grid) and sent" } else if even { "(not the share's pace: stamped as they come) and sent" } else { "(stamped as they come) and sent" }) {
                assert!(said, "the 5 s line shows how evenly pictures come and leave");
            }
            let log = std::fs::read_to_string(dir.0.join("packets.log")).unwrap();
            assert_eq!(log.lines().filter(|l| l.contains(" frame offered=")).count(), units.len());
            rig.handle.stop();
        }
    });
}

#[test]
fn golive_e2e_no_packet_leaves_before_speaking_has_been_read() {
    run(60, async {
        let mut rig = Rig::new(PARAMS, |t| t.speaking_lead = Duration::from_millis(250)).await;
        rig.announce().await;
        let mut conn = rig.accept().await;
        let room = Room::new(GROUP);
        // (The handshake ends with the gateway reading Speaking, the key package and Video.)
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        let read = Instant::now();
        rig.sfu.listen(Duration::from_millis(120)).await;
        assert!(rig.sfu.audio.is_empty() && rig.sfu.video.is_empty(), "nothing is sent while the server may not have read Speaking yet");
        rig.sfu.until("the first silence packet", |s| !s.audio.is_empty()).await;
        assert!(rig.sfu.audio_at[0].duration_since(read) >= Duration::from_millis(100));
        assert_eq!(rig.sfu.strays, 0);
    });
}

#[test]
fn golive_e2e_a_gateway_that_closes_after_the_handshake_is_given_up_on() {
    run(120, async {
        // The server takes the whole handshake and then ends the session, every time.
        let mut rig = Rig::new(PARAMS, |t| t.retry_delays = vec![Duration::from_millis(20); 5]).await;
        rig.announce().await;
        let mls = MlsGateway::new();
        for _ in 0..5 {
            let mut conn = rig.accept().await;
            rig.handshake(&mut conn, 1, Some(&mls.external_sender())).await.expect("a key package");
            conn.close(4006).await;
        }
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("4006".into())));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        assert!(timeout(Duration::from_millis(250), rig.listener.accept()).await.is_err(), "no sixth session");

        // The same with a session that is resumed and dropped again, every time.
        let (mut rig, mut conn, _room) = live(PARAMS, |t| t.retry_delays = vec![Duration::from_millis(20); 5]).await;
        for round in 0..5 {
            let seq = conn.seq;
            conn.close(1001).await;
            if round == 4 {
                break;
            }
            conn = rig.accept().await;
            conn.seq = seq;
            conn.expect_json(7).await;
            conn.send_plain(8, json!({ "v": 8, "heartbeat_interval": 1000 })).await;
            conn.send_plain(9, Value::Null).await;
            rig.status_when("live again", |s| s.phase == Phase::Live).await;
        }
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("1001".into())));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        assert!(timeout(Duration::from_millis(250), rig.listener.accept()).await.is_err(), "no sixth connection");

        // A session that was in service for a while before it dropped is not a failed
        // attempt: it is resumed however often that happens.
        let (rig, mut conn, _room) = live(PARAMS, |t| {
            t.retry_delays = vec![Duration::from_millis(20); 5];
            t.settled_after = Duration::from_millis(60);
        })
        .await;
        for _ in 0..7 {
            sleep(Duration::from_millis(120)).await;
            let seq = conn.seq;
            conn.close(1001).await;
            conn = rig.accept().await;
            conn.seq = seq;
            conn.expect_json(7).await;
            conn.send_plain(8, json!({ "v": 8, "heartbeat_interval": 1000 })).await;
            conn.send_plain(9, Value::Null).await;
            rig.status_when("live again", |s| s.phase == Phase::Live).await;
        }
        assert!(rig.status().failure.is_none());
    });
}

#[test]
fn golive_e2e_a_deletion_the_main_gateway_could_not_take_is_sent_later() {
    run(90, async {
        // The button is pressed while the main gateway is between two connections.
        let (mut rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        rig.main_up.store(false, Ordering::SeqCst);
        rig.handle.stop();
        assert_eq!(conn.expect_json(12).await["video_ssrc"], 0);
        assert_eq!(conn.expect_close().await, Some(1000));
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert!(status.failure.is_none());
        assert!(rig.handle.finished(), "for the application the stream is over");
        sleep(Duration::from_millis(300)).await;
        assert!(rig.main_idle() && !rig.handle.thread_finished(), "its thread stays until Discord has been told");
        rig.main_up.store(true, Ordering::SeqCst);
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        rig.when("the worker's thread ends", |r| r.handle.thread_finished()).await;
        assert!(rig.main_idle(), "one opcode 19");

        // The stream fails while the main gateway is down: the same.
        let (mut rig, conn, _room) = live(PARAMS, |_| {}).await;
        rig.main_up.store(false, Ordering::SeqCst);
        conn.close(4014).await;
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("4014".into())));
        // (Stop, pressed again meanwhile, changes nothing.)
        rig.handle.stop();
        sleep(Duration::from_millis(200)).await;
        assert!(rig.main_idle());
        rig.main_up.store(true, Ordering::SeqCst);
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
        rig.when("the worker's thread ends", |r| r.handle.thread_finished()).await;

        // A new stream is requested before the old deletion could be sent: it has the same
        // key, so that deletion is dropped.
        let (mut rig, conn, _room) = live(PARAMS, |_| {}).await;
        rig.main_up.store(false, Ordering::SeqCst);
        conn.close(4014).await;
        rig.status_when("off", |s| s.phase == Phase::Off).await;
        rig.handle.forget();
        rig.when("the worker's thread ends", |r| r.handle.thread_finished()).await;
        rig.main_up.store(true, Ordering::SeqCst);
        sleep(Duration::from_millis(1300)).await;
        assert!(rig.main_idle(), "nothing is deleted");

        // The main gateway never comes back: given up after a while.
        let (mut rig, conn, _room) = live(PARAMS, |t| t.delete_retry_for = Duration::from_millis(1200)).await;
        rig.main_up.store(false, Ordering::SeqCst);
        conn.close(4014).await;
        rig.when("the worker's thread ends", |r| r.handle.thread_finished()).await;
        assert!(rig.main_idle());
    });
}

#[test]
fn golive_e2e_the_events_of_an_earlier_stream_do_not_end_this_one() {
    run(60, async {
        // The button was turned off and on again before the first request was answered:
        // this worker's request is out, and what arrives first is the earlier stream.
        let mut rig = Rig::new(PARAMS, |_| {}).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        rig.handle.event(rig.create_event());
        rig.handle.event(rig.server_event());
        let mut earlier = rig.accept().await;
        earlier.expect_json(0).await;
        // The answer to the stop before this start: that stream is deleted.
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "user_requested".into(), unavailable: false });
        assert!(matches!(earlier.next().await, Got::Closed(_)), "the earlier stream's server is left");
        let status = rig.status_when("waiting for its own stream", |s| s.phase == Phase::Creating).await;
        assert!(status.failure.is_none());
        assert!(rig.main_idle(), "nothing to delete: Discord did");
        // This request's own stream follows, and is served.
        rig.handle.event(rig.create_event());
        rig.handle.event(rig.server_event());
        let mut conn = rig.accept().await;
        let mut room = Room::new(GROUP);
        rig.handshake(&mut conn, 1, Some(&room.mls.external_sender())).await.unwrap();
        room.join(&mut conn, V1, false).await;
        Room::started(&mut conn, AUDIO_SSRC).await;
        rig.status_when("live", |s| s.phase == Phase::Live).await;
        rig.handle.stop();
        stopped(&mut rig, &mut conn).await;

        // Once the session was in service, "user_requested" is somebody ending this stream
        // (from another device of the account): it ends, with a notice.
        let (mut rig, _conn, _room) = live(PARAMS, |_| {}).await;
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "user_requested".into(), unavailable: false });
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Ended("user_requested".into())));
        assert!(rig.main_idle());

        // No stream follows the stale deletion: the request counts as unanswered.
        let mut rig = Rig::new(PARAMS, |t| t.create_timeout = Duration::from_millis(400)).await;
        rig.shared().capture_ready();
        rig.main_op(18).await;
        rig.handle.event(rig.create_event());
        rig.handle.event(Event::Delete { stream_key: rig.key(), reason: "user_requested".into(), unavailable: false });
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::NoAnswer));
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }));
    });
}

#[test]
fn golive_e2e_an_internal_error_ends_the_stream_and_not_the_share() {
    run(60, async {
        let unit = synth::stream(1).remove(0);
        let (mut rig, mut conn, room) = live(PARAMS, |_| {}).await;
        // Something the worker calls panics (here the list of voice channel members, whose
        // lock is poisoned on purpose).
        let users = std::sync::Arc::clone(&rig.voice_users);
        let _ = std::thread::spawn(move || {
            let _held = users.lock().unwrap();
            panic!("poisoned on purpose, for golive_e2e_an_internal_error_ends_the_stream_and_not_the_share");
        })
        .join();
        let (_viewer, key_package) = Viewer::new(false, V2, GROUP, &room.mls);
        conn.send_binary(27, &room.mls.propose_add(GROUP, room.epoch, &[&key_package])).await;
        let status = rig.status_when("off", |s| s.phase == Phase::Off).await;
        assert_eq!(status.failure, Some(Failure::Lost("internal".into())));
        assert!(!rig.shared().wanted(), "the share stops encoding for Discord");
        assert_eq!(rig.shared().take_notice(), Some(Failure::Lost("internal".into())), "and the user is told");
        assert_eq!(rig.main_op(19).await, json!({ "stream_key": rig.key() }), "Discord is told the stream is over");
        rig.when("the worker's thread ends", |r| r.handle.thread_finished()).await;
        assert!(rig.handle.finished());
        // Pictures offered afterwards ask nothing of the encoder.
        let before = rig.keyframes();
        for _ in 0..10 {
            rig.handle.offer_frame(&unit);
        }
        assert_eq!(rig.keyframes(), before);
    });
}

#[test]
fn golive_e2e_a_transition_without_a_version_is_not_a_downgrade() {
    run(60, async {
        let units = synth::stream(6);
        let (mut rig, mut conn, mut room) = live(PARAMS, |_| {}).await;
        // Opcode 21 without the fields it needs: ignored, the stream stays end-to-end encrypted.
        conn.send_json(21, json!({ "transition_id": 0 })).await;
        conn.send_json(21, json!({ "protocol_version": 0 })).await;
        conn.send_json(21, json!({ "transition_id": "0", "protocol_version": null })).await;
        sleep(Duration::from_millis(100)).await;
        assert!(rig.status().e2ee);
        let frame = send(&mut rig, &units[0]).await;
        room.decrypt(&frame, &prepared(&units[0]), 0..1);
        // A downgrade the gateway really announces is followed, and the status says so (the
        // window shows it).
        conn.send_json(21, json!({ "transition_id": 0, "protocol_version": 0 })).await;
        rig.status_when("not end-to-end encrypted", |s| !s.e2ee && s.phase == Phase::Live).await;
        let frame = send(&mut rig, &units[1]).await;
        assert_eq!(frame.data, prepared(&units[1]), "sent as it is");
        // With nobody connected, a stream without DAVE sends nothing.
        conn.send_json(13, json!({ "user_id": V1.to_string() })).await;
        rig.status_when("waiting for viewers", |s| s.phase == Phase::Waiting).await;
        assert!(!rig.shared().wanted());
    });
}

#[test]
fn golive_e2e_the_periodic_keyframe_of_a_watched_stream() {
    run(60, async {
        // One a minute by default: official senders aim at that and answer requests.
        let (rig, mut conn, _room) = live(PARAMS, |_| {}).await;
        assert_eq!(rig.shared().keyframe_interval(), Some(Duration::from_secs(60)));
        // What the gateway names is logged and, by default, not followed.
        conn.send_json(14, json!({ "video_codec": "H264", "keyframe_interval": 4000 })).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(rig.shared().keyframe_interval(), Some(Duration::from_secs(60)));
        if let Some(said) = logged("names a keyframe interval of 4000 ms") {
            assert!(said);
        }
        // LITECORD_GOLIVE_KEYINT=server follows it; 0 is none at all; share leaves the
        // share's own rule (None: every 2 s).
        let (rig, mut conn, _room) = live(PARAMS, |t| t.switches.keyint = KeyInt::Server).await;
        assert_eq!(rig.shared().keyframe_interval(), Some(Duration::from_secs(60)), "until the gateway names one");
        conn.send_json(14, json!({ "video_codec": "H264", "keyframe_interval": 4000 })).await;
        rig.when("the gateway's interval is followed", |r| r.shared().keyframe_interval() == Some(Duration::from_secs(4))).await;
        let rig = Rig::new(PARAMS, |t| t.switches.keyint = KeyInt::Seconds(0)).await;
        rig.when("no periodic keyframe", |r| r.shared().keyframe_interval() == Some(Duration::MAX)).await;
        let rig = Rig::new(PARAMS, |t| t.switches.keyint = KeyInt::Share).await;
        sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.shared().keyframe_interval(), None);
        let rig = Rig::new(PARAMS, |t| t.switches.keyint = KeyInt::Seconds(10)).await;
        rig.when("every ten seconds", |r| r.shared().keyframe_interval() == Some(Duration::from_secs(10))).await;
    });
}

#[test]
fn golive_e2e_the_playout_delay_and_the_constant_elements_can_be_changed() {
    run(90, async {
        let units = synth::stream(3);
        let cases: [(Playout, u8, &[u8], Option<&[u8]>); 4] = [
            // LITECORD_GOLIVE_PLAYOUT=0: as the first live runs sent it.
            (Playout::ASAP, 0, &[2, 3, 4, 5, 6, 7, 8, 10, 11], Some(&[0, 0, 0])),
            // LITECORD_GOLIVE_PLAYOUT=none: no element at all.
            (Playout::None, 0, &[2, 3, 4, 5, 7, 8, 10, 11], None),
            // LITECORD_GOLIVE_PLAYOUT=20,400: 200 ms to 4 s.
            (Playout::Limits { min: 20, max: 400 }, 0, &[2, 3, 4, 5, 6, 7, 8, 10, 11], Some(&[0x01, 0x41, 0x90])),
            // LITECORD_GOLIVE_EXT_OMIT=toffset,orientation,content,timing with the default playout delay.
            (Playout::SMOOTH, OMIT_TOFFSET | OMIT_ORIENTATION | OMIT_CONTENT | OMIT_TIMING, &[3, 5, 6, 10, 11], Some(&[0x00, 0x53, 0xe8])),
        ];
        for (playout, omit, ids, element) in cases {
            let (mut rig, _conn, mut room) = live(PARAMS, |t| {
                t.switches.playout = playout;
                t.switches.ext_omit = omit;
            })
            .await;
            for unit in &units {
                let frame = send(&mut rig, unit).await;
                room.decrypt(&frame, &prepared(unit), 0..1);
                for p in &frame.packets {
                    assert_eq!(p.extension.iter().map(|e| e.0).collect::<Vec<_>>(), ids, "{playout:?}, omit {omit}");
                    assert_eq!(p.element(6), element);
                }
            }
            // A retransmission carries the same elements, with the repaired rid.
            let lost = rig.sfu.video[0].sequence;
            rig.sfu.send_rtcp(&rtcp::nack(7, VIDEO_SSRC, &[lost])).await;
            rig.sfu.until("the retransmission", |s| !s.rtx.is_empty()).await;
            assert_eq!(rig.sfu.rtx[0].element(6), element);
            assert_eq!(rig.sfu.rtx[0].element(12), Some(&b"100"[..]));
            assert_eq!(rig.sfu.strays, 0);
            rig.handle.stop();
        }
    });
}
