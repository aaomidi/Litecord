//! Receive side of a voice call, without any device or socket: Opus packets in, queued
//! 48 kHz stereo pairs out (`VoiceRxDecoder`), and queued pairs in, mixed output frames
//! out (`VoiceMixer`). The network loop and both output callbacks of `voice.rs` use these,
//! and so does the offline harness (`src/bin/voice_rx_harness.rs`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use chacha20poly1305::XChaCha20Poly1305;
use opus_rs::OpusDecoder;

// 48 kHz stereo pairs. Cap after admission, including concealed Opus frames.
pub const VOICE_QUEUE_LIMIT: usize = 28800;
pub const VOICE_PREBUFFER_SAMPLES: usize = 1920;
pub const VOICE_RESET_INACTIVE_CALLBACKS: u32 = 3;

/// Queue keys from this value up are stream (screen share) audio, not members of the call.
const STREAM_SSRC_BASE: u32 = 0x8000_0000;

/// "Smart Voice Ducking" (Settings > Voice & Audio). Off unless the user turns it on.
static SMART_DUCKING: AtomicBool = AtomicBool::new(false);

pub fn set_smart_ducking(enabled: bool) {
    SMART_DUCKING.store(enabled, Ordering::Relaxed);
}

pub fn smart_ducking() -> bool {
    SMART_DUCKING.load(Ordering::Relaxed)
}

/// Times a member's audio ran out while it was not silent (a late or missing packet).
pub static VOICE_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

pub fn decoded_voice_pairs(pcm: &[f32], channels: usize) -> Vec<(f32, f32)> {
    // A sample that is not a number would be held and replayed by everything downstream.
    let clean = |s: f32| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
    if channels == 2 {
        pcm.chunks_exact(2).map(|s| (clean(s[0]), clean(s[1]))).collect()
    } else {
        pcm.iter().map(|s| { let mono = clean(*s); (mono, mono) }).collect()
    }
}

pub fn push_voice_frame(q: &mut VecDeque<(f32, f32)>, plc: &[(f32, f32)], decoded: &[(f32, f32)]) {
    let incoming = plc.len().saturating_add(decoded.len());
    let discard = q.len().saturating_add(incoming).saturating_sub(VOICE_QUEUE_LIMIT);
    q.drain(..discard.min(q.len()));
    q.extend(plc.iter().chain(decoded).skip(incoming.saturating_sub(VOICE_QUEUE_LIMIT)).copied());
}

// ---------------------------------------------------------------------------------------
// Transport encryption of voice packets (the two "rtpsize" modes)
// ---------------------------------------------------------------------------------------

pub const VOICE_MODE_AES256_GCM: &str = "aead_aes256_gcm_rtpsize";
pub const VOICE_MODE_XCHACHA20: &str = "aead_xchacha20_poly1305_rtpsize";

/// The mode to ask the voice server for, out of the ones it offers: AES-256-GCM when it is
/// listed, otherwise XChaCha20-Poly1305 (which every voice server has to offer).
pub fn select_voice_mode<'a>(offered: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let mut xchacha = false;
    for mode in offered {
        if mode == VOICE_MODE_AES256_GCM {
            return Some(VOICE_MODE_AES256_GCM);
        }
        xchacha |= mode == VOICE_MODE_XCHACHA20;
    }
    xchacha.then_some(VOICE_MODE_XCHACHA20)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtpOpenError {
    /// Too short for its own header, authentication tag and nonce.
    Malformed,
    /// The key, the mode or the packet is wrong: the tag did not verify.
    Auth,
    /// The padding bit is set but the padding count is not possible.
    BadPadding,
}

/// Encrypts and decrypts the RTP packets of a voice connection.
///
/// Layout of both modes (the same as SRTP, RFC 3711, with the nonce appended):
///   [fixed header 12][CSRCs 4*cc][extension preamble 4, if X]   in the clear, authenticated
///   [extension body 4*words, if X][media payload][RTP padding, if P]   encrypted
///   [tag 16][nonce counter 4]
/// The nonce is the 4 trailing bytes followed by zeros (12 bytes for AES-GCM, 24 for
/// XChaCha20). Padding is inside the ciphertext: its count is the last byte of the plaintext.
pub enum VoiceCipher {
    Aes256Gcm(Box<Aes256Gcm>),
    XChaCha20(Box<XChaCha20Poly1305>),
}

const RTP_TAG_LEN: usize = 16;
const RTP_NONCE_LEN: usize = 4;

impl VoiceCipher {
    pub fn new(mode: &str, key: &[u8]) -> Option<Self> {
        match mode {
            VOICE_MODE_AES256_GCM => Aes256Gcm::new_from_slice(key).ok().map(|c| Self::Aes256Gcm(Box::new(c))),
            VOICE_MODE_XCHACHA20 => XChaCha20Poly1305::new_from_slice(key).ok().map(|c| Self::XChaCha20(Box::new(c))),
            _ => None,
        }
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::Aes256Gcm(_) => VOICE_MODE_AES256_GCM,
            Self::XChaCha20(_) => VOICE_MODE_XCHACHA20,
        }
    }

    /// The media payload of a received RTP packet: decrypted, without the extension body
    /// and without RTP padding. Empty for a packet that carries only padding (a probe).
    pub fn open(&self, pkt: &[u8]) -> Result<Vec<u8>, RtpOpenError> {
        if pkt.len() < 12 {
            return Err(RtpOpenError::Malformed);
        }
        let has_padding = (pkt[0] & 0x20) != 0;
        let has_ext = (pkt[0] & 0x10) != 0;
        let preamble = 12 + 4 * (pkt[0] & 0x0F) as usize;
        let aad_len = preamble + if has_ext { 4 } else { 0 };
        if pkt.len() < aad_len + RTP_TAG_LEN + RTP_NONCE_LEN {
            return Err(RtpOpenError::Malformed);
        }
        let ext_body = if has_ext { 4 * u16::from_be_bytes([pkt[preamble + 2], pkt[preamble + 3]]) as usize } else { 0 };
        let nonce_at = pkt.len() - RTP_NONCE_LEN;
        let sealed = Payload { msg: &pkt[aad_len..nonce_at], aad: &pkt[..aad_len] };
        let mut plain = match self {
            Self::Aes256Gcm(cipher) => {
                let mut nonce = [0u8; 12];
                nonce[..RTP_NONCE_LEN].copy_from_slice(&pkt[nonce_at..]);
                cipher.decrypt(aes_gcm::Nonce::from_slice(&nonce), sealed)
            }
            Self::XChaCha20(cipher) => {
                let mut nonce = [0u8; 24];
                nonce[..RTP_NONCE_LEN].copy_from_slice(&pkt[nonce_at..]);
                cipher.decrypt(chacha20poly1305::XNonce::from_slice(&nonce), sealed)
            }
        }.map_err(|_| RtpOpenError::Auth)?;

        let mut end = plain.len();
        if has_padding {
            // RFC 3550, 5.1: the last byte counts the padding bytes, itself included.
            let count = plain.last().copied().unwrap_or(0) as usize;
            if count == 0 || ext_body + count > end {
                return Err(RtpOpenError::BadPadding);
            }
            end -= count;
        }
        if ext_body > end {
            return Err(RtpOpenError::Malformed);
        }
        plain.truncate(end);
        plain.drain(..ext_body);
        Ok(plain)
    }

    /// A packet to send: `header` (in the clear, authenticated), the encrypted payload with
    /// its tag, then the nonce counter.
    pub fn seal(&self, header: &[u8], payload: &[u8], nonce_counter: u32) -> Option<Vec<u8>> {
        let counter = nonce_counter.to_be_bytes();
        let plain = Payload { msg: payload, aad: header };
        let sealed = match self {
            Self::Aes256Gcm(cipher) => {
                let mut nonce = [0u8; 12];
                nonce[..RTP_NONCE_LEN].copy_from_slice(&counter);
                cipher.encrypt(aes_gcm::Nonce::from_slice(&nonce), plain)
            }
            Self::XChaCha20(cipher) => {
                let mut nonce = [0u8; 24];
                nonce[..RTP_NONCE_LEN].copy_from_slice(&counter);
                cipher.encrypt(chacha20poly1305::XNonce::from_slice(&nonce), plain)
            }
        }.ok()?;
        let mut pkt = Vec::with_capacity(header.len() + sealed.len() + RTP_NONCE_LEN);
        pkt.extend_from_slice(header);
        pkt.extend_from_slice(&sealed);
        pkt.extend_from_slice(&counter);
        Some(pkt)
    }
}

// ---------------------------------------------------------------------------------------
// Decoding and loss concealment
// ---------------------------------------------------------------------------------------

/// A pause longer than this starts a new talkspurt: nothing is concealed across it.
const TALKSPURT_GAP_MS: u64 = 100;
/// Longest run of lost packets that is concealed.
const MAX_CONCEALED_PACKETS: u16 = 4;
/// A sequence number up to this far behind the expected one is a late packet or a
/// duplicate. Anything else that is not a small step forward is the sender starting its
/// numbering again: it is played, and nothing is concealed for it.
const LATE_WINDOW_PACKETS: u16 = 64;
/// A decoder for the other channel count that sat unused for more packets than this is
/// started afresh when the sender switches back to it, instead of resuming old state.
const STALE_DECODER_PACKETS: u64 = 5;
/// What is kept of the decoded audio to extrapolate from when the decoder cannot (40 ms).
const TAIL_PAIRS: usize = 1920;
/// The start of the first real frame after made-up audio is faded in over this (2.5 ms).
const REJOIN_FADE_PAIRS: usize = 120;
/// From its third decoder panic on, a member is not decoded for this long after each one:
/// every panic is written to the panic log, and a sender whose packets keep causing them
/// (50 a second) must not be able to fill it.
const PANIC_TIMEOUT_MS: u64 = 2000;
/// The frame the official client sends five times when someone stops talking.
const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

#[derive(Default, Clone, Debug, PartialEq)]
pub struct RxStats {
    pub packets: u64,
    pub silk: u64,
    pub hybrid: u64,
    pub celt: u64,
    pub stereo: u64,
    /// Packets that carry a low-bitrate copy of the previous frame (in-band FEC).
    pub with_fec: u64,
    pub silence_frames: u64,
    /// Lost frames replaced by extrapolated audio (by the decoder for SILK and hybrid
    /// packets, by `extrapolate` for CELT-only ones), or by silence next to silence.
    pub concealed: u64,
    /// Lost frames that could only be replaced by silence although the member was not
    /// silent: the second and later frames of a run of lost CELT-only packets.
    pub concealed_as_silence: u64,
    pub conceal_failed: u64,
    /// Lost frames left out because the hole had already played (the queue was empty).
    pub conceal_skipped: u64,
    /// Duplicates and packets that arrived after their place had been concealed.
    pub late_dropped: u64,
    /// Jumps of the sequence number that are neither a loss nor a late packet.
    pub seq_jumps: u64,
    pub decode_errors: u64,
    /// Packets on which the decoder panicked (its state was thrown away).
    pub decoder_panics: u64,
}

#[derive(Default)]
pub struct RxFrame {
    /// Audio made up for the packets lost before this one.
    pub concealed: Vec<(f32, f32)>,
    pub decoded: Vec<(f32, f32)>,
    pub conceal_calls: u32,
    pub conceal_ok: u32,
    pub late: bool,
    pub error: Option<&'static str>,
}

struct LastPacket {
    next_seq: u16,
    timestamp: u32,
    at_ms: u64,
    toc: u8,
    channels: usize,
    samples: usize,
    silence: bool,
}

/// True if a SILK or hybrid packet carries a redundant copy of the previous frame
/// (same test as libopus `opus_packet_has_lbrr`, single-frame packets only).
pub fn opus_packet_has_fec(packet: &[u8]) -> bool {
    if packet.len() < 2 || (packet[0] & 0x80) != 0 || (packet[0] & 0x03) != 0 {
        return false;
    }
    let config = packet[0] >> 3;
    // SILK-only frames are 10, 20, 40 or 60 ms; hybrid ones 10 or 20 ms.
    let silk_frames = if config < 12 { [1u8, 1, 2, 3][(config & 3) as usize] } else { 1 };
    let first = packet[1];
    let mut fec = (first >> (7 - silk_frames)) & 1 != 0;
    if (packet[0] & 0x04) != 0 {
        fec |= (first >> (6 - 2 * silk_frames)) & 1 != 0;
    }
    fec
}

/// One call into opus-rs. It indexes out of bounds on some malformed SILK payloads; a panic
/// must not end the task that receives everyone's audio. `Err(())` = it panicked.
fn guarded_decode(dec: &mut OpusDecoder, packet: &[u8], pcm: &mut [f32]) -> Result<Result<usize, &'static str>, ()> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dec.decode(packet, 5760, pcm))) {
        Ok(Ok(n)) if n > 5760 => Ok(Err("decoder returned more samples than fit")),
        Ok(res) => Ok(res),
        Err(_) => Err(()),
    }
}

/// 20 ms (`n` pairs) that continue `tail`: its last pitch period repeated and faded to
/// silence. For lost CELT-only frames, which opus-rs replaces with zeros. Empty when there
/// is nothing to extrapolate from.
fn extrapolate(tail: &[(f32, f32)], n: usize) -> Vec<(f32, f32)> {
    const WINDOW: usize = 480; // 10 ms compared against earlier audio
    const MIN_LAG: usize = 96; // 500 Hz
    const MAX_LAG: usize = 960; // 50 Hz
    const JOIN: usize = 96; // 2 ms
    let len = tail.len();
    if len < WINDOW + MIN_LAG + 1 {
        return Vec::new();
    }
    let mono: Vec<f32> = tail.iter().map(|p| (p.0 + p.1) * 0.5).collect();
    let recent = &mono[len - WINDOW..];
    let energy: f32 = recent.iter().map(|v| v * v).sum();
    if energy < 1e-7 {
        return Vec::new();
    }
    let (mut best_lag, mut best) = (MIN_LAG, f32::MIN);
    for lag in MIN_LAG..=MAX_LAG.min(len - WINDOW) {
        let earlier = &mono[len - WINDOW - lag..len - lag];
        let (mut cross, mut e2) = (0.0f32, 0.0f32);
        for (a, b) in recent.iter().zip(earlier) {
            cross += a * b;
            e2 += b * b;
        }
        let score = cross / (energy * e2).sqrt().max(1e-9);
        if score > best {
            best = score;
            best_lag = lag;
        }
    }
    // Audio with no clear period (noise, consonants) repeated for 20 ms would buzz.
    let fade_len = if best >= 0.5 { n } else { n / 4 };
    let from = len - best_lag;
    // Without this the repeat would start with the step between two unrelated samples.
    let join = if from > 0 {
        (tail[len - 1].0 - tail[from - 1].0, tail[len - 1].1 - tail[from - 1].1)
    } else {
        (0.0, 0.0)
    };
    (0..n).map(|i| {
        let s = tail[from + i % best_lag];
        let j = if i < JOIN { 1.0 - i as f32 / JOIN as f32 } else { 0.0 };
        let g = if i < fade_len { 0.5 + 0.5 * (std::f32::consts::PI * i as f32 / fade_len as f32).cos() } else { 0.0 };
        (((s.0 + join.0 * j) * g).clamp(-1.0, 1.0), ((s.1 + join.1 * j) * g).clamp(-1.0, 1.0))
    }).collect()
}

/// Decoder state of one remote member (one SSRC).
pub struct VoiceRxDecoder {
    // opus-rs rejects a packet whose channel flag differs from the decoder's. The number
    // is `decode_no` when that decoder was last used.
    decoders: HashMap<usize, (OpusDecoder, u64)>,
    /// Packets given to a decoder so far (silence frames are not).
    decode_no: u64,
    last: Option<LastPacket>,
    pcm: Vec<f32>,
    /// The most recent decoded audio, to extrapolate from.
    tail: Vec<(f32, f32)>,
    /// Lost frames in a row that the decoder could not conceal.
    made_up_run: u32,
    /// The next decoded frame follows made-up audio or silence it knows nothing about.
    rejoin: bool,
    last_ms: u64,
    not_decoded_until_ms: u64,
    pub stats: RxStats,
}

impl Default for VoiceRxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceRxDecoder {
    pub fn new() -> Self {
        Self {
            decoders: HashMap::new(), decode_no: 0, last: None, pcm: vec![0.0; 11520],
            tail: Vec::with_capacity(TAIL_PAIRS + 5760), made_up_run: 0, rejoin: false, last_ms: 0,
            not_decoded_until_ms: 0,
            stats: RxStats::default(),
        }
    }

    /// Milliseconds (of the clock given to `packet`) since this member's last packet.
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_ms)
    }

    /// One Opus payload (already decrypted) with its RTP sequence number and timestamp.
    /// `now_ms` is any monotonic clock. Every lost frame keeps its place in time.
    pub fn packet(&mut self, seq: u16, timestamp: u32, opus: &[u8], now_ms: u64) -> RxFrame {
        self.packet_for_playout(seq, timestamp, opus, now_ms, None)
    }

    /// As `packet`, for live playout. `queued` is how many pairs of this member are still
    /// waiting to be played. If none are, the hole left by the lost packets has already been
    /// heard: only the frame next to this packet is concealed, because queueing all of them
    /// would play old audio after the gap and leave the member that much later.
    pub fn packet_for_playout(&mut self, seq: u16, timestamp: u32, opus: &[u8], now_ms: u64, queued: Option<usize>) -> RxFrame {
        let mut frame = RxFrame::default();
        if opus.is_empty() {
            return frame;
        }
        self.last_ms = now_ms;
        if now_ms < self.not_decoded_until_ms {
            frame.error = Some("not decoded: the decoder panicked repeatedly on this member's packets");
            return frame;
        }
        if let Some(last) = self.last.as_ref() {
            let elapsed = now_ms.saturating_sub(last.at_ms);
            let missed = seq.wrapping_sub(last.next_seq);
            if missed > u16::MAX - LATE_WINDOW_PACKETS {
                if elapsed <= TALKSPURT_GAP_MS {
                    // Its place was already filled (concealed, or it is a duplicate).
                    self.stats.late_dropped += 1;
                    frame.late = true;
                    return frame;
                }
            } else if missed >= 0x8000 {
                self.stats.seq_jumps += 1;
            } else if missed > 0 && missed <= MAX_CONCEALED_PACKETS && elapsed <= TALKSPURT_GAP_MS + 20 * missed as u64 {
                // A sequence number that carried no audio (a probe or padding packet)
                // leaves no hole in the timestamps: nothing to conceal for it.
                let by_time = (timestamp.wrapping_sub(last.timestamp) as usize / last.samples.max(1)).saturating_sub(1);
                let lost = (missed as usize).min(by_time);
                let conceal = if queued == Some(0) { lost.min(1) } else { lost };
                self.stats.conceal_skipped += (lost - conceal) as u64;
                self.conceal(conceal, &mut frame);
            }
        }

        let toc = opus[0];
        let channels = if (toc & 0x04) != 0 { 2 } else { 1 };
        let silence = opus == OPUS_SILENCE_FRAME;
        self.stats.packets += 1;
        match toc >> 3 {
            0..=11 => self.stats.silk += 1,
            12..=15 => self.stats.hybrid += 1,
            _ => self.stats.celt += 1,
        }
        if channels == 2 { self.stats.stereo += 1; }
        if opus_packet_has_fec(opus) { self.stats.with_fec += 1; }

        let mut samples = 0usize;
        if silence {
            self.stats.silence_frames += 1;
            samples = 960;
            frame.decoded = vec![(0.0, 0.0); samples];
            self.made_up_run = 0;
            self.rejoin = false;
        } else {
            self.decode_no += 1;
            let packet_no = self.decode_no;
            // A decoder that last ran several packets ago (the sender went to the other
            // channel count and came back) would resume from state that no longer matches.
            if self.decoders.get(&channels).is_some_and(|(_, used)| packet_no - *used > STALE_DECODER_PACKETS) {
                self.decoders.remove(&channels);
            }
            if !self.decoders.contains_key(&channels) {
                match OpusDecoder::new(48000, channels) {
                    Ok(dec) => { self.decoders.insert(channels, (dec, packet_no)); }
                    Err(e) => {
                        self.stats.decode_errors += 1;
                        frame.error = Some(e);
                    }
                }
            }
            if let Some((dec, used)) = self.decoders.get_mut(&channels) {
                *used = packet_no;
                match guarded_decode(dec, opus, &mut self.pcm[..]) {
                    Ok(Ok(n)) if n > 0 => {
                        samples = n;
                        frame.decoded = decoded_voice_pairs(&self.pcm[..n * channels], channels);
                        if self.rejoin {
                            for (i, p) in frame.decoded.iter_mut().take(REJOIN_FADE_PAIRS).enumerate() {
                                let w = (i + 1) as f32 / (REJOIN_FADE_PAIRS + 1) as f32;
                                *p = (p.0 * w, p.1 * w);
                            }
                        }
                        self.made_up_run = 0;
                        self.rejoin = false;
                        self.tail.extend_from_slice(&frame.decoded);
                        let excess = self.tail.len().saturating_sub(TAIL_PAIRS);
                        self.tail.drain(..excess);
                    }
                    Ok(Ok(_)) => {
                        self.stats.decode_errors += 1;
                        frame.error = Some("decoder returned no samples");
                    }
                    Ok(Err(e)) => {
                        self.stats.decode_errors += 1;
                        frame.error = Some(e);
                    }
                    Err(()) => {
                        // Whatever it was doing, its state cannot be trusted any more.
                        self.decoders.remove(&channels);
                        self.panicked();
                        frame.error = Some("decoder panicked on this packet; its state was reset");
                    }
                }
            }
        }

        let next_seq = seq.wrapping_add(1);
        if samples > 0 {
            self.last = Some(LastPacket { next_seq, timestamp, at_ms: now_ms, toc, channels, samples, silence });
        } else if self.last.is_some() {
            // Not decodable: it still used 20 ms of the member's time, so within a
            // talkspurt it is treated as a lost packet.
            if self.last.as_ref().is_some_and(|l| now_ms.saturating_sub(l.at_ms) <= TALKSPURT_GAP_MS) {
                self.conceal(1, &mut frame);
            }
            if let Some(last) = self.last.as_mut() {
                last.next_seq = next_seq;
                last.timestamp = timestamp;
                last.at_ms = now_ms;
            }
        }
        frame
    }

    fn panicked(&mut self) {
        self.stats.decoder_panics += 1;
        if self.stats.decoder_panics >= 3 {
            self.not_decoded_until_ms = self.last_ms + PANIC_TIMEOUT_MS;
        }
    }

    fn conceal(&mut self, packets: usize, frame: &mut RxFrame) {
        let Some(last) = self.last.as_ref() else { return };
        // opus-rs has no concealment for CELT-only packets: it returns 20 ms of zeros.
        let celt_only = (last.toc & 0x80) != 0;
        for _ in 0..packets {
            frame.conceal_calls += 1;
            let before = frame.concealed.len();
            let mut as_silence = false;
            if last.silence {
                // Silence next to silence.
            } else if celt_only {
                if self.made_up_run == 0 {
                    frame.concealed.extend(extrapolate(&self.tail, last.samples));
                }
                as_silence = frame.concealed.len() == before;
                self.made_up_run += 1;
                self.rejoin = true;
            } else if let Some((dec, _)) = self.decoders.get_mut(&last.channels) {
                // A packet that is only its first byte is "frame lost" to the decoder, which
                // then runs its own concealment from the previous frames. (An empty packet,
                // which this code used to pass, is rejected by opus-rs.)
                match guarded_decode(dec, &[last.toc & 0xFC], &mut self.pcm[..]) {
                    Ok(Ok(n)) => {
                        let pairs = decoded_voice_pairs(&self.pcm[..n * last.channels], last.channels);
                        self.tail.extend_from_slice(&pairs);
                        frame.concealed.extend(pairs);
                    }
                    Ok(Err(_)) => {}
                    Err(()) => {
                        self.decoders.remove(&last.channels);
                        self.stats.decoder_panics += 1;
                        if self.stats.decoder_panics >= 3 {
                            self.not_decoded_until_ms = self.last_ms + PANIC_TIMEOUT_MS;
                        }
                    }
                }
            }
            if as_silence {
                self.stats.concealed_as_silence += 1;
            } else if frame.concealed.len() > before || last.silence {
                frame.conceal_ok += 1;
                self.stats.concealed += 1;
            } else {
                self.stats.conceal_failed += 1;
                self.rejoin = true;
            }
            // Whatever happened, the lost packet keeps its place in time.
            let have = frame.concealed.len() - before;
            frame.concealed.extend(std::iter::repeat((0.0, 0.0)).take(last.samples.saturating_sub(have)));
        }
        let excess = self.tail.len().saturating_sub(TAIL_PAIRS);
        self.tail.drain(..excess);
    }
}

// ---------------------------------------------------------------------------------------
// Resampler (48 kHz to the device rate)
// ---------------------------------------------------------------------------------------

const KERNEL_BASE_TAPS: usize = 64;
const KERNEL_MAX_TAPS: usize = 512;
const KERNEL_PHASES: usize = 512;
const KAISER_BETA: f64 = 8.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0f64, 1.0f64, x * x / 4.0);
    for k in 1..40 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 { break; }
    }
    sum
}

/// Windowed-sinc interpolation table: band-limited to the lower of the two rates, so
/// neither images of the 48 kHz signal nor aliases reach the output.
struct Kernel {
    taps: usize,
    table: Vec<f32>,
}

impl Kernel {
    fn new(out_rate: u32) -> Self {
        let scale = (out_rate as f64 / 48000.0).min(1.0);
        let taps = (((KERNEL_BASE_TAPS as f64 / scale).ceil() as usize + 1) & !1).clamp(8, KERNEL_MAX_TAPS);
        let half = taps as f64 / 2.0;
        let i0_beta = bessel_i0(KAISER_BETA);
        let mut table = Vec::with_capacity((KERNEL_PHASES + 1) * taps);
        let mut row = vec![0.0f64; taps];
        for p in 0..=KERNEL_PHASES {
            let frac = p as f64 / KERNEL_PHASES as f64;
            let mut sum = 0.0;
            for (k, c) in row.iter_mut().enumerate() {
                // Distance, in input samples, from tap k to the output instant.
                let x = k as f64 - (half - 1.0) - frac;
                let u = (x / half).clamp(-1.0, 1.0);
                let window = bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) / i0_beta;
                let arg = std::f64::consts::PI * x * scale;
                let sinc = if arg.abs() < 1e-9 { 1.0 } else { arg.sin() / arg };
                *c = sinc * scale * window;
                sum += *c;
            }
            table.extend(row.iter().map(|c| (*c / sum) as f32));
        }
        Self { taps, table }
    }

    #[inline]
    fn interpolate(&self, hist: &[(f32, f32)], phase: f64) -> (f32, f32) {
        let pos = phase.clamp(0.0, 1.0) * KERNEL_PHASES as f64;
        let row = (pos as usize).min(KERNEL_PHASES - 1);
        let w = (pos - row as f64) as f32;
        let a = &self.table[row * self.taps..(row + 1) * self.taps];
        let b = &self.table[(row + 1) * self.taps..(row + 2) * self.taps];
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for ((s, ca), cb) in hist.iter().zip(a).zip(b) {
            let c = ca + (cb - ca) * w;
            l += s.0 * c;
            r += s.1 * c;
        }
        (l, r)
    }
}

// ---------------------------------------------------------------------------------------
// Per-member playout: start/stop, underrun handling, resampling
// ---------------------------------------------------------------------------------------

/// When a queue runs dry the last sample is faded out at this rate per 48 kHz sample
/// (below -60 dB within 5 ms) instead of being held and then cut.
const DRY_DECAY: f32 = 0.97;
/// Longest crossfade, in 48 kHz samples, from the faded-out tail into audio that resumes.
const RESUME_FADE_SAMPLES: u32 = 96;
/// A member counts as speaking, for ducking, above this peak (about -46 dBFS)...
const DUCK_SPEECH_PEAK: f32 = 0.005;
/// ...and for this long after the last such callback.
const DUCK_HOLD_SECONDS: f64 = 0.3;
/// Time constant of gain changes (ducking, volume slider).
const GAIN_SMOOTH_SECONDS: f64 = 0.02;

fn fade_step(v: (f32, f32)) -> (f32, f32) {
    let f = |x: f32| if x.abs() < 1e-4 { 0.0 } else { x * DRY_DECAY };
    (f(v.0), f(v.1))
}

struct Playout {
    started: bool,
    inactive: u32,
    // Resampler history, written twice so the last `taps` samples are always one slice.
    hist: Vec<(f32, f32)>,
    pos: usize,
    phase: f64,
    // Underrun handling.
    hold: (f32, f32),
    fade_from: (f32, f32),
    dry_run: u32,
    fade_len: u32,
    fade_pos: u32,
    zeros_in: usize,
    // Gain.
    gain: f32,
    gain_set: bool,
    muted: bool,
    vol: f32,
    prio: i32,
    speech_hold: u32,
}

impl Playout {
    fn new(taps: usize) -> Self {
        Self {
            started: false, inactive: 0,
            hist: vec![(0.0, 0.0); taps * 2], pos: 0, phase: 0.0,
            hold: (0.0, 0.0), fade_from: (0.0, 0.0), dry_run: RESUME_FADE_SAMPLES, fade_len: 0, fade_pos: 0,
            zeros_in: usize::MAX / 2,
            gain: 1.0, gain_set: false, muted: false, vol: 1.0, prio: 0, speech_hold: 0,
        }
    }

    fn reset(&mut self) {
        self.started = false;
        self.hist.fill((0.0, 0.0));
        self.pos = 0;
        self.phase = 0.0;
        self.hold = (0.0, 0.0);
        self.fade_from = (0.0, 0.0);
        self.dry_run = RESUME_FADE_SAMPLES;
        self.fade_len = 0;
        self.fade_pos = 0;
        self.zeros_in = usize::MAX / 2;
        self.gain_set = false;
    }

    /// Nothing left to play out: the fade is over and the resampler holds only zeros.
    fn flushed(&self, taps: usize) -> bool {
        self.zeros_in >= taps
    }

    /// The next 48 kHz sample: from the queue, or a fade-out of the last one if it is empty.
    #[inline]
    fn next_input(&mut self, q: &mut VecDeque<(f32, f32)>, dry: &mut usize) -> (f32, f32) {
        let out = match q.pop_front() {
            Some(s) => {
                if self.dry_run > 0 {
                    self.fade_len = (self.dry_run.saturating_mul(2)).min(RESUME_FADE_SAMPLES);
                    self.fade_pos = 0;
                    self.fade_from = self.hold;
                    self.dry_run = 0;
                }
                if self.fade_pos < self.fade_len {
                    self.fade_pos += 1;
                    let w = self.fade_pos as f32 / (self.fade_len + 1) as f32;
                    self.fade_from = fade_step(self.fade_from);
                    (s.0 * w + self.fade_from.0 * (1.0 - w), s.1 * w + self.fade_from.1 * (1.0 - w))
                } else {
                    s
                }
            }
            None => {
                if self.dry_run == 0 && self.hold != (0.0, 0.0) && self.started {
                    VOICE_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                }
                self.dry_run = self.dry_run.saturating_add(1);
                *dry += 1;
                fade_step(self.hold)
            }
        };
        self.hold = out;
        self.zeros_in = if out == (0.0, 0.0) { self.zeros_in.saturating_add(1) } else { 0 };
        out
    }

    #[inline]
    fn next_output(&mut self, q: &mut VecDeque<(f32, f32)>, kernel: Option<&Kernel>, step: f64, dry: &mut usize) -> (f32, f32) {
        let Some(kernel) = kernel else {
            // Device at 48 kHz: the decoded samples go out as they are.
            return self.next_input(q, dry);
        };
        self.phase += step;
        let pops = self.phase as usize;
        self.phase -= pops as f64;
        for _ in 0..pops {
            let s = self.next_input(q, dry);
            self.hist[self.pos] = s;
            self.hist[self.pos + kernel.taps] = s;
            self.pos = (self.pos + 1) % kernel.taps;
        }
        kernel.interpolate(&self.hist[self.pos..self.pos + kernel.taps], self.phase)
    }
}

// ---------------------------------------------------------------------------------------
// Limiter on the mix
// ---------------------------------------------------------------------------------------

/// Look-ahead peak limiter. Below full scale the mix passes through unchanged (delayed by
/// the look-ahead); above it the gain is lowered smoothly ahead of the peak and released
/// slowly afterwards, so several loud members do not clip or get waveshaped.
struct Limiter {
    len: usize,
    delay: Vec<(f32, f32)>,
    env_ring: Vec<f32>,
    min_ring: Vec<f32>,
    pos: usize,
    env_pos: usize,
    env: f32,
    sum: f64,
    release: f32,
    quiet: usize,
    pending: usize,
}

const LIMITER_LOOKAHEAD_SECONDS: f64 = 0.002;
const LIMITER_RELEASE_SECONDS: f64 = 0.1;

impl Limiter {
    fn new(rate: u32) -> Self {
        let len = ((rate as f64 * LIMITER_LOOKAHEAD_SECONDS) as usize).max(8);
        Self {
            len,
            delay: vec![(0.0, 0.0); len],
            env_ring: vec![1.0; len + 1],
            min_ring: vec![1.0; len],
            pos: 0, env_pos: 0, env: 1.0, sum: len as f64,
            release: (1.0 - (-1.0 / (LIMITER_RELEASE_SECONDS * rate as f64)).exp()) as f32,
            quiet: usize::MAX / 2,
            pending: 0,
        }
    }

    /// No audio left inside and no gain reduction in progress.
    fn idle(&self) -> bool {
        self.pending == 0 && self.quiet > 2 * self.len + 2
    }

    #[inline]
    fn process(&mut self, x: (f32, f32)) -> (f32, f32) {
        let peak = x.0.abs().max(x.1.abs());
        let need = if peak > 1.0 { 1.0 / peak } else { 1.0 };
        // Instant attack, slow release; the look-ahead below turns the attack into a ramp.
        let mut env = need.min(self.env + (1.0 - self.env) * self.release);
        // In f32 the release stops rising about 0.00014 short of 1 (the step falls below
        // half a unit in the last place), so the last 0.004 dB are a snap.
        if env > 0.9995 { env = 1.0; }
        self.env = env;
        self.quiet = if env >= 1.0 { self.quiet.saturating_add(1) } else { 0 };
        self.pending = if x != (0.0, 0.0) { self.len } else { self.pending.saturating_sub(1) };

        self.env_ring[self.env_pos] = env;
        self.env_pos = (self.env_pos + 1) % (self.len + 1);
        let delayed = std::mem::replace(&mut self.delay[self.pos], x);

        let gain = if self.quiet > 2 * self.len + 2 {
            self.min_ring[self.pos] = 1.0;
            self.sum = self.len as f64;
            1.0
        } else {
            // Lowest gain needed by anything still inside the look-ahead, then averaged
            // over the look-ahead: never above what the sample leaving the delay needs.
            let lowest = self.env_ring.iter().copied().fold(1.0f32, f32::min);
            self.sum += lowest as f64 - self.min_ring[self.pos] as f64;
            self.min_ring[self.pos] = lowest;
            ((self.sum / self.len as f64) as f32).min(1.0)
        };
        self.pos = (self.pos + 1) % self.len;
        if gain >= 1.0 {
            delayed
        } else {
            ((delayed.0 * gain).clamp(-1.0, 1.0), (delayed.1 * gain).clamp(-1.0, 1.0))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------------------

/// What one callback did with one queue (for the offline harness and the tests).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MixDiag {
    pub ready: bool,
    pub dry_samples: usize,
    pub gain: f32,
}

/// Everything between the per-member queues and the samples handed to the device.
pub struct VoiceMixer {
    out_rate: u32,
    step: f64,
    kernel: Option<Kernel>,
    taps: usize,
    states: HashMap<u32, Playout>,
    limiter: Limiter,
    gain_coef: f32,
    active: Vec<u32>,
    mix: Vec<(f32, f32)>,
    out: Vec<(f32, f32)>,
    pub diag: HashMap<u32, MixDiag>,
}

impl VoiceMixer {
    pub fn new(out_rate: u32) -> Self {
        let out_rate = out_rate.max(1);
        let kernel = (out_rate != 48000).then(|| Kernel::new(out_rate));
        let taps = kernel.as_ref().map_or(1, |k| k.taps);
        Self {
            out_rate,
            step: 48000.0 / out_rate as f64,
            kernel,
            taps,
            states: HashMap::new(),
            limiter: Limiter::new(out_rate),
            gain_coef: (1.0 - (-1.0 / (GAIN_SMOOTH_SECONDS * out_rate as f64)).exp()) as f32,
            active: Vec::new(),
            mix: Vec::new(),
            out: Vec::new(),
            diag: HashMap::new(),
        }
    }

    /// For the first callback of a new output stream: what piled up while no stream was
    /// running (a device change, a rebuild) is old. Keeps the newest part of each queue,
    /// as much as a member starts playing with.
    pub fn drop_backlog(queues: &mut HashMap<u32, VecDeque<(f32, f32)>>) {
        for q in queues.values_mut() {
            let excess = q.len().saturating_sub(VOICE_PREBUFFER_SAMPLES);
            q.drain(..excess);
        }
    }

    /// Delay added by the limiter look-ahead, in output frames.
    pub fn limiter_delay(&self) -> usize {
        self.limiter.len
    }

    /// Mixes `frames` output frames from the queues. `settings` gives (muted, volume,
    /// priority) of a member's SSRC. Returns an empty slice when there is nothing to play.
    pub fn mix(
        &mut self,
        queues: &mut HashMap<u32, VecDeque<(f32, f32)>>,
        frames: usize,
        ducking: bool,
        mut settings: impl FnMut(u32) -> (bool, f32, i32),
    ) -> &[(f32, f32)] {
        self.active.clear();
        self.diag.clear();
        self.out.clear();
        self.states.retain(|ssrc, _| queues.contains_key(ssrc));

        let taps = self.taps;
        let mut to_remove = Vec::new();
        for (&ssrc, q) in queues.iter() {
            let st = self.states.entry(ssrc).or_insert_with(|| Playout::new(taps));
            st.speech_hold = st.speech_hold.saturating_sub(1);
            if q.is_empty() {
                st.inactive += 1;
                if st.inactive > VOICE_RESET_INACTIVE_CALLBACKS && st.started {
                    st.reset();
                }
                if st.inactive > 150 {
                    to_remove.push(ssrc);
                    continue;
                }
            } else {
                st.inactive = 0;
            }
            if !st.started && ((ssrc >= STREAM_SSRC_BASE && !q.is_empty()) || q.len() >= VOICE_PREBUFFER_SAMPLES) {
                st.started = true; // 40 ms pre-buffer for members, none for stream audio
            }
            if st.started && (!q.is_empty() || !st.flushed(taps)) {
                self.active.push(ssrc);
                self.diag.insert(ssrc, MixDiag { ready: !q.is_empty(), dry_samples: 0, gain: 1.0 });
            }
        }
        for ssrc in to_remove {
            queues.remove(&ssrc);
            self.states.remove(&ssrc);
        }

        if self.active.is_empty() && self.limiter.idle() {
            return &self.out;
        }

        for &ssrc in &self.active {
            let (muted, vol, prio) = if ssrc >= STREAM_SSRC_BASE { (false, 1.0, 0) } else { settings(ssrc) };
            if let Some(st) = self.states.get_mut(&ssrc) {
                st.muted = muted;
                st.vol = vol;
                st.prio = prio;
            }
        }
        // The highest priority among the members heard speaking a moment ago.
        let top_priority = if ducking {
            self.states.values().filter(|st| !st.muted && st.speech_hold > 0).map(|st| st.prio).max().unwrap_or(0).max(0)
        } else {
            0
        };
        let speech_hold_callbacks = ((DUCK_HOLD_SECONDS * self.out_rate as f64 / frames.max(1) as f64).ceil() as u32).max(2);

        self.mix.clear();
        self.mix.resize(frames, (0.0, 0.0));
        for &ssrc in &self.active {
            let (Some(st), Some(q)) = (self.states.get_mut(&ssrc), queues.get_mut(&ssrc)) else { continue };
            if st.muted {
                // Do not let a muted member's audio pile up and play when unmuted.
                q.clear();
                st.reset();
                continue;
            }
            let duck = if top_priority > 0 && st.prio < top_priority {
                (0.5 - ((top_priority - st.prio) as f32 - 1.0) * 0.1).max(0.05)
            } else {
                1.0
            };
            let target = st.vol * duck;
            if !st.gain_set {
                st.gain = target;
                st.gain_set = true;
            }
            let mut dry = 0usize;
            let mut peak = 0.0f32;
            for m in self.mix.iter_mut() {
                let s = st.next_output(q, self.kernel.as_ref(), self.step, &mut dry);
                peak = peak.max(s.0.abs()).max(s.1.abs());
                if st.gain != target {
                    st.gain += (target - st.gain) * self.gain_coef;
                    if (target - st.gain).abs() < 1e-4 { st.gain = target; }
                }
                m.0 += s.0 * st.gain;
                m.1 += s.1 * st.gain;
            }
            if peak > DUCK_SPEECH_PEAK {
                st.speech_hold = speech_hold_callbacks;
            }
            if let Some(d) = self.diag.get_mut(&ssrc) {
                if d.ready { d.dry_samples = dry; }
                d.gain = st.gain;
            }
        }

        for &m in &self.mix {
            let limited = self.limiter.process(m);
            self.out.push(limited);
        }
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_rs::{Application, OpusEncoder};

    fn tone(freq: f64, amp: f32, n: usize) -> Vec<(f32, f32)> {
        (0..n).map(|i| {
            let v = amp * (2.0 * std::f64::consts::PI * freq * i as f64 / 48000.0).sin() as f32;
            (v, v)
        }).collect()
    }

    /// Amplitude of `freq` in `x` (Hann-windowed correlation).
    fn level(x: &[f32], freq: f64, rate: f64) -> f64 {
        let n = x.len() as f64;
        let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos();
            let a = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
            re += *v as f64 * w * a.cos();
            im += *v as f64 * w * a.sin();
            wsum += w;
        }
        2.0 * (re * re + im * im).sqrt() / wsum
    }

    fn play(mixer: &mut VoiceMixer, input: &[(f32, f32)], frames: usize, callbacks: usize) -> Vec<(f32, f32)> {
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, input.iter().copied().collect());
        let mut out = Vec::new();
        for _ in 0..callbacks {
            let got = mixer.mix(&mut queues, frames, false, |_| (false, 1.0, 0));
            if got.is_empty() { out.extend(std::iter::repeat((0.0, 0.0)).take(frames)); } else { out.extend_from_slice(got); }
        }
        out
    }

    #[test]
    fn at_48_khz_the_samples_go_out_unchanged() {
        let input = tone(1000.0, 0.9, 48000);
        let mut mixer = VoiceMixer::new(48000);
        let delay = mixer.limiter_delay();
        let out = play(&mut mixer, &input, 960, 40);
        // After the 2 ms fade-in at the start, bit for bit.
        assert_eq!(&out[delay + 200..delay + 30000], &input[200..30000]);
    }

    #[test]
    fn at_44100_hz_the_top_is_flat_and_free_of_false_tones() {
        for freq in [6000.0, 12000.0, 15000.0, 18000.0] {
            let input = tone(freq, 0.5, 48000);
            let mut mixer = VoiceMixer::new(44100);
            let out = play(&mut mixer, &input, 896, 45);
            let left: Vec<f32> = out[8000..36000].iter().map(|p| p.0).collect();
            let main = level(&left, freq, 44100.0);
            let image = level(&left, freq - 3900.0, 44100.0);
            assert!((20.0 * (main / 0.5).log10()).abs() < 0.1, "{freq} Hz comes out at {main}");
            assert!(20.0 * (image / main).log10() < -70.0, "{freq} Hz: false tone at {image}");
        }
    }

    #[test]
    fn a_queue_that_runs_dry_fades_out_and_back_in_without_a_jump() {
        let input = tone(200.0, 0.5, 4800);
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, input[..2460].iter().copied().collect());
        let mut mixer = VoiceMixer::new(48000);
        let mut out = Vec::new();
        for cb in 0..8 {
            if cb == 4 { queues.get_mut(&1).unwrap().extend(input[2460..].iter().copied()); }
            let got = mixer.mix(&mut queues, 960, false, |_| (false, 1.0, 0)).to_vec();
            out.extend(if got.is_empty() { vec![(0.0, 0.0); 960] } else { got });
        }
        // A 200 Hz tone at 0.5 moves at most 0.0131 per sample; a held or cut sample would jump far more.
        let biggest = out.windows(2).map(|w| (w[1].0 - w[0].0).abs()).fold(0.0f32, f32::max);
        assert!(biggest < 0.03, "largest step {biggest}");
        assert!(out[2460 + 96 + 400..3840].iter().all(|p| p.0 == 0.0), "silent once the fade is over");
    }

    #[test]
    fn several_loud_members_are_limited_without_exceeding_full_scale() {
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        for (ssrc, freq) in [(1u32, 300.0), (2, 470.0), (3, 810.0)] {
            queues.insert(ssrc, tone(freq, 0.9, 9600).into_iter().collect());
        }
        let mut mixer = VoiceMixer::new(48000);
        let mut out = Vec::new();
        for _ in 0..10 { out.extend_from_slice(mixer.mix(&mut queues, 960, false, |_| (false, 1.0, 0))); }
        let peak = out.iter().map(|p| p.0.abs()).fold(0.0f32, f32::max);
        assert!(peak <= 1.0 && peak > 0.9, "peak {peak}");
    }

    #[test]
    fn ducking_is_off_unless_asked_for_and_ramps_when_on() {
        for (ducking, expected) in [(false, 1.0f32), (true, 0.5)] {
            let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
            queues.insert(1, vec![(0.25, 0.25); 48000].into());
            queues.insert(2, tone(300.0, 0.3, 48000).into_iter().collect());
            let mut mixer = VoiceMixer::new(48000);
            let mut gains = Vec::new();
            for _ in 0..40 {
                mixer.mix(&mut queues, 960, ducking, |ssrc| (false, 1.0, if ssrc == 2 { 1 } else { 0 }));
                gains.push(mixer.diag[&1].gain);
            }
            assert_eq!(gains[0], 1.0);
            assert!((gains[39] - expected).abs() < 0.001, "ducking={ducking}: {gains:?}");
            if ducking {
                assert!(gains[1] < 1.0 && gains[1] > 0.6, "first callback after speech is a ramp, not a step: {}", gains[1]);
            }
        }
    }

    #[test]
    fn a_silent_priority_member_does_not_duck_the_others() {
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, vec![(0.25, 0.25); 19200].into());
        queues.insert(2, vec![(0.0, 0.0); 19200].into());
        let mut mixer = VoiceMixer::new(48000);
        for _ in 0..15 {
            mixer.mix(&mut queues, 960, true, |ssrc| (false, 1.0, if ssrc == 2 { 3 } else { 0 }));
            assert_eq!(mixer.diag[&1].gain, 1.0);
        }
    }

    #[test]
    fn a_muted_member_does_not_pile_up_audio() {
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, vec![(0.25, 0.25); 9600].into());
        let mut mixer = VoiceMixer::new(48000);
        mixer.mix(&mut queues, 960, false, |_| (true, 1.0, 0));
        assert!(queues[&1].is_empty());
    }

    fn encoded_speechlike(packets: usize) -> Vec<Vec<u8>> {
        let mut enc = OpusEncoder::new(48000, 1, Application::Voip).unwrap();
        let mut out = Vec::new();
        for p in 0..packets {
            let pcm: Vec<f32> = (0..960).map(|i| {
                let t = (p * 960 + i) as f64 / 48000.0;
                (0.3 * (2.0 * std::f64::consts::PI * 180.0 * t).sin() + 0.15 * (2.0 * std::f64::consts::PI * 720.0 * t).sin()) as f32
            }).collect();
            let mut buf = vec![0u8; 1000];
            let n = enc.encode(&pcm, 960, &mut buf).unwrap();
            buf.truncate(n);
            out.push(buf);
        }
        out
    }

    #[test]
    fn a_lost_packet_is_concealed_and_keeps_its_20_ms() {
        let packets = encoded_speechlike(30);
        let mut rx = VoiceRxDecoder::new();
        let mut total = 0usize;
        for (i, p) in packets.iter().enumerate() {
            if i == 20 { continue; }
            let f = rx.packet(i as u16, i as u32 * 960, p, i as u64 * 20);
            assert!(f.error.is_none());
            if i == 21 {
                assert_eq!((f.conceal_calls, f.conceal_ok, f.concealed.len()), (1, 1, 960));
            } else {
                assert!(f.concealed.is_empty());
            }
            total += f.concealed.len() + f.decoded.len();
        }
        assert_eq!(total, 30 * 960);
        assert_eq!(rx.stats.concealed, 1);
    }

    #[test]
    fn a_sequence_number_without_audio_is_not_concealed() {
        let packets = encoded_speechlike(12);
        let mut rx = VoiceRxDecoder::new();
        for (i, p) in packets.iter().enumerate() {
            // Sequence number 5 was used by something else: every later one is shifted by
            // one, but the timestamps have no hole.
            let seq = if i >= 5 { i as u16 + 1 } else { i as u16 };
            let f = rx.packet(seq, i as u32 * 960, p, i as u64 * 20);
            assert!(f.concealed.is_empty(), "packet {i}");
        }
    }

    #[test]
    fn a_duplicate_or_late_packet_is_not_played_twice() {
        let packets = encoded_speechlike(12);
        let mut rx = VoiceRxDecoder::new();
        for (i, p) in packets.iter().enumerate().take(10) {
            rx.packet(i as u16, i as u32 * 960, p, i as u64 * 20);
        }
        let again = rx.packet(8, 8 * 960, &packets[8], 200);
        assert!(again.late && again.decoded.is_empty() && again.concealed.is_empty());
        let next = rx.packet(10, 10 * 960, &packets[10], 205);
        assert_eq!((next.decoded.len(), next.concealed.len()), (960, 0));
    }

    #[test]
    fn nothing_is_concealed_across_a_pause() {
        let packets = encoded_speechlike(12);
        let mut rx = VoiceRxDecoder::new();
        rx.packet(0, 0, &packets[0], 0);
        rx.packet(1, 960, &packets[1], 20);
        let f = rx.packet(4, 48000, &packets[2], 1000);
        assert!(f.concealed.is_empty() && f.decoded.len() == 960);
    }

    #[test]
    fn the_silence_frame_is_zeros_and_a_loss_next_to_it_stays_silent() {
        let packets = encoded_speechlike(6);
        let mut rx = VoiceRxDecoder::new();
        for (i, p) in packets.iter().enumerate() {
            rx.packet(i as u16, i as u32 * 960, p, i as u64 * 20);
        }
        let s = rx.packet(6, 6 * 960, &OPUS_SILENCE_FRAME, 120);
        assert!(s.decoded.len() == 960 && s.decoded.iter().all(|p| *p == (0.0, 0.0)));
        let after = rx.packet(8, 8 * 960, &OPUS_SILENCE_FRAME, 160);
        assert!(after.concealed.len() == 960 && after.concealed.iter().all(|p| *p == (0.0, 0.0)));
    }

    // ----- transport encryption -----

    const KEY: [u8; 32] = [
        0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
    ];

    /// A voice packet as the server sends it, built here from the primitives and the
    /// published layout (not with `VoiceCipher::seal`): `cc` CSRCs, an extension of
    /// `ext_words` words if given, `pad` bytes of RTP padding.
    fn server_packet(mode: &str, cc: usize, ext_words: Option<usize>, frame: &[u8], pad: usize, counter: u32) -> Vec<u8> {
        let mut header = vec![0x80 | cc as u8, 0x78, 0x12, 0x34, 0, 0, 0x03, 0xC0, 0, 0, 0x17, 0x5A];
        if pad > 0 { header[0] |= 0x20; }
        for i in 0..cc { header.extend_from_slice(&(0xC0FF_EE00u32 + i as u32).to_be_bytes()); }
        let mut plain = Vec::new();
        if let Some(words) = ext_words {
            header[0] |= 0x10;
            header.extend_from_slice(&[0xBE, 0xDE]);
            header.extend_from_slice(&(words as u16).to_be_bytes());
            plain.extend((0..words * 4).map(|i| 0x30 + i as u8));
        }
        plain.extend_from_slice(frame);
        if pad > 0 {
            plain.extend(std::iter::repeat(0u8).take(pad - 1));
            plain.push(pad as u8);
        }
        let nonce4 = counter.to_be_bytes();
        let sealed = if mode == VOICE_MODE_AES256_GCM {
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&nonce4);
            Aes256Gcm::new_from_slice(&KEY).unwrap()
                .encrypt(aes_gcm::Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &header }).unwrap()
        } else {
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&nonce4);
            XChaCha20Poly1305::new_from_slice(&KEY).unwrap()
                .encrypt(chacha20poly1305::XNonce::from_slice(&nonce), Payload { msg: &plain, aad: &header }).unwrap()
        };
        assert_eq!(sealed.len(), plain.len() + 16);
        let mut pkt = header;
        pkt.extend_from_slice(&sealed);
        pkt.extend_from_slice(&nonce4);
        pkt
    }

    /// A frame as DAVE leaves it: something, then the marker davey looks for at the end.
    fn dave_like_frame(len: usize) -> Vec<u8> {
        let mut f: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let n = f.len();
        f[n - 2] = 0xFA;
        f[n - 1] = 0xFA;
        f
    }

    #[test]
    fn padded_packets_are_decrypted_and_lose_their_padding_in_both_modes() {
        for mode in [VOICE_MODE_AES256_GCM, VOICE_MODE_XCHACHA20] {
            let cipher = VoiceCipher::new(mode, &KEY).unwrap();
            assert_eq!(cipher.mode(), mode);
            let mut counter = 0x0002_314Cu32;
            for cc in [0usize, 2, 15] {
                for ext_words in [None, Some(0usize), Some(1), Some(2)] {
                    for pad in [0usize, 1, 2, 4, 37, 255] {
                        for frame_len in [3usize, 61, 118] {
                            counter += 1;
                            let frame = dave_like_frame(frame_len);
                            let pkt = server_packet(mode, cc, ext_words, &frame, pad, counter);
                            let got = cipher.open(&pkt);
                            assert_eq!(got.as_deref(), Ok(&frame[..]), "{mode} cc={cc} ext={ext_words:?} pad={pad} frame={frame_len}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_packet_shaped_like_the_ones_that_failed_live_is_decrypted() {
        // Live: first byte B0 (padding and extension bits), Opus, a 2-word BEDE extension,
        // 154 bytes. 154 = 16 header + 8 extension body + 110 payload and padding + 16 + 4.
        let cipher = VoiceCipher::new(VOICE_MODE_AES256_GCM, &KEY).unwrap();
        let frame = dave_like_frame(97);
        let pkt = server_packet(VOICE_MODE_AES256_GCM, 0, Some(2), &frame, 13, 0x4C31_0200);
        assert_eq!(pkt.len(), 154);
        assert_eq!((pkt[0], pkt[1], &pkt[12..16], &pkt[150..]), (0xB0, 0x78, &[0xBE, 0xDE, 0x00, 0x02][..], &[0x4C, 0x31, 0x02, 0x00][..]));
        let payload = cipher.open(&pkt).unwrap();
        assert_eq!(payload, frame);
        assert!(payload.ends_with(&[0xFA, 0xFA]), "DAVE finds its marker at the end");
        // The slicing used before read the last tag byte as a padding count.
        assert_eq!(pkt[154 - 5], pkt[149]);
    }

    #[test]
    fn a_padding_only_packet_gives_an_empty_payload() {
        for mode in [VOICE_MODE_AES256_GCM, VOICE_MODE_XCHACHA20] {
            let cipher = VoiceCipher::new(mode, &KEY).unwrap();
            for ext_words in [None, Some(2usize)] {
                let pkt = server_packet(mode, 0, ext_words, &[], 20, 9);
                assert_eq!(cipher.open(&pkt), Ok(Vec::new()), "{mode} ext={ext_words:?}");
            }
        }
    }

    #[test]
    fn wrong_or_damaged_packets_are_refused_not_played() {
        for mode in [VOICE_MODE_AES256_GCM, VOICE_MODE_XCHACHA20] {
            let cipher = VoiceCipher::new(mode, &KEY).unwrap();
            let frame = dave_like_frame(40);
            let pkt = server_packet(mode, 0, Some(2), &frame, 4, 77);
            // Any changed byte: header, extension preamble, ciphertext, tag or nonce.
            for at in [0usize, 3, 13, 15, 16, 30, pkt.len() - 6, pkt.len() - 1] {
                let mut bad = pkt.clone();
                bad[at] ^= 0x01;
                assert!(cipher.open(&bad).is_err(), "{mode}: byte {at} changed");
            }
            // The other mode, another key.
            let other = if mode == VOICE_MODE_AES256_GCM { VOICE_MODE_XCHACHA20 } else { VOICE_MODE_AES256_GCM };
            assert_eq!(VoiceCipher::new(other, &KEY).unwrap().open(&pkt), Err(RtpOpenError::Auth));
            assert_eq!(VoiceCipher::new(mode, &[7u8; 32]).unwrap().open(&pkt), Err(RtpOpenError::Auth));
            // Too short for header, tag and nonce (every length up to the minimum).
            for len in 0..36 {
                assert_eq!(cipher.open(&pkt[..len]), Err(RtpOpenError::Malformed), "{mode}: {len} bytes");
            }
            assert_eq!(cipher.open(&pkt[..36]), Err(RtpOpenError::Auth));
            // An authentic packet with an impossible padding count, or an extension longer
            // than the packet: dropped. (Built with the padding bit and a last byte of
            // 0, and of more than there is.)
            for last in [0u8, 200] {
                let mut plain = vec![0x55u8; 8];
                plain.extend_from_slice(&[1, 2, 3, last]);
                let mut header = vec![0xB0, 0x78, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9, 0xBE, 0xDE, 0, 2];
                let sealed = cipher.seal(&header, &plain, 5).unwrap();
                assert_eq!(cipher.open(&sealed), Err(RtpOpenError::BadPadding), "{mode}: padding count {last}");
                header[0] = 0x90;
                header[15] = 9;
                let sealed = cipher.seal(&header, &plain, 6).unwrap();
                assert_eq!(cipher.open(&sealed), Err(RtpOpenError::Malformed), "{mode}: extension of 36 bytes in 12");
            }
        }
        assert!(VoiceCipher::new("xsalsa20_poly1305", &KEY).is_none());
        assert!(VoiceCipher::new(VOICE_MODE_AES256_GCM, &KEY[..16]).is_none());
    }

    #[test]
    fn sent_packets_keep_the_aes_layout_and_the_other_mode_round_trips() {
        let header = [0x80, 0x78, 0x00, 0x2A, 0x00, 0x00, 0x9D, 0x80, 0x00, 0x01, 0xE2, 0x40];
        let opus = [0xF8u8, 0xFF, 0xFE];
        // What the send loop did before it used VoiceCipher.
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&42u32.to_be_bytes());
        let mut expected = header.to_vec();
        expected.extend(Aes256Gcm::new_from_slice(&KEY).unwrap()
            .encrypt(aes_gcm::Nonce::from_slice(&nonce), Payload { msg: &opus, aad: &header }).unwrap());
        expected.extend_from_slice(&42u32.to_be_bytes());
        let aes = VoiceCipher::new(VOICE_MODE_AES256_GCM, &KEY).unwrap();
        assert_eq!(aes.seal(&header, &opus, 42).unwrap(), expected);
        assert_eq!(aes.open(&expected).unwrap(), opus);

        let xchacha = VoiceCipher::new(VOICE_MODE_XCHACHA20, &KEY).unwrap();
        let sent = xchacha.seal(&header, &opus, 42).unwrap();
        assert_eq!(sent.len(), 12 + 3 + 16 + 4);
        assert_eq!((&sent[..12], &sent[31..]), (&header[..], &42u32.to_be_bytes()[..]));
        assert_ne!(sent, expected);
        assert_eq!(xchacha.open(&sent).unwrap(), opus);
        // Same key, nonce and payload as a packet built from the primitives: same ciphertext.
        assert_eq!(server_packet(VOICE_MODE_XCHACHA20, 0, None, &opus, 0, 42)[12..15], sent[12..15]);
    }

    #[test]
    fn the_mode_is_chosen_by_name_not_by_position() {
        assert_eq!(select_voice_mode(["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize"]), Some(VOICE_MODE_AES256_GCM));
        assert_eq!(select_voice_mode(["xsalsa20_poly1305_lite_rtpsize", "aead_xchacha20_poly1305_rtpsize", "xsalsa20_poly1305"]), Some(VOICE_MODE_XCHACHA20));
        assert_eq!(select_voice_mode(["xsalsa20_poly1305"]), None);
        assert_eq!(select_voice_mode([]), None);
    }

    // ----- decoder robustness -----

    #[test]
    fn garbage_payloads_do_not_take_the_decoder_down() {
        let real = encoded_speechlike(40);
        let mut rx = VoiceRxDecoder::new();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || { state ^= state << 13; state ^= state >> 7; state ^= state << 17; state };
        let (mut seq, mut pairs, mut now, mut refused) = (0u16, 0usize, 0u64, 0usize);
        for i in 0..200_000usize {
            let len = 60 + (rand() % 61) as usize;
            let garbage: Vec<u8> = (0..len).map(|_| rand() as u8).collect();
            let panics_before = rx.stats.decoder_panics;
            let f = rx.packet(seq, seq as u32 * 960, &garbage, now);
            assert!(f.concealed.iter().chain(&f.decoded).all(|p| p.0.is_finite() && p.1.is_finite() && p.0.abs() <= 1.0 && p.1.abs() <= 1.0));
            pairs += f.concealed.len() + f.decoded.len();
            if f.error.is_some_and(|e| e.starts_with("not decoded")) { refused += 1; }
            seq = seq.wrapping_add(1);
            now += 20;
            if rx.stats.decoder_panics > panics_before && rx.stats.decoder_panics >= 3 {
                // From the third panic on the member is left alone for two seconds...
                for p in &real[..5] {
                    let f = rx.packet(seq, seq as u32 * 960, p, now);
                    assert!(f.error.is_some() && f.decoded.is_empty());
                    seq = seq.wrapping_add(1);
                    now += 20;
                }
                now += PANIC_TIMEOUT_MS;
            }
            if i % 1000 == 999 {
                // ...and real packets decode again afterwards.
                now += PANIC_TIMEOUT_MS;
                for p in &real[..5] {
                    let f = rx.packet(seq, seq as u32 * 960, p, now);
                    assert!(f.error.is_none() && f.decoded.len() == 960, "after {i} garbage packets: {:?}", f.error);
                    seq = seq.wrapping_add(1);
                    now += 20;
                }
            }
        }
        println!("garbage: 200000 packets, decoder_panics={} decode_errors={} refused_after_panics={refused} pairs={pairs}", rx.stats.decoder_panics, rx.stats.decode_errors);
        assert!(rx.stats.decoder_panics > 0, "the test no longer reaches the panic it guards against");
    }

    #[test]
    fn an_undecodable_packet_keeps_its_20_ms() {
        let packets = encoded_speechlike(12);
        let mut rx = VoiceRxDecoder::new();
        let mut total = 0usize;
        for (i, p) in packets.iter().enumerate() {
            let bad = [0x79u8, 1, 2, 3];
            let f = rx.packet(i as u16, i as u32 * 960, if i == 6 { &bad[..] } else { &p[..] }, i as u64 * 20);
            assert_eq!(f.error.is_some(), i == 6, "packet {i}: {:?}", f.error);
            total += f.concealed.len() + f.decoded.len();
        }
        assert_eq!(total, 12 * 960);
        assert_eq!(rx.stats.decode_errors, 1);
    }

    #[test]
    fn a_sender_that_restarts_its_numbering_is_played_not_dropped() {
        let packets = encoded_speechlike(30);
        for restart_at in [5u16, 40000] {
            let mut rx = VoiceRxDecoder::new();
            let mut total = 0usize;
            for (i, p) in packets.iter().enumerate() {
                let seq = if i < 10 { 1000 + i as u16 } else { restart_at.wrapping_add(i as u16 - 10) };
                let f = rx.packet(seq, i as u32 * 960, p, i as u64 * 20);
                assert!(!f.late && f.concealed.is_empty(), "packet {i}");
                total += f.decoded.len();
            }
            assert_eq!(total, 30 * 960);
            assert_eq!((rx.stats.seq_jumps, rx.stats.late_dropped), (1, 0));
        }
    }

    #[test]
    fn a_hole_that_already_played_is_concealed_with_one_frame_only() {
        let packets = encoded_speechlike(30);
        for (queued, lost, expected) in [(None, 3usize, 3usize), (Some(480), 3, 3), (Some(0), 3, 1), (Some(0), 1, 1), (Some(0), 4, 1)] {
            let mut rx = VoiceRxDecoder::new();
            for (i, p) in packets.iter().enumerate() {
                if (10..10 + lost).contains(&i) { continue; }
                let f = rx.packet_for_playout(i as u16, i as u32 * 960, p, i as u64 * 20, queued);
                let want = if i == 10 + lost { expected * 960 } else { 0 };
                assert_eq!(f.concealed.len(), want, "queued={queued:?} lost={lost} packet {i}");
            }
            assert_eq!(rx.stats.conceal_skipped as usize, lost - expected);
        }
        // A run of four whose next packet is a little late is still concealed.
        let mut rx = VoiceRxDecoder::new();
        for (i, p) in packets.iter().enumerate().take(10) { rx.packet(i as u16, i as u32 * 960, p, i as u64 * 20); }
        let f = rx.packet(14, 14 * 960, &packets[14], 9 * 20 + 100 + 25);
        assert_eq!(f.concealed.len(), 4 * 960);
    }

    /// Queue depth before each 10 ms callback with packets every 20 ms, runs of `burst`
    /// lost packets, the decoder told (or not) how much is queued.
    fn median_queue_with_bursts(burst: usize, tell_queue: bool) -> usize {
        let packets = encoded_speechlike(400);
        let lost = |i: usize| i >= 50 && (i - 50) % 30 < burst;
        let mut rx = VoiceRxDecoder::new();
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        let mut mixer = VoiceMixer::new(48000);
        let mut depths = Vec::new();
        for tick in 0..800usize {
            if tick % 2 == 0 && !lost(tick / 2) {
                let i = tick / 2;
                let queued = queues.get(&1).map_or(0, |q| q.len());
                let f = rx.packet_for_playout(i as u16, i as u32 * 960, &packets[i], i as u64 * 20, tell_queue.then_some(queued));
                push_voice_frame(queues.entry(1).or_default(), &f.concealed, &f.decoded);
            }
            if tick > 200 { depths.push(queues.get(&1).map_or(0, |q| q.len())); }
            mixer.mix(&mut queues, 480, false, |_| (false, 1.0, 0));
        }
        depths.sort_unstable();
        depths[depths.len() / 2]
    }

    #[test]
    fn runs_of_lost_packets_do_not_leave_the_member_later() {
        let steady = median_queue_with_bursts(0, true);
        for burst in 1..=4 {
            let told = median_queue_with_bursts(burst, true);
            let untold = median_queue_with_bursts(burst, false);
            println!("burst {burst}: median queue {told} pairs (was {untold}); no loss {steady}");
            assert!(told <= steady + 480, "burst {burst}: {told} against {steady}");
        }
        assert!(median_queue_with_bursts(3, false) >= steady + 960, "the test would notice the old behaviour");
    }

    fn encoded_celt(packets: usize) -> Vec<Vec<u8>> {
        let mut enc = OpusEncoder::new(48000, 1, Application::RestrictedLowDelay).unwrap();
        let mut out = Vec::new();
        for p in 0..packets {
            let pcm: Vec<f32> = (0..960).map(|i| {
                let t = (p * 960 + i) as f64 / 48000.0;
                (0.3 * (2.0 * std::f64::consts::PI * 180.0 * t).sin() + 0.15 * (2.0 * std::f64::consts::PI * 720.0 * t).sin()) as f32
            }).collect();
            let mut buf = vec![0u8; 1000];
            let n = enc.encode(&pcm, 960, &mut buf).unwrap();
            buf.truncate(n);
            assert!(buf[0] & 0x80 != 0, "CELT-only packet expected, first byte {:02X}", buf[0]);
            out.push(buf);
        }
        out
    }

    fn rms(x: &[(f32, f32)]) -> f32 {
        (x.iter().map(|p| p.0 * p.0).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_lost_celt_packet_is_extrapolated_not_cut_to_zeros() {
        let packets = encoded_celt(40);
        let mut rx = VoiceRxDecoder::new();
        let mut out = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            if i == 20 || i == 30 || i == 31 { continue; }
            let f = rx.packet(i as u16, i as u32 * 960, p, i as u64 * 20);
            if i == 21 {
                assert_eq!((f.conceal_ok, f.concealed.len()), (1, 960));
                let before = rms(&out[out.len() - 960..]);
                println!("celt: rms before {before}, first half of the made-up frame {}, second half {}", rms(&f.concealed[..480]), rms(&f.concealed[480..]));
                assert!(rms(&f.concealed[..480]) > 0.5 * before, "the lost frame starts as audio, not as silence");
                assert!(f.concealed[959].0.abs() < 0.01, "and ends faded out");
            }
            if i == 32 {
                assert_eq!((f.conceal_calls, f.conceal_ok, f.concealed.len()), (2, 1, 1920));
                assert!(f.concealed[960..].iter().all(|p| *p == (0.0, 0.0)));
            }
            out.extend_from_slice(&f.concealed);
            out.extend_from_slice(&f.decoded);
        }
        assert_eq!(out.len(), 40 * 960);
        assert_eq!((rx.stats.concealed, rx.stats.concealed_as_silence), (2, 1));
        // 180 Hz + 720 Hz at 0.3 + 0.15 moves at most about 0.022 per sample.
        let (mut biggest, mut at) = (0.0f32, 0);
        for i in 12 * 960..out.len() - 1 {
            let step = (out[i + 1].0 - out[i].0).abs();
            if step > biggest { biggest = step; at = i; }
        }
        println!("celt: largest step {biggest} at pair {at} (frame {})", at / 960);
        assert!(biggest < 0.08, "largest step {biggest} at pair {at}");
    }

    // ----- mixer -----

    #[test]
    fn the_limiter_lets_go_completely_after_an_overload() {
        let mut input = tone(1000.0, 0.3, 48000 * 4);
        for s in input[4800..5040].iter_mut() { *s = (s.0 * 10.0, s.1 * 10.0); }
        let mut mixer = VoiceMixer::new(48000);
        let delay = mixer.limiter_delay();
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, input.iter().copied().collect());
        let mut out = Vec::new();
        let mut idle_callbacks = 0;
        for _ in 0..400 {
            let got = mixer.mix(&mut queues, 960, false, |_| (false, 1.0, 0));
            if got.is_empty() { idle_callbacks += 1; }
            if out.len() < input.len() { out.extend_from_slice(got); }
        }
        assert!(out[delay + 4800..delay + 5040].iter().all(|p| p.0.abs() <= 1.0));
        // Two seconds after the burst: bit for bit again.
        let from = 5040 + 2 * 48000;
        assert_eq!(&out[delay + from..delay + from + 48000], &input[from..from + 48000]);
        // 200 callbacks of audio, then nothing to play: the mixer goes idle.
        assert!(idle_callbacks >= 190, "idle callbacks: {idle_callbacks}");
    }

    #[test]
    fn a_new_stream_starts_from_the_newest_audio() {
        // A new output stream finds 0.6 s waiting: it starts 40 ms from the newest.
        let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
        queues.insert(1, tone(300.0, 0.5, 28800).into_iter().collect());
        queues.insert(2, tone(300.0, 0.5, 700).into_iter().collect());
        let newest = queues[&1][28799];
        VoiceMixer::drop_backlog(&mut queues);
        assert_eq!((queues[&1].len(), queues[&2].len(), queues[&1][VOICE_PREBUFFER_SAMPLES - 1]), (VOICE_PREBUFFER_SAMPLES, 700, newest));
    }

    #[test]
    fn a_sample_that_is_not_a_number_becomes_silence() {
        assert_eq!(decoded_voice_pairs(&[0.5, f32::NAN, f32::INFINITY, -2.0], 1), vec![(0.5, 0.5), (0.0, 0.0), (0.0, 0.0), (-1.0, -1.0)]);
        assert_eq!(decoded_voice_pairs(&[f32::NAN, 0.25, f32::NEG_INFINITY, 3.0], 2), vec![(0.0, 0.25), (0.0, 1.0)]);
    }
}
