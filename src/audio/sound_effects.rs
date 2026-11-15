use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use log::{info, warn};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSound {
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    JoinChannel,
    LeaveChannel,
}

static SOUND_SENDER: std::sync::OnceLock<SyncSender<UiSound>> = std::sync::OnceLock::new();

/// Initializes the background audio thread for instant, zero-latency UI sound playback.
pub fn init_sound_effects() {
    if SOUND_SENDER.get().is_some() {
        return;
    }

    let (tx, rx) = sync_channel::<UiSound>(16);
    let _ = SOUND_SENDER.set(tx);

    std::thread::Builder::new()
        .name("ui-sound-player".to_string())
        .spawn(move || {
            info!("🔊 [SOUND EFFECTS] Motor de áudio tátil iniciado com sucesso via CPAL");

            while let Ok(sound) = rx.recv() {
                // Drain any excessive queued sounds to only play the latest if flooded
                let mut current_sound = sound;
                while let Ok(next) = rx.try_recv() {
                    current_sound = next;
                }

                if let Err(e) = play_sound_cpal(current_sound) {
                    warn!("⚠️ [SOUND EFFECTS] Falha na reprodução CPAL ({:?}), tentando fallback nativo...", e);
                    play_sound_fallback(current_sound);
                }
            }
        })
        .expect("Falha ao iniciar thread de áudio de efeitos sonoros");
}

/// Plays a tactile UI sound effect immediately.
pub fn play_ui_sound(sound: UiSound) {
    #[cfg(feature = "automation")]
    if crate::utils::test_hooks::mute_output() || crate::utils::test_hooks::silent_playback() {
        return;
    }
    if let Some(sender) = SOUND_SENDER.get() {
        let _ = sender.try_send(sound);
    } else {
        init_sound_effects();
        if let Some(sender) = SOUND_SENDER.get() {
            let _ = sender.try_send(sound);
        } else {
            play_sound_fallback(sound);
        }
    }
}

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
fn generate_sound_samples(sound: UiSound, sample_rate: u32) -> Vec<f32> {
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

fn play_sound_cpal(sound: UiSound) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    let samples = generate_sound_samples(sound, sample_rate);
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

fn play_sound_fallback(sound: UiSound) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Media::Audio::{PlaySoundA, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

        let sample_rate = 44100u32;
        let samples = generate_sound_samples(sound, sample_rate);
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
        let samples = generate_sound_samples(sound, sample_rate);
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
