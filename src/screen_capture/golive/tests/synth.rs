//! A small H.264 stream made on the spot with OpenH264 (software, no device), and its
//! software decoder to compare pictures.

use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;
/// A keyframe every this many frames.
pub const KEYFRAME_EVERY: usize = 10;

/// `frames` access units of a moving pattern, 320x240, a keyframe every 10 frames. Its SPS
/// has no VUI (the other branch of the normaliser than the fork's GPU encoders take).
pub fn stream(frames: usize) -> Vec<Vec<u8>> {
    let config = EncoderConfig::new()
        .skip_frames(false)
        .bitrate(BitRate::from_bps(900_000))
        .max_frame_rate(FrameRate::from_hz(30.0))
        .intra_frame_period(IntraFramePeriod::from_num_frames(KEYFRAME_EVERY as u32))
        .num_threads(1);
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).expect("OpenH264 encoder");
    (0..frames)
        .map(|n| {
            if n % KEYFRAME_EVERY == 0 {
                encoder.force_intra_frame();
            }
            let unit = encoder.encode(&picture(n)).expect("encode").to_vec();
            assert!(!unit.is_empty(), "frame {n} was skipped by the encoder");
            unit
        })
        .collect()
}

fn picture(n: usize) -> YUVBuffer {
    let mut data = vec![0u8; WIDTH * HEIGHT * 3 / 2];
    let (luma, chroma) = data.split_at_mut(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            // Diagonal bars that move, and a block that travels: something for P frames to do.
            let bars = ((x + y + n * 4) / 16 % 2) as u8 * 120 + 60;
            let block = (x / 8 + n) % 40 < 6 && (y / 8 + n / 2) % 30 < 6;
            luma[y * WIDTH + x] = if block { 235 } else { bars };
        }
    }
    let (u, v) = chroma.split_at_mut(WIDTH * HEIGHT / 4);
    for y in 0..HEIGHT / 2 {
        for x in 0..WIDTH / 2 {
            u[y * WIDTH / 2 + x] = (96 + (x + n) % 64) as u8;
            v[y * WIDTH / 2 + x] = (96 + (y + n * 2) % 64) as u8;
        }
    }
    YUVBuffer::from_vec(data, WIDTH, HEIGHT)
}

/// Decodes access units in software, one picture (RGB) per unit that gives one.
pub struct SoftwareDecoder(Decoder);

impl SoftwareDecoder {
    pub fn new() -> Self {
        Self(Decoder::new().expect("OpenH264 decoder"))
    }

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Vec<u8>>, String> {
        use openh264::formats::YUVSource;
        match self.0.decode(access_unit) {
            Ok(Some(picture)) => {
                let (w, h) = picture.dimensions();
                let mut rgb = vec![0u8; w * h * 3];
                picture.write_rgb8(&mut rgb);
                Ok(Some(rgb))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}
