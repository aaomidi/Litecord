//! Transport encryption between this client and Discord's media server, for the two
//! modes the server offers today (the "rtpsize" modes of the voice docs):
//!
//! ```text
//! RTP :  clear = 12-byte header (+ CSRCs) + the 4-byte extension preamble when X = 1
//!        body  = extension elements + payload
//! RTCP:  clear = the first 8 bytes (header and sender SSRC);  body = the rest
//! packet = clear || AEAD(key, nonce, body, aad = clear) || tag(16) || counter(4, big endian)
//! nonce  = counter(4, big endian) || zeros     12 bytes for AES-256-GCM, 24 for XChaCha20-Poly1305
//! ```
//!
//! One counter per connection, shared by audio, video, retransmissions and RTCP, starting
//! at 1. This is the layer under DAVE: it protects packets on the way to the server, which
//! reads them; DAVE (see `dave`) is what the server cannot read.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::Aes256Gcm;
use chacha20poly1305::XChaCha20Poly1305;

pub const TAG_LEN: usize = 16;
pub const COUNTER_LEN: usize = 4;
/// What sealing adds to a packet.
pub const OVERHEAD: usize = TAG_LEN + COUNTER_LEN;
const RTCP_CLEAR: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Aes256Gcm,
    XChaCha20Poly1305,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Mode::XChaCha20Poly1305 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [Mode::Aes256Gcm, Mode::XChaCha20Poly1305].into_iter().find(|m| m.name() == name)
    }

    /// The mode to ask for among those the server lists: AES-256-GCM when it is there,
    /// XChaCha20-Poly1305 otherwise. Chosen by name, never by position.
    pub fn pick<'a>(offered: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let offered: Vec<Mode> = offered.into_iter().filter_map(Mode::from_name).collect();
        [Mode::Aes256Gcm, Mode::XChaCha20Poly1305].into_iter().find(|m| offered.contains(m))
    }
}

enum Cipher {
    Aes(Box<Aes256Gcm>),
    XChaCha(Box<XChaCha20Poly1305>),
}

/// The cipher of one media connection and its packet counter.
pub struct Transport {
    cipher: Cipher,
    mode: Mode,
    /// The counter of the next packet sealed; `None` once all 2^32 - 1 are used up (the
    /// key must not see a counter twice).
    next: Option<u32>,
}

impl Transport {
    pub fn new(mode: Mode, key: &[u8; 32]) -> Self {
        let cipher = match mode {
            Mode::Aes256Gcm => Cipher::Aes(Box::new(Aes256Gcm::new(key.into()))),
            Mode::XChaCha20Poly1305 => Cipher::XChaCha(Box::new(XChaCha20Poly1305::new(key.into()))),
        };
        Self { cipher, mode, next: Some(1) }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Packets sealed so far.
    pub fn sealed(&self) -> u32 {
        self.next.map_or(u32::MAX, |n| n - 1)
    }

    #[cfg(test)]
    pub fn set_next_counter(&mut self, counter: u32) {
        self.next = Some(counter);
    }

    /// A whole RTP packet (header, extension, payload), sealed. `None`: not RTP, or the
    /// counter is used up.
    pub fn seal_rtp(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        let clear = rtp_clear_len(packet)?;
        self.seal(packet, clear)
    }

    /// A whole RTCP packet (compound or not), sealed.
    pub fn seal_rtcp(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < RTCP_CLEAR {
            return None;
        }
        self.seal(packet, RTCP_CLEAR)
    }

    /// The RTP packet inside a sealed one (header, extension, payload). `None`: not for
    /// this key, or damaged.
    pub fn open_rtp(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let clear = rtp_clear_len(sealed)?;
        self.open(sealed, clear)
    }

    /// The RTCP packet inside a sealed one.
    pub fn open_rtcp(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        self.open(sealed, RTCP_CLEAR)
    }

    fn seal(&mut self, packet: &[u8], clear: usize) -> Option<Vec<u8>> {
        let counter = self.next?;
        self.next = counter.checked_add(1);
        let mut out = Vec::with_capacity(packet.len() + OVERHEAD);
        out.extend_from_slice(packet);
        let (aad, body) = out.split_at_mut(clear);
        let tag: [u8; TAG_LEN] = match &self.cipher {
            Cipher::Aes(c) => c.encrypt_in_place_detached(&nonce::<12>(counter).into(), aad, body).ok()?.into(),
            Cipher::XChaCha(c) => c.encrypt_in_place_detached(&nonce::<24>(counter).into(), aad, body).ok()?.into(),
        };
        out.extend_from_slice(&tag);
        out.extend_from_slice(&counter.to_be_bytes());
        Some(out)
    }

    fn open(&self, sealed: &[u8], clear: usize) -> Option<Vec<u8>> {
        if sealed.len() < clear + OVERHEAD {
            return None;
        }
        let (packet, suffix) = sealed.split_at(sealed.len() - COUNTER_LEN);
        let (packet, tag) = packet.split_at(packet.len() - TAG_LEN);
        let counter = u32::from_be_bytes(suffix.try_into().ok()?);
        let mut out = packet.to_vec();
        let (aad, body) = out.split_at_mut(clear);
        match &self.cipher {
            Cipher::Aes(c) => c.decrypt_in_place_detached(&nonce::<12>(counter).into(), aad, body, tag.into()).ok()?,
            Cipher::XChaCha(c) => c.decrypt_in_place_detached(&nonce::<24>(counter).into(), aad, body, tag.into()).ok()?,
        }
        Some(out)
    }
}

fn nonce<const N: usize>(counter: u32) -> [u8; N] {
    let mut nonce = [0u8; N];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// How many bytes of an RTP packet stay readable: the fixed header, the CSRCs and, when
/// the packet has an extension, its 4-byte preamble (not its elements).
pub fn rtp_clear_len(packet: &[u8]) -> Option<usize> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let csrcs = (packet[0] & 0x0f) as usize;
    let extension = packet[0] & 0x10 != 0;
    let clear = 12 + 4 * csrcs + if extension { 4 } else { 0 };
    (packet.len() >= clear).then_some(clear)
}

/// Whether a datagram on the media socket is RTCP (RFC 5761: packet types 192 to 223).
pub fn is_rtcp(datagram: &[u8]) -> bool {
    datagram.len() >= 2 && (192..=223).contains(&datagram[1])
}
