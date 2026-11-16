//! Rust side of the stream controls on the streamer's own stream
//! (ui/components/live_stream_controls.slint): the main window and the detached stream
//! window each have a `LiveStream` global, both showing the one running share
//! (`screen_capture::live_controls`).

use crate::screen_capture::live_controls::{self, Path, Status};
use crate::utils::i18n_ui::{self, UiStrings};
use crate::video_settings;
use crate::{AppWindow, LiveStream, PopoutStreamWindow, StreamQuality};
use slint::ComponentHandle;
use std::cell::Cell;
use std::time::{Duration, Instant};

/// The quality floor only acts on the VA-API encoder.
const HAS_FLOOR: bool = cfg!(all(target_os = "linux", feature = "vaapi"));

/// What is in effect now, as the panel's read-only line: the rate asked of the encoder, the
/// profile, and the encoder's quality floor once a VA-API encoder has shown a keyframe. A
/// floor that was asked for while nobody watches has no encoder yet and is named after it
/// (with viewers the replacement takes a moment and needs no words).
pub fn status_line(s: &Status, t: &UiStrings, has_floor: bool) -> String {
    let profile = if s.main { format!("{} (Main)", t.profile_efficient) } else { format!("{} (Baseline)", t.profile_compatible) };
    let mut line = format!("{} {} {:.1} Mbit/s · {}", t.live_now, t.live_target, s.target_bps as f64 / 1e6, profile);
    if has_floor && s.encoder_min_qp != 0 {
        line.push_str(&format!(" · {} {}", t.live_floor_short, s.encoder_min_qp));
        if s.encoder_min_qp != s.min_qp && !s.moving && !s.watched {
            line.push_str(&format!(" · {}", t.live_floor_pending.replace("{}", &s.min_qp.to_string())));
        }
    }
    line
}

fn wire(live: &LiveStream, app: slint::Weak<AppWindow>) {
    live.set_has_floor(HAS_FLOOR);
    live.set_min_qp_lowest(video_settings::STREAM_MIN_QP_RANGE.0 as f32);
    live.set_min_qp_highest(video_settings::STREAM_MIN_QP_RANGE.1 as f32);
    live.set_max_bitrate_lowest(video_settings::STREAM_MAX_BITRATE_MBPS_RANGE.0 as f32);
    live.set_max_bitrate_highest(video_settings::STREAM_MAX_BITRATE_MBPS_RANGE.1 as f32);
    live.on_set_max_bitrate(|mbps, released| {
        live_controls::set_max_bitrate_mbps(mbps.max(0) as u32, released);
    });
    live.on_set_min_qp(|qp, released| {
        live_controls::set_min_qp(qp.max(0) as u32, released);
    });
    live.on_save_default(move || {
        // (Only what the panel shows: the floor is not on it without the VA-API encoder.)
        let Some((mbps, qp)) = live_controls::save_as_default(HAS_FLOOR) else { return };
        // The settings page shows the saved values.
        if let Some(app) = app.upgrade() {
            let quality = app.global::<StreamQuality>();
            quality.set_max_bitrate(mbps as f32);
            quality.set_min_qp(qp as f32);
        }
    });
}

/// Pictures are being sent and no VA-API encoder has reported for `NO_FLOOR_AFTER`: the
/// share runs on another encoder (OpenH264, NVENC), which has no quality floor.
const NO_FLOOR_AFTER: Duration = Duration::from_secs(2);
/// A panel whose button has not been there for this long is closed (`anchored`).
const ANCHOR_GONE_AFTER: Duration = Duration::from_millis(700);

thread_local! {
    static NO_FLOOR_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// The main window's button: its last beat, and when that was new.
    static ANCHOR_BEAT: Cell<Option<(i32, Instant)>> = const { Cell::new(None) };
}

fn floor_inactive(s: &Status, now: Instant) -> bool {
    NO_FLOOR_SINCE.with(|since| {
        if !(HAS_FLOOR && s.watched && s.encoder == Path::Unknown) {
            since.set(None);
            return false;
        }
        let from = since.get().unwrap_or(now);
        since.set(Some(from));
        now.saturating_duration_since(from) >= NO_FLOOR_AFTER
    })
}

/// The main window's panel hangs under a button on the own stream's tile, which says it is
/// there while the panel is open (`anchor-beat`). When the tile goes away without a click
/// (another page, a tile too narrow for the button) the panel goes with it.
fn anchored(live: &LiveStream, now: Instant) {
    let beat = live.get_anchor_beat();
    ANCHOR_BEAT.with(|last| match last.get() {
        Some((seen, at)) if live.get_open() && seen == beat => {
            if now.saturating_duration_since(at) >= ANCHOR_GONE_AFTER {
                live.set_open(false);
            }
        }
        _ => last.set(Some((beat, now))),
    });
}

fn refresh(live: &LiveStream, s: &Status, t: &UiStrings) {
    live.set_sharing(s.sharing);
    if !s.sharing {
        // The next share's panel starts closed.
        live.set_open(false);
        return;
    }
    // (Also what the other window's sliders did, and a floor the encoder refused.)
    live.set_max_bitrate(s.max_bitrate_mbps as f32);
    live.set_min_qp(s.min_qp as f32);
    live.set_status(status_line(s, t, HAS_FLOOR).into());
    live.set_capped(if s.web_capped { t.live_web_capped.replace("{}", &format!("{:.0}", s.web_cap_bps as f64 / 1e6)) } else { String::new() }.into());
    live.set_floor_note(if floor_inactive(s, Instant::now()) { t.live_floor_inactive } else { "" }.into());
    live.set_is_default(s.max_bitrate_mbps == video_settings::get_stream_max_bitrate_mbps() && (!HAS_FLOOR || s.min_qp == video_settings::get_stream_min_qp()));
}

/// Fills both windows' controls now (the timer of `install` does it a few times a second).
pub fn refresh_now(app: &AppWindow, popout: Option<&PopoutStreamWindow>) {
    let (status, t) = (live_controls::status(), i18n_ui::ui_text());
    refresh(&app.global::<LiveStream>(), &status, t);
    anchored(&app.global::<LiveStream>(), Instant::now());
    if let Some(popout) = popout {
        refresh(&popout.global::<LiveStream>(), &status, t);
    }
}

/// Connects the controls of both windows and keeps them up to date while the returned
/// timer lives. The detached window gets its strings here too (it has its own `Tr`).
pub fn install(app: &AppWindow, popout: &PopoutStreamWindow) -> slint::Timer {
    wire(&app.global::<LiveStream>(), app.as_weak());
    wire(&popout.global::<LiveStream>(), app.as_weak());
    let (app, popout) = (app.as_weak(), popout.as_weak());
    let popout_strings = std::cell::Cell::new(std::ptr::null::<UiStrings>());
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(250), move || {
        let (Some(app), Some(popout)) = (app.upgrade(), popout.upgrade()) else { return };
        let t = i18n_ui::ui_text();
        if popout_strings.replace(t) != t as *const UiStrings {
            i18n_ui::apply_to_popout(&popout, t);
        }
        refresh_now(&app, Some(&popout));
    });
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status {
            sharing: true,
            max_bitrate_mbps: 20,
            min_qp: 24,
            moving: false,
            revision: 0,
            target_bps: 8_000_000,
            web_capped: true,
            web_cap_bps: 8_000_000,
            watched: false,
            main: false,
            encoder: Path::GpuCapture,
            encoder_min_qp: 24,
        }
    }

    #[test]
    fn live_controls_status_line_names_rate_profile_and_floor() {
        let t = &i18n_ui::EN_UI;
        assert_eq!(status_line(&status(), t, true), "Now: target 8.0 Mbit/s · Compatible (Baseline) · floor QP 24");
        let s = Status { target_bps: 19_500_000, web_capped: false, main: true, ..status() };
        assert_eq!(status_line(&s, t, true), "Now: target 19.5 Mbit/s · Efficient (Main) · floor QP 24");
        // A floor asked for that the encoder does not have yet.
        let s = Status { encoder_min_qp: 30, ..status() };
        assert_eq!(status_line(&s, t, true), "Now: target 8.0 Mbit/s · Compatible (Baseline) · floor QP 30 · floor QP 24 when someone watches");
        // With viewers its encoder is on the way: nothing to wait for.
        let s = Status { encoder_min_qp: 30, watched: true, ..status() };
        assert_eq!(status_line(&s, t, true), "Now: target 8.0 Mbit/s · Compatible (Baseline) · floor QP 30");
        // No VA-API encoder (another build, or nobody has watched yet): no floor to name.
        assert_eq!(status_line(&status(), t, false), "Now: target 8.0 Mbit/s · Compatible (Baseline)");
        let s = Status { encoder: Path::Unknown, encoder_min_qp: 0, ..status() };
        assert_eq!(status_line(&s, t, true), "Now: target 8.0 Mbit/s · Compatible (Baseline)");
    }

    #[test]
    fn live_controls_floor_is_inactive_only_after_pictures_went_without_a_vaapi_encoder() {
        if !HAS_FLOOR {
            return;
        }
        let t0 = Instant::now();
        let other = Status { encoder: Path::Unknown, encoder_min_qp: 0, watched: true, ..status() };
        assert!(!floor_inactive(&other, t0));
        assert!(!floor_inactive(&other, t0 + NO_FLOOR_AFTER - Duration::from_millis(1)));
        assert!(floor_inactive(&other, t0 + NO_FLOOR_AFTER));
        // Nobody watches (nothing is encoded), or a VA-API encoder reported: not inactive.
        assert!(!floor_inactive(&Status { watched: false, ..other.clone() }, t0 + NO_FLOOR_AFTER * 2));
        assert!(!floor_inactive(&other, t0 + NO_FLOOR_AFTER * 2));
        assert!(!floor_inactive(&Status { watched: true, ..status() }, t0 + NO_FLOOR_AFTER * 4));
    }
}
