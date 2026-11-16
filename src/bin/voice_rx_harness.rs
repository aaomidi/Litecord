//! Offline harness for the voice receive chain. Opens no audio device and no socket.
//!
//!   voice_rx_harness decode <packets.bin> <out.f32> <log.csv> [mode]
//!   voice_rx_harness mix <out.f32> <diag.csv> <rate> <cb_frames> <f32|i16> <n_callbacks> <speaker>...
//!       speaker = pcm.f32:arrivals.txt:volume:priority:muted(0/1)
//!       Smart Voice Ducking is off, as in the app, unless VOICE_RX_DUCKING=1 is set.
//!   voice_rx_harness limit <in.f32> <out.f32>   (the mix limiter on a mono file, delay removed)
//!
//! `decode` and `mix` run the app's own code: `VoiceRxDecoder` and `VoiceMixer` from
//! src/gateway/voice_rx.rs, which the network loop and the output callbacks of voice.rs use.
//!
//! packets.bin: records of u32 LE length + bytes. Length 0xFFFF_FFFF = packet lost in
//! transit (sequence number consumed, 20 ms pass). Length 0xFFFF_FFFE = sender sent
//! nothing for 20 ms (DTX: no sequence number consumed).
//! arrivals.txt: lines "t_ms n_pairs": at t_ms the next n_pairs pairs of pcm.f32
//! (interleaved stereo f32) are pushed to the speaker queue.

use std::collections::{HashMap, VecDeque};
use std::io::Write;

use litecord::gateway::voice_rx::{push_voice_frame, VoiceMixer, VoiceRxDecoder, VOICE_QUEUE_LIMIT};

fn read_f32(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn write_f32(path: &str, data: &[f32]) {
    let mut out = Vec::with_capacity(data.len() * 4);
    for s in data { out.extend_from_slice(&s.to_le_bytes()); }
    std::fs::write(path, out).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

fn decode_cmd(args: &[String]) {
    let raw = std::fs::read(&args[0]).expect("packets");
    let mut rx = VoiceRxDecoder::new();
    let mut out: Vec<f32> = Vec::new();
    let mut log = String::from("rec,kind,seq,bytes,toc,channels,plc_calls,plc_ok,plc_pairs,decoded_pairs,err\n");
    let mut now_ms = 0u64;
    let mut seq = 0u16;
    let (mut idx, mut rec) = (0usize, 0usize);
    while idx + 4 <= raw.len() {
        let len = u32::from_le_bytes([raw[idx], raw[idx + 1], raw[idx + 2], raw[idx + 3]]);
        idx += 4;
        now_ms += 20;
        let timestamp = (rec as u32).wrapping_mul(960);
        if len == 0xFFFF_FFFF {
            log.push_str(&format!("{rec},lost,{seq},0,,,,,0,0,\n"));
            seq = seq.wrapping_add(1);
            rec += 1;
            continue;
        }
        if len == 0xFFFF_FFFE {
            log.push_str(&format!("{rec},nosend,,0,,,,,0,0,\n"));
            rec += 1;
            continue;
        }
        let raw_opus = &raw[idx..idx + len as usize];
        idx += len as usize;
        let rtp_seq = seq;
        seq = seq.wrapping_add(1);
        if raw_opus.is_empty() { rec += 1; continue; }

        let frame = rx.packet(rtp_seq, timestamp, raw_opus, now_ms);
        for p in frame.concealed.iter().chain(frame.decoded.iter()) { out.push(p.0); out.push(p.1); }
        log.push_str(&format!("{rec},pkt,{rtp_seq},{},{},{},{},{},{},{},{}\n",
            raw_opus.len(), raw_opus[0], if (raw_opus[0] & 0x04) != 0 { 2 } else { 1 },
            frame.conceal_calls, frame.conceal_ok, frame.concealed.len(), frame.decoded.len(), frame.error.unwrap_or("")));
        rec += 1;
    }
    write_f32(&args[1], &out);
    std::fs::write(&args[2], log).expect("log");
    println!("records={rec} pairs_out={} stats={:?}", out.len() / 2, rx.stats);
}

struct Speaker {
    ssrc: u32,
    pcm: Vec<f32>,
    pos: usize,
    arrivals: Vec<(f64, usize)>,
    next: usize,
    settings: (bool, f32, i32),
}

fn mix_cmd(args: &[String]) {
    let out_sample_rate: u32 = args[2].parse().unwrap();
    let cb_frames: usize = args[3].parse().unwrap();
    let i16_path = args[4] == "i16";
    let n_callbacks: usize = args[5].parse().unwrap();
    let ducking = std::env::var("VOICE_RX_DUCKING").map(|v| v == "1").unwrap_or(false);
    let mut speakers: Vec<Speaker> = Vec::new();
    for (i, spec) in args[6..].iter().enumerate() {
        let p: Vec<&str> = spec.split(':').collect();
        let pcm = read_f32(p[0]);
        let arrivals: Vec<(f64, usize)> = if p[1].is_empty() {
            (0..pcm.len() / 2 / 960).map(|k| (k as f64 * 20.0, 960)).collect()
        } else {
            std::fs::read_to_string(p[1]).expect("arrivals").lines().filter(|l| !l.trim().is_empty()).map(|l| {
                let mut it = l.split_whitespace();
                (it.next().unwrap().parse().unwrap(), it.next().unwrap().parse().unwrap())
            }).collect()
        };
        let vol: f32 = p.get(2).map(|s| s.parse().unwrap()).unwrap_or(1.0);
        let prio: i32 = p.get(3).map(|s| s.parse().unwrap()).unwrap_or(0);
        let muted = p.get(4).map(|s| *s == "1").unwrap_or(false);
        speakers.push(Speaker { ssrc: 100 + i as u32, pcm, pos: 0, arrivals, next: 0, settings: (muted, vol, prio) });
    }

    let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
    let mut mixer = VoiceMixer::new(out_sample_rate);
    let mut all_out: Vec<f32> = Vec::with_capacity(n_callbacks * cb_frames * 2);
    let mut diag = String::from("cb,t_ms,ssrc,queue_before,ready,dry_pops,dropped_pairs,mult\n");

    for cb in 0..n_callbacks {
        let t_ms = cb as f64 * cb_frames as f64 * 1000.0 / out_sample_rate as f64;
        let mut dropped: HashMap<u32, usize> = HashMap::new();
        for sp in speakers.iter_mut() {
            while sp.next < sp.arrivals.len() && sp.arrivals[sp.next].0 <= t_ms {
                let n = sp.arrivals[sp.next].1;
                sp.next += 1;
                let end = (sp.pos + n * 2).min(sp.pcm.len());
                let pairs: Vec<(f32, f32)> = sp.pcm[sp.pos..end].chunks_exact(2).map(|c| (c[0], c[1])).collect();
                sp.pos = end;
                let q = queues.entry(sp.ssrc).or_insert_with(|| VecDeque::with_capacity(VOICE_QUEUE_LIMIT));
                let before = q.len();
                push_voice_frame(q, &[], &pairs);
                *dropped.entry(sp.ssrc).or_insert(0) += before + pairs.len() - q.len();
            }
        }
        let queue_before: HashMap<u32, usize> = queues.iter().map(|(k, q)| (*k, q.len())).collect();

        let settings: HashMap<u32, (bool, f32, i32)> = speakers.iter().map(|s| (s.ssrc, s.settings)).collect();
        let mixed = mixer.mix(&mut queues, cb_frames, ducking, |ssrc| settings.get(&ssrc).copied().unwrap_or((false, 1.0, 0)));
        // As the two callbacks of voice.rs write it: f32 as it is, i16 by truncation.
        if mixed.is_empty() {
            all_out.resize(all_out.len() + cb_frames * 2, 0.0);
        }
        for &(l, r) in mixed {
            if i16_path {
                all_out.push(((l * 32767.0) as i16) as f32 / 32768.0);
                all_out.push(((r * 32767.0) as i16) as f32 / 32768.0);
            } else {
                all_out.push(l);
                all_out.push(r);
            }
        }
        for sp in &speakers {
            let d = mixer.diag.get(&sp.ssrc).copied().unwrap_or_default();
            diag.push_str(&format!("{cb},{t_ms:.3},{},{},{},{},{},{}\n", sp.ssrc,
                queue_before.get(&sp.ssrc).copied().unwrap_or(0),
                d.ready as u8,
                d.dry_samples,
                dropped.get(&sp.ssrc).copied().unwrap_or(0),
                if mixer.diag.contains_key(&sp.ssrc) { d.gain / sp.settings.1.max(1e-9) } else { 1.0 }));
        }
    }
    write_f32(&args[0], &all_out);
    std::fs::File::create(&args[1]).unwrap().write_all(diag.as_bytes()).unwrap();
    println!("callbacks={n_callbacks} frames_out={} limiter_delay={}", all_out.len() / 2, mixer.limiter_delay());
}

fn limit_cmd(args: &[String]) {
    let x = read_f32(&args[0]);
    let mut mixer = VoiceMixer::new(48000);
    let delay = mixer.limiter_delay();
    let mut queues: HashMap<u32, VecDeque<(f32, f32)>> = HashMap::new();
    // A stream-audio key: no pre-buffer, no per-member settings, straight into the mix.
    let key = 0x8000_0001u32;
    let mut y: Vec<f32> = Vec::with_capacity(x.len() + delay);
    let mut pos = 0usize;
    while y.len() < x.len() + delay {
        let q = queues.entry(key).or_default();
        let end = (pos + 960).min(x.len());
        q.extend(x[pos..end].iter().map(|v| (*v, *v)));
        q.extend(std::iter::repeat((0.0, 0.0)).take(960 - (end - pos)));
        pos = end;
        let mixed = mixer.mix(&mut queues, 960, false, |_| (false, 1.0, 0));
        if mixed.is_empty() { y.resize(y.len() + 960, 0.0); }
        y.extend(mixed.iter().map(|p| p.0));
    }
    write_f32(&args[1], &y[delay..delay + x.len()]);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("decode") => decode_cmd(&args[1..]),
        Some("mix") => mix_cmd(&args[1..]),
        Some("limit") => limit_cmd(&args[1..]),
        _ => eprintln!("usage: voice_rx_harness decode|mix|limit ... (see the file header)"),
    }
}
