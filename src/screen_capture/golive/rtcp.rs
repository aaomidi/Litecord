//! RTCP for a Discord stream: the sender reports this client sends, and the feedback the
//! media server sends back (receiver reports, NACK, PLI, FIR, REMB), which arrives on the
//! same UDP socket as RTP would.
//!
//! The builders of feedback packets are what a receiver sends; milestone 1 uses them in its
//! tests (the scripted media server), milestone 2 (watching) will send them for real.

use std::time::Duration;

pub const SENDER_REPORT: u8 = 200;
pub const RECEIVER_REPORT: u8 = 201;
/// Transport layer feedback (RFC 4585): generic NACK is format 1, transport-wide
/// congestion control feedback is format 15.
pub const RTPFB: u8 = 205;
/// Payload-specific feedback: PLI is format 1, FIR format 4, REMB format 15.
pub const PSFB: u8 = 206;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// The NTP timestamp (seconds since 1900 and a 32-bit fraction) of a wall-clock time.
pub fn ntp(since_unix_epoch: Duration) -> (u32, u32) {
    let seconds = (since_unix_epoch.as_secs() + NTP_UNIX_OFFSET) as u32;
    let fraction = (((since_unix_epoch.subsec_nanos() as u64) << 32) / 1_000_000_000) as u32;
    (seconds, fraction)
}

/// The middle 32 bits of an NTP timestamp, which receiver reports echo back.
pub fn ntp_middle(ntp: (u32, u32)) -> u32 {
    (ntp.0 << 16) | (ntp.1 >> 16)
}

/// A sender report without reception blocks, 28 bytes:
/// `80 C8 00 06 | ssrc | NTP seconds | NTP fraction | RTP timestamp | packets | octets`.
/// `rtp_timestamp` is the stream's timestamp at the instant `ntp` names.
pub fn sender_report(ssrc: u32, ntp: (u32, u32), rtp_timestamp: u32, packets: u32, octets: u32) -> [u8; 28] {
    let mut out = [0u8; 28];
    out[0] = 0x80;
    out[1] = SENDER_REPORT;
    out[2..4].copy_from_slice(&6u16.to_be_bytes());
    for (i, word) in [ssrc, ntp.0, ntp.1, rtp_timestamp, packets, octets].into_iter().enumerate() {
        out[4 + i * 4..8 + i * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// What a receiver says about one stream it receives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportBlock {
    pub ssrc: u32,
    /// Packets lost since the last report, as a fraction of 256.
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    pub highest_sequence: u32,
    pub jitter: u32,
    /// Middle 32 bits of the NTP time of the last sender report received (0: none yet).
    pub last_sr: u32,
    /// Time since that report was received, in 1/65536 s.
    pub delay_since_last_sr: u32,
}

impl ReportBlock {
    /// Loss in permille, the unit the stream's rate control works in.
    pub fn loss_permille(&self) -> u16 {
        (self.fraction_lost as u32 * 1000 / 256) as u16
    }

    /// The round-trip time this block gives at wall-clock `now`, when it answers one of
    /// our sender reports.
    pub fn round_trip(&self, now: (u32, u32)) -> Option<Duration> {
        if self.last_sr == 0 {
            return None;
        }
        let units = ntp_middle(now).wrapping_sub(self.last_sr).wrapping_sub(self.delay_since_last_sr);
        // More than a minute is a clock that moved, not a network.
        (units < 60 << 16).then(|| Duration::from_micros(units as u64 * 1_000_000 / 65_536))
    }
}

/// One packet of a compound RTCP packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    SenderReport { ssrc: u32, ntp: (u32, u32), rtp_timestamp: u32, packets: u32, octets: u32, blocks: Vec<ReportBlock> },
    ReceiverReport { ssrc: u32, blocks: Vec<ReportBlock> },
    /// The receiver misses these packets of `media_ssrc`.
    Nack { sender_ssrc: u32, media_ssrc: u32, sequences: Vec<u16> },
    /// Picture loss indication: the receiver needs a keyframe of `media_ssrc`.
    Pli { sender_ssrc: u32, media_ssrc: u32 },
    /// Full intra request, for each (SSRC, request number) listed.
    Fir { sender_ssrc: u32, requests: Vec<(u32, u8)> },
    /// The receiver's estimate of the bitrate it can take for these SSRCs.
    Remb { sender_ssrc: u32, bitrate: u64, ssrcs: Vec<u32> },
    /// Anything else (transport-wide feedback, SDES, BYE...).
    Other { packet_type: u8, format: u8 },
}

/// The packets of a compound RTCP packet, after transport decryption. Stops at the first
/// packet that does not fit; what was read until then is returned.
pub fn parse(compound: &[u8]) -> Vec<Packet> {
    let mut out = Vec::new();
    let mut rest = compound;
    while rest.len() >= 4 {
        if rest[0] >> 6 != 2 {
            break;
        }
        let padding = rest[0] & 0x20 != 0;
        let format = rest[0] & 0x1f;
        let packet_type = rest[1];
        let len = (u16::from_be_bytes([rest[2], rest[3]]) as usize + 1) * 4;
        if rest.len() < len {
            break;
        }
        let mut body = &rest[4..len];
        rest = &rest[len..];
        if padding {
            let pad = body.last().copied().unwrap_or(0) as usize;
            if pad == 0 || pad > body.len() {
                break;
            }
            body = &body[..body.len() - pad];
        }
        out.push(parse_one(packet_type, format, body).unwrap_or(Packet::Other { packet_type, format }));
    }
    out
}

fn word(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn blocks(data: &[u8], count: usize) -> Option<Vec<ReportBlock>> {
    (0..count)
        .map(|i| {
            let b = data.get(i * 24..i * 24 + 24)?;
            let lost = u32::from_be_bytes([0, b[5], b[6], b[7]]);
            Some(ReportBlock {
                ssrc: word(b, 0)?,
                fraction_lost: b[4],
                // 24 bits, signed.
                cumulative_lost: ((lost << 8) as i32) >> 8,
                highest_sequence: word(b, 8)?,
                jitter: word(b, 12)?,
                last_sr: word(b, 16)?,
                delay_since_last_sr: word(b, 20)?,
            })
        })
        .collect()
}

fn parse_one(packet_type: u8, format: u8, body: &[u8]) -> Option<Packet> {
    let sender_ssrc = word(body, 0)?;
    match (packet_type, format) {
        (SENDER_REPORT, count) => Some(Packet::SenderReport {
            ssrc: sender_ssrc,
            ntp: (word(body, 4)?, word(body, 8)?),
            rtp_timestamp: word(body, 12)?,
            packets: word(body, 16)?,
            octets: word(body, 20)?,
            blocks: blocks(body.get(24..)?, count as usize)?,
        }),
        (RECEIVER_REPORT, count) => Some(Packet::ReceiverReport { ssrc: sender_ssrc, blocks: blocks(body.get(4..)?, count as usize)? }),
        (RTPFB, 1) => {
            let media_ssrc = word(body, 4)?;
            let mut sequences = Vec::new();
            for pair in body.get(8..)?.chunks_exact(4) {
                let first = u16::from_be_bytes([pair[0], pair[1]]);
                let following = u16::from_be_bytes([pair[2], pair[3]]);
                sequences.push(first);
                // Bit i of the mask: packet first + i + 1 is missing too.
                sequences.extend((0..16).filter(|i| following >> i & 1 == 1).map(|i| first.wrapping_add(i + 1)));
            }
            Some(Packet::Nack { sender_ssrc, media_ssrc, sequences })
        }
        (PSFB, 1) => Some(Packet::Pli { sender_ssrc, media_ssrc: word(body, 4)? }),
        (PSFB, 4) => {
            let requests = body.get(8..)?.chunks_exact(8).map(|e| (u32::from_be_bytes([e[0], e[1], e[2], e[3]]), e[4])).collect();
            Some(Packet::Fir { sender_ssrc, requests })
        }
        (PSFB, 15) => {
            let fci = body.get(8..)?;
            if fci.get(..4)? != b"REMB" {
                return None;
            }
            let count = *fci.get(4)? as usize;
            let exponent = (fci.get(5)? >> 2) as u32;
            let mantissa = ((fci[5] & 0x03) as u64) << 16 | (*fci.get(6)? as u64) << 8 | *fci.get(7)? as u64;
            let ssrcs = (0..count).map(|i| word(fci, 8 + i * 4)).collect::<Option<Vec<u32>>>()?;
            Some(Packet::Remb { sender_ssrc, bitrate: mantissa.checked_shl(exponent).unwrap_or(u64::MAX), ssrcs })
        }
        _ => None,
    }
}

fn header(out: &mut Vec<u8>, format: u8, packet_type: u8, body_len: usize) {
    out.push(0x80 | (format & 0x1f));
    out.push(packet_type);
    out.extend_from_slice(&((body_len / 4) as u16).to_be_bytes());
}

/// A receiver report (what the media server sends a sender about its streams).
pub fn receiver_report(sender_ssrc: u32, blocks: &[ReportBlock]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + blocks.len() * 24);
    header(&mut out, blocks.len() as u8, RECEIVER_REPORT, 4 + blocks.len() * 24);
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    for b in blocks {
        out.extend_from_slice(&b.ssrc.to_be_bytes());
        out.push(b.fraction_lost);
        out.extend_from_slice(&b.cumulative_lost.to_be_bytes()[1..]);
        for word in [b.highest_sequence, b.jitter, b.last_sr, b.delay_since_last_sr] {
            out.extend_from_slice(&word.to_be_bytes());
        }
    }
    out
}

/// A picture loss indication.
pub fn pli(sender_ssrc: u32, media_ssrc: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    header(&mut out, 1, PSFB, 8);
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&media_ssrc.to_be_bytes());
    out
}

/// A full intra request for one SSRC; `request` counts the requests made so far.
pub fn fir(sender_ssrc: u32, media_ssrc: u32, request: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    header(&mut out, 4, PSFB, 16);
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&media_ssrc.to_be_bytes());
    out.extend_from_slice(&[request, 0, 0, 0]);
    out
}

/// A generic NACK for the packets `sequences` (in ascending order) of `media_ssrc`.
pub fn nack(sender_ssrc: u32, media_ssrc: u32, sequences: &[u16]) -> Vec<u8> {
    let mut pairs: Vec<(u16, u16)> = Vec::new();
    for &sequence in sequences {
        match pairs.last_mut() {
            Some((first, mask)) if sequence != *first && sequence.wrapping_sub(*first) <= 16 => *mask |= 1 << (sequence.wrapping_sub(*first) - 1),
            Some((first, _)) if sequence == *first => {}
            _ => pairs.push((sequence, 0)),
        }
    }
    let mut out = Vec::with_capacity(12 + pairs.len() * 4);
    header(&mut out, 1, RTPFB, 8 + pairs.len() * 4);
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&media_ssrc.to_be_bytes());
    for (first, mask) in pairs {
        out.extend_from_slice(&first.to_be_bytes());
        out.extend_from_slice(&mask.to_be_bytes());
    }
    out
}

/// A receiver estimated maximum bitrate message.
pub fn remb(sender_ssrc: u32, bitrate: u64, ssrcs: &[u32]) -> Vec<u8> {
    let mut exponent = 0u8;
    let mut mantissa = bitrate;
    while mantissa >= 1 << 18 {
        mantissa >>= 1;
        exponent += 1;
    }
    let mut out = Vec::with_capacity(20 + ssrcs.len() * 4);
    header(&mut out, 15, PSFB, 16 + ssrcs.len() * 4);
    out.extend_from_slice(&sender_ssrc.to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(b"REMB");
    out.push(ssrcs.len() as u8);
    out.push(exponent << 2 | (mantissa >> 16) as u8);
    out.extend_from_slice(&(mantissa as u16).to_be_bytes());
    for ssrc in ssrcs {
        out.extend_from_slice(&ssrc.to_be_bytes());
    }
    out
}
