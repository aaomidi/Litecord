//! T6: DAVE for the stream. The sender is `golive::dave` (davey 0.2.0); the gateway is an
//! MLS external sender made with openmls, as Discord's is; the viewers are DAVE sessions of
//! their own, one on davey 0.2.0 and one on davey 0.1.4 (the crate that already talks to
//! official clients in voice calls). Frames travel as they do on the wire: encrypted,
//! packetized, put back together the way libwebrtc's receiver does it, decrypted.

use serde_json::json;

use super::super::dave::*;
use super::super::depacketize::frame_of;
use super::super::h264::{nal_units, Preparer, Skip, NAL_SPS};
use super::super::rtp::{packetize_h264, MAX_PAYLOAD, OPUS_SILENCE};
use super::fake::{key_package_of, split_commit_welcome, through_webrtc_receiver, MlsGateway, Viewer, GROUP, RTC_CHANNEL, RTC_SERVER};
use super::h264::webrtc_leaves_it_alone;
use super::{access_units, samples, synth};

const S: u64 = 1_111_111_111_111_111;
const V: u64 = 2_222_222_222_222_222;
const W: u64 = 3_333_333_333_333_333;
const X: u64 = 4_444_444_444_444_444;

fn binary(out: &Out) -> &[u8] {
    match out {
        Out::Binary(bytes) => bytes,
        other => panic!("expected a binary message, got {other:?}"),
    }
}

fn ready(transition: u16) -> Out {
    Out::Json(json!({ "op": 23, "d": { "transition_id": transition } }))
}

fn invalid(transition: u16) -> Out {
    Out::Json(json!({ "op": 31, "d": { "transition_id": transition } }))
}

/// A slice with enough in it for DAVE to encrypt.
fn unit(seed: u8) -> Vec<u8> {
    let mut unit = vec![0, 0, 0, 1, 0x41, 0x9a];
    unit.extend((0..200u8).map(|i| i.wrapping_mul(7).wrapping_add(seed) | 0x10));
    unit
}

/// Encrypts a prepared unit and carries it the way the wire and libwebrtc's receiver do:
/// packets, then every NAL unit back behind a 4-byte start code.
fn carried(dave: &mut Dave, prepared: &[u8]) -> Vec<u8> {
    let encrypted = dave.encrypt_video(prepared).expect("a key to encrypt with");
    assert_eq!(&encrypted[encrypted.len() - 2..], &[0xfa, 0xfa], "the DAVE marker ends the frame");
    let payloads = packetize_h264(&encrypted, MAX_PAYLOAD);
    let rebuilt = frame_of(payloads.iter().map(Vec::as_slice)).expect("the frame is rebuilt from its packets");
    assert_eq!(rebuilt, encrypted, "what the packets carry is what DAVE made, byte for byte");
    rebuilt
}

/// The streamer alone on its connection, the gateway's messages in the order given.
fn streamer(mls: &MlsGateway, group: GroupChoice) -> (Dave, Vec<u8>) {
    super::log_init();
    let mut dave = Dave::new(S, group_candidates(group, RTC_SERVER, RTC_CHANNEL), false);
    // The external sender may come before Session Description: it is kept.
    assert!(dave.on_external_sender(&mls.external_sender()).is_empty());
    assert_eq!(dave.sending(), Sending::Nothing, "nothing goes out before the server said what is negotiated");
    let out = dave.on_session_description(1);
    assert_eq!(out.len(), 1, "one key package");
    let key_package = key_package_of(binary(&out[0]));
    (dave, key_package)
}

/// A stream and the people watching it, with the gateway's bookkeeping.
struct Room {
    mls: MlsGateway,
    dave: Dave,
    group: u64,
    epoch: u64,
    transition: u16,
    users: Vec<u64>,
    viewers: Vec<Viewer>,
}

impl Room {
    fn new() -> Self {
        let mls = MlsGateway::new();
        let (dave, _) = streamer(&mls, GroupChoice::Auto);
        Self { mls, dave, group: GROUP, epoch: 0, transition: 0, users: vec![S], viewers: Vec::new() }
    }

    /// `user` starts watching, and the streamer's commit is the one the gateway announces.
    fn join(&mut self, user: u64, old: bool) {
        let (mut viewer, key_package) = Viewer::new(old, user, self.group, &self.mls);
        let proposals = self.mls.propose_add(self.group, self.epoch, &[&key_package]);
        self.users.push(user);
        let out = self.dave.on_proposals(&proposals, Some(&self.users));
        assert_eq!(out.len(), 1, "one commit");
        let (commit, welcome) = split_commit_welcome(binary(&out[0]));
        self.transition += 1;
        let transition = self.transition;
        // (The members get the proposals too; their own commits lose.)
        for member in &mut self.viewers {
            member.proposals(&proposals);
        }
        assert_eq!(self.dave.on_announce_commit(transition, &commit), vec![ready(transition)]);
        for member in &mut self.viewers {
            member.committed(&commit, transition);
        }
        viewer.welcomed(&welcome.expect("a commit that adds a member comes with a welcome"), transition);
        assert!(self.dave.on_execute_transition(transition).is_empty());
        for member in &mut self.viewers {
            member.execute(transition);
        }
        viewer.execute(transition);
        self.epoch += 1;
        self.viewers.push(viewer);
    }

    /// Every viewer decrypts what the streamer sends of `prepared`, to exactly `prepared`.
    fn all_decrypt(&mut self, prepared: &[u8]) {
        let frame = carried(&mut self.dave, prepared);
        // The SPS is as libwebrtc wants it, so its receiver changes nothing.
        let received = through_webrtc_receiver(&frame);
        assert_eq!(received, frame);
        for (n, viewer) in self.viewers.iter_mut().enumerate() {
            assert_eq!(viewer.decrypt_video(S, &received).unwrap_or_else(|e| panic!("viewer {n}: {e}")), prepared, "viewer {n}");
        }
    }
}

#[test]
fn golive_dave_alone_nothing_is_produced() {
    let mls = MlsGateway::new();
    let (mut dave, _) = streamer(&mls, GroupChoice::Auto);
    assert_eq!(dave.group_id(), GROUP, "rtc_server_id - 1 is the first guess");
    assert_eq!(dave.sending(), Sending::Nothing);
    assert!(dave.end_to_end());
    assert_eq!(dave.encrypt_video(&unit(1)), None, "screen content never goes out without a key");
    assert_eq!(dave.encrypt_opus(&[0x78, 1, 2, 3]), None);
    assert_eq!(dave.members(), vec![S]);
    // The gateway's "sole member reset" while alone: a new key package, still nothing to send.
    let out = dave.on_prepare_epoch(1, 1);
    key_package_of(binary(&out[0]));
    assert!(dave.on_prepare_transition(0, 1).is_empty(), "transition 0 takes effect at once and is not answered");
    assert_eq!(dave.sending(), Sending::Nothing);
    assert_eq!(dave.encrypt_video(&unit(2)), None);
    // An execute for a transition that was never prepared, and a commit while no proposals
    // are queued, are ignored (no opcode 31).
    assert!(dave.on_execute_transition(77).is_empty());
    assert!(dave.on_announce_commit(3, &[0xde, 0xad, 0xbe, 0xef]).is_empty());
    assert!(!dave.stalled());
    // Other epochs than 1 announce nothing this client has to act on.
    assert!(dave.on_prepare_epoch(5, 1).is_empty());
}

#[test]
fn golive_dave_first_viewer_and_our_commit() {
    let mls = MlsGateway::new();
    let (mut dave, _) = streamer(&mls, GroupChoice::Auto);
    let (mut viewer, key_package) = Viewer::new(false, V, GROUP, &mls);
    let proposals = mls.propose_add(GROUP, 0, &[&key_package]);
    assert_eq!(group_id_of_proposals(&proposals[1..]), Some(GROUP), "the gateway's message names the group");
    let generation = dave.key_generation();
    let out = dave.on_proposals(&proposals, Some(&[S, V]));
    assert_eq!(out.len(), 1);
    // Opcode 28: the commit, then the welcome, with nothing in between.
    let (commit, welcome) = split_commit_welcome(binary(&out[0]));
    assert_eq!(group_id_of_message(&commit), Some(GROUP));
    assert_eq!(dave.sending(), Sending::Nothing);
    // Our commit is announced with transition 7.
    assert_eq!(dave.on_announce_commit(7, &commit), vec![ready(7)]);
    assert_eq!(dave.sending(), Sending::Nothing, "the new key is used from the execute on, not before");
    assert_eq!(dave.encrypt_video(&unit(1)), None);
    viewer.welcomed(&welcome.unwrap(), 7);
    assert!(dave.on_execute_transition(7).is_empty());
    viewer.execute(7);
    assert_eq!(dave.sending(), Sending::Encrypted);
    assert!(dave.key_generation() > generation, "the worker is told the key changed");
    assert_eq!((dave.epoch(), dave.members()), (Some(1), vec![S, V]));
    for seed in 0..20 {
        let prepared = unit(seed);
        let frame = carried(&mut dave, &prepared);
        assert_eq!(viewer.decrypt_video(S, &frame).unwrap(), prepared);
    }
    // Both ends show the same code for the call.
    let code = dave.privacy_code().expect("a privacy code once the group is there");
    assert_eq!(code.len(), 30);
    assert_eq!(Some(code), viewer.privacy_code());
    // The commit was answered: the log has nothing to say about it later.
    assert!(!dave.commit_unanswered(std::time::Instant::now() + std::time::Duration::from_secs(60)));
    // The gateway's credential again, while the group is established: ignored.
    assert!(dave.on_external_sender(&mls.external_sender()).is_empty());
    assert_eq!(dave.sending(), Sending::Encrypted);
}

#[test]
fn golive_dave_second_viewer_and_another_members_commit() {
    let mut room = Room::new();
    room.join(V, false);
    room.all_decrypt(&unit(1));
    // W arrives; this time the first viewer's commit is the one announced.
    let (mut w, key_package) = Viewer::new(false, W, GROUP, &room.mls);
    let proposals = room.mls.propose_add(GROUP, room.epoch, &[&key_package]);
    let ours = room.dave.on_proposals(&proposals, Some(&[S, V, W]));
    split_commit_welcome(binary(&ours[0]));
    let (their_commit, their_welcome) = room.viewers[0].proposals(&proposals);
    let generation = room.dave.key_generation();
    assert_eq!(room.dave.on_announce_commit(8, &their_commit), vec![ready(8)]);
    // Between "ready" and "execute" the stream goes on with the old key: the viewer that is
    // already watching keeps its picture, whether it has processed the commit yet or not.
    assert_eq!(room.dave.sending(), Sending::Encrypted);
    assert_eq!(room.dave.key_generation(), generation, "no new key yet");
    room.all_decrypt(&unit(2));
    room.viewers[0].committed(&their_commit, 8);
    w.welcomed(&their_welcome.unwrap(), 8);
    room.all_decrypt(&unit(3));
    assert!(room.dave.on_execute_transition(8).is_empty());
    room.viewers[0].execute(8);
    w.execute(8);
    room.viewers.push(w);
    assert!(room.dave.key_generation() > generation, "the execute changes the key: the worker asks for a keyframe");
    for seed in 4..12 {
        room.all_decrypt(&unit(seed));
    }
    assert_eq!((room.dave.epoch(), room.dave.members()), (Some(2), vec![S, V, W]));
}

#[test]
fn golive_dave_unexpected_user_is_refused_and_the_stream_goes_on() {
    let mut room = Room::new();
    room.join(V, false);
    let (_stranger, key_package) = Viewer::new(false, X, GROUP, &room.mls);
    let proposals = room.mls.propose_add(GROUP, room.epoch, &[&key_package]);
    // (A commit the gateway never answers is said so in the log, once.)
    let (_other, key_package) = Viewer::new(false, W, GROUP, &room.mls);
    let sent = std::time::Instant::now();
    assert_eq!(room.dave.on_proposals(&room.mls.propose_add(GROUP, room.epoch, &[&key_package]), Some(&[S, V, W])).len(), 1);
    assert!(!room.dave.commit_unanswered(sent + std::time::Duration::from_secs(4)));
    assert!(room.dave.commit_unanswered(sent + std::time::Duration::from_secs(6)));
    assert!(!room.dave.commit_unanswered(sent + std::time::Duration::from_secs(60)));
    // X is neither in the voice channel nor announced by the stream server.
    assert!(room.dave.on_proposals(&proposals, Some(&[S, V])).is_empty(), "no commit for a user nobody announced");
    assert_eq!(room.dave.sending(), Sending::Encrypted);
    assert_eq!(room.dave.members(), vec![S, V]);
    room.all_decrypt(&unit(1));
    // With the check turned off (LITECORD_GOLIVE_DAVE_CHECK=0) the same proposal is committed.
    let mut open = Room::new();
    open.join(V, false);
    let (_stranger, key_package) = Viewer::new(false, X, GROUP, &open.mls);
    let out = open.dave.on_proposals(&open.mls.propose_add(GROUP, open.epoch, &[&key_package]), None);
    assert!(split_commit_welcome(binary(&out[0])).1.is_some());
}

#[test]
fn golive_dave_viewers_leave_then_the_group_starts_over() {
    let mut room = Room::new();
    room.join(V, false);
    room.join(W, false);
    room.all_decrypt(&unit(1));
    // W stops watching: a remove proposal, a commit without a welcome.
    let w = room.viewers.pop().unwrap();
    let proposals = room.mls.propose_remove(GROUP, room.epoch, w.leaf());
    let out = room.dave.on_proposals(&proposals, Some(&[S, V]));
    let (commit, welcome) = split_commit_welcome(binary(&out[0]));
    assert!(welcome.is_none(), "nobody was added: opcode 28 is the commit alone");
    room.viewers[0].proposals(&proposals);
    assert_eq!(room.dave.on_announce_commit(9, &commit), vec![ready(9)]);
    room.viewers[0].committed(&commit, 9);
    room.dave.on_execute_transition(9);
    room.viewers[0].execute(9);
    assert_eq!(room.dave.members(), vec![S, V]);
    room.all_decrypt(&unit(2));

    // The last viewer leaves: the gateway starts a new group (opcode 24, epoch 1), then
    // has it take effect at once (opcode 21, transition 0).
    let out = room.dave.on_prepare_epoch(1, 1);
    let new_package = key_package_of(binary(&out[0]));
    assert_eq!(room.dave.sending(), Sending::Encrypted, "the old key is in force until the transition");
    assert!(room.dave.on_prepare_transition(0, 1).is_empty());
    assert_eq!(room.dave.sending(), Sending::Nothing, "alone again: nothing is sent");
    assert_eq!(room.dave.encrypt_video(&unit(3)), None);

    // Somebody watches again, and this time their commit wins: we are welcomed (opcode 30)
    // into the new group with transition 0, which is in force at once.
    let (mut viewer, key_package) = Viewer::new(false, V, GROUP, &room.mls);
    let ours = room.dave.on_proposals(&room.mls.propose_add(GROUP, 0, &[&key_package]), Some(&[S, V]));
    split_commit_welcome(binary(&ours[0]));
    let (their_commit, their_welcome) = viewer.proposals(&room.mls.propose_add(GROUP, 0, &[&new_package]));
    assert!(room.dave.on_welcome(0, &their_welcome.unwrap(), Some(&[S, V])).is_empty(), "transition 0 is not answered");
    assert_eq!(room.dave.sending(), Sending::Encrypted);
    viewer.committed(&their_commit, 0);
    room.viewers = vec![viewer];
    for seed in 4..10 {
        room.all_decrypt(&unit(seed));
    }
    // A second welcome while in the group is ignored.
    let before = room.dave.key_generation();
    assert!(room.dave.on_welcome(4, &[1, 2, 3], Some(&[S, V])).is_empty());
    assert_eq!((room.dave.sending(), room.dave.key_generation()), (Sending::Encrypted, before));
}

#[test]
fn golive_dave_group_id_is_read_from_the_gateway() {
    // The gateway's group is not the one guessed (here: rtc_channel_id, as one implementation
    // has it). Its proposals name the id: the session starts over with it and the same
    // proposals are committed.
    let mls = MlsGateway::new();
    let (mut dave, _) = streamer(&mls, GroupChoice::Auto);
    let (mut viewer, key_package) = Viewer::new(false, V, RTC_CHANNEL, &mls);
    let out = dave.on_proposals(&mls.propose_add(RTC_CHANNEL, 0, &[&key_package]), Some(&[S, V]));
    assert_eq!(out.len(), 2, "a key package for the new session, then the commit");
    key_package_of(binary(&out[0]));
    let (commit, welcome) = split_commit_welcome(binary(&out[1]));
    assert_eq!(dave.group_id(), RTC_CHANNEL);
    assert_eq!(dave.on_announce_commit(1, &commit), vec![ready(1)]);
    viewer.welcomed(&welcome.unwrap(), 1);
    dave.on_execute_transition(1);
    viewer.execute(1);
    let frame = carried(&mut dave, &unit(1));
    assert_eq!(viewer.decrypt_video(S, &frame).unwrap(), unit(1));
    // The id that was learned stays when the gateway starts the group over.
    key_package_of(binary(&dave.on_prepare_epoch(1, 1)[0]));
    assert_eq!(dave.group_id(), RTC_CHANNEL);

    // An announced commit names its group as well.
    let (mut dave, _) = streamer(&mls, GroupChoice::Auto);
    let (mut member, key_package) = Viewer::new(false, V, RTC_CHANNEL, &mls);
    let (mut first, first_package) = Viewer::new(false, W, RTC_CHANNEL, &mls);
    let (commit, welcome) = first.proposals(&mls.propose_add(RTC_CHANNEL, 0, &[&key_package]));
    first.committed(&commit, 0);
    member.welcomed(&welcome.unwrap(), 0);
    let _ = first_package;
    let (their_commit, _) = first.proposals(&mls.propose_remove(RTC_CHANNEL, 1, member.leaf()));
    let out = dave.on_announce_commit(6, &their_commit);
    assert_eq!(out[0], invalid(6), "a commit for another group than ours cannot be processed");
    key_package_of(binary(&out[1]));
    assert_eq!(dave.group_id(), RTC_CHANNEL, "but its group id is taken over");
}

#[test]
fn golive_dave_a_welcome_for_another_group_moves_to_the_next_guess() {
    // A welcome does not show its group id. One for another group than the session's is
    // answered with opcode 31 and a key package for the next candidate.
    let mls = MlsGateway::new();
    let (mut dave, key_package) = streamer(&mls, GroupChoice::Auto);
    let (mut viewer, _) = Viewer::new(false, V, RTC_CHANNEL, &mls);
    let (_, welcome) = viewer.proposals(&mls.propose_add(RTC_CHANNEL, 0, &[&key_package]));
    let out = dave.on_welcome(0, &welcome.unwrap(), Some(&[S, V]));
    assert_eq!(out[0], invalid(0));
    let next_package = key_package_of(binary(&out[1]));
    assert_eq!(dave.group_id(), RTC_CHANNEL, "rtc_channel_id is the second candidate");
    assert_eq!(dave.sending(), Sending::Nothing);
    // The gateway proposes us again, with the new package.
    let (mut viewer, _) = Viewer::new(false, V, RTC_CHANNEL, &mls);
    let (commit, welcome) = viewer.proposals(&mls.propose_add(RTC_CHANNEL, 0, &[&next_package]));
    assert!(dave.on_welcome(0, &welcome.unwrap(), Some(&[S, V])).is_empty());
    viewer.committed(&commit, 0);
    assert_eq!(dave.sending(), Sending::Encrypted);
    let frame = carried(&mut dave, &unit(1));
    assert_eq!(viewer.decrypt_video(S, &frame).unwrap(), unit(1));

    // LITECORD_GOLIVE_GROUP puts another candidate first.
    let (dave, _) = streamer(&mls, GroupChoice::Channel);
    assert_eq!(dave.group_id(), RTC_CHANNEL);
    let (dave, _) = streamer(&mls, GroupChoice::Server);
    assert_eq!(dave.group_id(), RTC_SERVER);
    let (dave, _) = streamer(&mls, GroupChoice::Id(42));
    assert_eq!(dave.group_id(), 42);

    // A welcome that names a member nobody announced is refused.
    let (mut dave, key_package) = streamer(&mls, GroupChoice::Auto);
    let (mut viewer, _) = Viewer::new(false, X, GROUP, &mls);
    let (_, welcome) = viewer.proposals(&mls.propose_add(GROUP, 0, &[&key_package]));
    let out = dave.on_welcome(0, &welcome.unwrap(), Some(&[S, V]));
    assert_eq!(out[0], invalid(0));
    assert_eq!(dave.sending(), Sending::Nothing);
}

#[test]
fn golive_dave_failures_are_reported_three_times_then_left_alone() {
    let mls = MlsGateway::new();
    let (mut dave, _) = streamer(&mls, GroupChoice::Auto);
    for round in 0..3u16 {
        let (_viewer, key_package) = Viewer::new(false, V, GROUP, &mls);
        let out = dave.on_proposals(&mls.propose_add(GROUP, 0, &[&key_package]), None);
        split_commit_welcome(binary(&out[0]));
        // The announced commit cannot be read.
        let out = dave.on_announce_commit(5 + round, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(out.len(), 2, "round {round}");
        assert_eq!(out[0], invalid(5 + round));
        key_package_of(binary(&out[1]));
        assert!(!dave.stalled());
    }
    // The fourth failure within thirty seconds: nothing more is sent.
    let (_viewer, key_package) = Viewer::new(false, V, GROUP, &mls);
    let out = dave.on_proposals(&mls.propose_add(GROUP, 0, &[&key_package]), None);
    split_commit_welcome(binary(&out[0]));
    assert!(dave.on_announce_commit(9, &[0xde, 0xad, 0xbe, 0xef]).is_empty());
    assert!(dave.stalled());
    assert_eq!(dave.sending(), Sending::Nothing);
    assert_eq!(dave.encrypt_video(&unit(1)), None);
    // A new group from the gateway (opcode 24) is a new start.
    key_package_of(binary(&dave.on_prepare_epoch(1, 1)[0]));
    assert!(!dave.stalled());
    let (mut viewer, key_package) = Viewer::new(false, V, GROUP, &mls);
    let out = dave.on_proposals(&mls.propose_add(GROUP, 0, &[&key_package]), None);
    let (commit, welcome) = split_commit_welcome(binary(&out[0]));
    assert_eq!(dave.on_announce_commit(10, &commit), vec![ready(10)]);
    viewer.welcomed(&welcome.unwrap(), 10);
    dave.on_execute_transition(10);
    viewer.execute(10);
    let frame = carried(&mut dave, &unit(1));
    assert_eq!(viewer.decrypt_video(S, &frame).unwrap(), unit(1));
    // Proposals that cannot be read are not a commit failure: ignored, nothing sent.
    assert!(dave.on_proposals(&[0, 0x05, 1, 2, 3, 4, 5], None).is_empty());
    assert!(dave.on_proposals(&[], None).is_empty());
    assert_eq!(dave.sending(), Sending::Encrypted);
}

#[test]
fn golive_dave_protocol_version_0_is_sent_plain_unless_dave_is_required() {
    super::log_init();
    let mut dave = Dave::new(S, group_candidates(GroupChoice::Auto, RTC_SERVER, RTC_CHANNEL), false);
    assert!(dave.on_session_description(0).is_empty(), "no key package without DAVE");
    assert_eq!(dave.sending(), Sending::Plain);
    assert!(!dave.end_to_end());
    assert_eq!(dave.encrypt_video(&unit(1)), Some(unit(1)), "frames go out as they are");
    assert_eq!(dave.encrypt_opus(&[0x78, 1, 2]), Some(vec![0x78, 1, 2]));
    // The owner's switch: never without DAVE.
    let mut strict = Dave::new(S, group_candidates(GroupChoice::Auto, RTC_SERVER, RTC_CHANNEL), true);
    assert!(strict.on_session_description(0).is_empty());
    assert_eq!(strict.sending(), Sending::Nothing);
    assert_eq!(strict.encrypt_video(&unit(1)), None);

    // A downgrade while encrypted: prepared with an id, in force at the execute.
    let mut room = Room::new();
    room.join(V, false);
    assert_eq!(room.dave.on_prepare_transition(20, 0), vec![ready(20)]);
    assert_eq!(room.dave.sending(), Sending::Encrypted);
    assert!(room.dave.on_execute_transition(20).is_empty());
    assert_eq!(room.dave.sending(), Sending::Plain);
    assert!(!room.dave.end_to_end());
    // And a new connection starts from "nothing until the server says" again.
    room.dave.forget_connection();
    assert_eq!(room.dave.sending(), Sending::Nothing);
    assert_eq!(room.dave.group_id(), GROUP);
}

#[test]
fn golive_dave_opus_round_trip_and_silence() {
    let mut room = Room::new();
    room.join(V, false);
    room.join(W, true);
    for n in 0..50u8 {
        let packet: Vec<u8> = std::iter::once(0x78).chain((0..60).map(|i| i ^ n)).collect();
        let encrypted = room.dave.encrypt_opus(&packet).unwrap();
        assert_ne!(encrypted, packet);
        assert_eq!(&encrypted[encrypted.len() - 2..], &[0xfa, 0xfa]);
        for viewer in &mut room.viewers {
            assert_eq!(viewer.decrypt_audio(S, &encrypted).unwrap(), packet);
        }
    }
    // The silence packet passes through DAVE unchanged, by the protocol's rule.
    assert_eq!(room.dave.encrypt_opus(&OPUS_SILENCE).unwrap(), OPUS_SILENCE);
}

#[test]
fn golive_dave_video_round_trip_of_the_openh264_stream() {
    // One viewer on each davey version, both welcomed by the streamer's commits.
    let mut room = Room::new();
    room.join(V, false);
    room.join(W, true);
    // The stream as OpenH264 makes it, then with an SPS that had to be rewritten.
    let stream = synth::stream(40);
    for stream in [synth::without_vui(&stream), stream] {
        let mut preparer = Preparer::new();
        let (mut units, mut keyframes) = (0, 0);
        for au in &stream {
            let prepared = preparer.prepare(au).unwrap();
            room.all_decrypt(&prepared.data);
            units += 1;
            keyframes += prepared.keyframe as usize;
        }
        assert_eq!((units, keyframes), (40, 4));
    }
}

#[test]
fn golive_dave_without_the_normaliser_keyframes_would_not_decrypt() {
    // The control: an SPS without a VUI is rewritten by libwebrtc's receiver, and a frame
    // whose clear part changed no longer authenticates.
    let mut room = Room::new();
    room.join(V, false);
    for (n, au) in synth::without_vui(&synth::stream(12)).iter().enumerate() {
        let frame = carried(&mut room.dave, au);
        let received = through_webrtc_receiver(&frame);
        let keyframe = n % synth::KEYFRAME_EVERY == 0;
        assert_eq!(received != frame, keyframe, "unit {n}: only a frame with an SPS is touched");
        assert_eq!(room.viewers[0].decrypt_video(S, &received).is_err(), keyframe, "unit {n}");
    }
}

#[test]
fn golive_dave_video_round_trip_of_the_sample_streams() {
    for sample in samples() {
        let mut room = Room::new();
        room.join(V, false);
        room.join(W, true);
        let units = access_units(&sample.data);
        let mut preparer = Preparer::new();
        let (mut sent, mut keyframes) = (0, 0);
        for (n, au) in units.iter().enumerate() {
            let prepared = match preparer.prepare(au) {
                Ok(prepared) => prepared,
                Err(Skip::NoPicture) => continue,
                Err(e) => panic!("{} unit {n}: {e:?}", sample.name),
            };
            room.all_decrypt(&prepared.data);
            sent += 1;
            keyframes += prepared.keyframe as usize;
        }
        // The control: the same units as the encoder made them. Where libwebrtc's receiver
        // rewrites the SPS, the keyframe does not decrypt; everything else does.
        let (mut rewritten, mut failed) = (0, 0);
        for (n, au) in units.iter().enumerate() {
            if !nal_units(au).any(|nal| matches!(nal[0] & 0x1f, 1 | 5)) {
                continue;
            }
            let touched = nal_units(au).any(|nal| nal[0] & 0x1f == NAL_SPS && !webrtc_leaves_it_alone(nal));
            let frame = carried(&mut room.dave, au);
            let received = through_webrtc_receiver(&frame);
            let result = room.viewers[0].decrypt_video(S, &received);
            assert_eq!(result.is_err(), touched, "{} unit {n}", sample.name);
            rewritten += touched as usize;
            failed += result.is_err() as usize;
        }
        println!("{}: {sent} units ({keyframes} keyframes) decrypted by both viewers; as the encoder made them, {failed} of {rewritten} keyframes with an SPS libwebrtc rewrites failed", sample.name);
        assert!(sent > 0 && keyframes > 0);
    }
}
