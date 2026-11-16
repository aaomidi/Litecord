//! H.264 access units as a Discord stream needs them.
//!
//! Official viewers run libwebrtc. Its receiver splits and rebuilds a frame by its own
//! rules before the frame reaches the DAVE decryptor, and the decryptor only accepts a
//! frame that is byte for byte what the sender encrypted. So before encrypting, the unit
//! is put in the shape libwebrtc will give it back in:
//!
//! - NAL units are found by libwebrtc's rule (`H264::FindNaluIndices`), every one is
//!   written behind a 4-byte start code, and access unit delimiters and filler are left
//!   out (they carry nothing and libwebrtc's own sender drops them);
//! - the SPS is rewritten the way libwebrtc's receiver would rewrite it
//!   (`SpsVuiRewriter`: a bitstream restriction with no frame reordering and no more
//!   decoded frames buffered than reference frames), so the receiver leaves it alone;
//! - a keyframe always carries its SPS and PPS.
//!
//! The peer-to-peer share and the web share keep the encoder's bytes: this works on a copy.

use std::borrow::Cow;

use crate::encoder::h264_vui::{escape, unescape, BitReader, BitWriter};

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
pub const NAL_FILLER: u8 = 12;

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Where one NAL unit lies in an Annex B buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nalu {
    /// First byte of its start code.
    pub start: usize,
    /// First byte after the start code: the NAL header.
    pub payload: usize,
    /// Bytes from the header to the next start code, or to the end of the buffer.
    pub len: usize,
}

/// The NAL units of `buf` as libwebrtc finds them: `00 00 01` starts one, a zero byte just
/// before it belongs to the start code (one only), and a unit runs to the next start code.
/// Zero bytes at the end of a unit therefore stay in it; in an encrypted frame they are
/// ciphertext, and a splitter that trims them breaks the frame.
pub fn find_nalus(buf: &[u8]) -> Vec<Nalu> {
    let mut out: Vec<Nalu> = Vec::new();
    if buf.len() < 3 {
        return out;
    }
    let end = buf.len() - 3;
    let mut i = 0;
    while i < end {
        if buf[i + 2] > 1 {
            i += 3;
        } else if buf[i + 2] == 1 {
            if buf[i + 1] == 0 && buf[i] == 0 {
                let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
                if let Some(last) = out.last_mut() {
                    last.len = start - last.payload;
                }
                out.push(Nalu { start, payload: i + 3, len: 0 });
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(last) = out.last_mut() {
        last.len = buf.len() - last.payload;
    }
    out
}

/// The NAL units of `buf` (header byte first), by `find_nalus`.
pub fn nal_units(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    find_nalus(buf).into_iter().map(move |n| &buf[n.payload..n.payload + n.len])
}

/// The types of the NAL units of `buf`, for log lines.
pub fn nal_types(buf: &[u8]) -> Vec<u8> {
    nal_units(buf).filter(|n| !n.is_empty()).map(|n| n[0] & 0x1f).collect()
}

/// What `normalize_sps` made of an SPS.
#[derive(Debug, PartialEq, Eq)]
pub enum Sps {
    /// libwebrtc's receiver would leave it as it is.
    Unchanged,
    /// The whole NAL (header byte first), as that receiver would have rewritten it.
    Rewritten(Vec<u8>),
}

/// An SPS NAL (header byte first, emulation prevention in place) such that libwebrtc's
/// receiver does not rewrite it: its VUI has a bitstream restriction with
/// `max_num_reorder_frames == 0` and `max_dec_frame_buffering <= max_num_ref_frames`.
/// Everything before the changed fields is copied bit for bit. `None`: not a readable SPS
/// (libwebrtc drops the packet of such an SPS).
pub fn normalize_sps(nal: &[u8]) -> Option<Sps> {
    if nal.len() < 4 || nal[0] & 0x1f != NAL_SPS {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let head = read_sps_to_vui_flag(&mut r)?;
    let refs = head.max_num_ref_frames;
    // How many bits of the original are kept, and what is written after them.
    let mut tail = BitWriter::default();
    let keep = if !head.vui_present {
        // vui_parameters_present_flag, then a VUI that says nothing but the restriction:
        // aspect ratio, overscan, video signal type, chroma location, timing, both HRDs
        // and picture structure absent.
        put(&mut tail, 1, 1);
        put(&mut tail, 0, 8);
        put(&mut tail, 1, 1);
        put_restriction(&mut tail, refs);
        head.vui_flag_at
    } else {
        let restriction_flag_at = read_vui_to_restriction_flag(&mut r)?;
        if r.bits(1)? == 0 {
            put(&mut tail, 1, 1);
            put_restriction(&mut tail, refs);
            restriction_flag_at
        } else {
            r.bits(1)?; // motion_vectors_over_pic_boundaries_flag
            for _ in 0..4 {
                r.ue()?; // max_bytes_per_pic_denom, max_bits_per_mb_denom, log2_max_mv_length_*
            }
            let reorder_at = r.pos;
            let max_num_reorder_frames = r.ue()?;
            let max_dec_frame_buffering = r.ue()?;
            if max_num_reorder_frames == 0 && max_dec_frame_buffering <= refs {
                return Some(Sps::Unchanged);
            }
            put_ue(&mut tail, 0);
            put_ue(&mut tail, refs);
            reorder_at
        }
    };
    let mut w = BitWriter::default();
    let mut src = BitReader::new(&rbsp);
    for _ in 0..keep {
        w.bit(src.bits(1)? as u8);
    }
    for i in 0..tail.n {
        w.bit((tail.bytes[i / 8] >> (7 - i % 8)) & 1);
    }
    // RBSP trailing bits: the stop bit, then zeros to the end of the byte.
    w.bit(1);
    while w.n % 8 != 0 {
        w.bit(0);
    }
    let mut out = Vec::with_capacity(w.bytes.len() + 4);
    out.push(nal[0]);
    out.extend_from_slice(&escape(&w.bytes));
    Some(Sps::Rewritten(out))
}

struct SpsHead {
    max_num_ref_frames: u32,
    /// Bit position of vui_parameters_present_flag.
    vui_flag_at: usize,
    vui_present: bool,
}

/// Reads an SPS up to and including vui_parameters_present_flag (H.264 7.3.2.1.1), with
/// the limits libwebrtc's `SpsParser` applies.
fn read_sps_to_vui_flag(r: &mut BitReader) -> Option<SpsHead> {
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bits(1)? == 1 {
                    skip_scaling_list(r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    if r.ue()? > 12 {
        return None; // log2_max_frame_num_minus4
    }
    match r.ue()? {
        0 => {
            if r.ue()? > 12 {
                return None; // log2_max_pic_order_cnt_lsb_minus4
            }
        }
        1 => {
            r.bits(1)?; // delta_pic_order_always_zero_flag
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?; // frame cropping offsets
        }
    }
    let vui_flag_at = r.pos;
    let vui_present = r.bits(1)? == 1;
    Some(SpsHead { max_num_ref_frames, vui_flag_at, vui_present })
}

/// Reads a VUI (H.264 E.1.1) up to bitstream_restriction_flag and returns that flag's bit
/// position.
fn read_vui_to_restriction_flag(r: &mut BitReader) -> Option<usize> {
    if r.bits(1)? == 1 && r.bits(8)? == 255 {
        r.bits(32)?; // sar_width, sar_height
    }
    if r.bits(1)? == 1 {
        r.bits(1)?; // overscan_appropriate_flag
    }
    if r.bits(1)? == 1 {
        r.bits(4)?; // video_format, video_full_range_flag
        if r.bits(1)? == 1 {
            r.bits(24)?; // colour primaries, transfer, matrix
        }
    }
    if r.bits(1)? == 1 {
        r.ue()?;
        r.ue()?; // chroma sample locations
    }
    if r.bits(1)? == 1 {
        r.bits(32)?; // num_units_in_tick
        r.bits(32)?; // time_scale
        r.bits(1)?; // fixed_frame_rate_flag
    }
    let nal_hrd = r.bits(1)? == 1;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.bits(1)? == 1;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bits(1)?; // low_delay_hrd_flag
    }
    r.bits(1)?; // pic_struct_present_flag
    Some(r.pos)
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cpb_cnt_minus1 = r.ue()?;
    if cpb_cnt_minus1 > 31 {
        return None;
    }
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
    }
    r.bits(20)?; // four 5-bit lengths
    Some(())
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn put(w: &mut BitWriter, value: u32, bits: u32) {
    for i in (0..bits).rev() {
        w.bit(((value >> i) & 1) as u8);
    }
}

fn put_ue(w: &mut BitWriter, value: u32) {
    let x = value as u64 + 1;
    let len = 64 - x.leading_zeros();
    for _ in 1..len {
        w.bit(0);
    }
    for i in (0..len).rev() {
        w.bit(((x >> i) & 1) as u8);
    }
}

/// The bitstream restriction libwebrtc writes: the defaults of an absent one, except that
/// nothing is reordered and no more frames are buffered than are referenced.
fn put_restriction(w: &mut BitWriter, max_num_ref_frames: u32) {
    put(w, 1, 1); // motion_vectors_over_pic_boundaries_flag
    put_ue(w, 2); // max_bytes_per_pic_denom
    put_ue(w, 1); // max_bits_per_mb_denom
    put_ue(w, 16); // log2_max_mv_length_horizontal
    put_ue(w, 16); // log2_max_mv_length_vertical
    put_ue(w, 0); // max_num_reorder_frames
    put_ue(w, max_num_ref_frames); // max_dec_frame_buffering
}

/// Why an access unit is not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// No start code in it.
    NotAnnexB,
    /// No picture in it: parameter sets, delimiters or filler only (the sets are kept).
    NoPicture,
    /// A keyframe without its SPS or PPS, and none seen before: a new keyframe is needed.
    NoParameterSets,
    /// Its SPS cannot be read; a viewer's libwebrtc would drop it.
    BadSps,
}

/// An access unit ready for DAVE and the packetizer: every NAL behind `00 00 00 01`.
#[derive(Debug)]
pub struct Prepared {
    pub data: Vec<u8>,
    /// It holds an IDR picture (with its SPS and PPS).
    pub keyframe: bool,
    /// The unit's own SPS was rewritten (`None`: it had none).
    pub sps_rewritten: Option<bool>,
    /// The SPS or PPS came from an earlier unit.
    pub sets_added: bool,
}

/// Prepares access units, remembering the stream's last SPS (normalised) and PPS.
#[derive(Default)]
pub struct Preparer {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Preparer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The unit the encoder produced, as it is sent. The parameter sets of a unit are
    /// remembered even when the unit itself is skipped.
    pub fn prepare(&mut self, access_unit: &[u8]) -> Result<Prepared, Skip> {
        let nalus = find_nalus(access_unit);
        if nalus.is_empty() {
            return Err(Skip::NotAnnexB);
        }
        let mut nals: Vec<Cow<[u8]>> = Vec::with_capacity(nalus.len() + 2);
        let (mut has_sps, mut has_pps, mut keyframe) = (false, false, false);
        let mut first_picture: Option<usize> = None;
        let mut sps_rewritten = None;
        for n in nalus {
            let nal = &access_unit[n.payload..n.payload + n.len];
            // (A unit of one byte has no content, and DAVE's frame walk leaves it out.)
            if nal.len() < 2 {
                continue;
            }
            match nal[0] & 0x1f {
                NAL_AUD | NAL_FILLER => {}
                NAL_SPS => {
                    let sps = match normalize_sps(nal).ok_or(Skip::BadSps)? {
                        Sps::Unchanged => {
                            sps_rewritten = Some(false);
                            Cow::Borrowed(nal)
                        }
                        Sps::Rewritten(fixed) => {
                            sps_rewritten = Some(true);
                            Cow::Owned(fixed)
                        }
                    };
                    self.sps = Some(sps.to_vec());
                    has_sps = true;
                    nals.push(sps);
                }
                NAL_PPS => {
                    self.pps = Some(nal.to_vec());
                    has_pps = true;
                    nals.push(Cow::Borrowed(nal));
                }
                kind => {
                    if kind == NAL_SLICE || kind == NAL_IDR {
                        first_picture.get_or_insert(nals.len());
                        keyframe |= kind == NAL_IDR;
                    }
                    nals.push(Cow::Borrowed(nal));
                }
            }
        }
        let first_picture = first_picture.ok_or(Skip::NoPicture)?;
        let sets_added = keyframe && !(has_sps && has_pps);
        if sets_added {
            if !has_pps {
                let pps = self.pps.clone().ok_or(Skip::NoParameterSets)?;
                nals.insert(first_picture, Cow::Owned(pps));
            }
            if !has_sps {
                let sps = self.sps.clone().ok_or(Skip::NoParameterSets)?;
                nals.insert(0, Cow::Owned(sps));
            }
        }
        let mut data = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for nal in &nals {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(nal);
        }
        Ok(Prepared { data, keyframe, sps_rewritten, sets_added })
    }
}
