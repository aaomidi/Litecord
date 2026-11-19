//! Opus decoding by libopus itself (through `audiopus_sys`: the system's library where
//! pkg-config finds one, as on Linux; otherwise the copy that crate builds).
//!
//! The decoder of `opus-rs` (still used for encoding) is not exact. Measured against
//! libopus on speech: 10 to 16 % of the frames of mono hybrid packets with FEC (what the
//! official client sends) came out wrong, with errors around the 8 kHz crossover up to
//! full scale, and 7 to 14 % of CELT-only frames (what this client sends). That was heard
//! as short electric-sounding bursts in other people's voices.
//!
//! `unsafe-libopus` 0.2.0 (libopus translated to Rust) was tried first: exact on packets
//! that arrive, but full-scale noise whenever it conceals a hybrid frame, which also
//! happens inside the decoder at every change from hybrid to CELT, that is at the silence
//! frame after each utterance of an official client.

use std::ptr::NonNull;

/// The longest Opus frame at 48 kHz (120 ms), in samples per channel.
pub const MAX_FRAME: usize = 5760;

pub struct OpusDecoder {
    st: NonNull<audiopus_sys::OpusDecoder>,
    channels: usize,
}

// The state is one heap block that only this value points to.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    /// `channels` is what comes out (1 or 2), whatever the packets were coded as.
    pub fn new(rate: i32, channels: usize) -> Result<Self, &'static str> {
        if channels != 1 && channels != 2 {
            return Err("an Opus decoder has one or two channels");
        }
        let mut err = 0i32;
        let st = unsafe { audiopus_sys::opus_decoder_create(rate, channels as i32, &mut err) };
        match NonNull::new(st) {
            Some(st) if err == 0 => Ok(Self { st, channels }),
            Some(st) => {
                unsafe { audiopus_sys::opus_decoder_destroy(st.as_ptr()) };
                Err("the Opus decoder could not be set up")
            }
            None => Err("the Opus decoder could not be created"),
        }
    }

    /// Decodes one packet into `pcm` (interleaved). Returns the samples per channel.
    /// `max_samples` is the same limit per channel that `opus-rs` took; `pcm` bounds it too.
    pub fn decode(&mut self, packet: &[u8], max_samples: usize, pcm: &mut [f32]) -> Result<usize, &'static str> {
        if packet.is_empty() {
            return Err("empty Opus packet");
        }
        self.run(packet.as_ptr(), packet.len(), max_samples, pcm, 0)
    }

    /// `samples` per channel that continue what was decoded last, for a lost packet.
    pub fn conceal(&mut self, samples: usize, pcm: &mut [f32]) -> Result<usize, &'static str> {
        // libopus conceals whole multiples of 2.5 ms.
        self.run(std::ptr::null(), 0, samples - samples % 120, pcm, 0)
    }

    /// The lost frame before `next_packet`, from the copy of it that packet carries (in-band
    /// FEC). Where the packet carries none, libopus conceals instead. `samples` is the
    /// length of the lost frame per channel. `next_packet` must then be decoded as usual.
    pub fn decode_fec(&mut self, next_packet: &[u8], samples: usize, pcm: &mut [f32]) -> Result<usize, &'static str> {
        if next_packet.is_empty() {
            return Err("empty Opus packet");
        }
        self.run(next_packet.as_ptr(), next_packet.len(), samples - samples % 120, pcm, 1)
    }

    fn run(&mut self, data: *const u8, len: usize, max_samples: usize, pcm: &mut [f32], fec: i32) -> Result<usize, &'static str> {
        let frame = max_samples.min(pcm.len() / self.channels).min(MAX_FRAME);
        if frame == 0 || len > i32::MAX as usize {
            return Err("no room for decoded Opus audio");
        }
        // libopus writes at most `frame` samples per channel and reads `len` bytes.
        let n = unsafe {
            audiopus_sys::opus_decode_float(self.st.as_ptr(), data, len as i32, pcm.as_mut_ptr(), frame as i32, fec)
        };
        match n {
            n if n > 0 => Ok(n as usize),
            0 => Err("the Opus decoder returned no samples"),
            -1 => Err("Opus: bad argument"),
            -2 => Err("Opus: the frame does not fit the output buffer"),
            -4 => Err("Opus: corrupted packet"),
            _ => Err("Opus: decoding failed"),
        }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_decoder_destroy(self.st.as_ptr()) };
    }
}

/// Packets from libopus' own encoder, for tests. (The encoder of `opus-rs` is only right
/// for two channels: its mono packets play about 13 dB too quiet in libopus.)
#[cfg(test)]
pub mod test_packets {
    /// A voiced sound (150 Hz and its harmonics) at 0.2 RMS, `packets` frames of 20 ms,
    /// coded for speech as the official client does. With `fec` every packet also carries
    /// a copy of the frame before it.
    pub fn speechlike(channels: usize, fec: bool, packets: usize) -> Vec<Vec<u8>> {
        let mut err = 0i32;
        let enc = unsafe { audiopus_sys::opus_encoder_create(48000, channels as i32, audiopus_sys::OPUS_APPLICATION_VOIP, &mut err) };
        assert!(!enc.is_null() && err == 0);
        unsafe {
            audiopus_sys::opus_encoder_ctl(enc, audiopus_sys::OPUS_SET_BITRATE_REQUEST, 64000i32);
            audiopus_sys::opus_encoder_ctl(enc, audiopus_sys::OPUS_SET_SIGNAL_REQUEST, audiopus_sys::OPUS_SIGNAL_VOICE);
            if fec {
                audiopus_sys::opus_encoder_ctl(enc, audiopus_sys::OPUS_SET_INBAND_FEC_REQUEST, 1i32);
                audiopus_sys::opus_encoder_ctl(enc, audiopus_sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 15i32);
            }
        }
        let out = (0..packets).map(|p| {
            let frame: Vec<f32> = (0..960).flat_map(|i| {
                let t = (p * 960 + i) as f32 / 48000.0;
                let v: f32 = (1..=8).map(|h| (t * 150.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32).sum();
                // Slightly uneven in level, as a voice is: a perfectly steady sound gets few copies.
                let swell = 1.0 + 0.15 * (t * 5.0 * std::f32::consts::TAU).sin();
                std::iter::repeat(v * swell * 0.2 / 0.874).take(channels)
            }).collect();
            let mut buf = [0u8; 1500];
            let n = unsafe { audiopus_sys::opus_encode_float(enc, frame.as_ptr(), 960, buf.as_mut_ptr(), buf.len() as i32) };
            assert!(n > 0, "libopus encoder: {n}");
            buf[..n as usize].to_vec()
        }).collect();
        unsafe { audiopus_sys::opus_encoder_destroy(enc) };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn speech_comes_back_at_its_level_from_mono_and_stereo_packets() {
        for coded in [1, 2] {
            let mut dec = OpusDecoder::new(48000, 2).unwrap();
            let mut pcm = vec![0f32; MAX_FRAME * 2];
            let mut last = Vec::new();
            for p in test_packets::speechlike(coded, false, 25) {
                let n = dec.decode(&p, MAX_FRAME, &mut pcm).unwrap();
                assert_eq!(n, 960);
                last = pcm[..n * 2].to_vec();
            }
            let level = rms(&last);
            assert!((0.17..0.23).contains(&level), "coded as {coded} channel(s): level {level}");
            assert!(last.chunks(2).all(|c| (c[0] - c[1]).abs() < 0.02), "both channels carry the sound");
        }
    }

    #[test]
    fn a_lost_packet_is_continued_not_left_silent() {
        let mut dec = OpusDecoder::new(48000, 2).unwrap();
        let mut pcm = vec![0f32; MAX_FRAME * 2];
        for p in test_packets::speechlike(1, false, 25) {
            dec.decode(&p, MAX_FRAME, &mut pcm).unwrap();
        }
        let n = dec.conceal(960, &mut pcm).unwrap();
        assert_eq!(n, 960);
        assert!(rms(&pcm[..n * 2]) > 0.08, "concealed audio continues the sound");
    }

    #[test]
    fn damaged_packets_are_refused_without_a_panic() {
        let mut dec = OpusDecoder::new(48000, 2).unwrap();
        let mut pcm = vec![0f32; MAX_FRAME * 2];
        assert!(dec.decode(&[], MAX_FRAME, &mut pcm).is_err());
        // Code 3 packet that announces more frames than it holds.
        assert!(dec.decode(&[0xFF, 0xFF, 0x01], MAX_FRAME, &mut pcm).is_err());
        let mut x = 12345u32;
        for len in 1..200usize {
            let junk: Vec<u8> = (0..len).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (x >> 24) as u8 }).collect();
            if let Ok(n) = dec.decode(&junk, MAX_FRAME, &mut pcm) {
                assert!(n <= MAX_FRAME);
            }
        }
        // A buffer too small for the frame is an error, not an overrun.
        let mut small = vec![0f32; 100];
        let packet = test_packets::speechlike(2, false, 1).remove(0);
        assert!(dec.decode(&packet, MAX_FRAME, &mut small).is_err());
    }
}
