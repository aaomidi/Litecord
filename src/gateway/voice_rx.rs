//! Receive side of a voice call, without any device or socket: Opus packets in, queued
//! 48 kHz stereo pairs out (`VoiceRxDecoder`), and queued pairs in, mixed output frames
//! out (`VoiceMixer`). The network loop and both output callbacks of `voice.rs` use these,
//! and so does the offline harness (`src/bin/voice_rx_harness.rs`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    if channels == 2 {
        pcm.chunks_exact(2).map(|s| (s[0].clamp(-1.0, 1.0), s[1].clamp(-1.0, 1.0))).collect()
    } else {
        pcm.iter().map(|s| { let mono = s.clamp(-1.0, 1.0); (mono, mono) }).collect()
    }
}

pub fn push_voice_frame(q: &mut VecDeque<(f32, f32)>, plc: &[(f32, f32)], decoded: &[(f32, f32)]) {
    let incoming = plc.len().saturating_add(decoded.len());
    let discard = q.len().saturating_add(incoming).saturating_sub(VOICE_QUEUE_LIMIT);
    q.drain(..discard.min(q.len()));
    q.extend(plc.iter().chain(decoded).skip(incoming.saturating_sub(VOICE_QUEUE_LIMIT)).copied());
}

// ---------------------------------------------------------------------------------------
// Decoding and loss concealment
// ---------------------------------------------------------------------------------------

/// A pause longer than this starts a new talkspurt: nothing is concealed across it.
const TALKSPURT_GAP_MS: u64 = 100;
/// Longest run of lost packets that is concealed.
const MAX_CONCEALED_PACKETS: u16 = 4;
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
    pub concealed: u64,
    pub conceal_failed: u64,
    /// Duplicates and packets that arrived after their place had been concealed.
    pub late_dropped: u64,
    pub decode_errors: u64,
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

/// Decoder state of one remote member (one SSRC).
pub struct VoiceRxDecoder {
    // opus-rs rejects a packet whose channel flag differs from the decoder's.
    decoders: HashMap<usize, OpusDecoder>,
    last: Option<LastPacket>,
    pcm: Vec<f32>,
    pub stats: RxStats,
}

impl Default for VoiceRxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceRxDecoder {
    pub fn new() -> Self {
        Self { decoders: HashMap::new(), last: None, pcm: vec![0.0; 11520], stats: RxStats::default() }
    }

    /// One Opus payload (already decrypted) with its RTP sequence number and timestamp.
    /// `now_ms` is any monotonic clock.
    pub fn packet(&mut self, seq: u16, timestamp: u32, opus: &[u8], now_ms: u64) -> RxFrame {
        let mut frame = RxFrame::default();
        if opus.is_empty() {
            return frame;
        }
        let same_talkspurt = self.last.as_ref().is_some_and(|l| now_ms.saturating_sub(l.at_ms) <= TALKSPURT_GAP_MS);
        if same_talkspurt {
            let (missed, by_time) = {
                let last = self.last.as_ref().unwrap();
                let missed = seq.wrapping_sub(last.next_seq);
                // A sequence number that carried no audio (a probe or padding packet)
                // leaves no hole in the timestamps: nothing to conceal for it.
                let frames = timestamp.wrapping_sub(last.timestamp) as usize / last.samples.max(1);
                (missed, frames.saturating_sub(1))
            };
            if missed >= 0x8000 {
                // Its place was already filled (concealed, or it is a duplicate).
                self.stats.late_dropped += 1;
                frame.late = true;
                return frame;
            }
            if missed > 0 && missed <= MAX_CONCEALED_PACKETS {
                self.conceal((missed as usize).min(by_time), &mut frame);
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
        } else {
            let decoded = match self.decoders.get_mut(&channels) {
                Some(dec) => dec.decode(opus, 5760, &mut self.pcm[..]),
                None => match OpusDecoder::new(48000, channels) {
                    Ok(mut dec) => {
                        let res = dec.decode(opus, 5760, &mut self.pcm[..]);
                        self.decoders.insert(channels, dec);
                        res
                    }
                    Err(e) => Err(e),
                },
            };
            match decoded {
                Ok(n) => {
                    samples = n;
                    frame.decoded = decoded_voice_pairs(&self.pcm[..n * channels], channels);
                }
                Err(e) => {
                    self.stats.decode_errors += 1;
                    frame.error = Some(e);
                }
            }
        }

        let next_seq = seq.wrapping_add(1);
        if samples > 0 {
            self.last = Some(LastPacket { next_seq, timestamp, at_ms: now_ms, toc, channels, samples, silence });
        } else if let Some(last) = self.last.as_mut() {
            // Not decodable: keep what the concealment needs from the last good packet.
            last.next_seq = next_seq;
            last.at_ms = now_ms;
        }
        frame
    }

    fn conceal(&mut self, packets: usize, frame: &mut RxFrame) {
        let Some(last) = self.last.as_ref() else { return };
        for _ in 0..packets {
            frame.conceal_calls += 1;
            let before = frame.concealed.len();
            if !last.silence {
                // A packet that is only its first byte is "frame lost" to the decoder, which
                // then runs its own concealment from the previous frames. (An empty packet,
                // which this code used to pass, is rejected by opus-rs.)
                if let Some(dec) = self.decoders.get_mut(&last.channels) {
                    if let Ok(n) = dec.decode(&[last.toc & 0xFC], 5760, &mut self.pcm[..]) {
                        frame.concealed.extend(decoded_voice_pairs(&self.pcm[..n * last.channels], last.channels));
                    }
                }
            }
            if frame.concealed.len() > before || last.silence {
                frame.conceal_ok += 1;
                self.stats.concealed += 1;
            } else {
                self.stats.conceal_failed += 1;
            }
            // Whatever happened, the lost packet keeps its place in time.
            let have = frame.concealed.len() - before;
            frame.concealed.extend(std::iter::repeat((0.0, 0.0)).take(last.samples.saturating_sub(have)));
        }
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
        if env > 0.99999 { env = 1.0; }
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
}
