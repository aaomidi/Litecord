//! A small H.264 stream made on the spot with OpenH264 (software, no device), and its
//! software decoder to compare pictures.
//!
//! OpenH264 writes its SPS the way libwebrtc wants it (libwebrtc encodes with it), so the
//! stream as it is exercises only the "nothing to change" branch of the SPS normaliser.
//! `without_vui` makes the other kind of stream of it: the same pictures behind an SPS
//! that says nothing beyond the picture format, which libwebrtc's receiver rewrites.

use crate::encoder::h264_vui::{escape, unescape, BitReader, BitWriter};
use crate::screen_capture::golive::h264::nal_units;

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

/// The same units with every SPS cut off before its VUI.
pub fn without_vui(units: &[Vec<u8>]) -> Vec<Vec<u8>> {
    units
        .iter()
        .map(|unit| {
            let mut out = Vec::with_capacity(unit.len());
            for nal in nal_units(unit) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                if nal[0] & 0x1f == 7 {
                    out.extend_from_slice(&sps_without_vui(nal));
                } else {
                    out.extend_from_slice(nal);
                }
            }
            out
        })
        .collect()
}

/// An SPS NAL of the Baseline, Main or Extended profile with `vui_parameters_present_flag`
/// cleared and nothing after it. (Read here field by field, not by the normaliser under test;
/// the tests check the result with `h264-reader`.)
pub fn sps_without_vui(nal: &[u8]) -> Vec<u8> {
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile = r.bits(8).unwrap();
    assert!(matches!(profile, 66 | 77 | 88), "profile {profile} has fields this does not read");
    r.bits(16).unwrap(); // constraint flags, level
    r.ue().unwrap(); // seq_parameter_set_id
    r.ue().unwrap(); // log2_max_frame_num_minus4
    match r.ue().unwrap() {
        0 => {
            r.ue().unwrap(); // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bits(1).unwrap();
            r.se().unwrap();
            r.se().unwrap();
            let cycle = r.ue().unwrap();
            for _ in 0..cycle {
                r.se().unwrap();
            }
        }
        _ => {}
    }
    r.ue().unwrap(); // max_num_ref_frames
    r.bits(1).unwrap(); // gaps_in_frame_num_value_allowed_flag
    r.ue().unwrap(); // pic_width_in_mbs_minus1
    r.ue().unwrap(); // pic_height_in_map_units_minus1
    if r.bits(1).unwrap() == 0 {
        r.bits(1).unwrap(); // mb_adaptive_frame_field_flag
    }
    r.bits(1).unwrap(); // direct_8x8_inference_flag
    if r.bits(1).unwrap() == 1 {
        for _ in 0..4 {
            r.ue().unwrap(); // frame cropping offsets
        }
    }
    let keep = r.pos;
    let mut w = BitWriter::default();
    let mut source = BitReader::new(&rbsp);
    for _ in 0..keep {
        w.bit(source.bits(1).unwrap() as u8);
    }
    w.bit(0); // vui_parameters_present_flag
    w.bit(1); // RBSP stop bit
    while w.n % 8 != 0 {
        w.bit(0);
    }
    [&nal[..1], &escape(&w.bytes)[..]].concat()
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
