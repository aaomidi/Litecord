#![allow(dead_code)]

use tokio::sync::mpsc;

use std::sync::Arc;
use std::collections::VecDeque;
use cpal::traits::{HostTrait, DeviceTrait, StreamTrait};
use log::info;
use crate::gateway;

#[cfg(any(target_os = "linux", test))]
fn voice_buffer_frames(rate: u32, supported: &cpal::SupportedBufferSize) -> u32 {
    // Nominal 20 ms, rounded to a 64-frame period before applying device limits.
    let frames = (rate / 50).max(128).div_ceil(64) * 64;
    match supported {
        cpal::SupportedBufferSize::Range { min, max } => frames.clamp(*min, *max),
        cpal::SupportedBufferSize::Unknown => frames,
    }
}

/// Probe only when opening a Linux voice device; no periodic device work.
#[cfg(target_os = "linux")]
pub(crate) fn linux_voice_stream_config(
    device: &cpal::Device, config: &cpal::SupportedStreamConfig, input: bool,
) -> cpal::StreamConfig {
    let frames = voice_buffer_frames(config.sample_rate().0, config.buffer_size());
    let mut fixed: cpal::StreamConfig = config.clone().into();
    fixed.buffer_size = cpal::BufferSize::Fixed(frames);
    let probe = match (input, config.sample_format()) {
        (true, cpal::SampleFormat::F32) => device.build_input_stream(&fixed, |_: &[f32], _| {}, |_| {}, None).map(|_| ()),
        (true, cpal::SampleFormat::I16) => device.build_input_stream(&fixed, |_: &[i16], _| {}, |_| {}, None).map(|_| ()),
        (true, cpal::SampleFormat::I32) => device.build_input_stream(&fixed, |_: &[i32], _| {}, |_| {}, None).map(|_| ()),
        (false, cpal::SampleFormat::F32) => device.build_output_stream(&fixed, |out: &mut [f32], _| out.fill(0.0), |_| {}, None).map(|_| ()),
        (false, cpal::SampleFormat::I16) => device.build_output_stream(&fixed, |out: &mut [i16], _| out.fill(0), |_| {}, None).map(|_| ()),
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    };
    match probe {
        Ok(()) => {
            info!("Buffer de voz Linux aceito: {} frames ({})", frames, if input { "entrada" } else { "saída" });
            fixed
        },
        Err(error) => {
            log::warn!("Buffer de voz Linux de {} frames recusado ({error}); usando padrão do driver", frames);
            let mut fallback: cpal::StreamConfig = config.clone().into();
            fallback.buffer_size = cpal::BufferSize::Default;
            fallback
        },
    }
}

/// The output format for voice: the device's default one, but at 48 kHz (the rate Opus
/// decodes to) when the device offers that rate with the same channels and sample type.
pub(crate) fn voice_output_config_at_48k(
    default: cpal::SupportedStreamConfig,
    ranges: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
) -> cpal::SupportedStreamConfig {
    const RATE: cpal::SampleRate = cpal::SampleRate(48000);
    if default.sample_rate() == RATE {
        return default;
    }
    ranges.into_iter()
        .find(|r| r.channels() == default.channels() && r.sample_format() == default.sample_format()
            && r.min_sample_rate() <= RATE && r.max_sample_rate() >= RATE)
        .map(|r| r.with_sample_rate(RATE))
        .unwrap_or(default)
}

#[cfg(test)]
mod voice_rx_output_rate_tests {
    use cpal::{SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange};

    fn range(channels: u16, min: u32, max: u32, format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, SampleRate(min), SampleRate(max), SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn voice_rx_output_asks_for_48_khz_when_the_device_offers_it() {
        let default = SupportedStreamConfig::new(2, SampleRate(44100), SupportedBufferSize::Unknown, SampleFormat::F32);
        // What cpal reports for the PipeWire/PulseAudio ALSA device: any rate.
        let chosen = super::voice_output_config_at_48k(default.clone(), [range(1, 1, 384000, SampleFormat::F32), range(2, 1, 384000, SampleFormat::F32)]);
        assert_eq!((chosen.sample_rate().0, chosen.channels(), chosen.sample_format()), (48000, 2, SampleFormat::F32));
        // Only other rates, other channel counts or other sample types: the default stays.
        for ranges in [vec![], vec![range(2, 44100, 44100, SampleFormat::F32)], vec![range(6, 48000, 48000, SampleFormat::F32)], vec![range(2, 48000, 48000, SampleFormat::I16)]] {
            assert_eq!(super::voice_output_config_at_48k(default.clone(), ranges).sample_rate().0, 44100);
        }
        let already = SupportedStreamConfig::new(2, SampleRate(48000), SupportedBufferSize::Unknown, SampleFormat::I16);
        assert_eq!(super::voice_output_config_at_48k(already.clone(), []), already);
    }
}

#[cfg(test)]
mod voice_buffer_tests {
    use super::*;

    #[test]
    fn twenty_ms_voice_period_is_aligned_and_respects_device_limits() {
        use cpal::SupportedBufferSize::{Range, Unknown};
        assert_eq!(voice_buffer_frames(48000, &Unknown), 960);
        assert_eq!(voice_buffer_frames(44100, &Unknown), 896);
        assert_eq!(voice_buffer_frames(8000, &Unknown), 192);
        assert_eq!(voice_buffer_frames(48000, &Range {min: 1024, max: 4096}), 1024);
        assert_eq!(voice_buffer_frames(48000, &Range {min: 64, max: 512}), 512);
    }
}

pub static CACHED_AUDIO_DEVICES: std::sync::OnceLock<Arc<std::sync::Mutex<Option<(Vec<String>, Vec<String>)>>>> = std::sync::OnceLock::new();

pub fn get_audio_devices_cache() -> Arc<std::sync::Mutex<Option<(Vec<String>, Vec<String>)>>> {
    CACHED_AUDIO_DEVICES.get_or_init(|| Arc::new(std::sync::Mutex::new(None))).clone()
}

/// Identifiers of the "system default" entries listed when no device is found. They are
/// stored in the saved audio settings and recognised with `contains("Padrão")` (here, in
/// sound_effects.rs and in gateway/voice.rs), so they never change with the language;
/// `device_display_name` gives the text to show.
pub const DEFAULT_INPUT_DEVICE_ID: &str = "Microfone Padrão do Sistema";
pub const DEFAULT_OUTPUT_DEVICE_ID: &str = "Alto-falantes Padrão do Sistema";

/// The name to show for a device identifier: the default entries in the UI language, real
/// devices as they are.
pub fn device_display_name(id: &str) -> String {
    if id == DEFAULT_INPUT_DEVICE_ID {
        crate::utils::i18n_ui::pick("System Default Microphone", DEFAULT_INPUT_DEVICE_ID).to_string()
    } else if id == DEFAULT_OUTPUT_DEVICE_ID {
        crate::utils::i18n_ui::pick("System Default Speakers", DEFAULT_OUTPUT_DEVICE_ID).to_string()
    } else {
        id.to_string()
    }
}

pub fn enumerate_audio_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    if let Ok(devices) = host.input_devices() {
        for dev in devices {
            if let Ok(name) = dev.name() {
                if !inputs.contains(&name) {
                    inputs.push(name);
                }
            }
        }
    }

    if let Ok(devices) = host.output_devices() {
        for dev in devices {
            if let Ok(name) = dev.name() {
                if !outputs.contains(&name) {
                    outputs.push(name);
                }
            }
        }
    }

    if inputs.is_empty() {
        inputs.push(DEFAULT_INPUT_DEVICE_ID.to_string());
    }

    if outputs.is_empty() {
        outputs.push(DEFAULT_OUTPUT_DEVICE_ID.to_string());
    }

    let result = (inputs, outputs);
    if let Ok(mut cache) = get_audio_devices_cache().lock() {
        *cache = Some(result.clone());
    }
    result
}

/// Publishes the mic level for the voice panel and the meter. Only the UI reads it,
/// so while the window is hidden the async task that forwards it is not woken.
fn report_mic_level(level_tx: &mpsc::Sender<f32>, level: f32) {
    gateway::set_self_mic_level(level);
    if crate::ui::helpers::APP_IS_VISIBLE.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = level_tx.try_send(level);
    }
}

pub fn push_to_mic_queues(pcm_q: &mut VecDeque<f32>, pcm_samples: &[f32], loop_samples: &[f32], level: f32) {
    for &s in pcm_samples {
        let clamped = s.clamp(-1.0, 1.0);
        if pcm_q.len() < 96000 { pcm_q.push_back(clamped); }
    }
    #[cfg(feature = "automation")]
    {
        crate::utils::test_hooks::mic_captured(pcm_samples.len());
        crate::utils::test_hooks::voice_captured(pcm_samples);
    }
    if pcm_q.len() >= 960 {
        gateway::MIC_FRAME_READY.notify_one();
    }
    if gateway::is_testing_mic() {
        let vad = gateway::get_vad_threshold();
        let is_active = level >= vad;
        pub static LAST_LOG: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
        let log_mtx = LAST_LOG.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
        if let Ok(mut last) = log_mtx.lock() {
            if last.elapsed().as_millis() >= 1000 {
                *last = std::time::Instant::now();
                info!("🧪 [TEST MIC STATS] mic_level={:.3}, vad_threshold={:.3}, active={}", level, vad, is_active);
            }
        }
        if let Ok(mut loop_q) = gateway::get_mic_loopback_queue().lock() {
            if loop_q.len() > 6000 {
                let excess = loop_q.len() - 3500;
                loop_q.drain(0..excess);
            }
            for &s in loop_samples {
                let sample_to_play = if is_active { s.clamp(-1.0, 1.0) } else { 0.0 };
                loop_q.push_back(sample_to_play);
            }
        }
    }
}

/// Which capture may feed the call: the one started last. A capture that has been
/// replaced keeps running for a moment (its device is closed by its own thread) but its
/// samples are dropped.
static CAPTURE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// What the capture did since the last report (see `take_capture_report`).
static CAPTURE_OPENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CAPTURE_BLOCKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CAPTURE_LATE_BLOCKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CAPTURE_WORST_GAP_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CAPTURE_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One line about the microphone capture since the last call, for the periodic send log:
/// blocks delivered, blocks that came more than half a block late, the longest wait
/// between two blocks, and how often the device was opened.
pub fn take_capture_report() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    format!(
        "capture: rate={} Hz blocks={} late_blocks={} longest_gap={} ms opens={}",
        CAPTURE_RATE.load(Relaxed), CAPTURE_BLOCKS.swap(0, Relaxed), CAPTURE_LATE_BLOCKS.swap(0, Relaxed),
        CAPTURE_WORST_GAP_MS.swap(0, Relaxed), CAPTURE_OPENS.swap(0, Relaxed),
    )
}

/// What a capture callback does with a block once it is mono: microphone gain, the level
/// for the meter, 48 kHz, and into the queue of the call.
struct CaptureSink {
    generation: u64,
    sample_rate: u32,
    level_tx: mpsc::Sender<f32>,
    queue: Arc<std::sync::Mutex<VecDeque<f32>>>,
    resampler: crate::audio::dsp::PhaseResampler,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    last_block: Option<std::time::Instant>,
}

impl CaptureSink {
    fn new(generation: u64, sample_rate: u32, level_tx: mpsc::Sender<f32>) -> Self {
        CAPTURE_RATE.store(sample_rate, std::sync::atomic::Ordering::Relaxed);
        Self {
            generation,
            sample_rate,
            level_tx,
            queue: gateway::get_mic_pcm_queue(),
            resampler: crate::audio::dsp::PhaseResampler::new(),
            mono: Vec::with_capacity(2048),
            resampled: Vec::with_capacity(2048),
            last_block: None,
        }
    }

    fn push(&mut self, samples: impl Iterator<Item = f32>) {
        use std::sync::atomic::Ordering::Relaxed;
        if CAPTURE_GENERATION.load(Relaxed) != self.generation {
            return;
        }
        let mic_gain = gateway::get_mic_volume();
        self.mono.clear();
        let mut sum_sq = 0.0f32;
        for s in samples {
            let s = s * mic_gain;
            sum_sq += s * s;
            self.mono.push(s);
        }
        let frames = self.mono.len();
        if frames == 0 {
            return;
        }

        // Timing of the blocks, for the report.
        let now = std::time::Instant::now();
        if let Some(last) = self.last_block.replace(now) {
            let gap_ms = now.duration_since(last).as_secs_f32() * 1000.0;
            let block_ms = frames as f32 * 1000.0 / self.sample_rate.max(1) as f32;
            if gap_ms > block_ms * 1.5 {
                CAPTURE_LATE_BLOCKS.fetch_add(1, Relaxed);
            }
            CAPTURE_WORST_GAP_MS.fetch_max(gap_ms as u32, Relaxed);
        }
        CAPTURE_BLOCKS.fetch_add(1, Relaxed);

        // The meter and the gate mark on the settings page are on one scale: the gate
        // looks at the voice after the automatic gain, so the meter shows it with that
        // gain too (1.0 while the automatic gain is off).
        let rms = (sum_sq / frames as f32).sqrt() * crate::audio::mic_gain::current_gain();
        let level = (rms * 6.0).min(1.0);
        report_mic_level(&self.level_tx, level);

        if let Ok(mut q) = self.queue.lock() {
            if self.sample_rate == 48000 {
                push_to_mic_queues(&mut q, &self.mono, &self.mono, level);
            } else {
                self.resampled.clear();
                self.resampler.resample(&self.mono, self.sample_rate, 48000, &mut self.resampled);
                push_to_mic_queues(&mut q, &self.resampled, &self.resampled, level);
            }
        }
    }
}

/// Microphone capture running on a thread of its own. Opening a WASAPI device enumerates
/// every endpoint and initializes the audio client, which can take seconds (wireless
/// headsets) and froze the UI when it ran on the UI thread. Dropping the handle stops the
/// capture and closes the device.
pub struct MicCapture {
    _stop: std::sync::mpsc::Sender<()>,
}

pub fn start_mic_capture(device_name: String, level_tx: mpsc::Sender<f32>) -> MicCapture {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    #[cfg(feature = "automation")]
    if crate::utils::test_hooks::mic_synth() {
        info!("🧪 [TEST MIC] Tom sintético no lugar do microfone.");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gen_stop = Arc::clone(&stop);
        let _ = std::thread::Builder::new().name("mic-synth".into())
            .spawn(move || crate::utils::test_hooks::run_synth_mic(gen_stop));
        let _ = std::thread::Builder::new().name("mic-capture-owner".into()).spawn(move || {
            let _ = stop_rx.recv();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        return MicCapture { _stop: stop_tx };
    }
    // From here on the capture that was running (if any) no longer feeds the call, even
    // though its device is closed a little later by its own thread: two captures never
    // write into the one queue at the same time.
    let generation = CAPTURE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    CAPTURE_OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("mic-capture-owner".into()).spawn(move || {
        let stream = open_mic_stream(device_name, level_tx, generation);
        if stream.is_none() {
            log::warn!("🎙️ Não foi possível abrir o microfone.");
        }
        // Blocks until the handle is dropped (the sender goes away), then closes the device.
        let _ = stop_rx.recv();
        drop(stream);
    });
    if let Err(e) = spawned {
        log::error!("Falha ao iniciar a thread do microfone: {:?}", e);
    }
    MicCapture { _stop: stop_tx }
}

fn open_mic_stream(
    device_name: String,
    level_tx: mpsc::Sender<f32>,
    generation: u64,
) -> Option<cpal::Stream> {
    let host = cpal::default_host();

    // Test builds can feed the "microphone" from whatever plays on the default output
    // (WASAPI loopback), so a reference recording can stand in for a voice.
    #[cfg(feature = "automation")]
    let loopback = crate::utils::test_hooks::mic_from_loopback();
    #[cfg(not(feature = "automation"))]
    let loopback = false;

    let (target_dev, config) = if loopback {
        let out = host.default_output_device()?;
        let cfg = out.default_output_config().ok()?;
        info!("🧪 [TEST MIC] Capturando a saída padrão por loopback como microfone: {}", out.name().unwrap_or_default());
        (out, cfg)
    } else {
        let devices = host.input_devices().ok()?;
        let dev = if device_name.is_empty() || device_name.contains("Padrão") {
            host.default_input_device()?
        } else {
            devices.into_iter().find(|d| {
                d.name().map(|n| n == device_name).unwrap_or(false)
            }).or_else(|| host.default_input_device())?
        };
        let default = dev.default_input_config().ok()?;
        // Opus runs at 48 kHz. The device's default format is often 44.1 kHz even when the
        // hardware runs at 48 (PipeWire's ALSA device says so): asking for 48 kHz saves two
        // conversions, the second of which (PhaseResampler, straight-line interpolation)
        // left images of the high frequencies in the voice.
        let cfg = voice_output_config_at_48k(default.clone(), dev.supported_input_configs().into_iter().flatten());
        if cfg.sample_rate() != default.sample_rate() {
            info!("Microphone: asked the device for 48000 Hz instead of its default {} Hz, so the voice is not resampled.", default.sample_rate().0);
        }
        (dev, cfg)
    };
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    info!("Capturando microfone: {}Hz, {} canal(is), formato={:?}", sample_rate, channels, config.sample_format());

    #[cfg(not(target_os = "linux"))]
    let stream_config: cpal::StreamConfig = config.clone().into();
    #[cfg(target_os = "linux")]
    let stream_config = linux_voice_stream_config(&target_dev, &config, true);

    // Each sample format only differs in how a frame becomes one mono sample.
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let mut sink = CaptureSink::new(generation, sample_rate, level_tx);
            target_dev.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &_| {
                    let n = channels.max(1);
                    sink.push(data.chunks_exact(n).map(|f| f.iter().sum::<f32>() / n as f32));
                },
                move |err| {
                    log::error!("Erro no Stream de Microfone: {:?}", err);
                },
                None,
            ).ok()?
        }
        cpal::SampleFormat::I16 => {
            let mut sink = CaptureSink::new(generation, sample_rate, level_tx);
            target_dev.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &_| {
                    let n = channels.max(1);
                    sink.push(data.chunks_exact(n).map(|f| f.iter().map(|&x| x as f32 / 32768.0).sum::<f32>() / n as f32));
                },
                move |err| {
                    log::error!("Erro no Stream de Microfone I16: {:?}", err);
                },
                None,
            ).ok()?
        }
        cpal::SampleFormat::I32 => {
            let mut sink = CaptureSink::new(generation, sample_rate, level_tx);
            target_dev.build_input_stream(
                &stream_config,
                move |data: &[i32], _: &_| {
                    let n = channels.max(1);
                    sink.push(data.chunks_exact(n).map(|f| f.iter().map(|&x| x as f32 / 2147483648.0).sum::<f32>() / n as f32));
                },
                move |err| {
                    log::error!("Erro no Stream de Microfone I32: {:?}", err);
                },
                None,
            ).ok()?
        }
        _ => return None,
    };

    if stream.play().is_ok() {
        info!("🎙️ Stream de Microfone INICIADO COM SUCESSO! Rate: {}Hz -> 48000Hz", sample_rate);
        Some(stream)
    } else {
        log::error!("Falha ao executar stream.play() no microfone!");
        None
    }
}

pub fn start_mic_loopback_stream(device_name: String) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let devices = host.output_devices().ok()?;

    let target_dev = if device_name.is_empty() || device_name.contains("Padrão") {
        host.default_output_device()?
    } else {
        devices.into_iter().find(|d| {
            d.name().map(|n| n == device_name).unwrap_or(false)
        }).or_else(|| host.default_output_device())?
    };

    let target_dev_name = target_dev.name().unwrap_or_else(|_| "desconhecido".to_string());
    let config = match target_dev.default_output_config() {
        // The microphone test plays the samples of the call's queue, which are at 48 kHz.
        Ok(c) => voice_output_config_at_48k(c, target_dev.supported_output_configs().into_iter().flatten()),
        Err(e) => {
            log::error!("❌ [LOOPBACK] Erro ao obter default_output_config para '{}': {:?}", target_dev_name, e);
            return None;
        }
    };
    let out_sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    let loop_q_arc = gateway::get_mic_loopback_queue();

    info!("🎧 [LOOPBACK] Iniciando no dispositivo '{}': {}Hz, {} canal(is), formato={:?}", target_dev_name, out_sample_rate, channels, config.sample_format());

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let q_arc = Arc::clone(&loop_q_arc);
            let mut is_started = false;
            target_dev.build_output_stream(
                &config.into(),
                move |output: &mut [f32], _| {
                    if !gateway::is_testing_mic() {
                        for s in output.iter_mut() { *s = 0.0; }
                        return;
                    }
                    if let Ok(mut loop_q) = q_arc.lock() {
                        if !is_started {
                            if loop_q.len() < 2000 {
                                for s in output.iter_mut() { *s = 0.0; }
                                return;
                            }
                            is_started = true;
                        }
                        let frames = output.len() / channels.max(1);
                        let mut sum_sq = 0.0f32;
                        for f in 0..frames {
                            let s = loop_q.pop_front().unwrap_or(0.0);
                            let sample = (s * 3.0).clamp(-1.0, 1.0);
                            sum_sq += sample * sample;
                            for ch in 0..channels {
                                output[f * channels + ch] = sample;
                            }
                        }
                        let out_rms = (sum_sq / frames.max(1) as f32).sqrt();
                        pub static LAST_OUT_LOG: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
                        let out_log_mtx = LAST_OUT_LOG.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
                        if let Ok(mut last) = out_log_mtx.lock() {
                            if last.elapsed().as_millis() >= 1000 {
                                *last = std::time::Instant::now();
                                info!("🎧 [LOOPBACK PLAYBACK F32] is_started={}, q_len={}, frames={}, out_rms={:.4}", is_started, loop_q.len(), frames, out_rms);
                            }
                        }
                    } else {
                        for s in output.iter_mut() { *s = 0.0; }
                    }
                },
                move |err| { log::error!("Erro no Stream Loopback F32: {:?}", err); },
                None,
            ).ok()?
        }
        cpal::SampleFormat::I16 => {
            let q_arc = Arc::clone(&loop_q_arc);
            let mut is_started = false;
            target_dev.build_output_stream(
                &config.into(),
                move |output: &mut [i16], _| {
                    if !gateway::is_testing_mic() {
                        for s in output.iter_mut() { *s = 0; }
                        return;
                    }
                    if let Ok(mut loop_q) = q_arc.lock() {
                        if !is_started {
                            if loop_q.len() < 2000 {
                                for s in output.iter_mut() { *s = 0; }
                                return;
                            }
                            is_started = true;
                        }
                        let frames = output.len() / channels.max(1);
                        let mut sum_sq = 0.0f32;
                        for f in 0..frames {
                            let s = loop_q.pop_front().unwrap_or(0.0);
                            let sample = (s * 3.0).clamp(-1.0, 1.0);
                            sum_sq += sample * sample;
                            let val = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                            for ch in 0..channels {
                                output[f * channels + ch] = val;
                            }
                        }
                        let out_rms = (sum_sq / frames.max(1) as f32).sqrt();
                        pub static LAST_OUT_LOG: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
                        let out_log_mtx = LAST_OUT_LOG.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
                        if let Ok(mut last) = out_log_mtx.lock() {
                            if last.elapsed().as_millis() >= 1000 {
                                *last = std::time::Instant::now();
                                info!("🎧 [LOOPBACK PLAYBACK I16] is_started={}, q_len={}, frames={}, out_rms={:.4}", is_started, loop_q.len(), frames, out_rms);
                            }
                        }
                    } else {
                        for s in output.iter_mut() { *s = 0; }
                    }
                },
                move |err| { log::error!("Erro no Stream Loopback I16: {:?}", err); },
                None,
            ).ok()?
        }
        cpal::SampleFormat::I32 => {
            let q_arc = Arc::clone(&loop_q_arc);
            let mut is_started = false;
            target_dev.build_output_stream(
                &config.into(),
                move |output: &mut [i32], _| {
                    if !gateway::is_testing_mic() {
                        for s in output.iter_mut() { *s = 0; }
                        return;
                    }
                    if let Ok(mut loop_q) = q_arc.lock() {
                        if !is_started {
                            if loop_q.len() < 2000 {
                                for s in output.iter_mut() { *s = 0; }
                                return;
                            }
                            is_started = true;
                        }
                        let frames = output.len() / channels.max(1);
                        let mut sum_sq = 0.0f32;
                        for f in 0..frames {
                            let s = loop_q.pop_front().unwrap_or(0.0);
                            let sample = (s * 3.0).clamp(-1.0, 1.0);
                            sum_sq += sample * sample;
                            let val = (sample * 2147483647.0).clamp(-2147483648.0, 2147483647.0) as i32;
                            for ch in 0..channels {
                                output[f * channels + ch] = val;
                            }
                        }
                        let out_rms = (sum_sq / frames.max(1) as f32).sqrt();
                        pub static LAST_OUT_LOG: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
                        let out_log_mtx = LAST_OUT_LOG.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
                        if let Ok(mut last) = out_log_mtx.lock() {
                            if last.elapsed().as_millis() >= 1000 {
                                *last = std::time::Instant::now();
                                info!("🎧 [LOOPBACK PLAYBACK I32] is_started={}, q_len={}, frames={}, out_rms={:.4}", is_started, loop_q.len(), frames, out_rms);
                            }
                        }
                    } else {
                        for s in output.iter_mut() { *s = 0; }
                    }
                },
                move |err| { log::error!("Erro no Stream Loopback I32: {:?}", err); },
                None,
            ).ok()?
        }
        cpal::SampleFormat::U16 => {
            let q_arc = Arc::clone(&loop_q_arc);
            let mut is_started = false;
            target_dev.build_output_stream(
                &config.into(),
                move |output: &mut [u16], _| {
                    if !gateway::is_testing_mic() {
                        for s in output.iter_mut() { *s = 32768; }
                        return;
                    }
                    if let Ok(mut loop_q) = q_arc.lock() {
                        if !is_started {
                            if loop_q.len() < 2000 {
                                for s in output.iter_mut() { *s = 32768; }
                                return;
                            }
                            is_started = true;
                        }
                        let frames = output.len() / channels.max(1);
                        let mut sum_sq = 0.0f32;
                        for f in 0..frames {
                            let s = loop_q.pop_front().unwrap_or(0.0);
                            let sample = (s * 3.0).clamp(-1.0, 1.0);
                            sum_sq += sample * sample;
                            let val = ((sample + 1.0) * 32767.5).clamp(0.0, 65535.0) as u16;
                            for ch in 0..channels {
                                output[f * channels + ch] = val;
                            }
                        }
                        let out_rms = (sum_sq / frames.max(1) as f32).sqrt();
                        pub static LAST_OUT_LOG: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
                        let out_log_mtx = LAST_OUT_LOG.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
                        if let Ok(mut last) = out_log_mtx.lock() {
                            if last.elapsed().as_millis() >= 1000 {
                                *last = std::time::Instant::now();
                                info!("🎧 [LOOPBACK PLAYBACK U16] is_started={}, q_len={}, frames={}, out_rms={:.4}", is_started, loop_q.len(), frames, out_rms);
                            }
                        }
                    } else {
                        for s in output.iter_mut() { *s = 32768; }
                    }
                },
                move |err| { log::error!("Erro no Stream Loopback U16: {:?}", err); },
                None,
            ).ok()?
        }
        _ => return None,
    };

    if stream.play().is_ok() {
        info!("🎧 Stream Loopback ('Se Ouvir') iniciado com sucesso!");
        Some(stream)
    } else {
        None
    }
}

