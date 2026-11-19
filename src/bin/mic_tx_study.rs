//! Offline study of the outgoing microphone path. Opens no audio device and no socket.
//!
//!   mic_tx_study sim <in_mono.f32> <in_rate> <out_prefix> [key=value ...]
//!
//! Runs a mono f32 file through the same steps as the app: the capture callback
//! (microphone gain, level meter, `PhaseResampler` to 48 kHz, `push_to_mic_queues`), then
//! the send loop of src/gateway/voice.rs rebuilt here step by step (queue pop with the
//! padding ramp, `VoiceProcessor::process_frame`, the voice gate with hangover, trailing
//! silence frames, keepalives, mono to stereo, opus-rs encoder created as voice.rs does).
//! The send loop itself lives inside a closure in voice.rs and cannot be called, so the
//! copy here has to be kept in step by hand.
//!
//! Keys (defaults are what the app does today):
//!   gain=1.0        microphone gain (0..2)
//!   ns=none         noise suppression: none | standard | advanced
//!   aec=0 agc=0     echo canceller / automatic gain
//!   vad=0.05        voice gate threshold (level = rms * 6)
//!   cb=0            capture callback size in input frames (0 = the app's 20 ms rule)
//!   gate=app        app | none (none: every frame is encoded and sent)
//!   hang=12 sil=5   hangover frames, trailing silence frames
//!   feed=sent       sent | always (always: frames that are not sent still go through the encoder)
//!   preroll=0       frames before the gate opens that are sent too
//!   fade=0          ms of fade-in on the first frame and fade-out on the last frame of an utterance
//!   fadein=1        0: with fade=N only the end of an utterance is faded
//!   reset=0         1: a new encoder at every utterance start
//!   zfeed=0         N: when an utterance ends (and at the start of the call) the encoder is given
//!                   N frames of zeros whose output is thrown away, so its state follows what
//!                   the receiver's decoder has after the silence frames
//!   zsend=0         1: the silence frames are the encoder's own coding of zeros, not F8 FF FE
//!   stall=K:N       capture callbacks K..K+N arrive together, late (shows the padding ramp)
//!   impl=old        old: the copy of the former send loop below, with the keys above.
//!                   new: the app's sender itself (gateway::voice_tx): only gain, ns, aec, agc,
//!                   vad, cb, gate and stall apply. A turn that waits for a late microphone
//!                   takes no packet slot and gives no record. Its packets are libopus'
//!                   (mono, speech coding), and <prefix>.proc.f32 holds the frames in the
//!                   order they are sent, 60 ms behind <prefix>.pre.f32.
//!
//! Output: <prefix>.pkts.bin (one record per 20 ms loop turn, the format of
//! voice_rx_harness: u32 LE length + bytes, 0xFFFF_FFFE = nothing sent), <prefix>.pre.f32
//! (frames as popped from the queue), <prefix>.proc.f32 (frames as given to the encoder),
//! <prefix>.ticks.csv, <prefix>.meter.csv (the level the settings page meter shows).

use std::collections::{HashMap, VecDeque};

use litecord::audio::dsp::{PhaseResampler, VoiceProcessor};
use litecord::audio::engine::push_to_mic_queues;
use litecord::gateway::voice_tx::{take_mic_frame, TxInput, TxSettings, VoiceTx};
use opus_rs::{Application, OpusEncoder};

const SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

fn read_f32(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn write_f32(path: &str, data: &[f32]) {
    let mut out = Vec::with_capacity(data.len() * 4);
    for s in data { out.extend_from_slice(&s.to_le_bytes()); }
    std::fs::write(path, out).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

fn db(rms: f32) -> f32 { 20.0 * rms.max(1e-9).log10() }

fn rms(x: &[f32]) -> f32 { (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt() }

fn new_encoder() -> OpusEncoder {
    // Exactly as src/gateway/voice.rs: nothing else is set on it.
    OpusEncoder::new(48000, 2, Application::Voip).expect("opus encoder")
}

fn encode(enc: &mut OpusEncoder, frame: &[f32; 960], out: &mut [u8]) -> Vec<u8> {
    let mut stereo = [0.0f32; 1920];
    for (i, &s) in frame.iter().enumerate() {
        stereo[i * 2] = s;
        stereo[i * 2 + 1] = s;
    }
    match enc.encode(&stereo, 960, out) {
        Ok(n) => out[..n].to_vec(),
        Err(_) => SILENCE.to_vec(),
    }
}

struct Tick {
    pre: [f32; 960],
    proc_: [f32; 960],
    level: f32,
    has_audio: bool,
    action: char, // A audio packet, S silence packet, N nothing sent
    packet: Option<Vec<u8>>,
    hang: usize,
    padded: char, // - full frame, P padding ramp, Z frame of zeros
    timeout: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 || args[0] != "sim" {
        eprintln!("usage: mic_tx_study sim <in_mono.f32> <in_rate> <out_prefix> [key=value ...] (see the file header)");
        std::process::exit(2);
    }
    let input = read_f32(&args[1]);
    let in_rate: u32 = args[2].parse().expect("rate");
    let prefix = &args[3];
    let kv: HashMap<String, String> = args[4..].iter().filter_map(|a| a.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let get = |k: &str, d: &str| kv.get(k).cloned().unwrap_or_else(|| d.to_string());
    let gain: f32 = get("gain", "1.0").parse().unwrap();
    let ns = get("ns", "none");
    let aec = get("aec", "0") == "1";
    let agc = get("agc", "0") == "1";
    let vad: f32 = get("vad", "0.05").parse().unwrap();
    let mut cb: usize = get("cb", "0").parse().unwrap();
    if cb == 0 {
        // voice_buffer_frames() of src/audio/engine.rs with no device limits.
        cb = ((in_rate / 50).max(128).div_ceil(64) * 64) as usize;
    }
    let gate_on = get("gate", "app") != "none";
    let hang_frames: usize = get("hang", "12").parse().unwrap();
    let sil_frames: usize = get("sil", "5").parse().unwrap();
    let feed_always = get("feed", "sent") == "always";
    let preroll: usize = get("preroll", "0").parse().unwrap();
    let fade_ms: f32 = get("fade", "0").parse().unwrap();
    let reset_enc = get("reset", "0") == "1";
    let zfeed: usize = get("zfeed", "0").parse().unwrap();
    let zsend = get("zsend", "0") == "1";
    let stall: Option<(usize, usize)> = kv.get("stall").and_then(|s| s.split_once(':')).map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()));
    let fade_n = ((fade_ms * 48.0) as usize).min(960);
    let fade_in_n = if get("fadein", "1") == "1" { fade_n } else { 0 };

    let mut resampler = PhaseResampler::new();
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut voice_proc = VoiceProcessor::new();
    let mut enc = new_encoder();
    let mut opus_out = vec![0u8; 1000];
    let mut ticks: Vec<Tick> = Vec::new();
    let mut meter = String::from("callback,t_ms,rms_db,level\n");

    // Send loop state, as in voice.rs.
    let mut remaining_silence_frames: usize = 10;
    let mut speech_hangover_frames: usize = 0;
    let mut silence_keepalive_counter: u32 = 0;
    let mut in_utterance = false;

    let cb_ms = cb as f64 * 1000.0 / in_rate as f64;
    let n_cb = input.len() / cb;
    let mut wait_start_ms = 0.0f64;

    let mut turn = |queue: &mut VecDeque<f32>, timeout: bool, ticks: &mut Vec<Tick>| {
        let mut pcm_frame = [0.0f32; 960];
        let mut padded = '-';
        if queue.len() > 960 * 10 {
            let drop_count = queue.len() - 960 * 2;
            queue.drain(0..drop_count);
        }
        if queue.len() >= 960 {
            for s in pcm_frame.iter_mut() { *s = queue.pop_front().unwrap_or(0.0); }
        } else if queue.len() >= 480 {
            let avail = queue.len();
            for s in pcm_frame.iter_mut().take(avail) { *s = queue.pop_front().unwrap_or(0.0); }
            let last = pcm_frame[avail - 1];
            for i in avail..960 {
                let decay = (960 - i) as f32 / (960 - avail) as f32;
                pcm_frame[i] = last * decay;
            }
            padded = 'P';
        } else {
            padded = 'Z';
        }
        let pre = pcm_frame;
        voice_proc.process_frame(&mut pcm_frame, &ns, aec, agc);
        let level = (rms(&pcm_frame) * 6.0).min(1.0);
        let has_audio = !gate_on || level >= vad;

        let was_in_utterance = in_utterance;
        let (send, is_silence_packet) = if has_audio {
            speech_hangover_frames = hang_frames;
            remaining_silence_frames = sil_frames;
            silence_keepalive_counter = 0;
            (true, false)
        } else if speech_hangover_frames > 0 {
            speech_hangover_frames -= 1;
            (true, false)
        } else if remaining_silence_frames > 0 {
            remaining_silence_frames -= 1;
            (true, true)
        } else {
            silence_keepalive_counter = silence_keepalive_counter.wrapping_add(1);
            if silence_keepalive_counter >= 125 {
                silence_keepalive_counter = 0;
                (true, true)
            } else {
                (false, false)
            }
        };
        let audio = send && !is_silence_packet;
        let first = audio && !was_in_utterance;
        let last = audio && !has_audio && speech_hangover_frames == 0;
        in_utterance = audio;

        if first && reset_enc { enc = new_encoder(); }
        if first && preroll > 0 {
            // Frames before the gate opened are sent after all: they take the place of the
            // loop turns that sent nothing (a sender with `preroll` frames of look-back).
            let n = ticks.len();
            let from = n.saturating_sub(preroll);
            let mut started = false;
            for t in ticks[from..].iter_mut() {
                if t.action == 'A' { continue; }
                let mut f = t.proc_;
                if !started && fade_in_n > 0 {
                    for (i, s) in f.iter_mut().take(fade_in_n).enumerate() { *s *= i as f32 / fade_in_n as f32; }
                }
                started = true;
                t.proc_ = f;
                t.packet = Some(encode(&mut enc, &f, &mut opus_out));
                t.action = 'A';
            }
        } else if first && fade_in_n > 0 {
            for (i, s) in pcm_frame.iter_mut().take(fade_in_n).enumerate() { *s *= i as f32 / fade_in_n as f32; }
        }
        if last && fade_n > 0 {
            for i in 0..fade_n { pcm_frame[960 - fade_n + i] *= 1.0 - (i + 1) as f32 / fade_n as f32; }
        }

        let (action, packet) = if audio {
            ('A', Some(encode(&mut enc, &pcm_frame, &mut opus_out)))
        } else {
            if feed_always { let _ = encode(&mut enc, &pcm_frame, &mut opus_out); }
            if was_in_utterance || ticks.is_empty() {
                for _ in 0..zfeed { let _ = encode(&mut enc, &[0.0f32; 960], &mut opus_out); }
            }
            if send && zsend { ('S', Some(encode(&mut enc, &[0.0f32; 960], &mut opus_out))) }
            else if send { ('S', Some(SILENCE.to_vec())) } else { ('N', None) }
        };
        ticks.push(Tick { pre, proc_: pcm_frame, level, has_audio, action, packet, hang: speech_hangover_frames, padded, timeout });
    };

    // The app's own sender.
    let use_new = get("impl", "old") == "new";
    let mut tx = VoiceTx::new();
    let mut new_ticks: Vec<Tick> = Vec::new();
    let tx_settings = TxSettings { noise_suppression: &ns, echo_cancellation: aec, auto_gain: agc, gate_threshold: if gate_on { vad } else { 0.0 } };
    let mut turn_new = |queue: &mut VecDeque<f32>, timeout: bool| {
        let (frame, dropped) = take_mic_frame(queue);
        tx.note_trim(dropped);
        let t = tx.turn(match frame.as_ref() { Some(f) => TxInput::Frame(f), None => TxInput::Stall }, &tx_settings);
        if !t.advance { return; }
        let mut action = 'N';
        let mut packet = None;
        for p in t.packets {
            action = if p.audio { 'A' } else { 'S' };
            packet = Some(p.opus);
        }
        new_ticks.push(Tick { pre: frame.unwrap_or([0.0; 960]), proc_: t.processed.map(|b| *b).unwrap_or([0.0; 960]), level: t.level, has_audio: t.speaking,
            action, packet, hang: 0, padded: if frame.is_some() { '-' } else { 'Z' }, timeout });
    };

    let mut pending: Vec<f32> = Vec::new(); // callbacks held back by a stall
    for k in 0..n_cb {
        let data = &input[k * cb..(k + 1) * cb];
        // Capture callback, as open_mic_stream() in src/audio/engine.rs (mono input: the
        // channel average is the sample itself).
        let mono: Vec<f32> = data.iter().map(|s| s * gain).collect();
        let level = (rms(&mono) * 6.0).min(1.0);
        meter.push_str(&format!("{k},{:.1},{:.2},{:.4}\n", k as f64 * cb_ms, db(rms(&mono)), level));
        let mut resampled = Vec::with_capacity(cb * 48000 / in_rate as usize + 32);
        if in_rate == 48000 { resampled.extend_from_slice(&mono); } else { resampler.resample(&mono, in_rate, 48000, &mut resampled); }

        let held = stall.map(|(s, n)| k >= s && k < s + n).unwrap_or(false);
        if held {
            pending.extend_from_slice(&resampled);
            continue;
        }
        let arrival_ms = (k + 1) as f64 * cb_ms;
        while arrival_ms - wait_start_ms >= 60.0 {
            if use_new { turn_new(&mut queue, true) } else { turn(&mut queue, true, &mut ticks) }
            wait_start_ms += 60.0;
        }
        if !pending.is_empty() {
            push_to_mic_queues(&mut queue, &pending, &[], 0.0);
            pending.clear();
        }
        push_to_mic_queues(&mut queue, &resampled, &[], level);
        while queue.len() >= 960 {
            if use_new { turn_new(&mut queue, false) } else { turn(&mut queue, false, &mut ticks) }
        }
        wait_start_ms = arrival_ms;
    }
    drop(turn);
    drop(turn_new);
    if use_new {
        eprintln!("{}", tx.take_report());
        ticks = new_ticks;
    }

    let mut pk = Vec::new();
    let mut pre = Vec::with_capacity(ticks.len() * 960);
    let mut proc_ = Vec::with_capacity(ticks.len() * 960);
    let mut csv = String::from("tick,pre_db,proc_db,level,has_audio,action,bytes,toc,hang,padded,timeout\n");
    let (mut n_a, mut n_s, mut n_n, mut bytes_a) = (0usize, 0usize, 0usize, 0usize);
    for (i, t) in ticks.iter().enumerate() {
        match &t.packet {
            Some(p) => { pk.extend_from_slice(&(p.len() as u32).to_le_bytes()); pk.extend_from_slice(p); }
            None => pk.extend_from_slice(&0xFFFF_FFFEu32.to_le_bytes()),
        }
        match t.action { 'A' => { n_a += 1; bytes_a += t.packet.as_ref().map(|p| p.len()).unwrap_or(0); } 'S' => n_s += 1, _ => n_n += 1 }
        pre.extend_from_slice(&t.pre);
        proc_.extend_from_slice(&t.proc_);
        csv.push_str(&format!("{i},{:.2},{:.2},{:.4},{},{},{},{},{},{},{}\n", db(rms(&t.pre)), db(rms(&t.proc_)), t.level, t.has_audio as u8, t.action,
            t.packet.as_ref().map(|p| p.len()).unwrap_or(0), t.packet.as_ref().map(|p| p[0] as i32).unwrap_or(-1), t.hang, t.padded, t.timeout as u8));
    }
    std::fs::write(format!("{prefix}.pkts.bin"), pk).expect("pkts");
    write_f32(&format!("{prefix}.pre.f32"), &pre);
    write_f32(&format!("{prefix}.proc.f32"), &proc_);
    std::fs::write(format!("{prefix}.ticks.csv"), csv).expect("csv");
    std::fs::write(format!("{prefix}.meter.csv"), meter).expect("meter");
    println!("ticks={} audio={} silence={} nosend={} mean_audio_bytes={:.1} cb={} frames ({:.2} ms)",
        ticks.len(), n_a, n_s, n_n, if n_a > 0 { bytes_a as f64 / n_a as f64 } else { 0.0 }, cb, cb_ms);
}
