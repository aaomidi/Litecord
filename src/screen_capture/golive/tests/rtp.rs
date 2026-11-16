//! T2 and T5: the packetizer against the depacketizer, and the layouts of the packets.

use std::time::Duration;

use super::super::depacketize::{frame_of, Depacketizer};
use super::super::h264::{nal_units, Preparer, Skip};
use super::super::rtp::*;
use super::{access_units, hex, samples, synth, to_hex};

/// `len` bytes of a NAL of type `kind` with no start code in them.
fn nal(kind: u8, len: usize, seed: u32) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend((1..len as u32).map(|i| ((i.wrapping_mul(2654435761).wrapping_add(seed) >> 13) as u8) | 0x04));
    out
}

fn frame(nals: &[Vec<u8>]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
}

/// Packetizes, checks the shape of the payloads, and returns what a receiver rebuilds.
fn round_trip(frame: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
    let payloads = packetize_h264(frame, MAX_PAYLOAD);
    assert!(!payloads.is_empty());
    let mut open = false;
    for p in &payloads {
        assert!(!p.is_empty() && p.len() <= MAX_PAYLOAD, "payload of {} bytes", p.len());
        if p[0] & 0x1f == 28 {
            let (start, end) = (p[1] & 0x80 != 0, p[1] & 0x40 != 0);
            assert!(!(start && end), "a fragmented unit is never sent in one fragment");
            assert_eq!(start, !open, "start bit exactly on the first fragment");
            assert!(p.len() > 2);
            open = !end;
        } else {
            assert!(!open, "a whole unit in the middle of a fragmented one");
            assert!((1..=23).contains(&(p[0] & 0x1f)));
        }
    }
    assert!(!open, "the last fragment has its end bit");
    let rebuilt = frame_of(payloads.iter().map(Vec::as_slice)).expect("the receiver rebuilds a frame");
    (payloads, rebuilt)
}

#[test]
fn golive_packets_rebuild_the_frame_byte_for_byte() {
    // Around the limit, far beyond it, and tiny.
    for len in [1, 2, 3, 100, 1159, 1160, 1161, 1162, 2317, 2318, 2319, 3475, 3476, 3477, 65_000, 300_000] {
        let f = frame(&[nal(0x65, len, len as u32)]);
        let (payloads, rebuilt) = round_trip(&f);
        assert_eq!(rebuilt, f, "one unit of {len} bytes");
        let expected = if len <= MAX_PAYLOAD { 1 } else { (len - 1).div_ceil(MAX_PAYLOAD - 2) };
        assert_eq!(payloads.len(), expected, "one unit of {len} bytes");
    }
    // A keyframe as the encoders send it, with several slices.
    let f = frame(&[nal(0x67, 34, 1), nal(0x68, 4, 2), nal(0x06, 30, 3), nal(0x65, 40_000, 4), nal(0x65, 1160, 5), nal(0x65, 1161, 6), nal(0x65, 9, 7)]);
    let (payloads, rebuilt) = round_trip(&f);
    assert_eq!(rebuilt, f);
    assert_eq!(payloads.len(), 3 + 35 + 1 + 2 + 1);
    // Units that end in zero bytes (in an encrypted frame those are ciphertext): the split
    // gives each start code one zero at most and leaves the rest in the unit before.
    for zeros in 1..=4 {
        let mut a = nal(0x41, 700, 8);
        a.extend(std::iter::repeat(0).take(zeros));
        let mut b = nal(0x41, 3000, 9);
        b.extend(std::iter::repeat(0).take(zeros));
        let f = frame(&[a.clone(), b.clone(), nal(0x41, 50, 10)]);
        let units: Vec<&[u8]> = nal_units(&f).collect();
        assert_eq!(units[0], &a[..], "{zeros} zeros");
        assert_eq!(units[1], &b[..], "{zeros} zeros");
        assert_eq!(round_trip(&f).1, f, "{zeros} zeros");
        // The last unit of a frame may end in zeros too (the DAVE trailer never does, but
        // a frame sent without DAVE can).
        let f = frame(&[nal(0x41, 50, 11), b.clone()]);
        assert_eq!(round_trip(&f).1, f, "{zeros} zeros at the end");
    }
}

#[test]
fn golive_fu_a_layout() {
    // 1160 bytes fit; one more is cut in two, with the unit's header rebuilt from both bytes.
    let whole = nal(0x65, 1160, 1);
    assert_eq!(packetize_h264(&frame(&[whole.clone()]), MAX_PAYLOAD), vec![whole]);
    let big = nal(0x65, 1161, 2);
    let payloads = packetize_h264(&frame(&[big.clone()]), MAX_PAYLOAD);
    assert_eq!(payloads.len(), 2);
    assert_eq!(&payloads[0][..2], &[0x7c, 0x85], "indicator: NRI of the unit, type 28; header: start, type 5");
    assert_eq!(&payloads[0][2..], &big[1..1159]);
    assert_eq!(payloads[0].len(), 1160);
    assert_eq!(&payloads[1][..2], &[0x7c, 0x45], "end");
    assert_eq!(&payloads[1][2..], &big[1159..]);
    // Three fragments: the middle one has neither bit. A non-reference slice keeps NRI 0.
    let payloads = packetize_h264(&frame(&[nal(0x01, 1 + 1158 * 2 + 1, 3)]), MAX_PAYLOAD);
    assert_eq!(payloads.iter().map(|p| (p[0], p[1], p.len())).collect::<Vec<_>>(), vec![(0x1c, 0x81, 1160), (0x1c, 0x01, 1160), (0x1c, 0x41, 3)]);
    // Three-byte start codes in the input are found as well.
    let mut f = hex("000001");
    f.extend_from_slice(&nal(0x67, 20, 4));
    f.extend_from_slice(&hex("000001"));
    f.extend_from_slice(&nal(0x68, 5, 5));
    assert_eq!(packetize_h264(&f, MAX_PAYLOAD), vec![nal(0x67, 20, 4), nal(0x68, 5, 5)]);
}

#[test]
fn golive_openh264_units_survive_the_packetizer() {
    let mut preparer = Preparer::new();
    let mut packets = 0;
    for (n, au) in synth::stream(25).iter().enumerate() {
        let prepared = preparer.prepare(au).unwrap();
        let (payloads, rebuilt) = round_trip(&prepared.data);
        assert_eq!(rebuilt, prepared.data, "unit {n}");
        packets += payloads.len();
    }
    assert!(packets > 25);
}

#[test]
fn golive_sample_units_survive_the_packetizer() {
    for sample in samples() {
        let mut preparer = Preparer::new();
        let (mut units, mut packets, mut fragments) = (0, 0, 0);
        for (n, au) in access_units(&sample.data).iter().enumerate() {
            let prepared = match preparer.prepare(au) {
                Ok(prepared) => prepared,
                Err(Skip::NoPicture) => continue,
                Err(e) => panic!("{} unit {n}: {e:?}", sample.name),
            };
            let (payloads, rebuilt) = round_trip(&prepared.data);
            assert_eq!(rebuilt, prepared.data, "{} unit {n}", sample.name);
            units += 1;
            packets += payloads.len();
            fragments += payloads.iter().filter(|p| p[0] & 0x1f == 28).count();
        }
        println!("{}: {units} units in {packets} packets ({fragments} of them fragments)", sample.name);
    }
}

#[test]
fn golive_depacketizer_gives_frames_at_the_marker_and_drops_broken_ones() {
    let frames: Vec<Vec<u8>> = (0..6).map(|n| frame(&[nal(0x41, 3000 + n * 500, n as u32), nal(0x41, 200, 99)])).collect();
    // (sequence, timestamp, marker, payload) as the session sends them.
    let mut packets = Vec::new();
    let mut sequence = 65_530u16; // wraps on the way
    for (n, f) in frames.iter().enumerate() {
        let payloads = packetize_h264(f, MAX_PAYLOAD);
        let count = payloads.len();
        for (i, p) in payloads.into_iter().enumerate() {
            sequence = sequence.wrapping_add(1);
            packets.push((sequence, n as u32 * 1500, i + 1 == count, p));
        }
    }
    // Marker only on the last packet of each unit, one timestamp per unit, sequence + 1.
    assert_eq!(packets.iter().filter(|p| p.2).count(), frames.len());
    let mut d = Depacketizer::new();
    let rebuilt: Vec<Vec<u8>> = packets.iter().filter_map(|(s, t, m, p)| d.push(*s, *t, *m, p)).collect();
    assert_eq!(rebuilt, frames);
    assert_eq!(d.dropped_frames, 0);

    // A packet lost in the middle of frame 1, the last packet (marker) of frame 3 lost.
    let lost_middle = packets.iter().position(|p| p.1 == 1500).unwrap() + 1;
    let lost_marker = packets.iter().rposition(|p| p.1 == 4500).unwrap();
    let mut d = Depacketizer::new();
    let rebuilt: Vec<Vec<u8>> = packets.iter().enumerate().filter(|(i, _)| *i != lost_middle && *i != lost_marker).filter_map(|(_, (s, t, m, p))| d.push(*s, *t, *m, p)).collect();
    // Frame 1 is dropped. Frame 3 never ends, and frame 4 might have lost its start with
    // it, so it is not trusted either.
    assert_eq!(rebuilt, vec![frames[0].clone(), frames[2].clone(), frames[5].clone()]);
    assert_eq!(d.dropped_frames, 3);

    // Unusable payloads never panic and never give a frame.
    let mut d = Depacketizer::new();
    for (i, bad) in [&[][..], &[0x7c][..], &[0x7c, 0x05, 1, 2][..], &[0x18][..], &[0x18, 0, 9, 1][..], &[0x19, 1, 2][..], &[0x1d, 0x85, 1][..]].iter().enumerate() {
        assert_eq!(d.push(i as u16, i as u32, true, bad), None, "{bad:02x?}");
    }
    // An aggregate (STAP-A) from another sender is taken apart the same way.
    assert_eq!(frame_of([&hex("18 0002 6742 0003 68ce3c")[..], &hex("6588")[..]]), Some(hex("00000001 6742 00000001 68ce3c 00000001 6588")));
}

#[test]
fn golive_extension_blocks_match_the_design() {
    let mut full = Vec::new();
    write_extension(&mut full, ExtProfile::Full, 0x123456, 0x0102, false);
    assert_eq!(to_hex(&full), "bede000b".to_owned() + "22000000" + "32123456" + "4000" + "510102" + "62000000" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    assert_eq!(full.len(), 4 + 44);
    let mut repaired = Vec::new();
    write_extension(&mut repaired, ExtProfile::Full, 0x123456, 0x0102, true);
    assert_eq!(repaired.len(), full.len());
    assert_eq!(to_hex(&repaired[44..]), "c2313030", "RTX names the stream by the repaired rid (id 12)");
    assert_eq!(repaired[..44], full[..44]);
    for repaired in [false, true] {
        let (mut min, mut none) = (Vec::new(), Vec::new());
        write_extension(&mut min, ExtProfile::Min, 0x123456, 0x0102, repaired);
        write_extension(&mut none, ExtProfile::None, 0x123456, 0x0102, repaired);
        assert_eq!(to_hex(&min), "bede000151000000");
        assert!(none.is_empty());
    }
    assert_eq!([ExtProfile::from_name("full"), ExtProfile::from_name("min"), ExtProfile::from_name("none"), ExtProfile::from_name("Full")], [Some(ExtProfile::Full), Some(ExtProfile::Min), Some(ExtProfile::None), None]);
}

#[test]
fn golive_playout_delay_and_omitted_elements() {
    let block = |playout: Playout, omit: u8, repaired: bool| {
        let mut out = Vec::new();
        write_extension(&mut out, Ext { profile: ExtProfile::Full, playout, omit }, 0x123456, 0x0102, repaired);
        to_hex(&out)
    };
    // A profile by itself is the block of the first live runs, byte for byte: 0/0.
    assert_eq!(Ext::from(ExtProfile::Full), Ext { profile: ExtProfile::Full, playout: Playout::ASAP, omit: 0 });
    assert_eq!(block(Playout::ASAP, 0, false), "bede000b".to_owned() + "22000000" + "32123456" + "4000" + "510102" + "62000000" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    // The default now: at least 50 ms (5 units), at most 10 s (1000 units), 12 bits each.
    // Same length, same places: only the three bytes of the element differ.
    assert_eq!(Playout::SMOOTH, Playout::Limits { min: 5, max: 1000 });
    assert_eq!(block(Playout::SMOOTH, 0, false), "bede000b".to_owned() + "22000000" + "32123456" + "4000" + "510102" + "620053e8" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    assert_eq!(block(Playout::Limits { min: 0, max: 10 }, 0, false)[34..42], *"6200000a", "0 to 100 ms, as Discord-video-stream sends it");
    assert_eq!(block(Playout::Limits { min: 0xfff, max: 0xfff }, 0, false)[34..42], *"62ffffff");
    assert_eq!(block(Playout::Limits { min: 40, max: 400 }, 0, true)[34..42], *"62028190");
    // No element: 40 bytes, 10 words.
    assert_eq!(block(Playout::None, 0, false), "bede000a".to_owned() + "22000000" + "32123456" + "4000" + "510102" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    // Elements left out: what stays keeps its order, and the block is padded with zero
    // bytes to whole words (here 22 bytes of elements, 2 of padding, 6 words).
    assert_eq!(block(Playout::SMOOTH, OMIT_TOFFSET | OMIT_ORIENTATION | OMIT_CONTENT | OMIT_TIMING, false), "bede0006".to_owned() + "32123456" + "510102" + "620053e8" + "a573637265656e" + "b2313030" + "0000");
    assert_eq!(block(Playout::SMOOTH, OMIT_TIMING, true), "bede0008".to_owned() + "22000000" + "32123456" + "4000" + "510102" + "620053e8" + "7001" + "a573637265656e" + "c2313030" + "0000");
    assert_eq!(block(Playout::None, OMIT_TOFFSET, false), "bede0009".to_owned() + "32123456" + "4000" + "510102" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    // The other profiles have no such element.
    let mut min = Vec::new();
    write_extension(&mut min, Ext { profile: ExtProfile::Min, playout: Playout::SMOOTH, omit: 0xff }, 1, 2, false);
    assert_eq!(to_hex(&min), "bede000151000000");

    // The values of LITECORD_GOLIVE_PLAYOUT and LITECORD_GOLIVE_EXT_OMIT.
    assert_eq!(Playout::from_name("none"), Some(Playout::None));
    assert_eq!(Playout::from_name("0"), Some(Playout::ASAP));
    assert_eq!(Playout::from_name("0,0"), Some(Playout::ASAP));
    assert_eq!(Playout::from_name("5,1000"), Some(Playout::SMOOTH));
    assert_eq!(Playout::from_name(" 20 , 400 "), Some(Playout::Limits { min: 20, max: 400 }));
    assert_eq!(Playout::from_name("4095,4095"), Some(Playout::Limits { min: 4095, max: 4095 }));
    for bad in ["", "1", "10", "10,5", "0,4096", "-1,5", "a,b", "1,2,3", "none,"] {
        assert_eq!(Playout::from_name(bad), None, "{bad:?}");
    }
    assert_eq!((Playout::None.name(), Playout::ASAP.name(), Playout::SMOOTH.name()), ("none".to_string(), "0,0".to_string(), "5,1000".to_string()));
    assert!(Playout::ASAP.describe().contains("no smoothing") && Playout::Limits { min: 0, max: 10 }.describe().contains("no smoothing"));
    assert!(Playout::SMOOTH.describe().contains("50 to 10000 ms") && Playout::SMOOTH.describe().contains("smooths"));
    assert_eq!(omit_from_names("timing"), Some(OMIT_TIMING));
    assert_eq!(omit_from_names("toffset, orientation,content,timing"), Some(15));
    assert_eq!(omit_from_names("timing,playout"), None, "the playout delay has its own switch");
    assert_eq!(omit_from_names(""), None);
    assert_eq!((omit_names(0), omit_names(OMIT_CONTENT | OMIT_TOFFSET)), ("none".to_string(), "toffset,content".to_string()));
}

#[test]
fn golive_packet_layouts() {
    let payload = hex("419a0001020304050607");
    let video = Header { marker: true, payload_type: 101, sequence: 0x1234, timestamp: 90_000, ssrc: 0x1e240 };
    assert_eq!(to_hex(&video_packet(&video, ExtProfile::Min, 0, 0, &payload)), "90e5123400015f900001e240bede000151000000419a0001020304050607");
    assert_eq!(to_hex(&video_packet(&Header { marker: false, ..video }, ExtProfile::None, 0, 0, &payload)), "8065123400015f900001e240419a0001020304050607");
    let full = video_packet(&video, ExtProfile::Full, 0xabcdef, 0xfffe, &payload);
    assert_eq!(to_hex(&full[..16]), "90e5123400015f900001e240bede000b");
    assert_eq!(to_hex(&full[16..60]), "22000000" .to_owned() + "32abcdef" + "4000" + "51fffe" + "62000000" + "7001" + "8c00000000000000000000000000" + "a573637265656e" + "b2313030");
    assert_eq!(&full[60..], &payload[..]);
    // The retransmission: the RTX payload type, SSRC and sequence, the original timestamp
    // and marker, then the original sequence number in front of the payload.
    let rtx = Header { marker: true, payload_type: 102, sequence: 0x0042, timestamp: 90_000, ssrc: 0x1e241 };
    assert_eq!(to_hex(&rtx_packet(&rtx, ExtProfile::Min, 0, 0, 0x1234, &payload)), "90e6004200015f900001e241bede0001510000001234419a0001020304050607");
    let full = rtx_packet(&rtx, ExtProfile::Full, 0xabcdef, 0xffff, 0x1234, &payload);
    assert_eq!(to_hex(&full[..16]), "90e6004200015f900001e241bede000b");
    assert_eq!(to_hex(&full[56..60]), "c2313030");
    assert_eq!(to_hex(&full[60..]), "1234419a0001020304050607");
    assert_eq!(to_hex(&rtx_packet(&rtx, ExtProfile::None, 0, 0, 0x1234, &payload)), "80e6004200015f900001e2411234419a0001020304050607");
    // Audio: no extension.
    let audio = Header { marker: false, payload_type: OPUS_PAYLOAD_TYPE, sequence: 7, timestamp: 960, ssrc: 0x1e23f };
    assert_eq!(to_hex(&audio_packet(&audio, &OPUS_SILENCE)), "80780007000003c00001e23ff8fffe");
    // With the audio extension of the native clients (off by default): level with the
    // voice-activity bit, abs-send-time, the speaking element (soundshare is bit 2), mid.
    assert_eq!(
        to_hex(&audio_packet_with_extension(&audio, 0xabcdef, 30, 2, &hex("78010203"))),
        "90780007000003c00001e23f".to_owned() + "bede0004" + "109e" + "32abcdef" + "9004" + "a4617564696f" + "0000" + "78010203"
    );
    // A silence packet says so: no voice activity, level 127, no flags.
    assert_eq!(to_hex(&audio_packet_with_extension(&audio, 0, 127, 0, &OPUS_SILENCE)[12..]), "bede0004".to_owned() + "107f" + "32000000" + "9000" + "a4617564696f" + "0000" + "f8fffe");
    // Voice is bit 1 and priority bit 0 of the element.
    assert_eq!(audio_packet_with_extension(&audio, 0, 0, 5, &[])[23], 0b011);
    // The largest datagram: header, preamble, 44 bytes of elements, a full payload.
    let largest = video_packet(&video, ExtProfile::Full, 0, 0, &vec![0x41; MAX_PAYLOAD]);
    assert_eq!(largest.len() + super::super::crypto::OVERHEAD, 1240);
}

#[test]
fn golive_abs_send_time_is_seconds_mod_64_in_6_18_fixed_point() {
    assert_eq!(abs_send_time(Duration::from_secs(0)), 0);
    assert_eq!(abs_send_time(Duration::from_secs(1)), 1 << 18);
    assert_eq!(abs_send_time(Duration::from_millis(63_500)), (63 << 18) + (1 << 17));
    assert_eq!(abs_send_time(Duration::from_secs(64)), 0);
    assert_eq!(abs_send_time(Duration::from_millis(1_794_700_000_250)), ((1_794_700_000u64 % 64) << 18) as u32 + (1 << 16));
    assert!(abs_send_time(Duration::new(63, 999_999_999)) < 1 << 24);
}

#[test]
fn golive_history_answers_for_what_was_sent_lately() {
    let mut history = History::new(4);
    assert!(history.get(5).is_none());
    for sequence in 65_533u16..=65_535 {
        history.push(Sent { sequence, timestamp: 1, marker: false, payload: vec![sequence as u8] });
    }
    history.push(Sent { sequence: 0, timestamp: 2, marker: true, payload: vec![0] });
    assert_eq!(history.len(), 4);
    assert_eq!(history.get(65_533).map(|p| p.payload.clone()), Some(vec![0xfd]));
    assert_eq!(history.get(0).map(|p| (p.timestamp, p.marker)), Some((2, true)));
    assert!(history.get(1).is_none() && history.get(65_532).is_none());
    history.push(Sent { sequence: 1, timestamp: 2, marker: true, payload: vec![1] });
    assert!(history.get(65_533).is_none(), "the oldest left");
    assert!(history.get(1).is_some() && history.get(65_534).is_some());
    // A jump in the numbering starts over.
    history.push(Sent { sequence: 500, timestamp: 3, marker: false, payload: vec![] });
    assert_eq!(history.len(), 1);
    assert!(history.get(1).is_none() && history.get(500).is_some());
    history.clear();
    assert!(history.get(500).is_none());
}
