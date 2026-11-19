//! Opus encoding by libopus (see `opus_decoder` for how it is linked).
//!
//! The encoder of `opus-rs` codes two-channel audio so that libopus plays it at the right
//! level, but it is a bare one: it almost never switches to short blocks (4 to 12 frames in
//! a minute of speech where libopus does it for 72 to 226) and never uses the pitch
//! filter. Without short blocks the sound of a sharp word start is smeared over the 20 ms
//! before it: decoded by libopus, the 10 ms before an onset came out 5 to 9 dB louder than
//! the original at the median and 17 to 25 dB louder in the worst tenth (libopus' own
//! encoder: 0 and 3 dB). Listeners heard that as a short electric noise where speech starts.

use std::ptr::NonNull;

/// Samples per channel in the 20 ms frames everything here uses.
pub const FRAME: usize = 960;

pub struct OpusEncoder {
    st: NonNull<audiopus_sys::OpusEncoder>,
    channels: usize,
}

// The state is one heap block that only this value points to.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    /// For a voice, as the official client codes it: mono, speech coding with a wideband
    /// top (SILK or hybrid), 64 kbit/s at most, and in every packet a low-rate copy of
    /// the frame before it (in-band FEC), from which a receiver rebuilds a lost packet.
    pub fn voice() -> Result<Self, &'static str> {
        let mut enc = Self::create(1, audiopus_sys::OPUS_APPLICATION_VOIP)?;
        enc.set(audiopus_sys::OPUS_SET_BITRATE_REQUEST, 64_000)?;
        enc.set(audiopus_sys::OPUS_SET_SIGNAL_REQUEST, audiopus_sys::OPUS_SIGNAL_VOICE)?;
        enc.set(audiopus_sys::OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        enc.set(audiopus_sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10)?;
        enc.set(audiopus_sys::OPUS_SET_DTX_REQUEST, 0)?;
        Ok(enc)
    }

    /// For any sound (the audio of a screen share): the music coding at `bitrate`.
    pub fn audio(channels: usize, bitrate: i32) -> Result<Self, &'static str> {
        let mut enc = Self::create(channels, audiopus_sys::OPUS_APPLICATION_AUDIO)?;
        enc.set(audiopus_sys::OPUS_SET_BITRATE_REQUEST, bitrate)?;
        Ok(enc)
    }

    fn create(channels: usize, application: i32) -> Result<Self, &'static str> {
        if channels != 1 && channels != 2 {
            return Err("an Opus encoder has one or two channels");
        }
        let mut err = 0i32;
        let st = unsafe { audiopus_sys::opus_encoder_create(48_000, channels as i32, application, &mut err) };
        match NonNull::new(st) {
            Some(st) if err == 0 => Ok(Self { st, channels }),
            Some(st) => {
                unsafe { audiopus_sys::opus_encoder_destroy(st.as_ptr()) };
                Err("the Opus encoder could not be set up")
            }
            None => Err("the Opus encoder could not be created"),
        }
    }

    fn set(&mut self, request: i32, value: i32) -> Result<(), &'static str> {
        // Every request used here takes one 32-bit integer.
        match unsafe { audiopus_sys::opus_encoder_ctl(self.st.as_ptr(), request, value) } {
            0 => Ok(()),
            _ => Err("the Opus encoder refused a setting"),
        }
    }

    /// Encodes 20 ms (`FRAME` samples per channel, interleaved) into `out`. Returns the
    /// length of the packet.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, &'static str> {
        if pcm.len() != FRAME * self.channels {
            return Err("an Opus frame here is 20 ms");
        }
        let room = out.len().min(i32::MAX as usize) as i32;
        // libopus reads FRAME samples per channel and writes at most `room` bytes.
        let n = unsafe { audiopus_sys::opus_encode_float(self.st.as_ptr(), pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), room) };
        if n > 0 { Ok(n as usize) } else { Err("Opus: encoding failed") }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_encoder_destroy(self.st.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::opus_decoder::{OpusDecoder, MAX_FRAME};

    fn voiced(packet: usize, channels: usize) -> Vec<f32> {
        (0..FRAME).flat_map(|i| {
            let t = (packet * FRAME + i) as f32 / 48_000.0;
            let v: f32 = (1..=8).map(|h| (t * 150.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32).sum();
            let swell = 1.0 + 0.15 * (t * 5.0 * std::f32::consts::TAU).sin();
            std::iter::repeat(v * swell * 0.2 / 0.874).take(channels)
        }).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_voice_is_coded_as_speech_with_copies_and_comes_back_at_its_level() {
        let mut enc = OpusEncoder::voice().unwrap();
        let mut dec = OpusDecoder::new(48_000, 2).unwrap();
        let (mut buf, mut pcm) = ([0u8; 1500], vec![0f32; MAX_FRAME * 2]);
        let (mut speech_coded, mut with_copy, mut level) = (0, 0, 0.0);
        for p in 0..50 {
            let n = enc.encode(&voiced(p, 1), &mut buf).unwrap();
            assert!(n <= 400, "packet of {n} bytes at 64 kbit/s");
            if buf[0] & 0x80 == 0 { speech_coded += 1; }
            if crate::gateway::voice_rx::opus_packet_has_fec(&buf[..n]) { with_copy += 1; }
            assert_eq!(buf[0] & 0x04, 0, "mono packets");
            let got = dec.decode(&buf[..n], MAX_FRAME, &mut pcm).unwrap();
            level = rms(&pcm[..got * 2]);
        }
        assert_eq!(speech_coded, 50);
        assert!(with_copy > 20, "{with_copy} of 50 packets carry a copy of the frame before");
        assert!((0.17..0.23).contains(&level), "level {level}");
    }

    #[test]
    fn silence_is_coded_small_and_decodes_to_silence() {
        let mut enc = OpusEncoder::voice().unwrap();
        let mut dec = OpusDecoder::new(48_000, 2).unwrap();
        let (mut buf, mut pcm) = ([0u8; 1500], vec![0f32; MAX_FRAME * 2]);
        for p in 0..20 {
            let n = enc.encode(&voiced(p, 1), &mut buf).unwrap();
            dec.decode(&buf[..n], MAX_FRAME, &mut pcm).unwrap();
        }
        let mut last = (0, 1.0);
        for _ in 0..5 {
            let n = enc.encode(&[0.0; FRAME], &mut buf).unwrap();
            let got = dec.decode(&buf[..n], MAX_FRAME, &mut pcm).unwrap();
            last = (n, rms(&pcm[..got * 2]));
        }
        assert!(last.0 <= 80 && last.1 < 1e-3, "fifth frame of silence: {} bytes, level {}", last.0, last.1);
    }

    #[test]
    fn stereo_audio_keeps_its_level() {
        let mut enc = OpusEncoder::audio(2, 96_000).unwrap();
        let mut dec = OpusDecoder::new(48_000, 2).unwrap();
        let (mut buf, mut pcm) = ([0u8; 1500], vec![0f32; MAX_FRAME * 2]);
        let mut level = 0.0;
        for p in 0..25 {
            let n = enc.encode(&voiced(p, 2), &mut buf).unwrap();
            let got = dec.decode(&buf[..n], MAX_FRAME, &mut pcm).unwrap();
            level = rms(&pcm[..got * 2]);
        }
        assert!((0.17..0.23).contains(&level), "level {level}");
        assert!(enc.encode(&[0.0; 100], &mut buf).is_err(), "a frame of the wrong length is refused");
    }
}
