//! Which of a capture's pictures go to the encoder, decided from their arrival times alone
//! (no GStreamer here: `linux_gpu_tx::add_gate` is the caller, the tests feed simulated
//! sources).
//!
//! A viewer's player shows each picture at its timestamp, so what a viewer sees as smooth
//! is the *spacing* of the pictures that pass, not how many pass. The schedule before this
//! one (`Policy::Average`) kept the average at the nominal rate: a picture that came late
//! was followed a quarter of a slot later, a 7 ms step after a 28 ms one on a 144 Hz
//! monitor (a stream of 60 measured with 14 % of its steps at 7 ms and 7 % at 28 ms).
//!
//! `Policy::Even` never catches up. A new picture passes when the one passed before is at
//! least `min_step` old, and `min_step` follows the source's rhythm, read from the steps
//! between its last 64 new pictures:
//! - the source is not faster than the stream (60 Hz into 60, a film at 60 on a 144 Hz
//!   monitor): every picture passes, in the source's own rhythm (but for one that comes
//!   less than 0.6 of a slot after another);
//! - the source is faster and its pictures sit on a grid (a monitor's refresh, also when a
//!   game only fills every first or second refresh): one picture per `every` refreshes at
//!   most, `every` being the fewest refreshes that are not shorter than the stream's slot
//!   (144 Hz into 60: every 3rd, 48 a second; 165: 55; 120: 60; 100: 50; 75: 37.5). The
//!   threshold lies half a refresh below, as far from both neighbours as it can be, so
//!   arrival jitter of a few milliseconds does not turn a 3 into a 2 or a 4;
//! - faster and on no grid (variable refresh that wanders): pictures at least 0.95 of a
//!   slot apart.
//! A source that holds a rate above the nominal one by more than 5 % is never passed whole
//! (the encoder's level and its bits per picture are sized for the nominal rate); the
//! stream is slower than nominal whenever the source's refresh does not divide into it.
//! Single stretches can be faster: while every picture passes the least step is 0.6 to 0.9
//! of a slot, and a source that passed whole keeps passing whole until it has been more
//! than 15 % faster than the stream for three readings in a row (about 1.5 s; at once when
//! it is a third faster). That band is there on purpose: a game around 60 to 68 pictures a
//! second would otherwise be halved and passed whole by turns, 32 and 64 a second, which
//! is worse than the schedule before. A source that swings wider (55 to 75) still changes
//! between the two; with "whole or every second picture" as the only choices nothing
//! prevents that.
//! Whether the source is faster than the stream is judged without the pictures that come
//! less than 0.3 of a slot after another (the same refresh for a viewer: a 60 Hz monitor
//! with a few stray buffers a second is still a 60 Hz monitor). When a fast source turns
//! slow (a game minimised: 144 to 60) twelve pictures in a row a slot apart are enough to
//! pass every picture again. A grid finer than a third of a slot (240 Hz) is not counted
//! in refreshes when arrivals are off by most of a refresh: its least step stays within
//! 0.85 to 0.95 of a slot.
//!
//! Simulated, a stream of 60 (`cargo test --release --lib -- cadence_table --nocapture`
//! prints all of it; pictures a second, then the step's 5th / 50th / 95th percentile in ms):
//!
//!   source                              before                      now
//!   144 Hz monitor                      60.0  13.3 / 14.3 / 21.4    48.0  20.2 / 20.8 / 21.5
//!   165 Hz                              60.0  11.9 / 18.0 / 18.8    55.0  17.5 / 18.2 / 18.9
//!   120 Hz                              60.0  15.9 / 16.7 / 17.4    the same
//!   100 Hz                              60.0   9.6 / 19.7 / 20.6    50.0  19.3 / 20.0 / 20.7
//!   75 Hz                               60.0  12.7 / 13.5 / 27.1    37.5  26.0 / 26.7 / 27.3
//!   60 and 59.94 Hz, 60 Hz into 30      the source's own            the same
//!   144 Hz, 8 % of refreshes empty (*)  60.0  10.9 / 15.2 / 22.6    46.7  18.5 / 20.9 / 27.6
//!   144 Hz, game every 1 or 2 refreshes 60.0   7.1 / 14.7 / 27.5    42.7  18.9 / 21.6 / 29.0
//!   variable refresh, game holding 70   60.0  13.4 / 14.5 / 28.8    34.9  27.3 / 28.6 / 29.9
//!   variable refresh 48-144, wandering  56.0  11.2 / 18.5 / 22.9    51.4  13.7 / 19.7 / 26.0
//!   240 Hz, arrivals as measured        60.0  14.0 / 16.7 / 19.5    57.7  15.9 / 16.8 / 20.7
//!   game drifting 60..68 (was whole)    59.9  14.4 / 15.8 / 29.4    64.1  14.3 / 15.6 / 17.0
//!
//! (*) the measured stream, rebuilt from its packet log: 144.00 Hz, about 133 new pictures
//! a second, one arrival in four up to 3 ms off the grid; which refreshes were empty is
//! not in the log and is drawn at random here.
//!
//! Steps much shorter or longer than the step before them (under 0.7, over 1.45 of it): on
//! the game that skips refreshes 59 % before and 6 % now, on the wandering one 25 % and 2 %.
//! The price is the rate where the source is between 63 and 120 pictures a second and
//! faster than the stream: half the source's pictures pass (75 Hz: 37.5; a game holding 70
//! on a variable refresh monitor: 35) where the schedule before left one in five or six out.
//! Other rules tried on the same sources: a fixed least step of 0.8 of a slot flaps on a
//! 75 Hz monitor (13.33 ms is exactly its refresh: 47 a second, 42 % short steps) and runs
//! a 144 Hz one at 69-72; a fixed 0.95 loses pictures of a 60 or 120 Hz monitor as soon as
//! they arrive half a millisecond early (45 a second with 1 ms of jitter); the picture
//! nearest to each ideal slot is the schedule before without its worst steps (144 Hz stays
//! 2-2-3, the game that skips refreshes keeps its 7 ms steps).

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

/// Steps between new pictures the rhythm is read from; it is read again every
/// `SETTLE_EVERY` of them, once there are `SETTLE_MIN`.
const WINDOW: usize = 64;
const SETTLE_EVERY: usize = 32;
const SETTLE_MIN: usize = 24;
/// The least step, in slots, until the rhythm is known.
const START_STEP: f64 = 0.9;
/// While every picture passes, two closer than this much of a slot are still one (a
/// viewer's 60 Hz screen could not show both).
const ALL_FLOOR: f64 = 0.6;
/// A step this much of a slot is as good as a slot (59.94 Hz into 60, clocks that differ).
const NOMINAL_SLACK: f64 = 0.95;
/// Once every picture passes, the source's mean step must fall below this much of a slot
/// before pictures are left out again (a source right at the limit does not flap).
const LEAVE_ALL: f64 = 0.85;
/// ... on this many readings in a row (every `SETTLE_EVERY` pictures), or on one reading
/// below `LEAVE_AT_ONCE` (a desktop at 60 that turns into a game at 144).
const LEAVE_AFTER: u32 = 3;
const LEAVE_AT_ONCE: f64 = 0.75;
/// A picture less than this much of a slot after another is the same refresh when the
/// source's own rate is counted (stray buffers do not make a 60 Hz monitor a fast one).
const SAME_REFRESH: f64 = 0.3;
/// A fast source has turned slow when this many steps in a row were a slot or more: the
/// rhythm is read at once from those alone.
const SLOWED_AFTER: usize = 12;
/// A grid finer than this much of a slot: arrival jitter is most of a period, so the
/// least step is kept within `FINE_LEAST` of a slot whatever the period was read as.
const FINE_GRID: f64 = 1.0 / 3.0;
const FINE_LEAST: (f64, f64) = (0.85, 0.95);
/// On a grid: this share of the steps within this much of a refresh of a whole number of
/// refreshes (steps drawn at random reach 0.6).
const GRID_SHARE: f64 = 0.75;
const GRID_TOLERANCE: f64 = 0.3;
/// Steps between passed pictures the report is made of.
const REPORT_WINDOW: usize = 300;
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// The report is announced once this many seconds of pictures have passed, and again when
/// the gate settles on something else, at most this often.
const ANNOUNCE_AFTER: f64 = 5.0;
const ANNOUNCE_AGAIN: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Even spacing, at the nominal rate or below it.
    Even,
    /// The schedule before: the nominal rate on average, late pictures made up for.
    Average,
}

/// What the gate has read from the source. Times in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rhythm {
    /// Too few new pictures so far.
    Unknown,
    /// Not faster than the stream: every picture passes. `step`: the mean between two.
    All { step: f64 },
    /// On a grid of `period`; at most one picture per `every` periods passes.
    Grid { period: f64, every: u32 },
    /// Faster than the stream and on no grid.
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The picture goes to the encoder.
    pub encode: bool,
    /// It is the picture that passed last, come again.
    pub repeat: bool,
    /// A new picture left out for the pace.
    pub too_early: bool,
}

/// What passed lately (the last `REPORT_WINDOW` steps between passed pictures).
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub policy: Policy,
    pub rhythm: Rhythm,
    pub nominal_fps: f64,
    /// New pictures a second the source brought when the rhythm was last read (0 before).
    pub source_per_s: f64,
    pub min_step_ms: f64,
    pub passed_per_s: f64,
    pub steps: usize,
    /// Steps that ended in a repeat of a still screen.
    pub repeats: usize,
    pub p5_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Steps shorter than 0.7 of the median, and longer than 1.5 of it.
    pub short: usize,
    pub long: usize,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.policy, self.rhythm) {
            (Policy::Average, _) => write!(f, "old schedule (the nominal rate on average, late pictures made up for)")?,
            (_, Rhythm::Unknown) => write!(f, "source rhythm not known yet (few new pictures) -> pictures at least {:.1} ms apart", self.min_step_ms)?,
            (_, Rhythm::All { step }) => write!(f, "source brings {:.1} new pictures/s, not faster than the stream -> every picture passes", 1.0 / step)?,
            (_, Rhythm::Grid { period, every }) => write!(
                f,
                "source brings {:.1} new pictures/s on a grid of about {:.2} ms ({:.1} a second) -> at most one picture per {} grid steps ({:.1}/s, least step {:.1} ms)",
                self.source_per_s,
                period * 1000.0,
                1.0 / period,
                every,
                1.0 / (period * every as f64),
                self.min_step_ms
            )?,
            (_, Rhythm::Free) => write!(f, "source brings {:.1} new pictures/s on no grid, faster than the stream -> pictures at least {:.1} ms apart", self.source_per_s, self.min_step_ms)?,
        }
        write!(
            f,
            " (as last read): {:.1} pictures/s passed (nominal {:.0}); step p5 {:.1} / p50 {:.1} / p95 {:.1} ms, {} shorter than 0.7 and {} longer than 1.5 of the median, over {} steps ({} repeats of a still screen)",
            self.passed_per_s, self.nominal_fps, self.p5_ms, self.p50_ms, self.p95_ms, self.short, self.long, self.steps, self.repeats
        )
    }
}

/// The value at `share` of a sorted list.
fn at(sorted: &[f64], share: f64) -> f64 {
    sorted[((sorted.len() as f64 * share) as usize).min(sorted.len() - 1)]
}

/// The gate's schedule. Times are since the gate's start.
pub struct Cadence {
    policy: Policy,
    interval: Duration,
    still_every: Duration,
    /// `Even`: the least time between two passed pictures, and what it was made from.
    min_step: Duration,
    rhythm: Rhythm,
    /// The picture that arrived last and when a new one last arrived; the steps between new
    /// pictures (seconds), and how many came since the rhythm was last read.
    seen: Option<u64>,
    last_new: Option<Duration>,
    steps: VecDeque<f64>,
    unsettled: usize,
    /// Steps of a slot or more in a row; readings in a row that found a source passing
    /// whole faster than the stream; the source's own mean step at the last reading.
    slow_run: usize,
    faster_reads: u32,
    own_step: f64,
    /// `Average`: when the next picture is due.
    due: Duration,
    /// The picture that passed last, and when.
    passed_any: bool,
    last_picture: u64,
    last_pass: Duration,
    /// For the report: steps between passed pictures (seconds; whether it ended in a
    /// repeat), not counting those across a time without viewers; their sum since the
    /// start; when the report was last made, and what was announced when.
    unwatched_since_pass: bool,
    passed: VecDeque<(f64, bool)>,
    passed_secs: f64,
    last_report: Duration,
    announced: Option<((u8, u32), Duration)>,
}

impl Cadence {
    /// `interval`: the stream's slot (1 / its nominal rate). `still_every`: how often the
    /// picture that passed last passes again while nothing new comes.
    pub fn new(policy: Policy, interval: Duration, still_every: Duration) -> Self {
        Self {
            policy,
            interval,
            still_every,
            min_step: interval.mul_f64(START_STEP),
            rhythm: Rhythm::Unknown,
            seen: None,
            last_new: None,
            steps: VecDeque::with_capacity(WINDOW),
            unsettled: 0,
            slow_run: 0,
            faster_reads: 0,
            own_step: 0.0,
            due: Duration::ZERO,
            passed_any: false,
            last_picture: 0,
            last_pass: Duration::ZERO,
            unwatched_since_pass: false,
            passed: VecDeque::with_capacity(REPORT_WINDOW),
            passed_secs: 0.0,
            last_report: Duration::ZERO,
            announced: None,
        }
    }

    /// A buffer arrived at `now` holding picture number `picture` (the keepalive brings the
    /// same number again). `forced`: a picture is wanted at once (a keyframe waits for one to
    /// ride on), viewers or not. `watched`: somebody is watching.
    pub fn arrived(&mut self, now: Duration, picture: u64, forced: bool, watched: bool) -> Verdict {
        self.observe(now, picture);
        let repeat = picture == self.last_picture;
        let since_pass = now.saturating_sub(self.last_pass);
        let mut too_early = false;
        let encode = if !forced && !watched {
            self.unwatched_since_pass = true;
            false
        } else if forced {
            true
        } else if repeat {
            since_pass + Duration::from_millis(2) >= self.still_every
        } else {
            // Too early: should the screen stop changing right now, the keepalive brings
            // this picture again and it passes then.
            too_early = match self.policy {
                Policy::Even => self.passed_any && since_pass < self.min_step,
                Policy::Average => now + self.interval / 8 < self.due,
            };
            !too_early
        };
        if encode {
            if self.passed_any && !self.unwatched_since_pass {
                if self.passed.len() == REPORT_WINDOW {
                    self.passed.pop_front();
                }
                self.passed.push_back((since_pass.as_secs_f64(), repeat));
                self.passed_secs += since_pass.as_secs_f64();
            }
            // (`Average`: the schedule keeps the average at the nominal rate whatever the
            // monitor's rhythm; a picture that came late may be followed sooner, and after
            // a pause the schedule starts over.)
            self.due = (self.due + self.interval).max(now + self.interval / 4);
            self.unwatched_since_pass = false;
            self.passed_any = true;
            self.last_picture = picture;
            self.last_pass = now;
        }
        Verdict { encode, repeat, too_early }
    }

    pub fn rhythm(&self) -> Rhythm {
        self.rhythm
    }

    /// Notes a new picture's arrival and reads the rhythm again when it is time to.
    fn observe(&mut self, now: Duration, picture: u64) {
        if self.seen == Some(picture) {
            return;
        }
        self.seen = Some(picture);
        let Some(before) = self.last_new.replace(now) else { return };
        let step = now.saturating_sub(before);
        // (A pause is not part of the rhythm: a still screen, a cursor that rests.)
        if step > (self.interval * 3).max(Duration::from_millis(100)) {
            self.slow_run = 0;
            return;
        }
        if self.steps.len() == WINDOW {
            self.steps.pop_front();
        }
        self.steps.push_back(step.as_secs_f64());
        self.unsettled += 1;
        let first = self.rhythm == Rhythm::Unknown && self.steps.len() >= SETTLE_MIN;
        // A source that was faster than the stream and is not any more (a game minimised,
        // 144 to 60): its least step would leave out every other picture until the old
        // steps have left the window. What came before does not count then.
        self.slow_run = if step.as_secs_f64() >= NOMINAL_SLACK * self.interval.as_secs_f64() { self.slow_run + 1 } else { 0 };
        let slowed = matches!(self.rhythm, Rhythm::Grid { .. } | Rhythm::Free) && self.slow_run >= SLOWED_AFTER;
        if slowed {
            self.slow_run = 0;
            self.steps.drain(..self.steps.len() - SLOWED_AFTER);
        }
        if first || slowed || self.unsettled >= SETTLE_EVERY {
            self.unsettled = 0;
            self.settle();
        }
    }

    /// Reads the source's rhythm from its last steps and sets the least step for it.
    fn settle(&mut self) {
        let slot = self.interval.as_secs_f64();
        let mut steps: Vec<f64> = self.steps.iter().copied().collect();
        steps.sort_by(|a, b| a.total_cmp(b));
        let mean = steps.iter().sum::<f64>() / steps.len() as f64;
        // The shortest step the source usually makes: the middle of its shortest tenth and
        // what lies close above (one refresh, when the pictures sit on a monitor's grid).
        let shortest = at(&steps, 0.1);
        let near = steps.partition_point(|s| *s < shortest * 1.5).max(1);
        let base = steps[near / 2].max(1e-4);
        // The source's own step: a picture right after another is the same refresh.
        let (mut counted, mut since) = (0usize, 0.0);
        for step in &self.steps {
            since += step;
            if since >= SAME_REFRESH * slot {
                counted += 1;
                since = 0.0;
            }
        }
        let own = mean * steps.len() as f64 / counted.max(1) as f64;
        self.own_step = own;
        // Every picture passes: from when the source is not faster than the stream, until
        // it has been clearly faster for a while (or much faster once).
        let all = if !matches!(self.rhythm, Rhythm::All { .. }) {
            own >= NOMINAL_SLACK * slot
        } else if own >= LEAVE_ALL * slot {
            self.faster_reads = 0;
            true
        } else {
            self.faster_reads += 1;
            own >= LEAVE_AT_ONCE * slot && self.faster_reads < LEAVE_AFTER
        };
        let (rhythm, least) = if all {
            // (Three quarters of its usual shortest step: a film at 60 on a 144 Hz monitor
            // comes 2, 2 and 3 refreshes apart and passes whole, half a refresh to spare;
            // should the source turn faster, less passes until the rhythm is read again.)
            (Rhythm::All { step: own }, (base * 0.75).clamp(slot * ALL_FLOOR, slot * START_STEP))
        } else {
            self.faster_reads = 0;
            // The grid's period: the time the steps took by the periods they come to (the
            // jitter of single arrivals cancels out over steps in a row), three times over
            // since the first guess is the short side of the shortest steps. On the grid
            // are the steps close to a whole number of periods.
            let (mut period, mut on_grid) = (base, 0usize);
            for _ in 0..3 {
                let periods: f64 = steps.iter().map(|step| (step / period).round().max(1.0)).sum();
                period = mean * steps.len() as f64 / periods;
                on_grid = steps.iter().filter(|step| (*step - (*step / period).round().max(1.0) * period).abs() <= GRID_TOLERANCE * period).count();
            }
            if on_grid as f64 >= GRID_SHARE * steps.len() as f64 {
                let every = (NOMINAL_SLACK * slot / period - 1e-9).ceil().max(1.0);
                let least = (every - 0.5) * period;
                let least = if period < FINE_GRID * slot { least.clamp(FINE_LEAST.0 * slot, FINE_LEAST.1 * slot) } else { least };
                (Rhythm::Grid { period, every: every as u32 }, least)
            } else {
                (Rhythm::Free, NOMINAL_SLACK * slot)
            }
        };
        self.rhythm = rhythm;
        self.min_step = Duration::from_secs_f64(least);
    }

    /// What passed lately; `None` before two pictures have.
    pub fn report(&self) -> Option<Report> {
        if self.passed.is_empty() {
            return None;
        }
        let mut steps: Vec<f64> = self.passed.iter().map(|(step, _)| *step).collect();
        steps.sort_by(|a, b| a.total_cmp(b));
        let median = at(&steps, 0.5);
        Some(Report {
            policy: self.policy,
            rhythm: self.rhythm,
            nominal_fps: 1.0 / self.interval.as_secs_f64(),
            source_per_s: if self.own_step > 0.0 { 1.0 / self.own_step } else { 0.0 },
            min_step_ms: self.min_step.as_secs_f64() * 1000.0,
            passed_per_s: steps.len() as f64 / steps.iter().sum::<f64>().max(1e-9),
            steps: steps.len(),
            repeats: self.passed.iter().filter(|(_, repeat)| *repeat).count(),
            p5_ms: at(&steps, 0.05) * 1000.0,
            p50_ms: median * 1000.0,
            p95_ms: at(&steps, 0.95) * 1000.0,
            short: steps.iter().filter(|s| **s < 0.7 * median).count(),
            long: steps.iter().filter(|s| **s > 1.5 * median).count(),
        })
    }

    /// Once a second: the report, and whether it is to be announced (in the log): once a
    /// few seconds of pictures have passed, and when the gate has settled on something else
    /// since.
    pub fn take_report(&mut self, now: Duration) -> Option<(Report, bool)> {
        if now.saturating_sub(self.last_report) < REPORT_EVERY {
            return None;
        }
        self.last_report = now;
        let report = self.report()?;
        let settled = match self.rhythm {
            Rhythm::Unknown => (0, 0),
            Rhythm::All { .. } => (1, 0),
            Rhythm::Grid { every, .. } => (2, every),
            Rhythm::Free => (3, 0),
        };
        let announce = match self.announced {
            None => self.passed_secs >= ANNOUNCE_AFTER,
            Some((before, at)) => before != settled && now.saturating_sub(at) >= ANNOUNCE_AGAIN,
        };
        if announce {
            self.announced = Some((settled, now));
        }
        Some((report, announce))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STILL: Duration = Duration::from_millis(100);
    const KEEPALIVE: f64 = 0.025;

    /// Numbers in [0, 1), the same on every run.
    struct Dice(u64);
    impl Dice {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next()
        }
        /// Roughly normal around 0 with deviation `sigma`.
        fn jitter(&mut self, sigma: f64) -> f64 {
            ((0..12).map(|_| self.next()).sum::<f64>() - 6.0) * sigma
        }
    }

    /// Arrival times (seconds) of the new pictures of a source.
    type Source = Vec<f64>;

    /// A monitor at `hz` that shows a new picture with every refresh.
    fn monitor(hz: f64, sigma_ms: f64, secs: f64) -> Source {
        let mut dice = Dice(0x9E3779B97F4A7C15 ^ hz.to_bits());
        (0..(secs * hz) as usize).map(|i| i as f64 / hz + dice.jitter(sigma_ms / 1000.0)).collect()
    }

    /// A 144 Hz monitor whose game brings a new picture after two refreshes with chance
    /// `two` and after one otherwise; one picture in four arrives up to 3 ms off the grid
    /// (the spread of the measured stream).
    fn game_on_144(two: f64, secs: f64) -> Source {
        let (mut dice, period) = (Dice(0xC0FFEE ^ two.to_bits()), 1.0 / 144.0);
        let (mut source, mut refresh) = (Vec::new(), 0u32);
        while (refresh as f64) * period < secs {
            let off = if dice.next() < 0.25 { dice.between(-0.003, 0.003) } else { dice.jitter(0.0003) };
            source.push(refresh as f64 * period + off);
            refresh += if dice.next() < two { 2 } else { 1 };
        }
        source.sort_by(|a, b| a.total_cmp(b));
        source
    }

    /// A film at 60 on a 144 Hz monitor: new pictures 2, 2 and 3 refreshes apart.
    fn film_on_144(secs: f64) -> Source {
        let (mut dice, period) = (Dice(0xF11A), 1.0 / 144.0);
        let (mut source, mut refresh, mut i) = (Vec::new(), 0u32, 0usize);
        while (refresh as f64) * period < secs {
            source.push(refresh as f64 * period + dice.jitter(0.0003));
            refresh += [2, 2, 3, 2, 3][i % 5];
            i += 1;
        }
        source
    }

    /// Variable refresh between 48 and 144 Hz: every step drawn anew, or a frame time that
    /// wanders.
    fn variable_refresh(wander: bool, secs: f64) -> Source {
        let mut dice = Dice(0x5EED ^ wander as u64);
        let (mut source, mut t, mut frame) = (Vec::new(), 0.0, 0.010);
        while t < secs {
            source.push(t);
            if wander {
                frame = (frame + dice.jitter(0.0004)).clamp(1.0 / 144.0, 1.0 / 48.0);
                t += frame + dice.jitter(0.0002);
            } else {
                t += dice.between(1.0 / 144.0, 1.0 / 48.0);
            }
        }
        source
    }

    /// Variable refresh following a game that holds `fps`, each frame within a few percent.
    fn steady_game(fps: f64, secs: f64) -> Source {
        let mut dice = Dice(0xABCD ^ fps.to_bits());
        let (mut source, mut t) = (Vec::new(), 0.0);
        while t < secs {
            source.push(t);
            t += (1.0 + dice.jitter(0.04)) / fps;
        }
        source
    }

    /// A still screen on a 144 Hz monitor; every 2 s the cursor moves for 0.3 s.
    fn cursor_now_and_then(secs: f64) -> Source {
        let (mut dice, period) = (Dice(0xC0550), 1.0 / 144.0);
        (0..(secs / period) as usize).map(|i| i as f64 * period).filter(|t| t % 2.0 < 0.3).map(|t| t + dice.jitter(0.0003)).collect()
    }

    /// The buffers pipewiresrc pushes for a source: each new picture, and the last one again
    /// every 25 ms while nothing new comes. (time, picture number)
    fn buffers(source: &Source) -> Vec<(f64, u64)> {
        let mut out = Vec::new();
        for (i, t) in source.iter().enumerate() {
            out.push((*t, i as u64 + 1));
            let next = source.get(i + 1).copied().unwrap_or(*t);
            let mut again = t + KEEPALIVE;
            while again < next - 1e-5 {
                out.push((again, i as u64 + 1));
                again += KEEPALIVE;
            }
        }
        out
    }

    #[derive(Debug)]
    struct Seen {
        rate: f64,
        p5: f64,
        p50: f64,
        p95: f64,
        least: f64,
        /// Shares of the steps under 0.7 and over 1.5 of the median, and of the steps that
        /// are under 0.7 or over 1.45 of the step before them.
        short: f64,
        long: f64,
        uneven: f64,
        repeats: usize,
        rhythm: Rhythm,
    }

    /// Runs a watched stream of `fps` over a source and looks at the steps between the
    /// pictures that passed after the first two seconds. Milliseconds.
    fn run(policy: Policy, fps: u32, source: &Source) -> Seen {
        let mut cadence = Cadence::new(policy, Duration::from_nanos(1_000_000_000 / fps as u64), STILL);
        let (mut passes, mut repeats) = (Vec::new(), 0);
        for (t, picture) in buffers(source) {
            let verdict = cadence.arrived(Duration::from_secs_f64(t.max(0.0)), picture, false, true);
            if verdict.encode && t >= 2.0 {
                passes.push(t * 1000.0);
                repeats += verdict.repeat as usize;
            }
        }
        let steps: Vec<f64> = passes.windows(2).map(|w| w[1] - w[0]).collect();
        let mut sorted = steps.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = at(&sorted, 0.5);
        let share = |n: usize| n as f64 / steps.len() as f64;
        Seen {
            rate: 1000.0 * steps.len() as f64 / (passes[passes.len() - 1] - passes[0]),
            p5: at(&sorted, 0.05),
            p50: median,
            p95: at(&sorted, 0.95),
            least: sorted[0],
            short: share(steps.iter().filter(|s| **s < 0.7 * median).count()),
            long: share(steps.iter().filter(|s| **s > 1.5 * median).count()),
            uneven: share(steps.windows(2).filter(|w| w[1] < 0.7 * w[0] || w[1] > 1.45 * w[0]).count()),
            repeats,
            rhythm: cadence.rhythm(),
        }
    }

    fn even(fps: u32, source: &Source) -> Seen {
        run(Policy::Even, fps, source)
    }

    /// Every step within a tenth of the median: as even as arrival jitter leaves it.
    fn assert_steady(seen: &Seen, rate: f64) {
        assert!((seen.rate - rate).abs() < 0.3, "rate {seen:?}");
        assert!(seen.p5 > 0.9 * seen.p50 && seen.p95 < 1.1 * seen.p50, "spread {seen:?}");
        assert_eq!((seen.short, seen.long, seen.uneven), (0.0, 0.0, 0.0), "{seen:?}");
    }

    #[test]
    fn regular_monitors_lock_to_whole_refreshes() {
        for (hz, rate, every) in [(144.0, 48.0, 3), (165.0, 55.0, 3), (120.0, 60.0, 2), (100.0, 50.0, 2), (75.0, 37.5, 2), (240.0, 60.0, 4)] {
            let seen = even(60, &monitor(hz, 0.3, 20.0));
            assert_steady(&seen, rate);
            assert!(matches!(seen.rhythm, Rhythm::Grid { every: e, period } if e == every && (period * hz - 1.0).abs() < 0.01), "{hz}: {seen:?}");
        }
    }

    #[test]
    fn a_source_at_the_stream_rate_passes_whole() {
        for hz in [60.0, 59.94] {
            let seen = even(60, &monitor(hz, 0.3, 20.0));
            assert_steady(&seen, hz);
            assert!(matches!(seen.rhythm, Rhythm::All { .. }), "{seen:?}");
        }
        // Also with arrival jitter of a millisecond, where a fixed threshold just under a
        // slot loses a third of the pictures.
        assert!((even(60, &monitor(60.0, 1.0, 20.0)).rate - 60.0).abs() < 0.3);
        // A film at 60 on a 144 Hz monitor keeps its own 2-2-3 rhythm: nothing is left out.
        let film = even(60, &film_on_144(20.0));
        assert!((film.rate - 60.0).abs() < 0.5 && film.least > 12.0, "{film:?}");
    }

    /// A source at the stream's rate on average whose pictures come one to four refreshes
    /// of a 144 Hz monitor apart (the measured stream's own pictures, taken as a source).
    #[test]
    fn a_ragged_source_at_the_stream_rate_loses_its_shortest_steps() {
        let (mut dice, period) = (Dice(0xBADBEEF), 1.0 / 144.0);
        let (mut source, mut refresh) = (Vec::new(), 0u32);
        while (refresh as f64) * period < 30.0 {
            source.push(refresh as f64 * period + dice.jitter(0.0003));
            let draw = dice.next();
            refresh += if draw < 0.13 { 1 } else if draw < 0.56 { 2 } else if draw < 0.93 { 3 } else { 4 };
        }
        let (old, seen) = (run(Policy::Average, 60, &source), even(60, &source));
        assert!(old.least < 8.0 && old.p5 < 8.0, "{old:?}");
        assert!(matches!(seen.rhythm, Rhythm::All { .. }) && seen.least > 9.5 && seen.p5 > 13.0, "{seen:?}");
    }

    #[test]
    fn sixty_hertz_into_thirty() {
        assert_steady(&even(30, &monitor(60.0, 0.3, 20.0)), 30.0);
        // 144 Hz into 30: every 5th refresh.
        assert_steady(&even(30, &monitor(144.0, 0.3, 20.0)), 28.8);
    }

    #[test]
    fn jitter_of_a_millisecond_does_not_break_the_lock() {
        let seen = even(60, &monitor(144.0, 1.0, 20.0));
        assert!((seen.rate - 48.0).abs() < 1.0 && seen.short == 0.0 && seen.long == 0.0 && seen.least > 14.0, "{seen:?}");
        let seen = even(60, &monitor(120.0, 1.0, 20.0));
        assert!((seen.rate - 60.0).abs() < 0.5 && seen.short == 0.0 && seen.long < 0.005, "{seen:?}");
    }

    /// A 144 Hz monitor whose game fills every first or second refresh.
    #[test]
    fn a_game_that_skips_refreshes_has_no_catch_up_steps() {
        for two in [0.2, 0.5, 0.8] {
            let source = game_on_144(two, 30.0);
            let old = run(Policy::Average, 60, &source);
            // What was measured: steps of one refresh behind steps of four.
            assert!(old.p5 < 10.0 && old.p95 > 25.0 && old.short > 0.03 && old.uneven > 0.4, "{old:?}");
            let seen = even(60, &source);
            // Three refreshes or four, nothing under two and a half (17.4 ms) but for jitter.
            assert!(seen.p5 > 17.0 && seen.least > 14.0 && seen.p95 < 31.0, "{two}: {seen:?}");
            assert!(seen.long < 0.01 && seen.uneven < 0.1, "{two}: {seen:?}");
            assert!(seen.rate > 36.0 && seen.rate < 48.5, "{two}: {seen:?}");
            assert!(matches!(seen.rhythm, Rhythm::Grid { every: 3, .. }), "{two}: {seen:?}");
        }
    }

    #[test]
    fn variable_refresh_is_never_faster_than_the_stream() {
        for source in [variable_refresh(false, 30.0), variable_refresh(true, 30.0)] {
            let (old, seen) = (run(Policy::Average, 60, &source), even(60, &source));
            assert!(seen.rate < 63.2 && seen.least > 0.5 * 16.667 && seen.p5 > 0.7 * 16.667, "{seen:?}");
            // Fewer steps that are much shorter or longer than the step before them.
            assert!(seen.short <= old.short && seen.uneven < 0.6 * old.uneven, "{old:?} {seen:?}");
        }
        // A game that holds its rate: every picture below the stream's rate, every second
        // one above it.
        assert_steady(&even(60, &steady_game(55.0, 20.0)), 55.0);
        assert_steady(&even(60, &steady_game(100.0, 20.0)), 50.0);
        assert_steady(&even(60, &steady_game(70.0, 20.0)), 35.0);
    }

    #[test]
    fn still_screen_with_a_cursor() {
        let source = cursor_now_and_then(30.0);
        let seen = even(60, &source);
        // The still picture again every 100 ms; the cursor's pictures three refreshes apart.
        assert!(seen.repeats > 150 && seen.p95 > 99.0 && seen.p95 < 101.0, "{seen:?}");
        assert!(seen.least > 14.0, "{seen:?}");
        assert!(matches!(seen.rhythm, Rhythm::Grid { every: 3, .. }), "{seen:?}");
        // No more repeats than before.
        assert!(seen.repeats <= run(Policy::Average, 60, &source).repeats + 5);
    }

    #[test]
    fn a_source_that_holds_its_rate_does_not_run_over_the_nominal_one() {
        let sources = [monitor(144.0, 0.3, 20.0), monitor(165.0, 1.0, 20.0), monitor(75.0, 0.3, 20.0), monitor(63.0, 0.3, 20.0), monitor(66.0, 0.3, 20.0), game_on_144(0.5, 20.0), variable_refresh(true, 20.0)];
        for source in &sources {
            let seen = even(60, source);
            assert!(seen.rate <= 63.2, "{seen:?}");
        }
    }

    #[test]
    fn the_rhythm_is_followed_when_it_changes() {
        // A desktop at 60 pictures a second, then a game at 144.
        let mut source = monitor(60.0, 0.3, 10.0);
        source.extend(monitor(144.0, 0.3, 10.0).iter().map(|t| t + 10.0));
        let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(16_666_666), STILL);
        let mut passes = Vec::new();
        for (t, picture) in buffers(&source) {
            if cadence.arrived(Duration::from_secs_f64(t.max(0.0)), picture, false, true).encode {
                passes.push(t);
            }
        }
        let steps = |from: f64, to: f64| passes.windows(2).filter(|w| w[0] >= from && w[1] < to).map(|w| (w[1] - w[0]) * 1000.0).collect::<Vec<_>>();
        assert!(steps(2.0, 10.0).iter().all(|s| (s - 16.67).abs() < 2.5));
        // Within half a second of the change: every third refresh.
        assert!(steps(10.5, 20.0).iter().all(|s| (s - 20.83).abs() < 2.5));
        assert!(matches!(cadence.rhythm(), Rhythm::Grid { every: 3, .. }));

        // The other way (the game minimised): 144, then 60. Every picture passes again
        // after a third of a second, not after a second at 30.
        let mut source = monitor(144.0, 0.3, 10.0);
        source.extend(monitor(60.0, 0.3, 10.0).iter().map(|t| t + 10.0));
        let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(16_666_666), STILL);
        let mut passes = Vec::new();
        for (t, picture) in buffers(&source) {
            if cadence.arrived(Duration::from_secs_f64(t.max(0.0)), picture, false, true).encode {
                passes.push(t);
            }
        }
        let steps = |from: f64, to: f64| passes.windows(2).filter(|w| w[0] >= from && w[1] < to).map(|w| (w[1] - w[0]) * 1000.0).collect::<Vec<_>>();
        assert!(steps(2.0, 10.0).iter().all(|s| (s - 20.83).abs() < 2.5));
        let after = steps(10.35, 20.0);
        assert!(after.len() > 570 && after.iter().all(|s| (s - 16.67).abs() < 2.5), "{} {:?}", after.len(), after.iter().cloned().fold(0.0, f64::max));
        assert!(matches!(cadence.rhythm(), Rhythm::All { .. }));
    }

    /// A monitor at `hz` where a refresh holds no new picture with chance `empty`, and one
    /// arrival in four is up to 3 ms off the grid: the measured stream rebuilt from its
    /// packet log (144.00 Hz, about 133 new pictures a second; which refreshes were empty
    /// is not in the log, they are drawn at random here).
    fn ragged_monitor(hz: f64, empty: f64, secs: f64) -> Source {
        let mut dice = Dice(0x10C ^ hz.to_bits() ^ empty.to_bits());
        let mut source: Source = (0..(secs * hz) as usize)
            .filter_map(|i| {
                let off = if dice.next() < 0.25 { dice.between(-0.003, 0.003) } else { dice.jitter(0.0003) };
                (dice.next() >= empty).then_some((i as f64 / hz + off).max(0.0))
            })
            .collect();
        source.sort_by(|a, b| a.total_cmp(b));
        source
    }

    /// A source with pictures added to it, sorted.
    fn with(mut source: Source, extra: impl IntoIterator<Item = f64>) -> Source {
        source.extend(extra);
        source.sort_by(|a, b| a.total_cmp(b));
        source
    }

    /// The fewest and the most pictures passed in a whole second, after the first `from`.
    fn per_second(fps: u32, source: &Source, from: usize) -> (usize, usize) {
        let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(1_000_000_000 / fps as u64), STILL);
        let mut seconds = vec![0usize; source[source.len() - 1] as usize];
        for (t, picture) in buffers(source) {
            if cadence.arrived(Duration::from_secs_f64(t.max(0.0)), picture, false, true).encode && (t as usize) < seconds.len() {
                seconds[t as usize] += 1;
            }
        }
        (*seconds[from..].iter().min().unwrap(), *seconds[from..].iter().max().unwrap())
    }

    #[test]
    fn the_measured_stream_gets_every_third_refresh() {
        for empty in [0.0, 0.08, 0.15] {
            let source = ragged_monitor(144.0, empty, 60.0);
            let old = run(Policy::Average, 60, &source);
            let seen = even(60, &source);
            // Before: steps of one refresh and less. Now: three refreshes, four where the
            // third was empty or the picture before came late.
            assert!(old.least < 8.0 && old.p5 < 13.0, "{empty}: {old:?}");
            assert!(seen.rate > 44.5 && seen.rate < 48.5, "{empty}: {seen:?}");
            assert!(seen.p5 > 18.0 && seen.p50 > 20.5 && seen.p50 < 21.3 && seen.p95 < 29.0 && seen.least > 14.0, "{empty}: {seen:?}");
            assert!(seen.short == 0.0 && seen.long < 0.04, "{empty}: {seen:?}");
            assert!(matches!(seen.rhythm, Rhythm::Grid { every: 3, .. }), "{empty}: {seen:?}");
            let (fewest, most) = per_second(60, &source, 2);
            assert!(fewest >= 41 && most <= 50, "{empty}: {fewest}-{most}");
        }
    }

    /// A game around the stream's rate is not halved and passed whole by turns.
    #[test]
    fn a_source_wandering_across_the_nominal_rate_does_not_flip() {
        // Variable refresh, the game drifting between 60 and 68 pictures a second.
        let mut dice = Dice(0xD21F7);
        let (mut source, mut t) = (Vec::new(), 0.0f64);
        while t < 60.0 {
            source.push(t);
            let fps = 64.0 - 4.0 * (t * std::f64::consts::TAU / 7.0).cos();
            t += (1.0 + dice.jitter(0.03)) / fps;
        }
        let (old, seen) = (run(Policy::Average, 60, &source), even(60, &source));
        let (fewest, most) = per_second(60, &source, 2);
        println!("drifting 60..68: old {old:?}\n new {seen:?} per second {fewest}-{most}");
        assert!(fewest >= 55 && most <= 70, "{fewest}-{most} {seen:?}");
        assert!(seen.long <= old.long && seen.short == 0.0, "{old:?} {seen:?}");
        // 60 Hz with clocks that differ, 61 Hz: whole.
        assert_steady(&even(60, &monitor(61.0, 0.3, 20.0)), 61.0);
        // A source that holds a rate above the stream's from the start is not passed whole
        // (steady at every second picture).
        assert_steady(&even(60, &monitor(66.0, 0.3, 20.0)), 33.0);
        // A 144 Hz monitor, the game every 2 or 3 refreshes (64 a second).
        let (mut source, mut refresh) = (Vec::new(), 0u32);
        while (refresh as f64) < 60.0 * 144.0 {
            source.push(refresh as f64 / 144.0 + dice.jitter(0.0003));
            refresh += if dice.next() < 0.75 { 2 } else { 3 };
        }
        let (old, seen) = (run(Policy::Average, 60, &source), even(60, &source));
        let (fewest, most) = per_second(60, &source, 10);
        println!("2 or 3 refreshes: old {old:?}\n new {seen:?} per second {fewest}-{most}");
        assert!(most as f64 <= 1.5 * fewest as f64 && seen.short == 0.0, "{fewest}-{most} {seen:?}");
        // What the band costs: a desktop at 60 and then a game holding 68 stays whole, 68 a
        // second (a share started on that game gets 34); a game holding 75 is halved
        // within two seconds.
        for (fps, rate) in [(68.0, 68.0), (75.0, 37.5)] {
            let mut source = monitor(60.0, 0.3, 5.0);
            source.extend(steady_game(fps, 20.0).iter().map(|t| t + 5.0));
            let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(16_666_666), STILL);
            let passed = buffers(&source).iter().filter(|(t, picture)| cadence.arrived(Duration::from_secs_f64(t.max(0.0)), *picture, false, true).encode && *t >= 7.0 && *t < 25.0).count();
            assert!((passed as f64 / 18.0 - rate).abs() < 0.6, "{fps}: {}", passed as f64 / 18.0);
        }
        // And one that wanders far (60 to 90): each second is whole or halved, but the
        // stream never runs far over its rate for long.
        let (mut source, mut t) = (Vec::new(), 0.0f64);
        while t < 60.0 {
            source.push(t);
            t += (1.0 + dice.jitter(0.03)) / (75.0 - 15.0 * (t * std::f64::consts::TAU / 20.0).cos());
        }
        let (fewest, most) = per_second(60, &source, 2);
        println!("wandering 60..90: per second {fewest}-{most} {:?}", even(60, &source));
        assert!(fewest >= 30 && most <= 76, "{fewest}-{most}");
    }

    /// A 60 Hz monitor stays a 60 Hz monitor when a few buffers too many come.
    #[test]
    fn sixty_hertz_with_stray_pictures_stays_sixty() {
        let mut dice = Dice(0x57A7);
        let strays = |dice: &mut Dice, a_second: usize| (0..30 * a_second).map(|_| dice.between(0.0, 30.0)).collect::<Vec<_>>();
        let four = with(monitor(60.0, 0.3, 30.0), strays(&mut dice, 4));
        let six = with(monitor(60.0, 0.3, 30.0), strays(&mut dice, 6));
        let bursts = with(monitor(60.0, 0.3, 30.0), (0..60).flat_map(|i| (0..5).map(move |k| 0.004 + i as f64 * 0.5 + k as f64 * 0.001)));
        for (name, source) in [("4 strays", &four), ("6 strays", &six), ("bursts", &bursts)] {
            let seen = even(60, source);
            let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(16_666_666), STILL);
            let mut passes = Vec::new();
            for (t, picture) in buffers(source) {
                if cadence.arrived(Duration::from_secs_f64(t.max(0.0)), picture, false, true).encode && t >= 2.0 {
                    passes.push(t * 1000.0);
                }
            }
            let longest = passes.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
            let (fewest, most) = per_second(60, source, 2);
            println!("60 Hz + {name}: longest {longest:.1} per second {fewest}-{most} {seen:?}");
            assert!(seen.rate > 58.8 && seen.rate < 61.5 && longest < 30.0, "{name}: longest {longest:.1} {seen:?}");
            assert!(matches!(seen.rhythm, Rhythm::All { .. }), "{name}: {seen:?}");
            assert!(fewest >= 57 && most <= 63, "{name}: {fewest}-{most}");
        }
    }

    /// 240 Hz with arrivals up to 3 ms off (most of a refresh): no period read wrong makes
    /// it every fifth refresh.
    #[test]
    fn a_fine_grid_with_coarse_jitter_keeps_its_rate() {
        let source = ragged_monitor(240.0, 0.0, 30.0);
        let seen = even(60, &source);
        let (fewest, most) = per_second(60, &source, 2);
        println!("240 Hz, log jitter: per second {fewest}-{most} {seen:?}");
        assert!(seen.rate > 56.0 && seen.rate < 63.2 && seen.short == 0.0 && seen.long == 0.0 && seen.least > 13.0 && seen.p95 < 21.5, "{seen:?}");
        assert!(fewest >= 54 && most <= 63, "{fewest}-{most}");
    }

    #[test]
    fn forced_unwatched_and_repeated_pictures() {
        let ms = Duration::from_millis;
        let mut cadence = Cadence::new(Policy::Even, ms(16), STILL);
        let pass = Verdict { encode: true, repeat: false, too_early: false };
        // The first picture passes whenever it comes.
        assert_eq!(cadence.arrived(ms(1), 1, false, true), pass);
        // Too early; the keepalive brings it again and it passes then, as a new picture.
        assert_eq!(cadence.arrived(ms(8), 2, false, true), Verdict { encode: false, repeat: false, too_early: true });
        assert_eq!(cadence.arrived(ms(33), 2, false, true), pass);
        // The picture that passed, again: every 100 ms (2 ms early will do).
        assert_eq!(cadence.arrived(ms(58), 2, false, true), Verdict { encode: false, repeat: true, too_early: false });
        assert_eq!(cadence.arrived(ms(131), 2, false, true), Verdict { encode: true, repeat: true, too_early: false });
        // Forced: at once, new or not, too early or not.
        assert_eq!(cadence.arrived(ms(133), 2, true, true), Verdict { encode: true, repeat: true, too_early: false });
        assert_eq!(cadence.arrived(ms(135), 3, true, true), pass);
        // Nobody watching: nothing passes, and nothing counts as left out for the pace.
        assert_eq!(cadence.arrived(ms(400), 4, false, false), Verdict { encode: false, repeat: false, too_early: false });
        assert_eq!(cadence.arrived(ms(425), 4, false, false), Verdict { encode: false, repeat: false, too_early: false });
        // ... but for a forced one.
        assert_eq!(cadence.arrived(ms(450), 4, true, false), pass);
        assert_eq!(cadence.arrived(ms(475), 4, false, true), Verdict { encode: false, repeat: true, too_early: false });
    }

    #[test]
    fn the_old_schedule_is_the_old_schedule() {
        // 144 Hz into 60: 2, 2 and 3 refreshes, 60 a second; and a late picture followed a
        // quarter of a slot later.
        let old = run(Policy::Average, 60, &monitor(144.0, 0.0, 10.0));
        assert!((old.rate - 60.0).abs() < 0.1 && old.p5 < 14.0 && old.p95 > 20.0, "{old:?}");
        let ms = Duration::from_millis;
        let mut cadence = Cadence::new(Policy::Average, ms(16), STILL);
        assert!(cadence.arrived(ms(0), 1, false, true).encode);
        assert!(cadence.arrived(ms(40), 2, false, true).encode);
        assert!(cadence.arrived(ms(41), 3, false, true).too_early);
        assert!(cadence.arrived(ms(44), 4, false, true).encode);
    }

    #[test]
    fn report_says_what_the_gate_settled_on() {
        let mut cadence = Cadence::new(Policy::Even, Duration::from_nanos(16_666_666), STILL);
        let mut announced = Vec::new();
        for (t, picture) in buffers(&monitor(144.0, 0.3, 12.0)) {
            let now = Duration::from_secs_f64(t.max(0.0));
            cadence.arrived(now, picture, false, true);
            if let Some((report, true)) = cadence.take_report(now) {
                announced.push((t, report));
            }
        }
        // Once, after five seconds of pictures.
        assert_eq!(announced.len(), 1);
        let (at, report) = &announced[0];
        assert!(*at > 5.0 && *at < 7.0);
        // (The first quarter second, before the rhythm is known, is in it.)
        assert!((report.passed_per_s - 48.0).abs() < 0.5 && report.short + report.long <= 2 && report.repeats == 0, "{report:?}");
        let line = report.to_string();
        assert!(line.contains("source brings 14") && line.contains("-> at most one picture per 3 grid steps (4") && line.contains("pictures/s passed (nominal 60)"), "{line}");
    }

    /// `cargo test --release --lib -- cadence_table --nocapture` prints old against new for
    /// every simulated source.
    #[test]
    fn cadence_table() {
        let sources: Vec<(&str, u32, Source)> = vec![
            ("144 Hz monitor", 60, monitor(144.0, 0.3, 20.0)),
            ("165 Hz monitor", 60, monitor(165.0, 0.3, 20.0)),
            ("120 Hz monitor", 60, monitor(120.0, 0.3, 20.0)),
            ("100 Hz monitor", 60, monitor(100.0, 0.3, 20.0)),
            ("75 Hz monitor", 60, monitor(75.0, 0.3, 20.0)),
            ("60 Hz monitor", 60, monitor(60.0, 0.3, 20.0)),
            ("59.94 Hz monitor", 60, monitor(59.94, 0.3, 20.0)),
            ("144 Hz, 8 % empty, log jitter (the log rebuilt)", 60, ragged_monitor(144.0, 0.08, 60.0)),
            ("144 Hz, 15 % empty, log jitter", 60, ragged_monitor(144.0, 0.15, 60.0)),
            ("144 Hz, game every 1 or 2 refreshes", 60, game_on_144(0.5, 30.0)),
            ("240 Hz monitor, log jitter", 60, ragged_monitor(240.0, 0.0, 30.0)),
            ("144 Hz, game mostly every 2nd refresh", 60, game_on_144(0.8, 30.0)),
            ("144 Hz, film at 60 (2-2-3)", 60, film_on_144(20.0)),
            ("variable refresh 48-144, every step random", 60, variable_refresh(false, 30.0)),
            ("variable refresh 48-144, wandering", 60, variable_refresh(true, 30.0)),
            ("variable refresh, game holding 70", 60, steady_game(70.0, 20.0)),
            ("60 Hz monitor into a stream of 30", 30, monitor(60.0, 0.3, 20.0)),
            ("still screen, cursor now and then (144 Hz)", 60, cursor_now_and_then(30.0)),
        ];
        println!("{:46} gate  rate/s  step p5 /  p50 /  p95 ms   <0.7 med   >1.5 med   uneven next  settled on", "source");
        for (name, fps, source) in &sources {
            for (label, policy) in [("old", Policy::Average), ("new", Policy::Even)] {
                let seen = run(policy, *fps, source);
                let settled = match (policy, seen.rhythm) {
                    (Policy::Average, _) => String::new(),
                    (_, Rhythm::Grid { period, every }) => format!("{:.1} Hz, every {every}", 1.0 / period),
                    (_, Rhythm::All { step }) => format!("{:.1}/s, all pass", 1.0 / step),
                    (_, other) => format!("{other:?}"),
                };
                println!(
                    "{:46} {label}   {:6.1}   {:6.1} / {:5.1} / {:5.1}      {:5.1} %    {:5.1} %    {:5.1} %      {settled}",
                    name,
                    seen.rate,
                    seen.p5,
                    seen.p50,
                    seen.p95,
                    seen.short * 100.0,
                    seen.long * 100.0,
                    seen.uneven * 100.0
                );
            }
        }
    }
}
