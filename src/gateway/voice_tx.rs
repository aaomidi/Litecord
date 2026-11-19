//! The sending side of a voice call: 20 ms microphone frames in, Opus packets out.
//!
//! Everything that decides what a listener hears where speech starts and stops is here, in
//! one place that can be run without a call (src/bin/mic_tx_study.rs drives it from files):
//! voice processing and automatic gain, the voice gate, and the Opus encoder.
//!
//! What it does, and why:
//! - The packets are libopus' (`audio::opus_encoder`), coded as the official client codes
//!   a voice. The encoder of opus-rs, used before, smeared sharp word starts into a burst
//!   of noise just ahead of them.
//! - Every packet comes from the encoder, in order, the silence frames after an utterance
//!   and the keepalives included (the encoder is given zeros for them). A receiver that
//!   decodes what it gets is therefore always in the state the encoder assumes. With
//!   canned silence frames it was not, and the first 20 to 60 ms of a word could come out
//!   9 to 15 dB too quiet.
//! - The start of a word is not lost: what is sent runs 60 ms behind the microphone, so
//!   when the gate opens the frames from just before it are still to come. They go out as
//!   ordinary packets, one per 20 ms. (They used to be sent at once together with the
//!   frame that opened the gate. To a receiver that is three packets arriving 60, 40 and
//!   20 ms late at the start of every utterance: its playout buffer drops the ones it has
//!   already played past, or grows and shrinks around them by stretching the audio.)
//! - The last frame of an utterance fades out over 5 ms instead of being cut.
//! - The gate closes 6 dB below where it opens and holds for 300 ms, so speech hovering
//!   around the threshold is not chopped.
//! - Nothing is made up: when the microphone delivers late, the turn sends nothing and
//!   waits for the real samples (the old loop padded with a ramp or a frame of zeros).

use std::collections::VecDeque;
use std::io::Write;

use crate::audio::dsp::VoiceProcessor;
use crate::audio::mic_gain::GainReport;
use crate::audio::opus_encoder::OpusEncoder;

pub const FRAME: usize = 960;
/// What libopus makes of a mono frame of silence in its music coding, and what Discord
/// documents as the silence frame. Only sent here if the encoder fails.
pub const SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// How far what is sent runs behind the microphone, in frames (60 ms): the look-back of
/// the gate.
const DELAY_FRAMES: usize = 3;
/// Frames sent after the last one above the gate (300 ms).
const HANGOVER_FRAMES: u32 = 15;
/// Silence frames after an utterance, and at the start of a call (opens the route).
const TRAILING_SILENCE: u32 = 5;
const INITIAL_SILENCE: u32 = 10;
/// One silence frame every 2.5 s while nothing is sent keeps the route open.
const KEEPALIVE_TURNS: u32 = 125;
/// Fade at the end of the last frame of an utterance (5 ms).
const FADE_OUT_SAMPLES: usize = 240;
/// While open, the gate stays open down to this fraction of its threshold (-6 dB).
const CLOSE_RATIO: f32 = 0.5;
/// Turns without microphone data (60 ms each) in the middle of speech before the
/// utterance is ended: the capture has stopped, not hiccuped.
const MAX_STALLS_IN_SPEECH: u32 = 5;
/// The microphone queue is cut back when it holds more than 200 ms, to 40 ms.
const BACKLOG_LIMIT: usize = FRAME * 10;
const BACKLOG_KEEP: usize = FRAME * 2;
const TRIM_CROSSFADE: usize = 240;

/// The settings a turn runs with (read fresh every turn by the caller).
pub struct TxSettings<'a> {
    pub noise_suppression: &'a str,
    pub echo_cancellation: bool,
    pub auto_gain: bool,
    /// Gate threshold on the level scale of the settings page (rms * 6, 0..1). 0 = no gate.
    pub gate_threshold: f32,
}

pub enum TxInput<'a> {
    /// 20 ms from the microphone.
    Frame(&'a [f32; FRAME]),
    /// The microphone delivered nothing in time.
    Stall,
    Muted,
}

pub struct TxPacket {
    pub opus: Vec<u8>,
    /// False for silence frames.
    pub audio: bool,
}

pub struct TxTurn {
    /// At most one: this turn's packet.
    pub packets: Vec<TxPacket>,
    /// Whether this turn took a packet slot (the RTP timestamp moves on by one frame).
    /// False while waiting out a microphone hiccup in the middle of speech.
    pub advance: bool,
    /// The level of the frame that just came from the microphone, after processing
    /// (rms * 6, 0..1), 0 without a frame.
    pub level: f32,
    pub speaking: bool,
    /// The processed frame this turn's packet slot belongs to: the one that came from the
    /// microphone `DELAY_FRAMES` turns ago. None until that many have come in.
    pub processed: Option<Box<[f32; FRAME]>>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    turns: u32,
    audio_packets: u32,
    silence_packets: u32,
    not_sent: u32,
    utterances: u32,
    lookback_frames: u32,
    stalls: u32,
    stalls_in_speech: u32,
    forced_ends: u32,
    encoder_errors: u32,
    trims: u32,
    trimmed_samples: u32,
    slow_turns: u32,
    longest_turn_gap_ms: u32,
}

/// A processed frame waiting for its turn to be sent.
struct Held {
    frame: Box<[f32; FRAME]>,
    /// Above the gate, or within its hangover.
    wanted: bool,
}

pub struct VoiceTx {
    processor: VoiceProcessor,
    encoder: OpusEncoder,
    scratch: Vec<u8>,
    /// The gate at the microphone: frames it stays open for after the last one above it.
    hang: u32,
    /// The latest `DELAY_FRAMES` frames, oldest first.
    held: VecDeque<Held>,
    /// What is being sent is inside an utterance.
    sending: bool,
    tail: u32,
    keepalive: u32,
    stalls: u32,
    counters: Counters,
    dump: Option<Dump>,
}

impl VoiceTx {
    pub fn new() -> Self {
        Self {
            processor: VoiceProcessor::new(),
            encoder: OpusEncoder::voice().expect("Opus encoder for 48 kHz speech"),
            scratch: vec![0u8; 1500],
            hang: 0,
            held: VecDeque::with_capacity(DELAY_FRAMES + 1),
            sending: false,
            tail: INITIAL_SILENCE,
            keepalive: 0,
            stalls: 0,
            counters: Counters::default(),
            dump: Dump::from_env(),
        }
    }

    /// Speech is on its way out: the gate is open, or frames it let through are still held.
    pub fn in_utterance(&self) -> bool {
        self.sending || self.hang > 0 || self.held.iter().any(|h| h.wanted)
    }

    /// One 20 ms turn of the send loop.
    pub fn turn(&mut self, input: TxInput, settings: &TxSettings) -> TxTurn {
        self.counters.turns += 1;
        let mut out = TxTurn { packets: Vec::new(), advance: true, level: 0.0, speaking: false, processed: None };
        match input {
            TxInput::Muted => {
                self.stalls = 0;
                self.end_utterance();
                self.idle(&mut out);
                self.dump_turn(None, 'M', &out);
            }
            TxInput::Stall => {
                self.counters.stalls += 1;
                if self.in_utterance() {
                    self.counters.stalls_in_speech += 1;
                    self.stalls += 1;
                    if self.stalls < MAX_STALLS_IN_SPEECH {
                        // Wait for the real samples: nothing is sent and no slot is used,
                        // so what follows joins on without a seam.
                        out.advance = false;
                        out.speaking = self.sending;
                        self.dump_turn(None, 'W', &out);
                        return out;
                    }
                    self.counters.forced_ends += 1;
                }
                // What is held is no longer next to what the microphone delivers next.
                self.end_utterance();
                self.idle(&mut out);
                self.dump_turn(None, 'S', &out);
            }
            TxInput::Frame(input) => {
                self.stalls = 0;
                let mut frame = Box::new(*input);
                self.processor.process_frame(&mut frame, settings.noise_suppression, settings.echo_cancellation, settings.auto_gain);
                let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
                out.level = (rms * 6.0).min(1.0);
                // The gate, on the frame that just came in.
                let threshold = if self.hang > 0 { settings.gate_threshold * CLOSE_RATIO } else { settings.gate_threshold };
                let wanted = if out.level >= threshold {
                    self.hang = HANGOVER_FRAMES;
                    true
                } else if self.hang > 0 {
                    self.hang -= 1;
                    true
                } else {
                    false
                };
                self.held.push_back(Held { frame, wanted });
                // What is sent, from the frame of DELAY_FRAMES turns ago: speech, and the
                // frames just before speech.
                let due = if self.held.len() > DELAY_FRAMES { self.held.pop_front() } else { None };
                match due {
                    Some(mut due) if due.wanted || self.held.iter().any(|h| h.wanted) => {
                        if !self.sending {
                            self.sending = true;
                            self.counters.utterances += 1;
                        }
                        if !due.wanted {
                            self.counters.lookback_frames += 1;
                        }
                        let last = !self.held.iter().any(|h| h.wanted);
                        if last {
                            for (i, s) in due.frame[FRAME - FADE_OUT_SAMPLES..].iter_mut().enumerate() {
                                *s *= 1.0 - (i + 1) as f32 / FADE_OUT_SAMPLES as f32;
                            }
                        }
                        let opus = self.encode(&due.frame);
                        out.packets.push(TxPacket { opus, audio: true });
                        out.speaking = true;
                        self.keepalive = 0;
                        if last {
                            self.sending = false;
                            self.tail = TRAILING_SILENCE;
                        }
                        self.dump_turn(Some(&due.frame), 'F', &out);
                        out.processed = Some(due.frame);
                    }
                    due => {
                        self.idle(&mut out);
                        let frame = due.map(|d| d.frame);
                        self.dump_turn(frame.as_deref(), 'F', &out);
                        out.processed = frame;
                    }
                }
            }
        }
        for packet in &out.packets {
            if packet.audio { self.counters.audio_packets += 1 } else { self.counters.silence_packets += 1 }
        }
        if out.packets.is_empty() {
            self.counters.not_sent += 1;
        }
        out
    }

    /// A turn outside an utterance: trailing silence frames, then a keepalive now and then.
    fn idle(&mut self, out: &mut TxTurn) {
        let send = if self.tail > 0 {
            self.tail -= 1;
            true
        } else {
            self.keepalive += 1;
            self.keepalive >= KEEPALIVE_TURNS
        };
        if send {
            self.keepalive = 0;
            let opus = self.encode(&[0.0; FRAME]);
            out.packets.push(TxPacket { opus, audio: false });
        }
    }

    /// Stops what is being sent here and now (muted, or the microphone is gone): the held
    /// frames are dropped, and silence frames follow if speech was on its way out.
    fn end_utterance(&mut self) {
        if self.in_utterance() {
            self.tail = TRAILING_SILENCE;
        }
        self.sending = false;
        self.hang = 0;
        self.held.clear();
    }

    fn encode(&mut self, frame: &[f32; FRAME]) -> Vec<u8> {
        match self.encoder.encode(frame, &mut self.scratch) {
            Ok(len) => self.scratch[..len].to_vec(),
            Err(_) => {
                self.counters.encoder_errors += 1;
                SILENCE_FRAME.to_vec()
            }
        }
    }

    /// Records that `samples` were cut from the microphone queue (see `take_mic_frame`).
    pub fn note_trim(&mut self, samples: usize) {
        if samples > 0 {
            self.counters.trims += 1;
            self.counters.trimmed_samples += samples as u32;
        }
    }

    /// Records how long after the previous turn this one started (see the send loop). A
    /// turn should start every 20 ms; more than 40 ms means the loop itself ran late (a
    /// busy runtime), which a listener hears as late packets even when the microphone
    /// was on time.
    pub fn note_turn_gap(&mut self, ms: u32) {
        if ms > 40 {
            self.counters.slow_turns += 1;
        }
        self.counters.longest_turn_gap_ms = self.counters.longest_turn_gap_ms.max(ms);
    }

    /// One line about the turns since the last report, and a new period.
    pub fn take_report(&mut self) -> String {
        let c = std::mem::take(&mut self.counters);
        let g: GainReport = self.processor.take_gain_report();
        let level = |v: f32| if v.is_finite() { format!("{v:.1}") } else { "-".to_string() };
        format!(
            "turns={} audio={} silence={} not_sent={} utterances={} lookback_frames={} | gain: speech_in={} dBFS speech_out={} dBFS gain={:+.1} dB noise_floor={:.1} dBFS speech_frames={} limiter_frames={} limiter_max={:.1} dB | mic: late_turns={} late_in_speech={} forced_ends={} queue_trims={} trimmed_ms={} encoder_errors={} | loop: slow_turns={} longest_gap={} ms",
            c.turns, c.audio_packets, c.silence_packets, c.not_sent, c.utterances, c.lookback_frames,
            level(g.speech_in_db), level(g.speech_out_db), g.gain_db, g.noise_db, g.speech_frames, g.limited_frames, g.limiter_max_db,
            c.stalls, c.stalls_in_speech, c.forced_ends, c.trims, c.trimmed_samples / 48, c.encoder_errors,
            c.slow_turns, c.longest_turn_gap_ms,
        )
    }

    fn dump_turn(&mut self, frame: Option<&[f32; FRAME]>, kind: char, out: &TxTurn) {
        if let Some(dump) = self.dump.as_mut() {
            dump.turn(frame, kind, out, self.hang);
        }
    }
}

/// The next 20 ms from the microphone queue, or None when it does not hold a whole frame
/// yet (nothing is taken then). A queue that has run more than 200 ms behind is cut back
/// to 40 ms first, with a 5 ms cross-fade over the cut; the second value is how many
/// samples that removed.
pub fn take_mic_frame(queue: &mut VecDeque<f32>) -> (Option<[f32; FRAME]>, usize) {
    let mut dropped = 0;
    if queue.len() > BACKLOG_LIMIT {
        dropped = queue.len() - BACKLOG_KEEP;
        let old: Vec<f32> = queue.iter().take(TRIM_CROSSFADE).copied().collect();
        queue.drain(0..dropped);
        for (i, (new, old)) in queue.iter_mut().zip(old).enumerate() {
            let w = (i + 1) as f32 / (TRIM_CROSSFADE + 1) as f32;
            *new = old * (1.0 - w) + *new * w;
        }
    }
    if queue.len() < FRAME {
        return (None, dropped);
    }
    let mut frame = [0.0f32; FRAME];
    for s in frame.iter_mut() {
        *s = queue.pop_front().unwrap_or(0.0);
    }
    (Some(frame), dropped)
}

/// LITECORD_VOICE_TX_DUMP=<directory>: what goes to the encoder is written to files there
/// (this is a recording of the microphone: off unless the variable is set, and announced
/// in the log). `tx-<time>.f32` holds every processed frame, mono 48 kHz f32, zeros for
/// turns without a frame; `tx-<time>.csv` one line per turn.
struct Dump {
    pcm: std::io::BufWriter<std::fs::File>,
    csv: std::io::BufWriter<std::fs::File>,
    turn: u64,
}

impl Dump {
    fn from_env() -> Option<Self> {
        let dir = std::env::var("LITECORD_VOICE_TX_DUMP").ok().filter(|d| !d.is_empty())?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let open = |ext: &str| std::fs::File::create(format!("{dir}/tx-{stamp}.{ext}")).map(std::io::BufWriter::new);
        match (std::fs::create_dir_all(&dir), open("f32"), open("csv")) {
            (Ok(()), Ok(pcm), Ok(mut csv)) => {
                log::warn!("Voice TX: LITECORD_VOICE_TX_DUMP is set: this call's processed microphone audio is being written to {dir}/tx-{stamp}.f32");
                let _ = writeln!(csv, "turn,kind,level,hang,packets,audio,advance");
                Some(Self { pcm, csv, turn: 0 })
            }
            _ => {
                log::warn!("Voice TX: LITECORD_VOICE_TX_DUMP is set but {dir} cannot be written; no dump.");
                None
            }
        }
    }

    fn turn(&mut self, frame: Option<&[f32; FRAME]>, kind: char, out: &TxTurn, hang: u32) {
        let zeros = [0.0f32; FRAME];
        for s in frame.unwrap_or(&zeros) {
            let _ = self.pcm.write_all(&s.to_le_bytes());
        }
        let audio = out.packets.iter().filter(|p| p.audio).count();
        let _ = writeln!(self.csv, "{},{},{:.4},{},{},{},{}", self.turn, kind, out.level, hang, out.packets.len(), audio, out.advance as u8);
        self.turn += 1;
        if self.turn % 50 == 0 {
            let _ = self.pcm.flush();
            let _ = self.csv.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF: TxSettings = TxSettings { noise_suppression: "none", echo_cancellation: false, auto_gain: false, gate_threshold: 0.05 };

    fn tone(level: f32) -> [f32; FRAME] {
        let mut f = [0.0f32; FRAME];
        for (i, s) in f.iter_mut().enumerate() {
            *s = level * (i as f32 * 0.05).sin();
        }
        f
    }

    /// Runs frames through a sender and returns, per turn, what was sent: 'A' audio,
    /// 'S' a silence frame, '-' nothing.
    fn kinds(tx: &mut VoiceTx, frames: &[[f32; FRAME]]) -> String {
        frames.iter().map(|f| {
            let turn = tx.turn(TxInput::Frame(f), &OFF);
            assert!(turn.packets.len() <= 1, "one packet per turn at most");
            match turn.packets.first() { Some(p) if p.audio => 'A', Some(_) => 'S', None => '-' }
        }).collect()
    }

    fn after_start(tx: &mut VoiceTx) {
        // The ten silence frames of a new call.
        for _ in 0..INITIAL_SILENCE {
            assert!(!tx.turn(TxInput::Frame(&[0.0; FRAME]), &OFF).packets[0].audio);
        }
    }

    #[test]
    fn the_frames_before_the_gate_opened_are_sent_first_one_per_turn() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let quiet = tone(0.001);
        let loud = tone(0.1);
        // The gate opens on the sixth frame. What goes out then is the frame of three
        // turns earlier, so three quiet frames are sent ahead of the loud ones.
        let sent = kinds(&mut tx, &[quiet, quiet, quiet, quiet, quiet, loud, loud, loud, loud, loud]);
        assert_eq!(sent, "-----AAAAA");
        // The audio of the first packet is the quiet frame, of the fourth the loud one.
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let mut levels = Vec::new();
        for f in [quiet, quiet, quiet, quiet, quiet, loud, loud, loud, loud] {
            let turn = tx.turn(TxInput::Frame(&f), &OFF);
            if turn.speaking {
                let p = turn.processed.unwrap();
                levels.push((p.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt());
            }
        }
        assert_eq!(levels.len(), 4);
        assert!(levels[..3].iter().all(|l| *l < 0.002) && levels[3] > 0.05, "{levels:?}");
    }

    #[test]
    fn an_utterance_ends_with_its_hangover_and_five_silence_frames() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let quiet = tone(0.001);
        let mut frames = vec![quiet; 4];
        frames.push(tone(0.1));
        frames.extend(std::iter::repeat(quiet).take(40));
        let sent = kinds(&mut tx, &frames);
        // Three frames of look-back, the loud frame, fifteen of hangover, five silence frames.
        let expected = format!("----{}{}{}", "A".repeat(3 + 1 + HANGOVER_FRAMES as usize), "S".repeat(TRAILING_SILENCE as usize), "-".repeat(17));
        assert_eq!(sent, expected);
    }

    #[test]
    fn speech_that_resumes_right_after_an_utterance_joins_on_without_a_gap() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let quiet = tone(0.001);
        let loud = tone(0.1);
        // The gate closes after the hangover and opens again two frames later: those two
        // frames are look-back of the second utterance, so audio never stops.
        let mut frames = vec![loud];
        frames.extend(std::iter::repeat(quiet).take(HANGOVER_FRAMES as usize + 2));
        frames.extend([loud; 4]);
        // (The first packets are the three frames held from before the first loud one.)
        let sent = kinds(&mut tx, &frames);
        assert_eq!(sent, "A".repeat(frames.len()));
    }

    #[test]
    fn every_packet_decodes_in_order_and_a_word_starts_at_its_level() {
        use crate::audio::opus_decoder::{OpusDecoder, MAX_FRAME};
        let mut tx = VoiceTx::new();
        let mut dec = OpusDecoder::new(48_000, 1).unwrap();
        let mut pcm = vec![0f32; MAX_FRAME];
        let voiced = |n: usize| -> [f32; FRAME] {
            let mut f = [0.0f32; FRAME];
            for (i, s) in f.iter_mut().enumerate() {
                let t = (n * FRAME + i) as f32 / 48_000.0;
                *s = 0.2 * (1..=6).map(|h| (t * 140.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32).sum::<f32>();
            }
            f
        };
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        // Three words with pauses long enough for the silence frames and a keepalive gap.
        let mut starts = Vec::new();
        let mut n = 0;
        for _word in 0..3 {
            for k in 0..60 {
                let frame = if k < 25 { voiced(n) } else { [0.0; FRAME] };
                n += 1;
                let turn = tx.turn(TxInput::Frame(&frame), &OFF);
                for p in &turn.packets {
                    let got = dec.decode(&p.opus, MAX_FRAME, &mut pcm).unwrap();
                    assert_eq!(got, FRAME);
                    if p.audio && k == 3 + DELAY_FRAMES {
                        // The fourth frame of the word, as a listener gets it.
                        starts.push(rms(&pcm[..got]) / rms(&frame));
                    }
                    if !p.audio { assert!(rms(&pcm[..got]) < 0.02, "a silence frame decodes to silence"); }
                }
            }
        }
        assert_eq!(starts.len(), 3);
        assert!(starts.iter().all(|r| (0.7..1.4).contains(r)), "word starts against the original: {starts:?}");
    }

    #[test]
    fn the_gate_closes_lower_than_it_opens() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        // Threshold 0.05 on rms * 6: a sine of amplitude a has rms a / 1.414.
        let below_open = tone(0.05 / 6.0 * 1.414 * 0.7);
        assert_eq!(kinds(&mut tx, &[below_open; 6]), "------", "0.7 of the threshold does not open the gate");
        let loud = tone(0.1);
        tx.turn(TxInput::Frame(&loud), &OFF);
        for _ in 0..HANGOVER_FRAMES * 3 {
            let turn = tx.turn(TxInput::Frame(&below_open), &OFF);
            assert!(turn.packets.len() == 1 && turn.packets[0].audio, "0.7 of the threshold keeps an open gate open");
        }
        assert!(tx.in_utterance());
    }

    #[test]
    fn a_late_microphone_in_speech_sends_nothing_and_uses_no_slot() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let loud = tone(0.1);
        for _ in 0..5 {
            tx.turn(TxInput::Frame(&loud), &OFF);
        }
        for _ in 0..MAX_STALLS_IN_SPEECH - 1 {
            let turn = tx.turn(TxInput::Stall, &OFF);
            assert!(turn.packets.is_empty() && !turn.advance);
        }
        // Speech goes on as if nothing happened.
        let turn = tx.turn(TxInput::Frame(&loud), &OFF);
        assert!(turn.advance && turn.packets.len() == 1 && turn.packets[0].audio);
        // A microphone that stays away ends the utterance with silence frames.
        for _ in 0..MAX_STALLS_IN_SPEECH - 1 {
            tx.turn(TxInput::Stall, &OFF);
        }
        let turn = tx.turn(TxInput::Stall, &OFF);
        assert!(turn.advance && turn.packets.len() == 1 && !turn.packets[0].audio && !tx.in_utterance());
    }

    #[test]
    fn muting_ends_the_utterance_at_once() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        for _ in 0..5 {
            tx.turn(TxInput::Frame(&tone(0.1)), &OFF);
        }
        let turn = tx.turn(TxInput::Muted, &OFF);
        assert!(!tx.in_utterance() && turn.packets.len() == 1 && !turn.packets[0].audio);
    }

    #[test]
    fn a_threshold_of_zero_sends_everything() {
        let mut tx = VoiceTx::new();
        let open = TxSettings { gate_threshold: 0.0, ..OFF };
        for i in 0..50 {
            let turn = tx.turn(TxInput::Frame(&[0.0; FRAME]), &open);
            // The first three turns fill the 60 ms that sending runs behind.
            assert!(turn.packets.len() == 1 && turn.packets[0].audio == (i >= DELAY_FRAMES), "turn {i}");
        }
    }

    #[test]
    fn the_queue_gives_whole_frames_only_and_trims_a_backlog_smoothly() {
        let mut queue: VecDeque<f32> = (0..FRAME - 1).map(|_| 0.5).collect();
        assert!(take_mic_frame(&mut queue).0.is_none());
        assert_eq!(queue.len(), FRAME - 1, "a short queue is left alone");
        queue.push_back(0.5);
        assert!(take_mic_frame(&mut queue).0.is_some());

        // 300 ms behind: old audio at +0.5, the newest 40 ms at -0.5.
        let mut queue: VecDeque<f32> = std::iter::repeat(0.5).take(FRAME * 13).chain(std::iter::repeat(-0.5).take(FRAME * 2)).collect();
        let (frame, dropped) = take_mic_frame(&mut queue);
        let frame = frame.unwrap();
        assert_eq!(dropped, FRAME * 13);
        // The frame starts near what would have come next and glides to the new audio.
        assert!(frame[0] > 0.45 && frame[TRIM_CROSSFADE] == -0.5);
        let worst = frame.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.01, "largest step across the cut {worst}");
    }
}
