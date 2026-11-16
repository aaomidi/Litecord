//! T4: transport encryption. The known answers were computed with two independent
//! implementations (OpenSSL through node, and the Rust crates) before any of this was written.

use super::super::crypto::*;
use super::super::rtcp;
use super::super::rtp::{self, ExtProfile, Header};
use super::{hex, to_hex};

fn key() -> [u8; 32] {
    std::array::from_fn(|i| i as u8)
}

const AES: [&str; 4] = [
    "90e5123400015f900001e240bede00011536cdaa4dc0f4a95d3f6195afba34ed86dc04f22ff3f88af7c5952b7bc400000001",
    "90e6004200015f900001e241bede0001fd7c5ace8034b7b45b41078f01045857b4f6e866c4518d68b3d3b310c24e00b100000002",
    "80c800060001e24055a6159c3b9b2a28304992f02c7ec8ca4e555d0ded7f0e05d4e38a2a4e62e1b52378932a00000003",
    "80780007000003c00001e23fd9aa239784e84e7dd7388cbec98edab02c45a201020304",
];
const XCHACHA: [&str; 4] = [
    "90e5123400015f900001e240bede000158a751202e9a91decce4a391c2372c81be97e6e7b980a931d292502b67b700000001",
    "90e6004200015f900001e241bede000143b73694bb2ed02b6029b664f3ef299ba0f6f2cb0f7629d8e8fba08d070d280c00000002",
    "80c800060001e24083c7f337401f384d22e0b800118556b89e2349ea9d846526f09f94564a6f8e2eb10c92d700000003",
    "80780007000003c00001e23f8e97ee4eb4af89016a3fbe26a26081b4fd54eb01020304",
];

/// The four packets of the known answers, built by the module's own packet builders: a
/// video packet (counter 1), its retransmission (2), a sender report (3) and an Opus
/// silence packet (counter 0x01020304).
fn packets() -> [Vec<u8>; 4] {
    let payload = hex("419a0001020304050607");
    let video = rtp::video_packet(&Header { marker: true, payload_type: 101, sequence: 0x1234, timestamp: 90_000, ssrc: 0x1e240 }, ExtProfile::Min, 0, 0, &payload);
    let rtx = rtp::rtx_packet(&Header { marker: true, payload_type: 102, sequence: 0x0042, timestamp: 90_000, ssrc: 0x1e241 }, ExtProfile::Min, 0, 0, 0x1234, &payload);
    let report = rtcp::sender_report(0x1e240, (0xeb00_0000, 0x8000_0000), 90_000, 17, 12_345).to_vec();
    let audio = rtp::audio_packet(&Header { marker: false, payload_type: 120, sequence: 7, timestamp: 960, ssrc: 0x1e23f }, &rtp::OPUS_SILENCE);
    assert_eq!(to_hex(&video), "90e5123400015f900001e240bede0001".to_owned() + "51000000419a0001020304050607");
    assert_eq!(to_hex(&rtx), "90e6004200015f900001e241bede0001".to_owned() + "510000001234419a0001020304050607");
    assert_eq!(to_hex(&report), "80c800060001e240".to_owned() + "eb0000008000000000015f900000001100003039");
    assert_eq!(to_hex(&audio), "80780007000003c00001e23f".to_owned() + "f8fffe");
    [video, rtx, report, audio]
}

#[test]
fn golive_transport_known_answers() {
    for (mode, expected) in [(Mode::Aes256Gcm, AES), (Mode::XChaCha20Poly1305, XCHACHA)] {
        let [video, rtx, report, audio] = packets();
        let mut transport = Transport::new(mode, &key());
        // One counter, starting at 1, whatever the kind of packet.
        let sealed = [transport.seal_rtp(&video).unwrap(), transport.seal_rtp(&rtx).unwrap(), transport.seal_rtcp(&report).unwrap()];
        assert_eq!(transport.sealed(), 3);
        transport.set_next_counter(0x0102_0304);
        let sealed_audio = transport.seal_rtp(&audio).unwrap();
        for (got, want) in sealed.iter().chain([&sealed_audio]).zip(expected) {
            assert_eq!(to_hex(got), want, "{mode:?}");
        }
        // And back, with a cipher that has sealed nothing itself.
        let receiver = Transport::new(mode, &key());
        assert_eq!(receiver.open_rtp(&sealed[0]).unwrap(), video);
        assert_eq!(receiver.open_rtp(&sealed[1]).unwrap(), rtx);
        assert_eq!(receiver.open_rtcp(&sealed[2]).unwrap(), report);
        assert_eq!(receiver.open_rtp(&sealed_audio).unwrap(), audio);
        assert_eq!(receiver.open_rtp(&hex(expected[0])).unwrap(), video);
    }
}

#[test]
fn golive_sealed_packets_open_again_and_damage_is_refused() {
    let payload: Vec<u8> = (0..1160u32).map(|i| (i * 7) as u8).collect();
    let header = Header { marker: true, payload_type: 101, sequence: 9, timestamp: 1, ssrc: 77 };
    let mut with_csrc = rtp::audio_packet(&header, &payload);
    with_csrc[0] |= 2; // two CSRCs: 8 more clear bytes
    let cases: Vec<(Vec<u8>, bool)> = vec![
        (rtp::video_packet(&header, ExtProfile::Full, 1, 2, &payload), true),
        (rtp::video_packet(&header, ExtProfile::Min, 1, 2, &payload), true),
        (rtp::video_packet(&header, ExtProfile::None, 1, 2, &payload), true),
        (rtp::rtx_packet(&header, ExtProfile::Full, 1, 2, 3, &payload), true),
        (rtp::audio_packet(&header, &[0xf8, 0xff, 0xfe]), true),
        (rtp::audio_packet(&header, &[]), true),
        (with_csrc, true),
        (rtcp::sender_report(1, (2, 3), 4, 5, 6).to_vec(), false),
        ([rtcp::receiver_report(1, &[rtcp::ReportBlock { ssrc: 77, ..Default::default() }]), rtcp::pli(1, 77)].concat(), false),
    ];
    for mode in [Mode::Aes256Gcm, Mode::XChaCha20Poly1305] {
        let mut sender = Transport::new(mode, &key());
        let receiver = Transport::new(mode, &key());
        let mut other_key = key();
        other_key[31] ^= 1;
        let stranger = Transport::new(mode, &other_key);
        for (n, (packet, is_rtp)) in cases.iter().enumerate() {
            let sealed = if *is_rtp { sender.seal_rtp(packet) } else { sender.seal_rtcp(packet) }.unwrap();
            assert_eq!(sealed.len(), packet.len() + OVERHEAD);
            assert_eq!(&sealed[sealed.len() - 4..], &(n as u32 + 1).to_be_bytes(), "the counter goes up by one per packet of any kind");
            let clear = if *is_rtp { rtp_clear_len(packet).unwrap() } else { 8 };
            assert_eq!(&sealed[..clear], &packet[..clear], "the header stays readable");
            if packet.len() > clear + 8 {
                assert_ne!(&sealed[clear..packet.len()], &packet[clear..], "the rest does not");
            }
            let open = |t: &Transport, data: &[u8]| if *is_rtp { t.open_rtp(data) } else { t.open_rtcp(data) };
            assert_eq!(open(&receiver, &sealed).as_ref(), Some(packet), "case {n} {mode:?}");
            assert_eq!(open(&stranger, &sealed), None, "another key");
            // Any byte changed: the header (authenticated), the body, the tag, the counter.
            for at in [0, 1, 5, clear.saturating_sub(1), clear.min(sealed.len() - 1), sealed.len() / 2, sealed.len() - 21, sealed.len() - 5, sealed.len() - 1] {
                let mut damaged = sealed.clone();
                damaged[at] ^= 0x40;
                assert_eq!(open(&receiver, &damaged), None, "case {n} {mode:?}: byte {at} changed");
            }
            assert_eq!(open(&receiver, &sealed[..sealed.len() - 1]), None);
            assert_eq!(open(&receiver, &sealed[..clear.min(sealed.len())]), None);
        }
        assert_eq!(sender.sealed() as usize, cases.len());
    }
}

#[test]
fn golive_a_counter_is_never_used_twice() {
    let packet = rtp::audio_packet(&Header { marker: false, payload_type: 120, sequence: 1, timestamp: 1, ssrc: 1 }, &[1, 2, 3]);
    let mut transport = Transport::new(Mode::Aes256Gcm, &key());
    transport.set_next_counter(u32::MAX - 1);
    let a = transport.seal_rtp(&packet).unwrap();
    let b = transport.seal_rtp(&packet).unwrap();
    assert_eq!((&a[a.len() - 4..], &b[b.len() - 4..]), (&[0xff, 0xff, 0xff, 0xfe][..], &[0xff; 4][..]));
    assert_ne!(a[12..15], b[12..15]);
    // All counters used: nothing more is sealed with this key.
    assert_eq!(transport.seal_rtp(&packet), None);
    assert_eq!(transport.seal_rtcp(&rtcp::sender_report(1, (0, 0), 0, 0, 0)), None);
}

#[test]
fn golive_what_is_not_a_packet_is_not_sealed_or_opened() {
    let mut transport = Transport::new(Mode::Aes256Gcm, &key());
    assert_eq!(transport.seal_rtp(&[0x80; 11]), None, "shorter than an RTP header");
    assert_eq!(transport.seal_rtp(&[0x00; 20]), None, "not RTP version 2");
    assert_eq!(transport.seal_rtp(&hex("9065000100000001000000 01 be de")), None, "an extension bit without the preamble");
    assert_eq!(transport.seal_rtcp(&[0x80, 200, 0, 0]), None);
    assert_eq!(transport.sealed(), 0, "nothing was counted");
    assert_eq!(transport.open_rtp(&[]), None);
    assert_eq!(transport.open_rtcp(&[0x80; 27]), None);
    assert_eq!(rtp_clear_len(&hex("80780007000003c00001e23f f8")), Some(12));
    assert_eq!(rtp_clear_len(&hex("90780007000003c00001e23f bede0001 51000000")), Some(16));
    assert_eq!(rtp_clear_len(&hex("92780007000003c00001e23f 00000001 00000002 bede0000")), Some(24));
    assert!(is_rtcp(&[0x80, 200]) && is_rtcp(&[0x81, 201, 0]) && is_rtcp(&[0x80, 205]) && is_rtcp(&[0x80, 206]));
    assert!(!is_rtcp(&[0x80, 120]) && !is_rtcp(&[0x90, 0xe5]) && !is_rtcp(&[0x90, 101]) && !is_rtcp(&[0x80]));
    // (A video packet with the marker bit: payload type 101 gives 0xe5 = 229, above the RTCP range.)
}

#[test]
fn golive_the_mode_is_picked_by_name() {
    let aes = "aead_aes256_gcm_rtpsize";
    let xchacha = "aead_xchacha20_poly1305_rtpsize";
    assert_eq!(Mode::pick([aes, xchacha]), Some(Mode::Aes256Gcm));
    assert_eq!(Mode::pick([xchacha, aes]), Some(Mode::Aes256Gcm), "not the first entry");
    assert_eq!(Mode::pick(["xsalsa20_poly1305", xchacha, "aead_aes256_gcm"]), Some(Mode::XChaCha20Poly1305));
    assert_eq!(Mode::pick(["xsalsa20_poly1305_lite", "aead_aes256_gcm"]), None);
    assert_eq!(Mode::pick([]), None);
    assert_eq!((Mode::Aes256Gcm.name(), Mode::XChaCha20Poly1305.name()), (aes, xchacha));
    assert_eq!(Mode::from_name(aes), Some(Mode::Aes256Gcm));
    assert_eq!(Mode::from_name("AEAD_AES256_GCM_RTPSIZE"), None);
}
