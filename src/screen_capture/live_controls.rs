//! Stream quality of the share that is running (the button on the streamer's own tile).
//!
//! A share starts from the saved settings (`video_settings`: Settings > Stream quality) and
//! keeps its own copy here: the maximum bitrate and the quality floor can be changed while
//! it runs, for this share only. The next share starts from the saved settings again,
//! unless `save_as_default` wrote the share's values there.
//!
//! - Maximum bitrate: the ceiling of the send loop's `watch::RateControl` (`turn`).
//! - Quality floor: `min-qp` of vah264enc, which the element does not take while it runs
//!   (gst-inspect-1.0 lists `bitrate` and `cpb-size` as "changeable in NULL, READY, PAUSED
//!   or PLAYING state", `min-qp` and `max-qp` only as "readable, writable"), so each capture
//!   path replaces its encoder: `linux_gpu_tx` swaps the encoder section, the memory-fed
//!   `VaapiEncoder` restarts its pipeline. `EncoderFloor` keeps that to one replacement at
//!   a time and remembers what to go back to.
//! - A slider that is being dragged reports every step: a value counts once it has been
//!   still for `SETTLE_AFTER`, or when the slider is released (`Settled`).

use super::watch::RateControl;
use crate::video_settings as settings;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a value has to stay the same before it is applied without a release.
pub const SETTLE_AFTER: Duration = Duration::from_millis(400);

/// A value a slider moves: what counts is `poll`, the last value that was released or
/// stayed still for `SETTLE_AFTER`.
#[derive(Clone, Debug)]
pub struct Settled {
    value: u32,
    moving: Option<(u32, Instant)>,
}

impl Settled {
    pub fn new(value: u32) -> Self {
        Self { value, moving: None }
    }

    /// A step of the slider. The same value again does not start the wait over.
    pub fn set(&mut self, value: u32, now: Instant) {
        if self.moving.map(|(v, _)| v) != Some(value) {
            self.moving = (value != self.value).then_some((value, now));
        }
    }

    /// The slider was released (or the value was typed, or came from a script).
    pub fn commit(&mut self) {
        if let Some((value, _)) = self.moving.take() {
            self.value = value;
        }
    }

    /// The value in effect at `now`.
    pub fn poll(&mut self, now: Instant) -> u32 {
        if self.moving.is_some_and(|(_, since)| now.saturating_duration_since(since) >= SETTLE_AFTER) {
            self.commit();
        }
        self.value
    }

    /// What the slider shows: the value on its way, if there is one.
    pub fn shown(&self) -> u32 {
        self.moving.map_or(self.value, |(v, _)| v)
    }

    pub fn is_moving(&self) -> bool {
        self.moving.is_some()
    }
}

/// The quality floor of one encoder path: which floor the encoder in use was made with,
/// and the one replacement that may be on its way. A replacement is "in flight" from the
/// moment it is decided until the new encoder's first keyframe (`proven`) or its failure
/// (`failed`); until then no other is started, whatever is asked for.
#[derive(Clone, Debug)]
pub struct EncoderFloor {
    /// Floor of the last encoder that showed its keyframe.
    applied: u32,
    /// Floor of the encoder being tried, and since when.
    trying: Option<(u32, Instant)>,
    /// A floor whose encoder failed: not tried again until something else is asked for.
    refused: Option<u32>,
}

impl EncoderFloor {
    pub fn new(applied: u32) -> Self {
        Self { applied, trying: None, refused: None }
    }

    /// The floor to replace the encoder with now, when `wanted` calls for it.
    pub fn next(&mut self, wanted: u32, now: Instant) -> Option<u32> {
        if self.trying.is_some() {
            return None;
        }
        if self.refused.is_some_and(|r| r != wanted) {
            self.refused = None;
        }
        if wanted == self.applied || self.refused == Some(wanted) {
            return None;
        }
        self.trying = Some((wanted, now));
        Some(wanted)
    }

    /// The replacement on its way: its floor and when it began.
    pub fn in_flight(&self) -> Option<(u32, Instant)> {
        self.trying
    }

    /// The floor of the encoder that proved itself last.
    pub fn applied(&self) -> u32 {
        self.applied
    }

    /// The new encoder put out its keyframe: (old floor, new floor, when it began).
    pub fn proven(&mut self) -> Option<(u32, u32, Instant)> {
        let (new, since) = self.trying.take()?;
        let old = std::mem::replace(&mut self.applied, new);
        Some((old, new, since))
    }

    /// The new encoder failed: (the floor that failed, the floor to go back to).
    pub fn failed(&mut self) -> Option<(u32, u32)> {
        let (new, _) = self.trying.take()?;
        self.refused = Some(new);
        Some((new, self.applied))
    }
}

/// Which encoder a floor report comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// No VA-API encoder has reported (another encoder, or nobody has watched yet).
    Unknown,
    /// `linux_gpu_tx`: captured and encoded on the GPU inside this process.
    GpuCapture,
    /// `encoder::vaapi::VaapiEncoder`: frames from memory (gst-launch or window capture).
    Memory,
}

impl Path {
    pub fn name(self) -> &'static str {
        match self {
            Path::Unknown => "no VA-API encoder yet",
            Path::GpuCapture => "GPU capture",
            Path::Memory => "memory-fed encoder",
        }
    }

    fn id(self) -> u8 {
        self as u8
    }

    fn from_id(id: u8) -> Self {
        match id {
            1 => Path::GpuCapture,
            2 => Path::Memory,
            _ => Path::Unknown,
        }
    }
}

/// The values of one share.
#[derive(Clone, Debug)]
pub struct Share {
    max_bitrate_mbps: Settled,
    min_qp: Settled,
    /// Floor of the last encoder of this share that showed its keyframe: what an encoder
    /// made later (a handover between the capture paths) starts with.
    proven_min_qp: u32,
    /// Counts the changes that did not come from the sliders (a floor that was refused).
    revision: u32,
}

fn clamp_mbps(mbps: u32) -> u32 {
    mbps.clamp(settings::STREAM_MAX_BITRATE_MBPS_RANGE.0, settings::STREAM_MAX_BITRATE_MBPS_RANGE.1)
}

fn clamp_qp(qp: u32) -> u32 {
    qp.clamp(settings::STREAM_MIN_QP_RANGE.0, settings::STREAM_MIN_QP_RANGE.1)
}

impl Share {
    /// A share starts from the saved settings.
    pub fn new(default_mbps: u32, default_min_qp: u32) -> Self {
        let (mbps, qp) = (clamp_mbps(default_mbps), clamp_qp(default_min_qp));
        Self { max_bitrate_mbps: Settled::new(mbps), min_qp: Settled::new(qp), proven_min_qp: qp, revision: 0 }
    }

    /// `released`: not a step of a slider that is still held.
    pub fn set_max_bitrate_mbps(&mut self, mbps: u32, released: bool, now: Instant) {
        self.max_bitrate_mbps.set(clamp_mbps(mbps), now);
        if released {
            self.max_bitrate_mbps.commit();
        }
    }

    pub fn set_min_qp(&mut self, qp: u32, released: bool, now: Instant) {
        self.min_qp.set(clamp_qp(qp), now);
        if released {
            self.min_qp.commit();
        }
    }

    /// What the engine works with at `now`: (maximum bitrate in bit/s, quality floor).
    pub fn in_effect(&mut self, now: Instant) -> (u32, u32) {
        (self.max_bitrate_mbps.poll(now) * 1_000_000, self.min_qp.poll(now))
    }

    /// What the controls show: (maximum bitrate in Mbit/s, quality floor).
    pub fn shown(&self) -> (u32, u32) {
        (self.max_bitrate_mbps.shown(), self.min_qp.shown())
    }

    /// An encoder could not be made with floor `refused`: the share is back on `kept`,
    /// unless something else has been asked for since.
    fn floor_refused(&mut self, refused: u32, kept: u32) {
        if self.min_qp.shown() == refused {
            self.min_qp = Settled::new(kept);
            self.revision += 1;
        }
    }
}

static SHARE: Mutex<Option<Share>> = Mutex::new(None);
// What is in effect, written by the send loop (`turn`) and by the encoders.
static TARGET_BPS: AtomicU32 = AtomicU32::new(0);
static WEB_CAPPED: AtomicBool = AtomicBool::new(false);
static WANT_MAIN: AtomicBool = AtomicBool::new(false);
static ENCODER_PATH: AtomicU8 = AtomicU8::new(0);
static ENCODER_MAIN: AtomicBool = AtomicBool::new(false);
static ENCODER_MIN_QP: AtomicU32 = AtomicU32::new(0);
static WEB_CAP_BPS: AtomicU32 = AtomicU32::new(0);
static WATCHED: AtomicBool = AtomicBool::new(false);

fn share() -> std::sync::MutexGuard<'static, Option<Share>> {
    SHARE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A share starts: its values are the saved settings. Returns the maximum bitrate (bit/s).
pub fn begin_share() -> u32 {
    let new = Share::new(settings::get_stream_max_bitrate_mbps(), settings::get_stream_min_qp());
    let max_bitrate = new.max_bitrate_mbps.value * 1_000_000;
    ENCODER_PATH.store(Path::Unknown.id(), Ordering::Relaxed);
    ENCODER_MIN_QP.store(0, Ordering::Relaxed);
    TARGET_BPS.store(max_bitrate, Ordering::Relaxed);
    WEB_CAPPED.store(false, Ordering::Relaxed);
    WATCHED.store(false, Ordering::Relaxed);
    *share() = Some(new);
    max_bitrate
}

pub fn end_share() {
    *share() = None;
}

/// False when no share is running: there is nothing to change.
pub fn set_max_bitrate_mbps(mbps: u32, released: bool) -> bool {
    share().as_mut().map(|s| s.set_max_bitrate_mbps(mbps, released, Instant::now())).is_some()
}

pub fn set_min_qp(qp: u32, released: bool) -> bool {
    share().as_mut().map(|s| s.set_min_qp(qp, released, Instant::now())).is_some()
}

/// Writes this share's values to the settings: the next shares start from them. The
/// quality floor only `with_floor`: where the controls do not show it (no VA-API encoder in
/// the build) the saved one is left alone. Returns (maximum bitrate in Mbit/s, quality
/// floor) as they are in the settings now.
pub fn save_as_default(with_floor: bool) -> Option<(u32, u32)> {
    let (mbps, qp) = {
        let mut guard = share();
        let s = guard.as_mut()?;
        s.max_bitrate_mbps.commit();
        s.min_qp.commit();
        s.shown()
    };
    settings::set_stream_max_bitrate_mbps(mbps);
    if with_floor {
        settings::set_stream_min_qp(qp);
        info!("🎚️ [LIVE] Stream controls saved as the default for new shares: max bitrate {} Mbit/s, quality floor (min-qp) {}", mbps, qp);
    } else {
        info!("🎚️ [LIVE] Stream controls saved as the default for new shares: max bitrate {} Mbit/s", mbps);
    }
    Some((mbps, settings::get_stream_min_qp()))
}

/// Whether somebody watches the share now (told by the send loop): only then are pictures
/// encoded, and only then can an encoder take a new floor.
pub fn set_watched(watched: bool) {
    WATCHED.store(watched, Ordering::Relaxed);
}

/// The send loop's turn. Moves the rate controller's ceiling to the share's maximum
/// bitrate and returns the quality floor the encoder should have: 0 when no share is
/// running (the loop's last turn after `end_share`), which asks nothing of an encoder.
/// `rate_cap`: the limit a browser viewer puts on the rate (`u32::MAX` without one), which
/// still wins; `efficient`: the profile asked of the encoder this turn.
pub fn turn(rate: &mut RateControl, rate_cap: u32, efficient: bool, now: Instant) -> u32 {
    let mut guard = share();
    let Some(s) = guard.as_mut() else { return 0 };
    let (ceiling, min_qp) = s.in_effect(now);
    drop(guard);
    if ceiling != rate.max() {
        let (old_ceiling, before) = (rate.max(), rate.current().min(rate_cap));
        let after = rate.set_max(ceiling).min(rate_cap);
        if after != before {
            // vah264enc takes a new rate with a keyframe: not at the next periodic one.
            super::types::request_intra_keyframe();
        }
        let capped = if rate.current() > rate_cap { format!(", held at {:.0} Mbit/s while a browser viewer watches", rate_cap as f64 / 1e6) } else { String::new() };
        info!(
            "🎚️ [LIVE] Max bitrate of this share: {} -> {} Mbit/s; target {:.1} -> {:.1} Mbit/s{} ({}, no encoder replaced)",
            old_ceiling / 1_000_000,
            ceiling / 1_000_000,
            before as f64 / 1e6,
            after as f64 / 1e6,
            capped,
            Path::from_id(ENCODER_PATH.load(Ordering::Relaxed)).name()
        );
    }
    TARGET_BPS.store(rate.current().min(rate_cap), Ordering::Relaxed);
    WEB_CAPPED.store(rate.current() > rate_cap, Ordering::Relaxed);
    if rate_cap != u32::MAX {
        WEB_CAP_BPS.store(rate_cap, Ordering::Relaxed);
    }
    WANT_MAIN.store(efficient, Ordering::Relaxed);
    min_qp
}

/// The quality floor a new encoder of the running share starts with: the last one an
/// encoder of this share proved (the saved setting before any change, and with no share).
/// A floor asked for since then reaches the new encoder like any change does.
pub fn min_qp_for_new_encoder() -> u32 {
    share().as_ref().map_or_else(settings::get_stream_min_qp, |s| s.proven_min_qp)
}

/// An encoder showed its first keyframe: what the viewers are being sent.
pub fn encoder_in_effect(path: Path, main: bool, min_qp: u32) {
    ENCODER_PATH.store(path.id(), Ordering::Relaxed);
    ENCODER_MAIN.store(main, Ordering::Relaxed);
    ENCODER_MIN_QP.store(min_qp, Ordering::Relaxed);
}

/// The encoder made for a new floor showed its keyframe. `gap`: how long the viewers were
/// without pictures, when a running encoder was replaced.
pub fn floor_applied(path: Path, old: u32, new: u32, gap: Option<Duration>) {
    if let Some(s) = share().as_mut() {
        s.proven_min_qp = new;
    }
    ENCODER_MIN_QP.store(new, Ordering::Relaxed);
    match gap {
        Some(gap) => info!(
            "🎚️ [LIVE] Quality floor of this share: min-qp {} -> {} ({}, encoder replaced, {:.0} ms without pictures)",
            old,
            new,
            path.name(),
            gap.as_secs_f64() * 1000.0
        ),
        None => info!("🎚️ [LIVE] Quality floor of this share: min-qp {} -> {} ({}, encoder started with it, no picture was being sent)", old, new, path.name()),
    }
}

/// The encoder for floor `wanted` failed: the share goes on with `kept`.
pub fn floor_refused(path: Path, wanted: u32, kept: u32, why: &str) {
    if let Some(s) = share().as_mut() {
        s.floor_refused(wanted, kept);
    }
    warn!("⚠️ [LIVE] Quality floor of this share stays at min-qp {}: the encoder for min-qp {} failed ({}, {})", kept, wanted, path.name(), why);
}

/// What the controls show and what is in effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub sharing: bool,
    /// This share's values, as the controls show them (a slider on its way included).
    pub max_bitrate_mbps: u32,
    pub min_qp: u32,
    /// A value is still waiting to settle.
    pub moving: bool,
    pub revision: u32,
    /// The rate asked of the encoder now.
    pub target_bps: u32,
    /// A browser viewer holds the rate below what the share would use, and at how much.
    pub web_capped: bool,
    pub web_cap_bps: u32,
    /// Somebody watches: pictures are being encoded.
    pub watched: bool,
    /// H.264 Main ("efficient") rather than Constrained Baseline ("compatible").
    pub main: bool,
    pub encoder: Path,
    /// Floor of the encoder in use; 0 when no VA-API encoder has reported one.
    pub encoder_min_qp: u32,
}

pub fn status() -> Status {
    let guard = share();
    let encoder = Path::from_id(ENCODER_PATH.load(Ordering::Relaxed));
    let (max_bitrate_mbps, min_qp) = guard.as_ref().map_or_else(|| (settings::get_stream_max_bitrate_mbps(), settings::get_stream_min_qp()), |s| s.shown());
    Status {
        sharing: guard.is_some(),
        max_bitrate_mbps,
        min_qp,
        moving: guard.as_ref().is_some_and(|s| s.max_bitrate_mbps.is_moving() || s.min_qp.is_moving()),
        revision: guard.as_ref().map_or(0, |s| s.revision),
        target_bps: TARGET_BPS.load(Ordering::Relaxed),
        web_capped: WEB_CAPPED.load(Ordering::Relaxed),
        web_cap_bps: WEB_CAP_BPS.load(Ordering::Relaxed),
        watched: WATCHED.load(Ordering::Relaxed),
        main: if encoder == Path::Unknown { WANT_MAIN.load(Ordering::Relaxed) } else { ENCODER_MAIN.load(Ordering::Relaxed) },
        encoder,
        encoder_min_qp: ENCODER_MIN_QP.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBIT: u32 = 1_000_000;

    #[test]
    fn ceiling_down_takes_the_rate_down_at_once() {
        let mut r = RateControl::new(30 * MBIT);
        assert_eq!(r.set_max(12 * MBIT), 12 * MBIT);
        assert_eq!((r.max(), r.current()), (12 * MBIT, 12 * MBIT));
        // A rate that loss had already brought below the new ceiling stays where it is.
        let mut r = RateControl::new(30 * MBIT);
        let t0 = Instant::now();
        r.update(Some(200), t0);
        r.update(Some(200), t0 + Duration::from_secs(1));
        assert_eq!(r.current(), 19_200_000);
        assert_eq!(r.set_max(25 * MBIT), 19_200_000);
        assert_eq!(r.set_max(10 * MBIT), 10 * MBIT);
    }

    #[test]
    fn ceiling_up_jumps_from_the_old_ceiling_and_climbs_after_loss() {
        // At its ceiling: nothing was holding the rate down, it goes to the new one.
        let mut r = RateControl::new(8 * MBIT);
        assert_eq!(r.set_max(30 * MBIT), 30 * MBIT);
        // Brought down by loss: it stays, and climbs at the usual pace to the new ceiling.
        let mut r = RateControl::new(8 * MBIT);
        let t0 = Instant::now();
        r.update(Some(200), t0);
        assert_eq!(r.set_max(30 * MBIT), 6_400_000);
        let mut t = t0;
        for _ in 0..5 {
            t += Duration::from_secs(1);
            r.update(Some(0), t);
        }
        assert_eq!(r.current(), 6_400_000);
        r.update(Some(0), t + Duration::from_secs(1));
        assert_eq!(r.current(), 6_900_000);
        for _ in 0..1000 {
            t += Duration::from_secs(1);
            r.update(Some(0), t);
        }
        assert_eq!(r.current(), 30 * MBIT);
        // Loss still cuts from the new ceiling, down to the usual minimum.
        for _ in 0..40 {
            t += Duration::from_secs(1);
            r.update(Some(300), t);
        }
        assert_eq!(r.current(), super::super::watch::MIN_BITRATE);
    }

    #[test]
    fn turn_moves_the_ceiling_and_the_browser_cap_still_wins() {
        // (The pure part of `turn`: the share's value becomes the controller's ceiling and
        // the cap is applied on top, as the send loop does.)
        let mut share = Share::new(8, 30);
        let mut r = RateControl::new(8 * MBIT);
        let t0 = Instant::now();
        share.set_max_bitrate_mbps(30, true, t0);
        let (ceiling, _) = share.in_effect(t0);
        r.set_max(ceiling);
        let web_cap = 8 * MBIT;
        assert_eq!(r.current().min(web_cap), 8 * MBIT);
        assert_eq!(r.current().min(u32::MAX), 30 * MBIT);
    }

    #[test]
    fn debounce_a_dragged_slider_counts_when_still_or_released() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut v = Settled::new(30);
        // Dragged from 30 to 20, a step every 50 ms: nothing applies on the way.
        for (i, qp) in (20..30).rev().enumerate() {
            v.set(qp, ms(50 * i as u64));
            assert_eq!(v.poll(ms(50 * i as u64 + 49)), 30, "step {i}");
            assert_eq!(v.shown(), qp);
        }
        // Held still at 20 (the slider repeats the value): applied SETTLE_AFTER after it got there.
        let arrived = 50 * 9;
        v.set(20, ms(arrived + 100));
        assert_eq!(v.poll(ms(arrived + SETTLE_AFTER.as_millis() as u64 - 1)), 30);
        assert_eq!(v.poll(ms(arrived + SETTLE_AFTER.as_millis() as u64)), 20);
        assert!(!v.is_moving());
        // Released: at once.
        v.set(24, ms(5000));
        v.commit();
        assert_eq!(v.poll(ms(5000)), 24);
        // Dragged away and back to the value in effect: nothing to apply.
        v.set(28, ms(6000));
        v.set(24, ms(6050));
        assert!(!v.is_moving());
        assert_eq!(v.poll(ms(9000)), 24);
    }

    #[test]
    fn one_replacement_in_flight_at_a_time() {
        let t0 = Instant::now();
        let mut f = EncoderFloor::new(30);
        assert_eq!(f.next(30, t0), None);
        assert_eq!(f.next(24, t0), Some(24));
        // Until the new encoder shows its keyframe nothing else starts, whatever is asked.
        assert_eq!(f.next(22, t0), None);
        assert_eq!(f.next(30, t0), None);
        assert_eq!(f.in_flight(), Some((24, t0)));
        assert_eq!(f.applied(), 30);
        assert_eq!(f.proven(), Some((30, 24, t0)));
        assert_eq!(f.proven(), None);
        // The value asked for in the meantime is the next replacement.
        let t1 = t0 + Duration::from_millis(40);
        assert_eq!(f.next(22, t1), Some(22));
        assert_eq!(f.next(22, t1), None);
        assert_eq!(f.proven(), Some((24, 22, t1)));
        assert_eq!(f.next(22, t1), None);
    }

    #[test]
    fn a_failed_replacement_goes_back_and_is_not_tried_again() {
        let t0 = Instant::now();
        let mut f = EncoderFloor::new(30);
        assert_eq!(f.next(18, t0), Some(18));
        assert_eq!(f.failed(), Some((18, 30)));
        assert_eq!(f.applied(), 30);
        // The same value again would fail again: only another one starts a replacement.
        assert_eq!(f.next(18, t0), None);
        assert_eq!(f.next(30, t0), None);
        assert_eq!(f.next(18, t0), Some(18));
        assert_eq!(f.failed(), Some((18, 30)));
        assert_eq!(f.next(20, t0), Some(20));
        assert_eq!(f.proven(), Some((30, 20, t0)));
    }

    #[test]
    fn override_is_this_shares_only_and_the_next_share_starts_from_the_defaults() {
        let t0 = Instant::now();
        let defaults = (8, 30);
        let mut share = Share::new(defaults.0, defaults.1);
        assert_eq!(share.in_effect(t0), (8 * MBIT, 30));
        share.set_max_bitrate_mbps(30, true, t0);
        share.set_min_qp(22, false, t0);
        // The bitrate was released, the floor is still being dragged.
        assert_eq!(share.in_effect(t0), (30 * MBIT, 30));
        assert_eq!(share.shown(), (30, 22));
        assert_eq!(share.in_effect(t0 + SETTLE_AFTER), (30 * MBIT, 22));
        // Out of range values are brought into the settings' ranges.
        share.set_max_bitrate_mbps(500, true, t0);
        share.set_min_qp(1, true, t0);
        assert_eq!(share.shown(), (settings::STREAM_MAX_BITRATE_MBPS_RANGE.1, settings::STREAM_MIN_QP_RANGE.0));
        // Nothing of that is in the defaults: the next share is made from them again.
        assert_eq!(defaults, (8, 30));
        assert_eq!(Share::new(defaults.0, defaults.1).in_effect(t0), (8 * MBIT, 30));
    }

    #[test]
    fn a_refused_floor_puts_the_share_back_unless_another_was_asked() {
        let t0 = Instant::now();
        let mut share = Share::new(8, 30);
        share.set_min_qp(18, true, t0);
        share.floor_refused(18, 30);
        assert_eq!((share.shown().1, share.revision), (30, 1));
        share.set_min_qp(20, true, t0);
        share.floor_refused(18, 30);
        assert_eq!((share.shown().1, share.revision), (20, 1));
    }
}
