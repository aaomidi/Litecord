//! T5: the sender report this client sends, and the feedback it reads.

use std::time::Duration;

use super::super::rtcp::*;
use super::{hex, to_hex};

#[test]
fn golive_sender_report_is_28_bytes_as_designed() {
    let report = sender_report(0x1e240, (0xeb00_0000, 0x8000_0000), 90_000, 17, 12_345);
    assert_eq!(to_hex(&report), "80c80006".to_owned() + "0001e240" + "eb000000" + "80000000" + "00015f90" + "00000011" + "00003039");
    assert_eq!(
        parse(&report),
        vec![Packet::SenderReport { ssrc: 0x1e240, ntp: (0xeb00_0000, 0x8000_0000), rtp_timestamp: 90_000, packets: 17, octets: 12_345, blocks: vec![] }]
    );
}

#[test]
fn golive_ntp_time_and_round_trip() {
    // 1 January 1970 is 2 208 988 800 seconds after 1 January 1900.
    assert_eq!(ntp(Duration::ZERO), (2_208_988_800, 0));
    assert_eq!(ntp(Duration::from_millis(1_500)), (2_208_988_801, 0x8000_0000));
    assert_eq!(ntp(Duration::new(10, 250_000_000)), (2_208_988_810, 0x4000_0000));
    assert_eq!(ntp_middle((0x1234_5678, 0x9abc_def0)), 0x5678_9abc);
    // The receiver got our report stamped T, held it 1.5 s, and its answer arrives 1.75 s
    // after T: a quarter second went on the two ways.
    let sent = ntp(Duration::from_secs(1_794_700_000));
    let block = ReportBlock { last_sr: ntp_middle(sent), delay_since_last_sr: 3 << 15, ..Default::default() };
    let now = ntp(Duration::from_millis(1_794_700_001_750));
    let round_trip = block.round_trip(now).unwrap();
    assert!((round_trip.as_micros() as i64 - 250_000).abs() < 100, "{round_trip:?}");
    assert_eq!(ReportBlock::default().round_trip(now), None, "no sender report seen yet");
    // An answer to a report from another time base is no measurement.
    assert_eq!(ReportBlock { last_sr: 5, ..Default::default() }.round_trip(now), None);
    assert_eq!(ReportBlock { fraction_lost: 0, ..Default::default() }.loss_permille(), 0);
    assert_eq!(ReportBlock { fraction_lost: 32, ..Default::default() }.loss_permille(), 125);
    assert_eq!(ReportBlock { fraction_lost: 255, ..Default::default() }.loss_permille(), 996);
}

/// Written by hand from RFC 3550, 4585, 5104 and the REMB draft.
const RECEIVER_REPORT: &str = "81c90007 00000001 0001e240 20 000005 00001234 0000000a 11223344 00010000";
const PLI: &str = "81ce0002 00000001 0001e240";
const NACK: &str = "81cd0003 00000001 0001e240 03e8 8005";
const FIR: &str = "84ce0004 00000001 00000000 0001e240 07000000";
const REMB: &str = "8fce0005 00000001 00000000 52454d42 01 17d090 0001e240";

#[test]
fn golive_feedback_is_read_from_a_compound_packet() {
    let block = ReportBlock { ssrc: 0x1e240, fraction_lost: 0x20, cumulative_lost: 5, highest_sequence: 0x1234, jitter: 10, last_sr: 0x1122_3344, delay_since_last_sr: 0x1_0000 };
    assert_eq!(
        parse(&hex(&format!("{RECEIVER_REPORT} {PLI}"))),
        vec![Packet::ReceiverReport { ssrc: 1, blocks: vec![block] }, Packet::Pli { sender_ssrc: 1, media_ssrc: 0x1e240 }]
    );
    // Bit i of the mask: packet 1000 + i + 1 is missing too.
    assert_eq!(parse(&hex(NACK)), vec![Packet::Nack { sender_ssrc: 1, media_ssrc: 0x1e240, sequences: vec![1000, 1001, 1003, 1016] }]);
    assert_eq!(parse(&hex(FIR)), vec![Packet::Fir { sender_ssrc: 1, requests: vec![(0x1e240, 7)] }]);
    assert_eq!(parse(&hex(REMB)), vec![Packet::Remb { sender_ssrc: 1, bitrate: 8_000_000, ssrcs: vec![0x1e240] }]);
    // All of them in one datagram, with something unknown in between (SDES, type 202) and
    // transport-wide feedback (type 205, format 15), which is not used.
    let all = hex(&format!("{RECEIVER_REPORT} 81ca0002 00000001 01004c00 {NACK} 8fcd0002 00000001 0001e240 {FIR} {REMB} {PLI}"));
    let kinds: Vec<String> = parse(&all).iter().map(|p| format!("{p:?}").split([' ', '{']).next().unwrap().to_string()).collect();
    assert_eq!(kinds, ["ReceiverReport", "Other", "Nack", "Other", "Fir", "Remb", "Pli"]);
    assert_eq!(parse(&all)[1], Packet::Other { packet_type: 202, format: 1 });
    assert_eq!(parse(&all)[3], Packet::Other { packet_type: 205, format: 15 });
    // A receiver report with no block at all, and one that lost packets in a negative count
    // (duplicates), which the 24-bit field allows.
    assert_eq!(parse(&hex("80c90001 00000009")), vec![Packet::ReceiverReport { ssrc: 9, blocks: vec![] }]);
    let duplicates = hex("81c90007 00000001 0001e240 00 fffffe 00001234 00000000 00000000 00000000");
    assert!(matches!(&parse(&duplicates)[0], Packet::ReceiverReport { blocks, .. } if blocks[0].cumulative_lost == -2));
    // A NACK that wraps around the sequence space.
    assert_eq!(parse(&hex("81cd0003 00000001 0001e240 ffff 0001")), vec![Packet::Nack { sender_ssrc: 1, media_ssrc: 0x1e240, sequences: vec![65_535, 0] }]);
}

#[test]
fn golive_feedback_builders_write_what_the_parser_reads() {
    let block = ReportBlock { ssrc: 0x1e240, fraction_lost: 0x20, cumulative_lost: 5, highest_sequence: 0x1234, jitter: 10, last_sr: 0x1122_3344, delay_since_last_sr: 0x1_0000 };
    assert_eq!(to_hex(&receiver_report(1, &[block])), RECEIVER_REPORT.replace(' ', ""));
    assert_eq!(to_hex(&pli(1, 0x1e240)), PLI.replace(' ', ""));
    assert_eq!(to_hex(&nack(1, 0x1e240, &[1000, 1001, 1003, 1016])), NACK.replace(' ', ""));
    assert_eq!(to_hex(&fir(1, 0x1e240, 7)), FIR.replace(' ', ""));
    assert_eq!(to_hex(&remb(1, 8_000_000, &[0x1e240])), REMB.replace(' ', ""));
    // Packets far apart need pairs of their own.
    let far = nack(1, 2, &[10, 11, 27, 28, 500]);
    assert_eq!(to_hex(&far), "81cd0005 00000001 00000002 000a0001 001b0001 01f40000".replace(' ', ""));
    assert_eq!(parse(&far), vec![Packet::Nack { sender_ssrc: 1, media_ssrc: 2, sequences: vec![10, 11, 27, 28, 500] }]);
    let negative = ReportBlock { cumulative_lost: -2, ..block };
    assert_eq!(parse(&receiver_report(3, &[block, negative])), vec![Packet::ReceiverReport { ssrc: 3, blocks: vec![block, negative] }]);
    for bitrate in [0, 1, 262_143, 262_144, 2_500_000, 50_000_000, 1 << 40] {
        let Packet::Remb { bitrate: read, .. } = parse(&remb(1, bitrate, &[5, 6]))[0].clone() else { panic!() };
        // (18 bits of mantissa: exact up to 262 143, rounded down above.)
        assert!(read <= bitrate && bitrate - read <= bitrate >> 17, "{bitrate} came back as {read}");
    }
}

#[test]
fn golive_damaged_feedback_is_dropped_without_a_panic() {
    assert!(parse(&[]).is_empty());
    assert!(parse(&[0x81, 201, 0]).is_empty());
    // A length that runs past the datagram: what came before it is kept.
    let mut cut = hex(&format!("{PLI} {RECEIVER_REPORT}"));
    cut.truncate(cut.len() - 5);
    assert_eq!(parse(&cut), vec![Packet::Pli { sender_ssrc: 1, media_ssrc: 0x1e240 }]);
    // Not version 2.
    assert!(parse(&hex("41ce0002 00000001 0001e240")).is_empty());
    // A report that announces more blocks than it has: not a report we can use.
    assert_eq!(parse(&hex("83c90001 00000009")), vec![Packet::Other { packet_type: 201, format: 3 }]);
    // Feedback too short for what its format needs.
    assert_eq!(parse(&hex("81ce0001 00000001")), vec![Packet::Other { packet_type: 206, format: 1 }]);
    assert_eq!(parse(&hex("8fce0003 00000001 00000000 52454d42")), vec![Packet::Other { packet_type: 206, format: 15 }]);
    assert_eq!(parse(&hex("8fce0004 00000001 00000000 52454d58 0117d090")), vec![Packet::Other { packet_type: 206, format: 15 }]);
    // Padding is taken off; a padding count that cannot be is not followed.
    assert_eq!(parse(&hex("a1ce0003 00000001 0001e240 00000004")), vec![Packet::Pli { sender_ssrc: 1, media_ssrc: 0x1e240 }]);
    assert!(parse(&hex("a1ce0002 00000001 000000ff")).is_empty());
    // Every prefix and every single-byte change of a real compound packet.
    let all = hex(&format!("{RECEIVER_REPORT} {NACK} {FIR} {REMB} {PLI}"));
    for len in 0..all.len() {
        parse(&all[..len]);
    }
    for at in 0..all.len() {
        for bit in 0..8 {
            let mut changed = all.clone();
            changed[at] ^= 1 << bit;
            parse(&changed);
        }
    }
}
