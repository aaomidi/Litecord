//! Automatic microphone gain for outgoing voice.
//!
//! Official Discord clients run WebRTC's automatic gain on the microphone, so a quiet
//! microphone is still heard at a normal level. Litecord used to send the microphone as it
//! came (its old gain stage could add 8 dB at most). This one follows the shape of WebRTC's
//! AGC2 adaptive digital controller: estimate the level of speech (never of the noise
//! between words), move a slow gain towards a target, and catch what is left with a limiter.
//!
//! All state is per call and lives on the voice send task: one frame of 960 samples (20 ms
//! at 48 kHz, mono) in, the same frame out, 5 ms late (the limiter's look-ahead).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

const FRAME: usize = 960;
const FRAME_SECONDS: f32 = 0.02;

/// Where loud speech frames should end up (dBFS RMS). With ordinary speech this puts the
/// active speech level around -21 dBFS, the range a WebRTC sender produces.
const TARGET_SPEECH_DB: f32 = -19.0;
/// Where the usual peaks of speech should end up. The limiter's ceiling is 1 dB above.
const TARGET_PEAK_DB: f32 = -2.0;
const LIMITER_CEILING_DB: f32 = -1.0;
/// Gain range. WebRTC allows up to 50 dB; 30 is enough for a microphone 10 dB quieter than
/// the quietest one measured, and keeps a wrong estimate from doing much harm.
const MAX_GAIN_DB: f32 = 30.0;
const MIN_GAIN_DB: f32 = -12.0;
/// Noise between words is never raised above this (AGC2: max_output_noise_level_dbfs).
const MAX_NOISE_OUT_DB: f32 = -50.0;
/// Gain at the start of the first call. Later calls start from where the last one ended.
const INITIAL_GAIN_DB: f32 = 6.0;
/// Gain moves this fast at most, and only while speech is present (AGC2 raises 6 dB/s).
const GAIN_UP_DB_PER_S: f32 = 6.0;
/// Until the gain has first reached what the estimates ask for, it may rise twice as fast:
/// the first words of a quiet microphone should not stay quiet for five seconds.
const FIRST_GAIN_UP_DB_PER_S: f32 = 12.0;
const GAIN_DOWN_DB_PER_S: f32 = 20.0;
/// A frame is speech when it is this far above the noise floor and above the absolute
/// minimum; estimates and gain only move after this many speech frames in a row (100 ms),
/// so a key click or a door does not count.
const SPEECH_OVER_NOISE_DB: f32 = 15.0;
const SPEECH_MIN_DB: f32 = -60.0;
const CONFIDENT_SPEECH_FRAMES: u32 = 5;
/// Speech frames of a call during which the estimates move fast in both directions (1 s).
const LEARNING_FRAMES: u32 = 50;

/// The gain in use, as a linear factor, for the level meter (1.0 while the gain is off).
static CURRENT_GAIN: AtomicU32 = AtomicU32::new(0x3F80_0000);
/// Where the next call starts (dB as f32 bits; NaN = no call yet).
static REMEMBERED_GAIN_DB: AtomicU32 = AtomicU32::new(0x7FC0_0000);

/// The automatic gain being applied right now as a linear factor.
pub fn current_gain() -> f32 {
    f32::from_bits(CURRENT_GAIN.load(Ordering::Relaxed))
}

fn publish_gain(lin: f32) {
    CURRENT_GAIN.store(lin.to_bits(), Ordering::Relaxed);
}

/// Called while the automatic gain is switched off, so the meter shows the plain level.
pub fn publish_gain_off() {
    publish_gain(1.0);
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// What the gain did since the last report, for the periodic log line.
#[derive(Debug, Clone, Copy, Default)]
pub struct GainReport {
    pub speech_frames: u32,
    /// Power means over the speech frames of the period (dBFS); NaN without speech.
    pub speech_in_db: f32,
    pub speech_out_db: f32,
    pub noise_db: f32,
    pub gain_db: f32,
    /// Deepest reduction by the limiter in the period (dB, 0 = never worked) and how many
    /// frames it worked in.
    pub limiter_max_db: f32,
    pub limited_frames: u32,
}

pub struct AutoGain {
    noise_db: f32,
    speech_db: f32,
    peak_db: f32,
    gain_db: f32,
    applied: f32,
    speech_run: u32,
    learned_frames: u32,
    settled: bool,
    limiter: Limiter,
    // Period statistics.
    speech_frames: u32,
    in_power: f64,
    out_power: f64,
    limiter_min: f32,
    limited_frames: u32,
}

impl AutoGain {
    pub fn new() -> Self {
        let remembered = f32::from_bits(REMEMBERED_GAIN_DB.load(Ordering::Relaxed));
        let gain_db = if remembered.is_finite() { remembered.clamp(MIN_GAIN_DB, MAX_GAIN_DB) } else { INITIAL_GAIN_DB };
        Self {
            noise_db: -60.0,
            // Estimates that agree with the starting gain, so it does not move before
            // there is evidence.
            speech_db: TARGET_SPEECH_DB - gain_db,
            peak_db: (TARGET_PEAK_DB - gain_db).min(-6.0),
            gain_db,
            applied: lin(gain_db),
            speech_run: 0,
            learned_frames: 0,
            // A gain carried over from the last call is where it should be already.
            settled: remembered.is_finite(),
            limiter: Limiter::new(lin(LIMITER_CEILING_DB)),
            speech_frames: 0,
            in_power: 0.0,
            out_power: 0.0,
            limiter_min: 1.0,
            limited_frames: 0,
        }
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// One 20 ms frame, in place.
    pub fn process(&mut self, frame: &mut [f32; FRAME]) {
        let mut sum_sq = 0.0f32;
        let mut peak = 0.0f32;
        for &s in frame.iter() {
            let s = if s.is_finite() { s } else { 0.0 };
            sum_sq += s * s;
            peak = peak.max(s.abs());
        }
        let in_power = sum_sq / FRAME as f32;
        let rms_db = db(in_power.sqrt());
        let peak_db = db(peak);

        let is_speech = rms_db > self.noise_db + SPEECH_OVER_NOISE_DB && rms_db > SPEECH_MIN_DB;
        // Noise floor: follows quiet frames down quickly, creeps up (0.5 dB/s) on frames
        // that are not speech, so a fan that starts is learned and speech is not.
        if rms_db < self.noise_db {
            self.noise_db += 0.3 * (rms_db - self.noise_db);
        } else if !is_speech {
            self.noise_db = (self.noise_db + 0.5 * FRAME_SECONDS).min(rms_db);
        }
        self.noise_db = self.noise_db.clamp(-96.0, -30.0);

        self.speech_run = if is_speech { self.speech_run + 1 } else { 0 };
        let confident = self.speech_run >= CONFIDENT_SPEECH_FRAMES;
        if confident {
            // The first second of speech of a call sets both estimates quickly, whichever
            // way they are off. After that: the level of the louder speech frames (up with
            // 0.5 s, down with 4 s) and the usual peaks (up with 0.4 s, since a short shout
            // is the limiter's job; down with 5 s).
            let learning = self.learned_frames < LEARNING_FRAMES;
            self.learned_frames = self.learned_frames.saturating_add(1);
            let k = if learning { 0.08 } else if rms_db > self.speech_db { 0.039 } else { 0.005 };
            self.speech_db += k * (rms_db - self.speech_db);
            let k = if learning { 0.08 } else if peak_db > self.peak_db { 0.049 } else { 0.004 };
            self.peak_db += k * (peak_db - self.peak_db);
        }

        let wanted = (TARGET_SPEECH_DB - self.speech_db)
            .min(TARGET_PEAK_DB - self.peak_db)
            .min(MAX_NOISE_OUT_DB - self.noise_db)
            .clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        if confident || wanted < self.gain_db && !is_speech && self.noise_db + self.gain_db > MAX_NOISE_OUT_DB {
            // Speech moves the gain both ways; without speech it only comes down, and only
            // when the noise would otherwise be too loud.
            if (wanted - self.gain_db).abs() < 1.0 {
                self.settled = true;
            }
            let up = if self.settled { GAIN_UP_DB_PER_S } else { FIRST_GAIN_UP_DB_PER_S };
            let step = if wanted > self.gain_db {
                (wanted - self.gain_db).min(up * FRAME_SECONDS)
            } else {
                (wanted - self.gain_db).max(-GAIN_DOWN_DB_PER_S * FRAME_SECONDS)
            };
            self.gain_db += step;
        }

        // The gain glides over the frame: a step at a frame edge every 20 ms is a buzz.
        let from = self.applied;
        let to = lin(self.gain_db);
        let mut out_sq = 0.0f32;
        let mut min_limit = 1.0f32;
        for (i, s) in frame.iter_mut().enumerate() {
            let g = from + (to - from) * ((i + 1) as f32 / FRAME as f32);
            let x = if s.is_finite() { *s * g } else { 0.0 };
            let (y, limit) = self.limiter.process(x);
            min_limit = min_limit.min(limit);
            out_sq += y * y;
            *s = y;
        }
        self.applied = to;
        publish_gain(to);
        REMEMBERED_GAIN_DB.store(self.gain_db.to_bits(), Ordering::Relaxed);

        if is_speech {
            self.speech_frames += 1;
            self.in_power += in_power as f64;
            self.out_power += (out_sq / FRAME as f32) as f64;
        }
        if min_limit < 0.999 {
            self.limited_frames += 1;
            self.limiter_min = self.limiter_min.min(min_limit);
        }
    }

    /// The statistics since the last call, and a new period.
    pub fn take_report(&mut self) -> GainReport {
        let mean_db = |p: f64, n: u32| if n == 0 { f32::NAN } else { db((p / n as f64).sqrt() as f32) };
        let report = GainReport {
            speech_frames: self.speech_frames,
            speech_in_db: mean_db(self.in_power, self.speech_frames),
            speech_out_db: mean_db(self.out_power, self.speech_frames),
            noise_db: self.noise_db,
            gain_db: self.gain_db,
            limiter_max_db: db(self.limiter_min),
            limited_frames: self.limited_frames,
        };
        self.speech_frames = 0;
        self.in_power = 0.0;
        self.out_power = 0.0;
        self.limiter_min = 1.0;
        self.limited_frames = 0;
        report
    }
}

/// Look-ahead of the limiter: 5 ms. A peak is known this long before it leaves, so the gain
/// reaches the needed value on a straight line instead of jumping.
const LOOKAHEAD: usize = 240;
/// The limiter lets go with a time constant of 60 ms (per-sample coefficient at 48 kHz).
const LIMITER_RELEASE: f32 = 3.47e-4;

struct Limiter {
    ceiling: f32,
    delay: [f32; LOOKAHEAD],
    pos: usize,
    n: u64,
    /// Candidates for the minimum of the needed gain over the last LOOKAHEAD + 1 samples.
    minima: VecDeque<(u64, f32)>,
    released: f32,
    window: [f32; LOOKAHEAD],
    window_sum: f64,
}

impl Limiter {
    fn new(ceiling: f32) -> Self {
        Self {
            ceiling,
            delay: [0.0; LOOKAHEAD],
            pos: 0,
            n: 0,
            minima: VecDeque::with_capacity(LOOKAHEAD + 2),
            released: 1.0,
            window: [1.0; LOOKAHEAD],
            window_sum: LOOKAHEAD as f64,
        }
    }

    /// One sample in, the sample from 5 ms ago out, with the gain it was given.
    fn process(&mut self, x: f32) -> (f32, f32) {
        let need = if x.abs() > self.ceiling { self.ceiling / x.abs() } else { 1.0 };
        while self.minima.back().is_some_and(|&(_, g)| g >= need) {
            self.minima.pop_back();
        }
        self.minima.push_back((self.n, need));
        while self.minima.front().is_some_and(|&(at, _)| at + (LOOKAHEAD as u64) < self.n) {
            self.minima.pop_front();
        }
        let lowest = self.minima.front().map_or(1.0, |&(_, g)| g);
        // Down at once, up slowly, never above what the window needs.
        self.released = lowest.min(self.released + (1.0 - self.released) * LIMITER_RELEASE);
        // The mean over the look-ahead turns the step into a ramp that is complete when
        // the peak comes out of the delay.
        self.window_sum += (self.released - self.window[self.pos]) as f64;
        self.window[self.pos] = self.released;
        let gain = (self.window_sum / LOOKAHEAD as f64) as f32;
        let out = (self.delay[self.pos] * gain).clamp(-self.ceiling, self.ceiling);
        self.delay[self.pos] = x;
        self.pos = (self.pos + 1) % LOOKAHEAD;
        self.n += 1;
        (out, gain.min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like test signal: a 180 Hz tone with harmonics in syllables of 200 ms with
    /// 100 ms pauses, over steady noise.
    fn speech(seconds: f32, speech_db: f32, noise_db: f32) -> Vec<f32> {
        let n = (seconds * 48000.0) as usize;
        let mut seed = 12345u32;
        (0..n).map(|i| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((seed >> 8) as f32 / 8388608.0 - 1.0) * lin(noise_db) * 1.73;
            let t = i as f32 / 48000.0;
            let in_syllable = (t % 0.3) < 0.2;
            let tone = if in_syllable {
                let w = 2.0 * std::f32::consts::PI * 180.0 * t;
                (w.sin() + 0.5 * (2.0 * w).sin() + 0.3 * (5.0 * w).sin()) * lin(speech_db) * 1.2
            } else {
                0.0
            };
            tone + noise
        }).collect()
    }

    fn run(gain: &mut AutoGain, signal: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(signal.len());
        for chunk in signal.chunks_exact(FRAME) {
            let mut frame = [0.0f32; FRAME];
            frame.copy_from_slice(chunk);
            gain.process(&mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    fn fresh() -> AutoGain {
        REMEMBERED_GAIN_DB.store(f32::NAN.to_bits(), Ordering::Relaxed);
        AutoGain::new()
    }

    #[test]
    fn a_quiet_microphone_is_raised_and_its_noise_stays_low() {
        let mut gain = fresh();
        let out = run(&mut gain, &speech(12.0, -35.0, -72.0));
        assert!((11.0..=19.0).contains(&gain.gain_db()), "gain {}", gain.gain_db());
        // The pauses of the last seconds: noise raised by the gain, still far below speech.
        let tail = &out[out.len() - 48000..];
        let quiet: Vec<f32> = tail.chunks_exact(480).map(|c| db((c.iter().map(|s| s * s).sum::<f32>() / 480.0).sqrt())).collect();
        let floor = quiet.iter().cloned().fold(f32::INFINITY, f32::min);
        assert!(floor < -50.0, "noise between words at {floor} dBFS");
        assert!(out.iter().all(|s| s.abs() <= lin(LIMITER_CEILING_DB) + 1e-6));
    }

    #[test]
    fn noise_and_clicks_alone_do_not_move_the_gain() {
        let mut gain = fresh();
        let mut signal = speech(20.0, -200.0, -65.0);
        for click in 0..40 {
            let at = click * 24000 + 1000;
            for i in 0..96 {
                signal[at + i] += 0.5 * (1.0 - i as f32 / 96.0);
            }
        }
        run(&mut gain, &signal);
        assert!((gain.gain_db() - INITIAL_GAIN_DB).abs() < 0.01, "gain {}", gain.gain_db());
    }

    #[test]
    fn a_hot_microphone_is_turned_down_and_never_clips() {
        let mut gain = fresh();
        let out = run(&mut gain, &speech(8.0, -6.0, -60.0));
        assert!(gain.gain_db() < 0.0, "gain {}", gain.gain_db());
        assert!(out.iter().all(|s| s.abs() <= lin(LIMITER_CEILING_DB) + 1e-6));
    }

    #[test]
    fn the_limiter_ramps_into_a_peak_without_overshoot() {
        let mut limiter = Limiter::new(0.5);
        let mut signal = vec![0.1f32; 2000];
        signal[1000] = 2.0;
        let out: Vec<(f32, f32)> = signal.iter().map(|&x| limiter.process(x)).collect();
        assert!(out.iter().all(|(y, _)| y.abs() <= 0.5 + 1e-6));
        // The peak comes out LOOKAHEAD samples later, at the ceiling.
        assert!((out[1000 + LOOKAHEAD].0 - 0.5).abs() < 1e-3, "{}", out[1000 + LOOKAHEAD].0);
        // The gain changes by small steps only (no click): at most 1/LOOKAHEAD per sample.
        let worst = out.windows(2).map(|w| (w[1].1 - w[0].1).abs()).fold(0.0f32, f32::max);
        assert!(worst <= 1.0 / LOOKAHEAD as f32 + 1e-6, "gain step {worst}");
    }

    #[test]
    fn broken_input_gives_silence_not_noise() {
        let mut gain = fresh();
        let mut frame = [f32::NAN; FRAME];
        frame[10] = f32::INFINITY;
        gain.process(&mut frame);
        gain.process(&mut frame);
        assert!(frame.iter().all(|s| *s == 0.0));
    }
}
