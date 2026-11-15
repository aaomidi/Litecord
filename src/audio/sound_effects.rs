use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use log::{info, warn};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crate::utils::video_settings::{get_ui_sound_pack, UiSoundPack};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSound {
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    JoinChannel,
    LeaveChannel,
}

/// A sound and the pack (never `Off`) that was selected when it was asked for.
type Cue = (UiSoundPack, UiSound);

static SOUND_SENDER: std::sync::OnceLock<SyncSender<Cue>> = std::sync::OnceLock::new();

/// Initializes the background audio thread for instant, zero-latency UI sound playback.
pub fn init_sound_effects() {
    if SOUND_SENDER.get().is_some() {
        return;
    }

    let (tx, rx) = sync_channel::<Cue>(16);
    let _ = SOUND_SENDER.set(tx);

    std::thread::Builder::new()
        .name("ui-sound-player".to_string())
        .spawn(move || {
            info!("🔊 [SOUND EFFECTS] Motor de áudio tátil iniciado com sucesso via CPAL");

            while let Ok(cue) = rx.recv() {
                // Drain any excessive queued sounds to only play the latest if flooded
                let (mut pack, mut current_sound) = cue;
                while let Ok(next) = rx.try_recv() {
                    (pack, current_sound) = next;
                }

                if let Err(e) = play_sound_cpal(pack, current_sound) {
                    warn!("⚠️ [SOUND EFFECTS] Falha na reprodução CPAL ({:?}), tentando fallback nativo...", e);
                    play_sound_fallback(pack, current_sound);
                }
            }
        })
        .expect("Falha ao iniciar thread de áudio de efeitos sonoros");
}

/// Plays a tactile UI sound effect immediately, from the sound pack chosen in the settings.
pub fn play_ui_sound(sound: UiSound) {
    #[cfg(feature = "automation")]
    if crate::utils::test_hooks::mute_output() || crate::utils::test_hooks::silent_playback() {
        // Still nothing is played; the voice log only notes what would have been.
        crate::utils::test_hooks::ui_sound_muted(get_ui_sound_pack().id(), &format!("{sound:?}"));
        return;
    }
    // Read for every sound, so a change in the settings applies to the next one.
    let pack = get_ui_sound_pack();
    if pack == UiSoundPack::Off {
        // Nothing is queued: the player thread is not woken and no output stream is opened.
        return;
    }
    if let Some(sender) = SOUND_SENDER.get() {
        let _ = sender.try_send((pack, sound));
    } else {
        init_sound_effects();
        if let Some(sender) = SOUND_SENDER.get() {
            let _ = sender.try_send((pack, sound));
        } else {
            play_sound_fallback(pack, sound);
        }
    }
}

/// The samples of `sound` in `pack`: mono, full scale = 1.0, at `sample_rate` Hz. Every
/// pack starts and ends its buffers with exact silence and stays below 0.2 of full scale
/// (the tests at the end of this file render all of them and check). Nothing for `Off`.
fn generate_sound_samples(pack: UiSoundPack, sound: UiSound, sample_rate: u32) -> Vec<f32> {
    match pack {
        UiSoundPack::SoftMallet => soft_mallet_samples(sound, sample_rate),
        UiSoundPack::GlassBell => glass_bell_samples(sound, sample_rate),
        UiSoundPack::BreathPad => breath_pad_samples(sound, sample_rate),
        UiSoundPack::Off => Vec::new(),
    }
}

// Pack "soft-mallet" (the default).

/// UI tones, "soft mallet" set: every sound is two gentle strikes of a felted
/// mallet on a wooden bar ("tok-dong"). The second strike is the one that lands.
///
/// Falling pitch means switching off or leaving, rising pitch means switching on
/// or arriving. The two sounds of a pair are identical except that the two pitches
/// swap places. The three pairs differ in interval, register and pace, so they can
/// be told apart by ear:
///
/// | pair              | interval    | pitches   | character                        |
/// |-------------------|-------------|-----------|----------------------------------|
/// | Mute / Unmute     | major third | B4 <-> G4 | quick and dry                    |
/// | Deafen / Undeafen | octave      | D5 <-> D4 | slow, dark and felted            |
/// | Leave / Join      | fourth      | G5 <-> D5 | unhurried, highest and brightest |
///
/// All five pitches belong to one justly tuned G major chord (3 : 4 : 5 : 6 : 8
/// over 99 Hz), so strikes that ring into each other never beat or clash.
///
/// Quiet and soft-edged on purpose, because these play on top of a voice call:
/// peak below 0.17 of full scale (the previous set peaked at 0.6-0.75),
/// raised-cosine onsets that take 16-26 ms to reach 90 %, no pitch sweeps, and
/// every strike fades to exactly zero. The two strikes overlap by summing; the
/// buffer is never split at a note boundary. It starts with 5 ms and ends with
/// 15 ms of exact silence so that opening and closing the output stream cannot
/// click. Everything is in seconds and Hz, so the result is the same at any
/// sample rate.
fn soft_mallet_samples(sound: UiSound, sample_rate: u32) -> Vec<f32> {
    // Exact silence after the last strike has faded out.
    const LEAD_OUT: f64 = 0.015;

    let rate = f64::from(sample_rate.max(1));
    let voices = mallet_voices(sound);
    let total = voices[1].start + voices[1].len + LEAD_OUT;
    let count = (total * rate).ceil() as usize;

    (0..count)
        .map(|i| {
            let t = i as f64 / rate;
            (mallet_sample(&voices[0], t, rate) + mallet_sample(&voices[1], t, rate)) as f32
        })
        .collect()
}

/// One strike of the mallet. Times in seconds, frequencies in Hz, full scale = 1.0.
#[derive(Clone, Copy)]
struct MalletVoice {
    /// Onset, measured from the start of the buffer.
    start: f64,
    /// Fundamental.
    freq: f64,
    /// Peak level of the fundamental divided by the peak of the raw envelope.
    gain: f64,
    /// Raised-cosine rise time from 0 to 100 % (90 % of the peak comes at about 0.7x this).
    attack: f64,
    /// Time constant of the exponential decay of the fundamental.
    tau: f64,
    /// The strike is exactly zero again this long after its onset.
    len: f64,
    /// Level of the 2nd harmonic relative to the fundamental; it dies 1.5x faster.
    second_partial: f64,
    /// Level of the 3rd harmonic relative to the fundamental; it dies 3x faster.
    third_partial: f64,
}

/// The two strikes that make up `sound`.
fn mallet_voices(sound: UiSound) -> [MalletVoice; 2] {
    // (attack, tau, len) of one strike.
    type Strike = (f64, f64, f64);
    // (time between the two onsets, first strike, second strike, 2nd partial, 3rd partial)
    type Motif = (f64, Strike, Strike, f64, f64);

    // Quick, dry double tap.
    #[rustfmt::skip]
    const TAP: Motif = (0.080, (0.024, 0.040, 0.150), (0.026, 0.050, 0.190), 0.10, 0.02);
    // Slow, dark and felted: softest onset, longest ring, almost a pure sine.
    #[rustfmt::skip]
    const DROP: Motif = (0.145, (0.030, 0.055, 0.200), (0.034, 0.075, 0.280), 0.05, 0.0);
    // Unhurried, with a touch more "wood" in the onset.
    #[rustfmt::skip]
    const CHIME: Motif = (0.125, (0.024, 0.050, 0.180), (0.026, 0.070, 0.290), 0.12, 0.035);

    // Just intonation over 99 Hz.
    const D4: f64 = 297.0;
    const G4: f64 = 396.0;
    const B4: f64 = 495.0;
    const D5: f64 = 594.0;
    const G5: f64 = 792.0;

    // Exact silence before the first strike.
    const LEAD_IN: f64 = 0.005;

    // Inside a pair only the order of the two pitches differs.
    let (first_freq, second_freq, motif) = match sound {
        UiSound::Mute => (B4, G4, TAP),
        UiSound::Unmute => (G4, B4, TAP),
        UiSound::Deafen => (D5, D4, DROP),
        UiSound::Undeafen => (D4, D5, DROP),
        UiSound::LeaveChannel => (G5, D5, CHIME),
        UiSound::JoinChannel => (D5, G5, CHIME),
    };
    let (gap, first, second, second_partial, third_partial) = motif;

    let voice = |start: f64, freq: f64, weight: f64, (attack, tau, len): Strike| {
        // Lower bars are struck a little harder and higher ones a little softer so that
        // all notes sound about equally loud (the ear is less sensitive towards 300 Hz).
        let level = weight * 0.15 * (G4 / freq).powf(0.35);
        // The raw envelope (window * decay) peaks where rise * decay peaks; dividing by
        // its value there makes `level` the real peak of the fundamental.
        let peak_at =
            2.0 * attack / std::f64::consts::PI * (std::f64::consts::PI * tau / attack).atan();
        let raw_peak = mallet_window(peak_at, attack, len) * (-peak_at / tau).exp();
        MalletVoice {
            start,
            freq,
            gain: level / raw_peak,
            attack,
            tau,
            len,
            second_partial,
            third_partial,
        }
    };

    [
        // The first strike is a lighter pickup; the second one lands.
        voice(LEAD_IN, first_freq, 0.8, first),
        voice(LEAD_IN + gap, second_freq, 1.0, second),
    ]
}

/// Raised-cosine rise over `attack` times a cos^2 fade over the whole strike, `x`
/// seconds after the onset. Smooth (value and slope) at 0, at `attack` and at `len`.
fn mallet_window(x: f64, attack: f64, len: f64) -> f64 {
    use std::f64::consts::{FRAC_PI_2, PI};

    let rise = if x < attack {
        0.5 - 0.5 * (PI * x / attack).cos()
    } else {
        1.0
    };
    let fade = (FRAC_PI_2 * x / len).cos();
    rise * fade * fade
}

/// Value of one strike at time `t` (seconds from the start of the buffer); exactly
/// zero before its onset and from `len` after it.
///
/// envelope = window (see `mallet_window`) * exponential decay. It reaches true zero
/// at `len` and has no step or kink anywhere, so there is no click at the onset, at
/// the peak or at the end.
fn mallet_sample(voice: &MalletVoice, t: f64, rate: f64) -> f64 {
    let x = t - voice.start;
    if x <= 0.0 || x >= voice.len {
        return 0.0;
    }

    let decay = (-x / voice.tau).exp();
    let (sin, cos) = (std::f64::consts::TAU * voice.freq * x).sin_cos();
    let mut wave = sin;
    // A little 2nd and 3rd harmonic that is gone within the first few tens of ms:
    // the "wood" of the strike. Left out if the device rate could not carry it.
    if voice.second_partial > 0.0 && 2.0 * voice.freq < 0.45 * rate {
        // sin(2a) = 2 sin(a) cos(a); the extra sqrt(decay) makes it die 1.5x faster.
        wave += voice.second_partial * decay.sqrt() * 2.0 * sin * cos;
    }
    if voice.third_partial > 0.0 && 3.0 * voice.freq < 0.45 * rate {
        // sin(3a) = sin(a) (3 - 4 sin^2(a)); the extra decay^2 makes it die 3x faster.
        wave += voice.third_partial * decay * decay * sin * (3.0 - 4.0 * sin * sin);
    }

    voice.gain * mallet_window(x, voice.attack, voice.len) * decay * wave
}

// Pack "glass-bell".

/// Synthesises a UI tone as mono f32 samples at the device sample rate.
///
/// The set is six quiet glass chimes built from one just-intonation pentatonic
/// scale (A4 = 440 Hz), so every interval is beat-free and the sounds belong together:
///
/// * Mute / Unmute         - two close notes a major third apart (E5 <-> C5), ~0.3 s
/// * Deafen / Undeafen     - two slow, low, rounder notes a fifth apart (D5 <-> G4), ~0.45 s
/// * Leave / Join channel  - quick three-note ripple over an octave (A5 <-> E5 <-> A4), ~0.45 s
///
/// Falling means off / leaving, rising means on / arriving; each pair is the same
/// notes in reverse order.
///
/// Everything is expressed in seconds and Hz, so the result is the same sound at any
/// sample rate. Notes are summed into the buffer (never spliced), each one swells in
/// with a raised cosine and fades to exactly zero, and the buffer starts and ends with
/// digital silence so opening / dropping the output stream cannot click or clip the tail.
fn glass_bell_samples(sound: UiSound, sample_rate: u32) -> Vec<f32> {
    // Just-intonation pitches in Hz (ratios 3:2, 4:3, 5:4, 2:1 between the notes used together).
    const G4: f64 = 396.0;
    const A4: f64 = 440.0;
    const C5: f64 = 528.0;
    const D5: f64 = 594.0;
    const E5: f64 = 660.0;
    const A5: f64 = 880.0;

    /// Peak amplitude of a 528 Hz note. Higher notes are scaled down and lower notes up
    /// (see below) so all notes are about equally loud; the loudest sound of the set
    /// peaks near 0.16 of full scale (-16 dBFS).
    const LEVEL: f64 = 0.13;
    const LEVEL_REF_HZ: f64 = 528.0;
    /// Notes before the last one are slightly softer, so the last note is the "landing".
    const LEAD_IN_GAIN: f64 = 0.8;
    /// Digital silence before the first note and after the last one, in seconds. The
    /// trailing part is longer because the player drops the stream shortly after handing
    /// over the last sample, while the end of the buffer may still be queued in the device.
    const LEAD_SILENCE: f64 = 0.008;
    const TRAIL_SILENCE: f64 = 0.030;

    // (notes low -> high, seconds between note onsets, swell time, decay time constant,
    //  total buffer length, level of the upper partials)
    let (rising, gap, swell, tau, total, brightness): (&[f64], f64, f64, f64, f64, f64) = match sound {
        UiSound::Mute | UiSound::Unmute => (&[C5, E5], 0.100, 0.036, 0.046, 0.300, 1.0),
        UiSound::Deafen | UiSound::Undeafen => (&[G4, D5], 0.160, 0.048, 0.068, 0.450, 0.5),
        UiSound::JoinChannel | UiSound::LeaveChannel => (&[A4, E5, A5], 0.085, 0.036, 0.066, 0.450, 1.0),
    };
    let falling = matches!(sound, UiSound::Mute | UiSound::Deafen | UiSound::LeaveChannel);

    let sr = sample_rate.max(1) as f64;
    let mut samples = vec![0.0f32; (total * sr).round() as usize];

    // Every note rings on underneath the following ones and they all reach zero together.
    let end = total - TRAIL_SILENCE;
    let count = rising.len();
    let release = 0.4 * (end - (LEAD_SILENCE + gap * (count - 1) as f64));
    for slot in 0..count {
        let freq = if falling { rising[count - 1 - slot] } else { rising[slot] };
        let onset = LEAD_SILENCE + gap * slot as f64;
        let slot_gain = if slot + 1 == count { 1.0 } else { LEAD_IN_GAIN };
        let level = LEVEL * slot_gain * (LEVEL_REF_HZ / freq).sqrt();
        add_chime(&mut samples, sr, onset, end, freq, level, swell, tau, release, brightness);
    }

    samples
}

/// Adds one glass-chime note to `samples`: a sine with a little 2nd and 3rd harmonic
/// that die away two and three times faster than the fundamental.
///
/// Envelope = raised-cosine swell (`swell` s) x exponential decay (`tau` s) x raised-cosine
/// release over the last `release` s before `end`. It is smooth (continuous slope)
/// everywhere and exactly zero at `onset` and at `end`; `level` is its true peak.
#[allow(clippy::too_many_arguments)]
fn add_chime(
    samples: &mut [f32],
    sr: f64,
    onset: f64,
    end: f64,
    freq: f64,
    level: f64,
    swell: f64,
    tau: f64,
    release: f64,
    brightness: f64,
) {
    use std::f64::consts::{PI, TAU};

    /// Starting level of the 2nd / 3rd harmonic relative to the fundamental. Because they
    /// decay faster, they are down to roughly -17 dB / -30 dB when the note peaks and gone
    /// soon after, leaving a pure sine tail.
    const SECOND: f64 = 0.26;
    const THIRD: f64 = 0.10;

    // The swell x decay product peaks slightly before the swell completes; normalise by
    // that maximum so `level` is the real peak of the fundamental's envelope.
    let t_peak = (2.0 * swell / PI) * (PI * tau / swell).atan();
    let norm = level / ((0.5 - 0.5 * (PI * t_peak / swell).cos()) * (-t_peak / tau).exp());

    let first = (onset * sr).ceil().max(0.0) as usize;
    let last = ((end * sr).ceil().max(0.0) as usize).min(samples.len());
    for (i, out) in samples.iter_mut().enumerate().take(last).skip(first) {
        let t = i as f64 / sr - onset;
        let left = end - onset - t;
        if left <= 0.0 {
            break;
        }
        let rise = if t < swell { 0.5 - 0.5 * (PI * t / swell).cos() } else { 1.0 };
        let fall = if left < release { 0.5 - 0.5 * (PI * left / release).cos() } else { 1.0 };

        let decay = (-t / tau).exp();
        let (s, c) = (TAU * freq * t).sin_cos();
        let second = 2.0 * s * c; // sin(2x)
        let third = s * (3.0 - 4.0 * s * s); // sin(3x)
        let tone = decay
            * (s + brightness * decay * (SECOND * second + THIRD * decay * third));

        *out += (norm * rise * fall * tone) as f32;
    }
}

// Pack "breath-pad".

/// Synthesises one UI cue as mono samples at `sample_rate` Hz.
///
/// All six cues are the same quiet gesture: two soft sine notes, the second
/// swelling in while the first is still fading. "Off" cues (mute, deafen,
/// leave) step down; the matching "on" cue plays the same two notes in the
/// opposite order. The three families differ in register, interval and pace:
///
/// | family            | notes (Hz)   | interval    | character              |
/// |-------------------|--------------|-------------|------------------------|
/// | mute / unmute     | 440, 550     | major third | middle, quick, legato  |
/// | deafen / undeafen | 293.3, 440   | fifth       | low, slow, humming     |
/// | join / leave      | 660, 880     | fourth      | higher, lightly struck |
///
/// Every envelope is a raised cosine in and a raised cosine (to a power) out,
/// so each note starts and ends at exactly zero with no step in level or slope.
/// All timing is in seconds and all pitch in Hz, so a cue sounds the same at
/// any device sample rate.
fn breath_pad_samples(sound: UiSound, sample_rate: u32) -> Vec<f32> {
    /// Silence before the first note, so a stream that is still starting up
    /// has nothing to clip.
    const LEAD_SILENCE: f64 = 0.010;
    /// Silence after the last note, so the stream is only ever closed on
    /// silence. (`play_sound_cpal` keeps the stream open until the device has
    /// played everything it was handed, so a few ms are enough.)
    const TAIL_SILENCE: f64 = 0.010;
    /// Peak amplitude of a 500 Hz note, full scale = 1.0 (about -19 dBFS).
    const LEVEL: f64 = 0.11;
    /// The first note is a pickup into the second, so it is a little softer.
    const PICKUP: f64 = 0.8;

    struct Family {
        low: f64,
        high: f64,
        /// Seconds between the two note onsets.
        gap: f64,
        /// Swell time of the first and of the second note, seconds.
        swell: [f64; 2],
        /// Fade time of the first and of the second note, seconds.
        fade: [f64; 2],
        /// Fade curve exponent: higher drops sooner and trails off longer.
        fade_power: i32,
        /// Amount of octave partial at the peak of a note.
        warmth: f64,
    }

    // Pitches are the ratios 8 : 12 : 15 : 18 : 24 over A4 = 440 Hz.
    const MUTE: Family = Family {
        low: 440.0,
        high: 550.0,
        gap: 0.074,
        swell: [0.036, 0.042],
        fade: [0.150, 0.134],
        fade_power: 2,
        warmth: 0.05,
    };
    const DEAFEN: Family = Family {
        low: 440.0 * 2.0 / 3.0,
        high: 440.0,
        gap: 0.150,
        swell: [0.050, 0.064],
        fade: [0.270, 0.200],
        fade_power: 2,
        warmth: 0.10,
    };
    const JOIN: Family = Family {
        low: 660.0,
        high: 880.0,
        gap: 0.115,
        swell: [0.026, 0.026],
        fade: [0.230, 0.256],
        fade_power: 3,
        warmth: 0.03,
    };

    let (family, rising) = match sound {
        UiSound::Mute => (&MUTE, false),
        UiSound::Unmute => (&MUTE, true),
        UiSound::Deafen => (&DEAFEN, false),
        UiSound::Undeafen => (&DEAFEN, true),
        UiSound::JoinChannel => (&JOIN, true),
        UiSound::LeaveChannel => (&JOIN, false),
    };
    let pitches = if rising {
        [family.low, family.high]
    } else {
        [family.high, family.low]
    };
    let onsets = [0.0, family.gap];
    // The ear is less sensitive towards the low end of this range, so lower
    // notes get slightly more level (3 dB per octave).
    let levels = [
        PICKUP * LEVEL * (500.0 / pitches[0]).sqrt(),
        LEVEL * (500.0 / pitches[1]).sqrt(),
    ];
    // The second note always outlasts the first.
    let length = family.gap + family.swell[1] + family.fade[1];

    let rate = sample_rate as f64;
    let total = ((LEAD_SILENCE + length + TAIL_SILENCE) * rate).ceil() as usize;

    (0..total)
        .map(|i| {
            let t = i as f64 / rate - LEAD_SILENCE;
            let first = levels[0]
                * soft_note(
                    t - onsets[0],
                    pitches[0],
                    family.swell[0],
                    family.fade[0],
                    family.fade_power,
                    family.warmth,
                );
            let second = levels[1]
                * soft_note(
                    t - onsets[1],
                    pitches[1],
                    family.swell[1],
                    family.fade[1],
                    family.fade_power,
                    family.warmth,
                );
            (first + second) as f32
        })
        .collect()
}

/// One note of a UI cue, `t` seconds after its onset: a sine that swells in
/// along a raised cosine over `swell` seconds and fades to exactly zero over
/// `fade` seconds. `warmth` adds an octave partial that follows the square of
/// the envelope, so it arrives later and dies away sooner than the note itself.
fn soft_note(t: f64, freq: f64, swell: f64, fade: f64, fade_power: i32, warmth: f64) -> f64 {
    use std::f64::consts::{PI, TAU};

    if t <= 0.0 || t >= swell + fade {
        return 0.0;
    }
    let envelope = if t < swell {
        0.5 - 0.5 * (PI * t / swell).cos()
    } else {
        (0.5 + 0.5 * (PI * (t - swell) / fade).cos()).powi(fade_power)
    };
    let phase = TAU * freq * t;
    envelope * (phase.sin() + warmth * envelope * (2.0 * phase).sin())
}

fn play_sound_cpal(pack: UiSoundPack, sound: UiSound) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let host = cpal::default_host();
    let target_dev_name = if let Ok(guard) = crate::gateway::get_selected_output_device_store().lock() {
        guard.clone()
    } else {
        String::new()
    };
    let device = if !target_dev_name.is_empty() && !target_dev_name.contains("Padrão") {
        host.output_devices().ok().and_then(|mut devs| devs.find(|d| d.name().map_or(false, |n| n == target_dev_name)))
            .or_else(|| host.default_output_device())
    } else {
        host.default_output_device()
    }.ok_or_else(|| "Nenhum dispositivo de áudio de saída encontrado")?;

    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    let samples = generate_sound_samples(pack, sound, sample_rate);
    let total_samples = samples.len();
    let samples_arc = Arc::new(samples);

    let current_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let playback_finished = Arc::new(AtomicBool::new(false));
    // Most frames the device has asked for in one go, i.e. how far ahead it buffers.
    let largest_request = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let idx_cb = Arc::clone(&current_idx);
    let finish_cb = Arc::clone(&playback_finished);
    let request_cb = Arc::clone(&largest_request);
    let smp_cb = Arc::clone(&samples_arc);

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                request_cb.fetch_max(data.len() / channels, Ordering::Relaxed);
                for frame in data.chunks_mut(channels) {
                    let idx = idx_cb.fetch_add(1, Ordering::Relaxed);
                    let val = if idx < total_samples {
                        smp_cb[idx]
                    } else {
                        finish_cb.store(true, Ordering::Relaxed);
                        0.0
                    };
                    for sample in frame.iter_mut() {
                        *sample = val;
                    }
                }
            },
            |err| warn!("Erro no stream de som CPAL: {}", err),
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                request_cb.fetch_max(data.len() / channels, Ordering::Relaxed);
                for frame in data.chunks_mut(channels) {
                    let idx = idx_cb.fetch_add(1, Ordering::Relaxed);
                    let val = if idx < total_samples {
                        smp_cb[idx]
                    } else {
                        finish_cb.store(true, Ordering::Relaxed);
                        0.0
                    };
                    let val_i16 = (val * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    for sample in frame.iter_mut() {
                        *sample = val_i16;
                    }
                }
            },
            |err| warn!("Erro no stream de som CPAL: {}", err),
            None,
        )?,
        _ => return Err("Formato de áudio não suportado para UI sounds".into()),
    };

    stream.play()?;

    // Wait until the device has taken the whole tone. The timeout is only a safety net for
    // a device that never asks for data.
    let rate = f64::from(sample_rate.max(1));
    let tone = Duration::from_secs_f64(total_samples as f64 / rate);
    let start = std::time::Instant::now();
    while !playback_finished.load(Ordering::Relaxed) && start.elapsed() < tone + Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(5));
    }

    // "Finished" only means the last sample has been handed over, not that it has been
    // heard: the device buffers ahead (cpal's ALSA backend takes 100 ms up front and keeps
    // that buffer full), and dropping the stream throws away whatever is still queued, which
    // chops the end off the tone. Keep the stream open, playing silence, until the tail is
    // out: the device was full (largest request) when it took the last sample, minus the
    // silence it has taken since, and never sooner than the tone's own length.
    if playback_finished.load(Ordering::Relaxed) {
        let silence_taken = current_idx.load(Ordering::Relaxed).saturating_sub(total_samples);
        let still_queued = largest_request.load(Ordering::Relaxed).saturating_sub(silence_taken);
        let heard_at = (start.elapsed() + Duration::from_secs_f64(still_queued as f64 / rate)).max(tone)
            + Duration::from_millis(30);
        std::thread::sleep(heard_at.saturating_sub(start.elapsed()));
    }
    drop(stream);
    Ok(())
}

fn play_sound_fallback(pack: UiSoundPack, sound: UiSound) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Media::Audio::{PlaySoundA, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

        let sample_rate = 44100u32;
        let samples = generate_sound_samples(pack, sound, sample_rate);
        let mut wav = Vec::with_capacity(44 + samples.len() * 2);

        let data_size = (samples.len() * 2) as u32;
        let file_size = 36 + data_size;

        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&file_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");

        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());

        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());

        for s in samples {
            let s_i16 = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            wav.extend_from_slice(&s_i16.to_le_bytes());
        }

        unsafe {
            PlaySoundA(
                wav.as_ptr(),
                0 as _,
                SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
            );
        }
    }

    #[cfg(not(windows))]
    {
        let sample_rate = 44100u32;
        let samples = generate_sound_samples(pack, sound, sample_rate);
        let mut wav = Vec::with_capacity(44 + samples.len() * 2);

        let data_size = (samples.len() * 2) as u32;
        let file_size = 36 + data_size;

        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&file_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");

        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());

        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());

        for s in samples {
            let s_i16 = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            wav.extend_from_slice(&s_i16.to_le_bytes());
        }

        for player in &["pw-play", "paplay", "aplay"] {
            if let Ok(mut child) = std::process::Command::new(player)
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(&wav);
                }
                let _ = child.wait();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOUNDS: [(UiSound, &str); 6] = [
        (UiSound::Mute, "mute"),
        (UiSound::Unmute, "unmute"),
        (UiSound::Deafen, "deafen"),
        (UiSound::Undeafen, "undeafen"),
        (UiSound::JoinChannel, "join"),
        (UiSound::LeaveChannel, "leave"),
    ];
    const PACKS: [UiSoundPack; 3] = [UiSoundPack::SoftMallet, UiSoundPack::GlassBell, UiSoundPack::BreathPad];
    const RATES: [u32; 3] = [44_100, 48_000, 96_000];

    /// Renders every sound of every pack in memory; no audio device is touched. Run with
    /// `-- --nocapture` for the measured table. With LITECORD_SOUND_DUMP_DIR set, each
    /// buffer is also written there as `<pack>/<sound>_<rate>.f32` (raw mono f32), to be
    /// analysed outside.
    #[test]
    fn every_pack_is_quiet_and_starts_and_ends_in_silence() {
        let dump = std::env::var_os("LITECORD_SOUND_DUMP_DIR").map(std::path::PathBuf::from);
        println!("pack         sound     rate   seconds  peak    silent start / end (ms)");
        for pack in PACKS {
            for (sound, name) in SOUNDS {
                for rate in RATES {
                    let samples = generate_sound_samples(pack, sound, rate);
                    let what = format!("{} {name} at {rate} Hz", pack.id());
                    let ms = |count: usize| count as f64 * 1000.0 / f64::from(rate);
                    let seconds = ms(samples.len()) / 1000.0;
                    let peak = samples.iter().fold(0.0f32, |max, s| max.max(s.abs()));
                    let silent_start = samples.iter().take_while(|s| **s == 0.0).count();
                    let silent_end = samples.iter().rev().take_while(|s| **s == 0.0).count();
                    println!(
                        "{:<12} {name:<9} {rate}  {seconds:.4}   {peak:.4}  {:.2} / {:.2}",
                        pack.id(),
                        ms(silent_start),
                        ms(silent_end),
                    );

                    assert!(samples.iter().all(|s| s.is_finite()), "{what}: NaN or infinity");
                    assert!((0.1..=1.2).contains(&seconds), "{what}: {seconds} s long");
                    // At most 0.2 of full scale (the upstream set peaked at 0.6-0.75), and audible.
                    assert!(peak <= 0.2, "{what}: peak {peak}");
                    assert!(peak >= 0.05, "{what}: peak {peak}, close to silent");
                    // Exact zeros at both ends, so opening and closing the stream cannot click.
                    assert!(ms(silent_start) >= 4.0, "{what}: only {silent_start} silent samples at the start");
                    assert!(ms(silent_end) >= 8.0, "{what}: only {silent_end} silent samples at the end");

                    if let Some(dir) = &dump {
                        let dir = dir.join(pack.id());
                        std::fs::create_dir_all(&dir).unwrap();
                        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                        std::fs::write(dir.join(format!("{name}_{rate}.f32")), bytes).unwrap();
                    }
                }
            }
        }
    }

    /// The largest difference between two buffers, a missing sample counting as silence.
    fn difference(a: &[f32], b: &[f32]) -> f32 {
        (0..a.len().max(b.len()))
            .map(|i| (a.get(i).copied().unwrap_or(0.0) - b.get(i).copied().unwrap_or(0.0)).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn packs_and_sounds_differ_from_each_other() {
        for rate in RATES {
            // The same sound in two packs.
            for (sound, name) in SOUNDS {
                for (i, a) in PACKS.iter().enumerate() {
                    for b in &PACKS[i + 1..] {
                        let d = difference(&generate_sound_samples(*a, sound, rate), &generate_sound_samples(*b, sound, rate));
                        assert!(d > 0.05, "{name} at {rate} Hz: {} and {} differ by only {d}", a.id(), b.id());
                    }
                }
            }
            // Two sounds of the same pack.
            for pack in PACKS {
                for (i, (a, a_name)) in SOUNDS.iter().enumerate() {
                    for (b, b_name) in &SOUNDS[i + 1..] {
                        let d = difference(&generate_sound_samples(pack, *a, rate), &generate_sound_samples(pack, *b, rate));
                        assert!(d > 0.05, "{} at {rate} Hz: {a_name} and {b_name} differ by only {d}", pack.id());
                    }
                }
            }
        }
    }

    #[test]
    fn off_renders_nothing() {
        for (sound, _) in SOUNDS {
            assert!(generate_sound_samples(UiSoundPack::Off, sound, 48_000).is_empty());
        }
    }
}
