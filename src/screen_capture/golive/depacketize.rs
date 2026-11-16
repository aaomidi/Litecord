//! H.264 RTP payloads back into Annex B frames, the way libwebrtc's receiver rebuilds them
//! before a frame reaches the DAVE decryptor: every NAL unit behind a 4-byte start code,
//! whether it came whole, inside a STAP-A or as FU-A fragments.
//!
//! Milestone 1 uses it to check that what the packetizer sends comes back byte for byte.
//! Watching Discord streams (milestone 2) builds on it; a receiver of ours must not rewrite
//! the SPS, or frames stop decrypting.

use super::h264::START_CODE;

const STAP_A: u8 = 24;
const FU_A: u8 = 28;

/// Why a payload was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Empty,
    /// A fragment that is not the first one, with no fragmented unit open.
    FragmentWithoutStart,
    /// A new unit while a fragmented one was still open.
    FragmentNotFinished,
    /// A STAP-A whose lengths do not add up.
    BadAggregate,
    /// A packet type this stream does not use (STAP-B, MTAP, FU-B).
    Unsupported(u8),
}

/// Puts the packets of one SSRC back together, frame by frame. Packets must come in order;
/// a frame with a gap in it is dropped whole.
#[derive(Default)]
pub struct Depacketizer {
    frame: Vec<u8>,
    timestamp: Option<u32>,
    next_sequence: Option<u16>,
    in_fragment: bool,
    broken: bool,
    /// Frames given up: a packet missing or out of place, or an unreadable payload.
    pub dropped_frames: u64,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// One packet of the stream. Returns the frame it completes (its marker bit is set and
    /// nothing was missing), as Annex B.
    pub fn push(&mut self, sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Option<Vec<u8>> {
        let gap = self.next_sequence.is_some_and(|expected| expected != sequence);
        self.next_sequence = Some(sequence.wrapping_add(1));
        if self.timestamp != Some(timestamp) {
            if self.timestamp.is_some() {
                // The frame before never got its last packet.
                self.dropped_frames += 1;
            }
            self.start_over();
            self.timestamp = Some(timestamp);
        }
        // (A gap just before a frame may have taken that frame's first packets with it.)
        if gap {
            self.broken = true;
        }
        if !self.broken && self.append(payload).is_err() {
            self.broken = true;
        }
        if !marker {
            return None;
        }
        let complete = !self.broken && !self.in_fragment && !self.frame.is_empty();
        let frame = std::mem::take(&mut self.frame);
        self.start_over();
        if complete {
            Some(frame)
        } else {
            self.dropped_frames += 1;
            None
        }
    }

    fn start_over(&mut self) {
        self.frame.clear();
        self.timestamp = None;
        self.in_fragment = false;
        self.broken = false;
    }

    fn append(&mut self, payload: &[u8]) -> Result<(), Error> {
        let first = *payload.first().ok_or(Error::Empty)?;
        match first & 0x1f {
            1..=23 => {
                if self.in_fragment {
                    return Err(Error::FragmentNotFinished);
                }
                self.frame.extend_from_slice(&START_CODE);
                self.frame.extend_from_slice(payload);
            }
            STAP_A => {
                if self.in_fragment {
                    return Err(Error::FragmentNotFinished);
                }
                let mut rest = &payload[1..];
                if rest.is_empty() {
                    return Err(Error::BadAggregate);
                }
                while !rest.is_empty() {
                    if rest.len() < 2 {
                        return Err(Error::BadAggregate);
                    }
                    let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    if len == 0 || rest.len() < 2 + len {
                        return Err(Error::BadAggregate);
                    }
                    self.frame.extend_from_slice(&START_CODE);
                    self.frame.extend_from_slice(&rest[2..2 + len]);
                    rest = &rest[2 + len..];
                }
            }
            FU_A => {
                let fu = *payload.get(1).ok_or(Error::Empty)?;
                if fu & 0x80 != 0 {
                    if self.in_fragment {
                        return Err(Error::FragmentNotFinished);
                    }
                    self.in_fragment = true;
                    self.frame.extend_from_slice(&START_CODE);
                    // The unit's own header: F and NRI of the indicator, type of the FU header.
                    self.frame.push((first & 0xe0) | (fu & 0x1f));
                } else if !self.in_fragment {
                    return Err(Error::FragmentWithoutStart);
                }
                self.frame.extend_from_slice(&payload[2..]);
                if fu & 0x40 != 0 {
                    self.in_fragment = false;
                }
            }
            other => return Err(Error::Unsupported(other)),
        }
        Ok(())
    }
}

/// The frame a list of payloads makes when nothing is lost (tests and tools).
pub fn frame_of<'a>(payloads: impl IntoIterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    let mut d = Depacketizer::new();
    let payloads: Vec<&[u8]> = payloads.into_iter().collect();
    let mut frame = None;
    for (i, payload) in payloads.iter().enumerate() {
        frame = d.push(i as u16, 0, i + 1 == payloads.len(), payload);
    }
    frame
}
