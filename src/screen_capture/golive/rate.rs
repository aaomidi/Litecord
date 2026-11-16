//! How much a Discord stream puts on the wire, and when.
//!
//! - `Budget`: the wire rate allowed right now (what opcode 12 announced, held down by the
//!   media server's REMB estimate), and what to ask of the share's encoder so that the
//!   packets really sent (RTP headers, the extension, the tag, retransmissions, audio and
//!   keyframes included) stay under it. What is asked moves in steps: the fork's GPU
//!   encoders start a new sequence, a keyframe, with every new bitrate.
//! - `Pacer`: a keyframe leaves as a stream of packets at a few times the budget, not as
//!   one burst the path between the media server and a viewer has to swallow.
//! - `KeyframeGate`: all viewers together get at most one keyframe a second.
//! - `EvenClock`: video timestamps one frame interval apart for a source that keeps the
//!   share's pace but not its rhythm; any other source keeps the clock's
//!   (`LITECORD_GOLIVE_TS=even`, the default).
//! - `AudioClock`: the audio timestamp that belongs to a wall-clock instant, for sender
//!   reports that do not jump about with the moment a chunk happened to arrive.
//!
//! Nothing here touches a socket or a clock of its own: every method takes the time.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The budget stays this far below the server's estimate.
const REMB_HEADROOM: f64 = 0.90;
/// An estimate older than this no longer holds the budget down.
const REMB_AGE: Duration = Duration::from_secs(10);
/// No estimate takes the stream below this: a 1080p stream below it is not worth sending,
/// and an estimate can only rise again while something is sent.
pub const MIN_WIRE: u32 = 500_000;
/// The budget follows an estimate down once it has stayed below the budget this long: the
/// media server's estimate jumps about (1.9, 6.8, 1.9 and 6.9 Mbit/s in four lines of the
/// third live run, without a lost packet), and every cut costs a keyframe and then seconds
/// at the lower rate.
const CUT_AFTER: Duration = Duration::from_secs(1);
/// Up, the budget goes by this share of itself a second (plus `RISE_STEP`, so that a low
/// budget moves too).
const RISE_PER_SECOND: f64 = 0.10;
const RISE_STEP: f64 = 50_000.0;
/// What stream audio takes of the budget when the share has audio.
const AUDIO_ALLOWANCE: f64 = 100_000.0;
/// The encoder is asked for this share of the video's budget at first: packet headers, the
/// extension and the authentication tag are about 7 % of a full packet, and an encoder
/// overshoots a little.
const ENCODER_SHARE: f64 = 0.88;
const ENCODER_SHARE_MIN: f64 = 0.60;
/// What is asked of the encoder goes down at once when the budget calls for this much less...
const ASK_CUT: f64 = 0.06;
/// ...and up when it allows this much more (or everything again), but not sooner than
/// `ASK_HOLD` after the last change: each change is a keyframe on the GPU encoders, and an
/// estimate that wobbles must not become a keyframe every few seconds.
const ASK_RISE: f64 = 0.15;
const ASK_HOLD: Duration = Duration::from_secs(8);
/// Seconds of what was sent that the share is corrected by.
const MEASURED_SECONDS: usize = 4;
/// After the budget was cut the encoder needs a moment: what is sent meanwhile is not
/// held against it.
const SETTLE_AFTER_CUT: Duration = Duration::from_secs(2);

/// The wire rate a stream is allowed, and the encoder rate that keeps it.
pub struct Budget {
    announced: u32,
    follow_remb: bool,
    audio: bool,
    remb: Option<(u64, Instant)>,
    /// Since when the estimate calls for less than the budget.
    low_since: Option<Instant>,
    wire: f64,
    share: f64,
    updated: Instant,
    /// The running second: when it began, the bits sent in it and the budget's integral.
    second: Instant,
    sent: f64,
    allowed: f64,
    /// Finished seconds: (bits sent, bits allowed).
    seconds: VecDeque<(f64, f64)>,
    settle_until: Option<Instant>,
    /// What the encoder is asked for, and since when.
    asked: u32,
    asked_at: Instant,
    /// Seconds in a row with the wire well under the budget (for the share's way back up).
    under: u32,
    /// The share was lowered because more went out than was allowed: the encoder is asked
    /// for less at once, however small the step.
    corrected: bool,
    /// The last whole window had more on the wire than was allowed.
    over: bool,
}

/// What `Budget::ask` did to the rate asked of the encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Same,
    /// Less than before: the encoder should take it at once.
    Cut,
    Raised,
}

impl Budget {
    /// `announced`: the highest bitrate opcode 12 names. `follow_remb`: whether the media
    /// server's estimate holds the budget down. `audio`: the share has stream audio.
    pub fn new(announced: u32, follow_remb: bool, audio: bool, now: Instant) -> Self {
        let announced = announced.max(MIN_WIRE);
        let mut budget =
            Self { announced, follow_remb, audio, remb: None, low_since: None, wire: announced as f64, share: ENCODER_SHARE, updated: now, second: now, sent: 0.0, allowed: 0.0, seconds: VecDeque::new(), settle_until: None, asked: 0, asked_at: now, under: 0, corrected: false, over: false };
        budget.asked = budget.encoder();
        budget
    }

    /// A REMB packet: the media server's estimate of what it can take, in bits per second.
    pub fn on_remb(&mut self, bitrate: u64, now: Instant) {
        self.remb = Some((bitrate, now));
        self.update(now);
    }

    /// A datagram left: its size on the wire without the IP and UDP headers.
    pub fn on_sent(&mut self, bytes: usize) {
        self.sent += bytes as f64 * 8.0;
    }

    fn target(&self, now: Instant) -> f64 {
        let estimate = self.remb.filter(|(_, at)| self.follow_remb && now.saturating_duration_since(*at) < REMB_AGE).map(|(bitrate, _)| bitrate as f64 * REMB_HEADROOM);
        estimate.unwrap_or(f64::MAX).min(self.announced as f64).max((MIN_WIRE as f64).min(self.announced as f64))
    }

    /// Moves the budget to where it belongs at `now`. Call it often (every turn of the worker).
    pub fn update(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.updated).as_secs_f64();
        let target = self.target(now);
        let before = self.wire;
        if target < self.wire {
            let since = *self.low_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= CUT_AFTER {
                self.wire = target;
                self.low_since = None;
            }
        } else {
            self.low_since = None;
            self.wire = target.min(self.wire * (1.0 + RISE_PER_SECOND * dt) + RISE_STEP * dt);
        }
        // (The integral with the budget as it was since the last call.)
        self.allowed += before * dt;
        self.updated = now;
        if self.wire < before * 0.9 {
            self.settle_until = Some(now + SETTLE_AFTER_CUT);
        }
        while now.saturating_duration_since(self.second) >= Duration::from_secs(1) {
            // (A long gap between two calls: one second takes what was counted, the rest are empty.)
            let (sent, allowed) = (std::mem::take(&mut self.sent), std::mem::take(&mut self.allowed));
            self.second += Duration::from_secs(1);
            if now.saturating_duration_since(self.second) >= Duration::from_secs(MEASURED_SECONDS as u64) {
                self.second = now;
                self.seconds.clear();
                break;
            }
            if self.settle_until.is_some_and(|until| now < until) {
                self.seconds.clear();
                continue;
            }
            self.settle_until = None;
            self.seconds.push_back((sent, allowed.max(1.0)));
            if self.seconds.len() > MEASURED_SECONDS {
                self.seconds.pop_front();
            }
            // Judged over whole windows only: one keyframe makes one second look over.
            if self.seconds.len() < MEASURED_SECONDS {
                continue;
            }
            let (sent, allowed) = self.seconds.iter().fold((0.0, 0.0), |sum, s| (sum.0 + s.0, sum.1 + s.1));
            let ratio = sent / allowed;
            if ratio > 1.01 {
                // More went out than was allowed. The first window that shows it may be
                // half from before, or hold a run of keyframes that is over by now: the
                // next one decides, and says by how much.
                if std::mem::replace(&mut self.over, true) {
                    // An encoder that gives more than it is asked for: ask for that much
                    // less. The share in force while this was measured is the one last
                    // asked of the encoder.
                    let in_force = (self.asked as f64 / self.video()).clamp(ENCODER_SHARE_MIN, ENCODER_SHARE);
                    self.share = (in_force / (ratio * 1.01)).max(ENCODER_SHARE_MIN);
                    self.corrected = true;
                    self.over = false;
                }
                self.seconds.clear();
                self.under = 0;
                continue;
            }
            self.over = false;
            if ratio < 0.97 {
                // Back up slowly, a point every ten seconds: what made the wire overshoot
                // is likely to come again.
                self.under += 1;
                if self.under >= 10 {
                    self.under = 0;
                    self.share = (self.share + 0.01).min(ENCODER_SHARE);
                }
            } else {
                self.under = 0;
            }
        }
    }

    /// The part of the budget that is the video's.
    fn video(&self) -> f64 {
        (self.wire - if self.audio { AUDIO_ALLOWANCE } else { 0.0 }).max(MIN_WIRE as f64 / 2.0)
    }

    /// What to ask of the share's encoder now: `encoder()` taken in steps, because the
    /// fork's GPU encoders start a new sequence (a keyframe) with every new bitrate. Down
    /// at once when the budget calls for clearly less (or the wire was over its budget);
    /// up only after a while, and only for a step worth a keyframe or to everything that
    /// can be asked.
    pub fn ask(&mut self, now: Instant) -> (u32, Ask) {
        let want = self.encoder();
        let corrected = std::mem::take(&mut self.corrected);
        if want < self.asked && (corrected || (want as f64) <= self.asked as f64 * (1.0 - ASK_CUT)) {
            (self.asked, self.asked_at) = (want, now);
            // (The encoder needs a moment; what it sends meanwhile is not held against it.)
            self.settle_until = Some(now + SETTLE_AFTER_CUT);
            self.seconds.clear();
            return (want, Ask::Cut);
        }
        let top = ((self.announced as f64 - if self.audio { AUDIO_ALLOWANCE } else { 0.0 }).max(MIN_WIRE as f64 / 2.0) * ENCODER_SHARE).round() as u32;
        let held = now.saturating_duration_since(self.asked_at) >= ASK_HOLD;
        if held && want > self.asked && (want as f64 >= self.asked as f64 * (1.0 + ASK_RISE) || want >= top) {
            (self.asked, self.asked_at) = (want, now);
            return (want, Ask::Raised);
        }
        (self.asked, Ask::Same)
    }

    /// What the encoder is asked for (the last answer of `ask`).
    pub fn asked(&self) -> u32 {
        self.asked
    }

    /// Nothing is being sent (nobody watches): what was measured is void.
    pub fn idle(&mut self, now: Instant) {
        self.update(now);
        self.seconds.clear();
        self.sent = 0.0;
        self.allowed = 0.0;
        self.second = now;
        self.under = 0;
        self.over = false;
    }

    /// Bits per second the stream may put on the wire now.
    pub fn wire(&self) -> u32 {
        self.wire.round() as u32
    }

    /// Bits per second to ask of the share's encoder.
    pub fn encoder(&self) -> u32 {
        (self.video() * self.share).round() as u32
    }

    /// The share of the video budget the encoder is asked for, in percent (for the log).
    pub fn encoder_share(&self) -> u32 {
        (self.share * 100.0).round() as u32
    }

    /// The server's last estimate, if it is recent enough to count.
    pub fn estimate(&self, now: Instant) -> Option<u64> {
        self.remb.filter(|(_, at)| now.saturating_duration_since(*at) < REMB_AGE).map(|(bitrate, _)| bitrate)
    }
}

/// The slowest the pacer goes, whatever the budget: a keyframe must not take forever.
const PACE_FLOOR: f64 = 3_000_000.0;
/// What may leave at once after a pause: this much time's worth, at least `PACE_BURST_MIN`.
const PACE_BURST_TIME: f64 = 0.005;
const PACE_BURST_MIN: f64 = 3.0 * 1240.0;
/// Whatever the budget, what is queued leaves within this time: a keyframe at a low budget
/// must not wait until it is too old to be sent.
const PACE_DRAIN_WITHIN: f64 = 0.15;

/// A token bucket in bytes: packets leave at `factor` times the budget.
pub struct Pacer {
    /// Tenths: 25 is two and a half times the budget. 0: the fixed pace of the first
    /// version, 12 full packets per millisecond with up to 48 at once.
    factor: u32,
    tokens: f64,
    at: Instant,
    rate: f64,
}

impl Pacer {
    pub fn new(factor: u32, now: Instant) -> Self {
        let mut pacer = Self { factor, tokens: 0.0, at: now, rate: 0.0 };
        pacer.set_budget(8_000_000);
        pacer.tokens = pacer.bucket();
        pacer
    }

    /// Bytes per second.
    fn rate_for(&self, wire_budget: u32) -> f64 {
        if self.factor == 0 {
            12_000.0 * 1240.0
        } else {
            (wire_budget as f64 * self.factor as f64 / 10.0).max(PACE_FLOOR) / 8.0
        }
    }

    fn bucket(&self) -> f64 {
        if self.factor == 0 {
            48.0 * 1240.0
        } else {
            (self.rate * PACE_BURST_TIME).max(PACE_BURST_MIN)
        }
    }

    pub fn set_budget(&mut self, wire_budget: u32) {
        self.rate = self.rate_for(wire_budget);
    }

    /// Bits per second packets leave at, at most.
    pub fn rate_bps(&self) -> u64 {
        (self.rate * 8.0) as u64
    }

    /// Counts what may leave since the last call. `queued`: the bytes waiting, the oldest
    /// of them for `waited`.
    pub fn refill(&mut self, now: Instant, queued: usize, waited: Duration) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        if self.factor == 0 {
            self.tokens = (self.tokens + self.rate * dt).min(self.bucket());
            return;
        }
        // (What is left of the time the oldest packet may wait, for all that is queued.)
        let left = (PACE_DRAIN_WITHIN - waited.as_secs_f64()).max(0.01);
        let rate = self.rate.max(queued as f64 / left);
        self.tokens = (self.tokens + rate * dt).min(self.bucket().max(rate * PACE_BURST_TIME));
    }

    /// Whether a packet may leave now. (A packet larger than what is left may still go when
    /// anything is left: the bucket then owes the difference.)
    pub fn ready(&self) -> bool {
        self.tokens > 0.0
    }

    pub fn took(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
    }

    /// How long until the next packet may leave.
    pub fn wait(&self) -> Duration {
        if self.tokens > 0.0 || self.rate <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((-self.tokens + 1.0) / self.rate)
        }
    }
}

/// Keyframes asked of the encoder: at most one per `every`, whoever asks and however often.
/// A request inside that time is kept and granted when the time is up, because whoever
/// asked still needs one (a viewer that joined gets nothing out of the keyframe before its key).
pub struct KeyframeGate {
    every: Duration,
    last: Option<Instant>,
    due: Option<Instant>,
}

/// A stream that is holding its pictures back for a keyframe (nothing reaches anybody until
/// one comes) may ask this often.
const URGENT_EVERY: Duration = Duration::from_millis(300);

impl KeyframeGate {
    pub fn new(every: Duration) -> Self {
        Self { every, last: None, due: None }
    }

    /// A keyframe is needed. `urgent`: the stream sends nothing until it comes (its own
    /// need); otherwise viewers asked, or a new one arrived, and the others go on watching.
    /// True: ask the encoder now. False: it is asked later (`due`).
    pub fn request(&mut self, now: Instant, urgent: bool) -> bool {
        let every = if urgent { URGENT_EVERY.min(self.every) } else { self.every };
        match self.last {
            Some(last) if now.saturating_duration_since(last) < every => {
                let due = last + every;
                self.due = Some(self.due.map_or(due, |already| already.min(due)));
                false
            }
            _ => {
                self.last = Some(now);
                self.due = None;
                true
            }
        }
    }

    /// When a kept request is to be granted.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// True once when a kept request is due: ask the encoder now.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.due {
            Some(due) if now >= due => {
                self.due = None;
                self.last = Some(now);
                true
            }
            _ => false,
        }
    }

    /// A keyframe went out (asked for here, or the share's own): the next is a full `every` away.
    pub fn sent(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// When the encoder was last asked, or a keyframe last went out.
    pub fn last(&self) -> Option<Instant> {
        self.last
    }
}

/// Video timestamps on an even grid, for a source that keeps the share's pace but not
/// its rhythm.
///
/// A screen is captured when the compositor draws. The fourth live run (a 144 Hz monitor
/// shared at 60 pictures a second, 15 303 pictures) left its pictures 7, 14, 21 or 28 ms
/// apart, exactly 60 a second on average: one in seven only half a frame interval after
/// the one before. Stamped with those instants, a receiver that shows pictures by their
/// timestamps on a 60 Hz screen never shows 5 % of them and shows as many twice: part of
/// the stutter the viewers reported, at any bitrate and without a lost packet. On the grid
/// every picture is one frame interval after the one before (for that run: no picture
/// left out, four refreshes repeated).
///
/// The grid is only used while the source keeps the share's pace: the mean step of its
/// last pictures (up to 120, at least 24) within 4 % of the frame interval, and that of the
/// last 16 within 15 %. Any other
/// source keeps the clock's timestamps as they are: a capture that passes one picture per
/// three refreshes of a 144 Hz monitor (a steady 48 a second, what the capture does once
/// its own rhythm is fixed), a game slower than the share, a still screen repeated every
/// 100 ms. Laid over those, a grid of 60 a second would make an even source uneven.
///
/// On the grid each picture is one interval after the last, moved a thirty-second of the
/// way towards its own instant so that the grid follows the source; more than three
/// intervals off (a pause, a burst), it is back on the clock. Leaving the grid, what it
/// was off the clock by fades over a few pictures instead of jumping.
pub struct EvenClock {
    interval: u32,
    last: Option<u32>,
    /// The clock timestamps of the last pictures, for the source's pace.
    recent: VecDeque<u32>,
    engaged: bool,
}

/// The pace of the source is the mean step of this many pictures at most...
const PACE_PICTURES: usize = 120;
/// ...and is not judged on fewer than this.
const PACE_PICTURES_MIN: usize = 24;
/// The share's pace: within this share of the frame interval.
const ON_PACE: f64 = 0.04;
/// The last few pictures alone may be further off (in the fourth live run sixteen of them
/// never were by more than 15 %), but not as far as another pace is.
const PACE_PICTURES_SHORT: usize = 16;
const ON_PACE_SHORT: f64 = 0.15;

impl EvenClock {
    /// `fps`: the pace of the share; 90 kHz timestamps.
    pub fn new(fps: u32) -> Self {
        Self { interval: 90_000 / fps.clamp(1, 240), last: None, recent: VecDeque::with_capacity(PACE_PICTURES + 2), engaged: false }
    }

    /// The timestamp to send for a picture whose clock timestamp is `clock`.
    pub fn stamp(&mut self, clock: u32) -> u32 {
        let interval = self.interval as i64;
        let step = self.recent.back().map_or(0, |before| clock.wrapping_sub(*before) as i32 as i64);
        // (A pause or a clock that jumped: the pace is measured anew.)
        if step < 0 || step > 4 * interval {
            self.recent.clear();
        }
        self.recent.push_back(clock);
        if self.recent.len() > PACE_PICTURES + 1 {
            self.recent.pop_front();
        }
        let Some(last) = self.last else {
            self.last = Some(clock);
            return clock;
        };
        let steps = self.recent.len() - 1;
        let mean_of_last = |pictures: usize| clock.wrapping_sub(self.recent[steps - pictures]) as f64 / pictures as f64;
        self.engaged = steps >= PACE_PICTURES_MIN
            && (mean_of_last(steps) - interval as f64).abs() <= ON_PACE * interval as f64
            // (A source that changes its pace shows in its last few pictures first.)
            && (mean_of_last(PACE_PICTURES_SHORT) - interval as f64).abs() <= ON_PACE_SHORT * interval as f64;
        // How late the picture is for the place after the last one.
        let lag = clock.wrapping_sub(last.wrapping_add(self.interval)) as i32 as i64;
        let out = if self.engaged && lag.abs() <= 3 * interval {
            last.wrapping_add((interval + lag / 32) as u32)
        } else {
            // The clock's own time; what the grid was off it by fades an eighth a picture.
            let off = last.wrapping_sub(clock.wrapping_sub(step as u32)) as i32 as i64;
            let off = if self.recent.len() < 2 { 0 } else { off - off / 8 - off.signum() };
            clock.wrapping_add(off.clamp(-3 * interval, 3 * interval) as u32)
        };
        // Forced to increase, whatever happened.
        let out = if (out.wrapping_sub(last) as i32) <= 0 { last.wrapping_add(1) } else { out };
        self.last = Some(out);
        out
    }

    /// The source keeps the share's pace: its pictures are being put on the grid.
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.recent.clear();
        self.engaged = false;
    }
}

/// Which audio timestamp belongs to an instant. Packets are stamped by counting samples
/// (960 per packet), and leave when the loopback delivered their last chunk, which it does
/// in uneven batches: the packet that happens to precede a sender report may have waited
/// 20 ms longer than the one five seconds before. Receivers build their lip sync on those
/// reports, so the report takes the least delay seen lately instead of the last one.
pub struct AudioClock {
    /// The mapping: at this instant the capture was at this timestamp.
    anchor: Option<(Instant, u32)>,
    /// The packet of the running window that left soonest after its samples, and when
    /// the window began.
    candidate: Option<(Instant, u32)>,
    window: Option<Instant>,
}

const SAMPLE_RATE: i64 = 48_000;
const PACKET_SAMPLES: u32 = 960;
/// The mapping is taken anew from the packets of this long (the capture's clock is not the
/// machine's: over minutes they drift apart; and a pause moves the timestamps by a whole
/// number of packets, up to 10 ms off).
const AUDIO_CLOCK_WINDOW: Duration = Duration::from_secs(5);

/// The timestamp a mapping gives for `now`.
fn mapped((at, timestamp): (Instant, u32), now: Instant) -> u32 {
    timestamp.wrapping_add((now.saturating_duration_since(at).as_micros() as i64 * SAMPLE_RATE / 1_000_000) as u32)
}

impl AudioClock {
    pub fn new() -> Self {
        Self { anchor: None, candidate: None, window: None }
    }

    /// A packet with `timestamp` (of its first sample) was sent at `now`.
    pub fn on_packet(&mut self, timestamp: u32, now: Instant) {
        // Its last sample was captured about now, when nothing held the packet up. The one
        // that is ahead of a mapping left sooner after its samples than the mapping knows:
        // it is the better anchor.
        let this = (now, timestamp.wrapping_add(PACKET_SAMPLES));
        let ahead_of = |mapping: Option<(Instant, u32)>| mapping.is_none_or(|m| (this.1.wrapping_sub(mapped(m, now)) as i32) > 0);
        if ahead_of(self.anchor) {
            self.anchor = Some(this);
        }
        if ahead_of(self.candidate) {
            self.candidate = Some(this);
        }
        match self.window {
            Some(began) if now.saturating_duration_since(began) < AUDIO_CLOCK_WINDOW => {}
            Some(_) => {
                // A capture clock slower than the machine's never gets ahead: the best
                // packet of the window that ended takes over.
                self.anchor = self.candidate.take().or(self.anchor);
                self.window = Some(now);
            }
            None => self.window = Some(now),
        }
    }

    /// The timestamp of the sample captured at `now`, by the mapping.
    pub fn timestamp_at(&self, now: Instant) -> u32 {
        self.anchor.map_or(0, |anchor| mapped(anchor, now))
    }

    pub fn is_set(&self) -> bool {
        self.anchor.is_some()
    }

    /// The timestamps jumped (a pause, a new encoder): the mapping starts over.
    pub fn reset(&mut self) {
        self.anchor = None;
        self.candidate = None;
        self.window = None;
    }
}
