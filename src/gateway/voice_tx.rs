//! The sending side of a voice call: 20 ms microphone frames in, Opus packets out.
//!
//! Everything that decides what a listener hears where speech starts and stops is here, in
//! one place that can be run without a call (src/bin/mic_tx_study.rs drives it from files):
//! voice processing and automatic gain, the voice gate, and the Opus encoder.
//!
//! What it does at the edges of speech, and why (measured against libopus as the receiver,
//! on recordings at the level of a quiet microphone):
//! - The start of a word is not lost. The frames from just before the gate opened are sent
//!   too (60 ms of look-back), in the packet slots that were left empty.
//! - The encoder agrees with the receiver when speech resumes. A receiver decodes the
//!   trailing silence frames and its decoder state decays; an encoder that was simply not
//!   fed kept the state of the last loud frame, and the first 20 to 60 ms of the next word
//!   came out 9 to 15 dB too quiet at about half of all starts. The encoder is now given
//!   two frames of silence when an utterance ends.
//! - The last frame of an utterance fades out over 5 ms instead of being cut.
//! - The gate closes 6 dB below where it opens and holds for 300 ms, so speech hovering
//!   around the threshold is not chopped.
//! - Nothing is made up: when the microphone delivers late, the turn sends nothing and
//!   waits for the real samples (the old loop padded with a ramp or a frame of zeros).

use std::collections::VecDeque;
use std::io::Write;

use opus_rs::{Application, OpusEncoder};

use crate::audio::dsp::VoiceProcessor;
use crate::audio::mic_gain::GainReport;

pub const FRAME: usize = 960;
/// The Opus silence frame Discord asks for when speech stops.
pub const SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Frames from before the gate opened that are still sent (60 ms).
const PREROLL_FRAMES: usize = 3;
/// Frames sent after the last one above the gate (300 ms).
const HANGOVER_FRAMES: u32 = 15;
/// Silence frames after an utterance, and at the start of a call (opens the route).
const TRAILING_SILENCE: u32 = 5;
const INITIAL_SILENCE: u32 = 10;
/// One silence frame every 2.5 s while nothing is sent keeps the route open.
const KEEPALIVE_TURNS: u32 = 125;
/// Frames of zeros the encoder is given (output thrown away) when an utterance ends.
const ZERO_FEED_FRAMES: usize = 2;
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
    /// 0: this turn's packet slot; n: the slot n turns ago (look-back frames).
    pub slots_back: u32,
    pub opus: Vec<u8>,
    /// False for silence frames.
    pub audio: bool,
}

pub struct TxTurn {
    pub packets: Vec<TxPacket>,
    /// Whether this turn took a packet slot (the RTP timestamp moves on by one frame).
    /// False while waiting out a microphone hiccup in the middle of speech.
    pub advance: bool,
    /// The level of the processed frame (rms * 6, 0..1), 0 without a frame.
    pub level: f32,
    pub speaking: bool,
    /// The frame after processing, as the encoder gets it (None without a frame).
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
}

pub struct VoiceTx {
    processor: VoiceProcessor,
    encoder: OpusEncoder,
    scratch: Vec<u8>,
    /// Frames to keep sending after the last one above the gate; above 0 = in an utterance.
    hang: u32,
    tail: u32,
    keepalive: u32,
    stalls: u32,
    /// The latest processed frames whose packet slots were left empty, oldest first.
    unsent: VecDeque<Box<[f32; FRAME]>>,
    counters: Counters,
    dump: Option<Dump>,
}

impl VoiceTx {
    pub fn new() -> Self {
        // As voice.rs always created it: 48 kHz, two channels (the mono microphone on
        // both), voice application.
        let encoder = OpusEncoder::new(48000, 2, Application::Voip).expect("Opus encoder for 48 kHz stereo");
        let mut tx = Self {
            processor: VoiceProcessor::new(),
            encoder,
            scratch: vec![0u8; 1500],
            hang: 0,
            tail: INITIAL_SILENCE,
            keepalive: 0,
            stalls: 0,
            unsent: VecDeque::with_capacity(PREROLL_FRAMES + 1),
            counters: Counters::default(),
            dump: Dump::from_env(),
        };
        tx.feed_silence();
        tx
    }

    pub fn in_utterance(&self) -> bool {
        self.hang > 0
    }

    /// One 20 ms turn of the send loop.
    pub fn turn(&mut self, input: TxInput, settings: &TxSettings) -> TxTurn {
        self.counters.turns += 1;
        let mut out = TxTurn { packets: Vec::new(), advance: true, level: 0.0, speaking: false, processed: None };
        match input {
            TxInput::Muted => {
                self.stalls = 0;
                self.unsent.clear();
                if self.in_utterance() {
                    self.end_utterance();
                }
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
                        out.speaking = true;
                        self.dump_turn(None, 'W', &out);
                        return out;
                    }
                    self.counters.forced_ends += 1;
                    self.end_utterance();
                }
                self.unsent.clear();
                self.idle(&mut out);
                self.dump_turn(None, 'S', &out);
            }
            TxInput::Frame(input) => {
                self.stalls = 0;
                let mut frame = Box::new(*input);
                self.processor.process_frame(&mut frame, settings.noise_suppression, settings.echo_cancellation, settings.auto_gain);
                let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
                out.level = (rms * 6.0).min(1.0);
                let threshold = if self.in_utterance() { settings.gate_threshold * CLOSE_RATIO } else { settings.gate_threshold };
                if out.level >= threshold {
                    if !self.in_utterance() {
                        self.counters.utterances += 1;
                        // Look-back: the frames from just before the gate opened.
                        let held = self.unsent.len() as u32;
                        for (i, earlier) in std::mem::take(&mut self.unsent).into_iter().enumerate() {
                            let opus = self.encode(&earlier);
                            out.packets.push(TxPacket { slots_back: held - i as u32, opus, audio: true });
                            self.counters.lookback_frames += 1;
                        }
                    }
                    self.hang = HANGOVER_FRAMES;
                    self.tail = TRAILING_SILENCE;
                    self.keepalive = 0;
                    let opus = self.encode(&frame);
                    out.packets.push(TxPacket { slots_back: 0, opus, audio: true });
                    out.speaking = true;
                } else if self.in_utterance() {
                    self.hang -= 1;
                    if self.hang == 0 {
                        // The last frame of the utterance.
                        for (i, s) in frame[FRAME - FADE_OUT_SAMPLES..].iter_mut().enumerate() {
                            *s *= 1.0 - (i + 1) as f32 / FADE_OUT_SAMPLES as f32;
                        }
                    }
                    let opus = self.encode(&frame);
                    out.packets.push(TxPacket { slots_back: 0, opus, audio: true });
                    out.speaking = true;
                    if self.hang == 0 {
                        self.end_utterance();
                    }
                } else {
                    self.idle(&mut out);
                    if out.packets.is_empty() {
                        if self.unsent.len() == PREROLL_FRAMES {
                            self.unsent.pop_front();
                        }
                        self.unsent.push_back(frame.clone());
                    }
                }
                self.dump_turn(Some(&frame), 'F', &out);
                out.processed = Some(frame);
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
            // The slot is taken: frames from before it cannot be sent later.
            self.unsent.clear();
            out.packets.push(TxPacket { slots_back: 0, opus: SILENCE_FRAME.to_vec(), audio: false });
        }
    }

    fn end_utterance(&mut self) {
        self.hang = 0;
        self.tail = TRAILING_SILENCE;
        self.unsent.clear();
        self.feed_silence();
    }

    /// The encoder hears the silence the receiver is about to decode.
    fn feed_silence(&mut self) {
        let zeros = [0.0f32; FRAME * 2];
        for _ in 0..ZERO_FEED_FRAMES {
            let _ = self.encoder.encode(&zeros, FRAME, &mut self.scratch);
        }
    }

    fn encode(&mut self, frame: &[f32; FRAME]) -> Vec<u8> {
        let mut stereo = [0.0f32; FRAME * 2];
        for (i, &s) in frame.iter().enumerate() {
            stereo[i * 2] = s;
            stereo[i * 2 + 1] = s;
        }
        match self.encoder.encode(&stereo, FRAME, &mut self.scratch) {
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

    /// One line about the turns since the last report, and a new period.
    pub fn take_report(&mut self) -> String {
        let c = std::mem::take(&mut self.counters);
        let g: GainReport = self.processor.take_gain_report();
        let level = |v: f32| if v.is_finite() { format!("{v:.1}") } else { "-".to_string() };
        format!(
            "turns={} audio={} silence={} not_sent={} utterances={} lookback_frames={} | gain: speech_in={} dBFS speech_out={} dBFS gain={:+.1} dB noise_floor={:.1} dBFS speech_frames={} limiter_frames={} limiter_max={:.1} dB | mic: late_turns={} late_in_speech={} forced_ends={} queue_trims={} trimmed_ms={} encoder_errors={}",
            c.turns, c.audio_packets, c.silence_packets, c.not_sent, c.utterances, c.lookback_frames,
            level(g.speech_in_db), level(g.speech_out_db), g.gain_db, g.noise_db, g.speech_frames, g.limited_frames, g.limiter_max_db,
            c.stalls, c.stalls_in_speech, c.forced_ends, c.trims, c.trimmed_samples / 48, c.encoder_errors,
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

    /// Runs frames through a sender and returns, per turn, the kinds of packets sent
    /// ('A' audio, 'S' silence) with their slot offsets.
    fn kinds(tx: &mut VoiceTx, frames: &[[f32; FRAME]]) -> Vec<Vec<(char, u32)>> {
        frames.iter().map(|f| tx.turn(TxInput::Frame(f), &OFF).packets.iter().map(|p| (if p.audio { 'A' } else { 'S' }, p.slots_back)).collect()).collect()
    }

    fn after_start(tx: &mut VoiceTx) {
        // The ten silence frames of a new call.
        for _ in 0..INITIAL_SILENCE {
            assert!(!tx.turn(TxInput::Frame(&[0.0; FRAME]), &OFF).packets[0].audio);
        }
    }

    #[test]
    fn the_frames_before_the_gate_opened_are_sent_in_their_own_slots() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let quiet = tone(0.001);
        let loud = tone(0.1);
        let sent = kinds(&mut tx, &[quiet, quiet, quiet, quiet, quiet, loud, loud]);
        assert!(sent[..5].iter().all(|turn| turn.is_empty()));
        assert_eq!(sent[5], vec![('A', 3), ('A', 2), ('A', 1), ('A', 0)]);
        assert_eq!(sent[6], vec![('A', 0)]);
    }

    #[test]
    fn a_slot_that_carried_a_silence_frame_is_not_sent_twice() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        let quiet = tone(0.001);
        let loud = tone(0.1);
        // One utterance, then its hangover and trailing silence frames, then two empty
        // turns and speech again: only the two empty slots can carry look-back frames.
        let mut frames = vec![loud];
        frames.extend(std::iter::repeat(quiet).take((HANGOVER_FRAMES + TRAILING_SILENCE) as usize + 2));
        frames.push(loud);
        let sent = kinds(&mut tx, &frames);
        let hang_end = HANGOVER_FRAMES as usize;
        assert!(sent[1..=hang_end].iter().all(|t| t == &vec![('A', 0)]), "hangover frames are audio");
        assert!(sent[hang_end + 1..hang_end + 1 + TRAILING_SILENCE as usize].iter().all(|t| t == &vec![('S', 0)]));
        assert_eq!(sent.last().unwrap(), &vec![('A', 2), ('A', 1), ('A', 0)]);
    }

    #[test]
    fn the_gate_closes_lower_than_it_opens() {
        let mut tx = VoiceTx::new();
        after_start(&mut tx);
        // Threshold 0.05 on rms * 6: a sine of amplitude a has rms a / 1.414.
        let below_open = tone(0.05 / 6.0 * 1.414 * 0.7);
        assert!(kinds(&mut tx, &[below_open, below_open]).iter().all(|t| t.is_empty()), "0.7 of the threshold does not open the gate");
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
        tx.turn(TxInput::Frame(&loud), &OFF);
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
        tx.turn(TxInput::Frame(&tone(0.1)), &OFF);
        let turn = tx.turn(TxInput::Muted, &OFF);
        assert!(!tx.in_utterance() && turn.packets.len() == 1 && !turn.packets[0].audio);
    }

    #[test]
    fn a_threshold_of_zero_sends_everything() {
        let mut tx = VoiceTx::new();
        let open = TxSettings { gate_threshold: 0.0, ..OFF };
        for _ in 0..50 {
            let turn = tx.turn(TxInput::Frame(&[0.0; FRAME]), &open);
            assert!(turn.packets.len() == 1 && turn.packets[0].audio);
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
