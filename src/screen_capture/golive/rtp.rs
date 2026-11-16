//! RTP for a Discord stream: headers, header extensions, H.264 payloads (RFC 6184: single
//! NAL unit packets and FU-A), retransmission packets (RFC 4588) and the packets sent lately.
//!
//! Layouts, before transport encryption:
//!
//! ```text
//! video  90 | M<<7 | pt | seq:2 | timestamp:4 | video ssrc:4 | BE DE 00 0B | 44 bytes of elements | payload
//! audio  80 |        120 | seq:2 | timestamp:4 | audio ssrc:4 | payload
//! RTX    90 | M<<7 | rtx pt | rtx seq:2 | original timestamp:4 | rtx ssrc:4 | BE DE 00 0B | elements
//!           | original seq:2 | original payload
//! ```

use std::collections::VecDeque;
use std::time::Duration;

use super::h264::nal_units;

/// The most H.264 bytes in one packet. With the full extension block the datagram is then
/// 12 + 4 + 44 + 1160 + 16 (tag) + 4 (nonce) = 1240 bytes, a size known to pass.
pub const MAX_PAYLOAD: usize = 1160;
pub const OPUS_PAYLOAD_TYPE: u8 = 120;
/// What an Opus encoder sends for silence; DAVE lets it through unencrypted by rule.
pub const OPUS_SILENCE: [u8; 3] = [0xf8, 0xff, 0xfe];

const FU_A: u8 = 28;

/// Which header extension elements video and RTX packets carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtProfile {
    /// What Discord's native clients put on screen-share video (ids of the native table):
    /// transmission offset, abs-send-time, orientation, transport-wide sequence, playout
    /// delay, content type, video timing, mid and rid.
    Full,
    /// One element, `51 00 00`, as the older UDP senders sent it.
    Min,
    /// No extension at all.
    None,
}

impl ExtProfile {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "full" => Some(Self::Full),
            "min" => Some(Self::Min),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Min => "min",
            Self::None => "none",
        }
    }
}

/// The fixed part of an RTP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl Header {
    fn write(&self, out: &mut Vec<u8>, extension: bool) {
        out.push(if extension { 0x90 } else { 0x80 });
        out.push((self.marker as u8) << 7 | (self.payload_type & 0x7f));
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// abs-send-time of a wall-clock time: seconds modulo 64 as 6.18 fixed point, 24 bits.
pub fn abs_send_time(since_unix_epoch: Duration) -> u32 {
    let seconds = since_unix_epoch.as_secs() % 64;
    let fraction = (since_unix_epoch.subsec_nanos() as u64) << 18;
    (((seconds << 18) + fraction / 1_000_000_000) & 0xff_ffff) as u32
}

/// Appends the extension block (4-byte preamble and elements) of a video packet, or of a
/// retransmission when `repaired`. Nothing for `ExtProfile::None`.
pub fn write_extension(out: &mut Vec<u8>, profile: ExtProfile, abs_send_time: u32, transport_sequence: u16, repaired: bool) {
    match profile {
        ExtProfile::None => {}
        ExtProfile::Min => out.extend_from_slice(&[0xbe, 0xde, 0x00, 0x01, 0x51, 0x00, 0x00, 0x00]),
        ExtProfile::Full => {
            // One-byte elements: (id << 4) | (length - 1), then the data. 44 bytes, 11 words.
            out.extend_from_slice(&[0xbe, 0xde, 0x00, 0x0b]);
            out.extend_from_slice(&[0x22, 0, 0, 0]); // 2: transmission offset 0
            let t = abs_send_time.to_be_bytes();
            out.extend_from_slice(&[0x32, t[1], t[2], t[3]]); // 3: abs-send-time
            out.extend_from_slice(&[0x40, 0]); // 4: video orientation 0
            let s = transport_sequence.to_be_bytes();
            out.extend_from_slice(&[0x51, s[0], s[1]]); // 5: transport-wide sequence
            out.extend_from_slice(&[0x62, 0, 0, 0]); // 6: playout delay, min 0 max 0
            out.extend_from_slice(&[0x70, 1]); // 7: video content type, 1 = screen
            out.push(0x8c); // 8: video timing, 13 zero bytes
            out.extend_from_slice(&[0; 13]);
            out.extend_from_slice(&[0xa5]); // 10: mid
            out.extend_from_slice(b"screen");
            out.push(if repaired { 0xc2 } else { 0xb2 }); // 11: rid, 12: repaired rid
            out.extend_from_slice(b"100");
        }
    }
}

/// A video packet as it is before transport encryption.
pub fn video_packet(header: &Header, profile: ExtProfile, abs_send_time: u32, transport_sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + 48 + payload.len());
    header.write(&mut out, profile != ExtProfile::None);
    write_extension(&mut out, profile, abs_send_time, transport_sequence, false);
    out.extend_from_slice(payload);
    out
}

/// The retransmission of a video packet: `header` is the RTX stream's (its payload type,
/// SSRC and own sequence, with the original's timestamp and marker), and the payload starts
/// with the original sequence number.
pub fn rtx_packet(header: &Header, profile: ExtProfile, abs_send_time: u32, transport_sequence: u16, original_sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + 50 + payload.len());
    header.write(&mut out, profile != ExtProfile::None);
    write_extension(&mut out, profile, abs_send_time, transport_sequence, true);
    out.extend_from_slice(&original_sequence.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// An Opus packet: no extension.
pub fn audio_packet(header: &Header, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    header.write(&mut out, false);
    out.extend_from_slice(payload);
    out
}

/// The RTP payloads of one frame (Annex B, as DAVE returned it or as it was prepared):
/// each NAL unit whole when it fits `max_payload`, as FU-A fragments otherwise. The frame
/// is split by libwebrtc's rule, so a receiver that puts every unit back behind a 4-byte
/// start code gets the frame's exact bytes.
pub fn packetize_h264(frame: &[u8], max_payload: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.len() / max_payload.max(1) + 4);
    for nal in nal_units(frame) {
        if nal.is_empty() {
            continue;
        }
        if nal.len() <= max_payload {
            out.push(nal.to_vec());
            continue;
        }
        let indicator = (nal[0] & 0xe0) | FU_A;
        let kind = nal[0] & 0x1f;
        let body = &nal[1..];
        let pieces = body.len().div_ceil(max_payload - 2);
        for (i, piece) in body.chunks(max_payload - 2).enumerate() {
            let mut payload = Vec::with_capacity(2 + piece.len());
            payload.push(indicator);
            payload.push(((i == 0) as u8) << 7 | ((i + 1 == pieces) as u8) << 6 | kind);
            payload.extend_from_slice(piece);
            out.push(payload);
        }
    }
    out
}

/// A video packet that was sent, kept to answer a NACK.
#[derive(Clone, Debug)]
pub struct Sent {
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// The last video packets sent, by sequence number.
pub struct History {
    packets: VecDeque<Sent>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self { packets: VecDeque::with_capacity(capacity.min(4096)), capacity }
    }

    pub fn push(&mut self, packet: Sent) {
        // Sequence numbers are handed out in order: anything else starts the history over.
        if self.packets.back().is_some_and(|last| last.sequence.wrapping_add(1) != packet.sequence) {
            self.packets.clear();
        }
        if self.packets.len() >= self.capacity {
            self.packets.pop_front();
        }
        self.packets.push_back(packet);
    }

    pub fn get(&self, sequence: u16) -> Option<&Sent> {
        let first = self.packets.front()?.sequence;
        self.packets.get(sequence.wrapping_sub(first) as usize).filter(|p| p.sequence == sequence)
    }

    pub fn clear(&mut self) {
        self.packets.clear();
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }
}
