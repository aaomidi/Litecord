//! Declaring the colours of an H.264 stream in its SPS, for encoders that leave them out.
//!
//! Quick Sync (through Media Foundation) writes the range of its pictures but no colour
//! description, whatever the media type says; decoders then guess the matrix (Litecord
//! assumes BT.601, browsers often BT.709). `declare_colour` rewrites each SPS whose VUI
//! lacks a colour description so that it carries one: inside its video signal type, or with
//! a whole video signal type (limited range) when there is none. Every other bit of the SPS
//! is kept, and the RBSP trailing bits are realigned.
//!
//! NVENC writes a VUI without bitstream restrictions: for Main nothing then tells decoders
//! that no picture is reordered, and they may hold pictures back. `declare_no_reordering`
//! adds them (`max_num_reorder_frames` 0).

/// Annex B `stream` with every SPS declaring (primaries, transfer, matrix); unchanged when
/// no SPS needs it.
pub fn declare_colour(stream: &[u8], primaries: u8, transfer: u8, matrix: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len() + 8);
    let mut last = 0;
    for range in crate::screen_capture::hw_decode::h264_state::nal_ranges(stream) {
        if stream[range.start] & 0x1f != 7 {
            continue;
        }
        let Some(rewritten) = rewrite_sps(&stream[range.clone()], primaries, transfer, matrix) else { continue };
        out.extend_from_slice(&stream[last..range.start]);
        out.extend_from_slice(&rewritten);
        last = range.end;
    }
    if last == 0 {
        return stream.to_vec();
    }
    out.extend_from_slice(&stream[last..]);
    out
}

/// Annex B `stream` with every SPS that has a VUI but no bitstream restrictions declaring
/// that no picture is reordered; unchanged when no SPS needs it.
pub fn declare_no_reordering(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len() + 8);
    let mut last = 0;
    for range in crate::screen_capture::hw_decode::h264_state::nal_ranges(stream) {
        if stream[range.start] & 0x1f != 7 {
            continue;
        }
        let Some(rewritten) = restrict_sps(&stream[range.clone()]) else { continue };
        out.extend_from_slice(&stream[last..range.start]);
        out.extend_from_slice(&rewritten);
        last = range.end;
    }
    if last == 0 {
        return stream.to_vec();
    }
    out.extend_from_slice(&stream[last..]);
    out
}

/// Reads an SPS RBSP up to its VUI: `None` without one. Returns max_num_ref_frames.
fn skip_to_vui(r: &mut BitReader) -> Option<u32> {
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
        if r.bits(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bits(1)? == 1 {
                    skip_scaling_list(r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_refs = r.ue()?;
    r.bits(1)?;
    r.ue()?;
    r.ue()?;
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.bits(1)? == 0 {
        return None; // no VUI
    }
    Some(max_refs)
}

/// One SPS NAL (header byte included, emulation prevention bytes in place), with the
/// colour description added; `None` when it has none to add or cannot be read.
fn rewrite_sps(nal: &[u8], primaries: u8, transfer: u8, matrix: u8) -> Option<Vec<u8>> {
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    skip_to_vui(&mut r)?;
    if r.bits(1)? == 1 && r.bits(8)? == 255 {
        r.bits(32)?; // sar_width, sar_height
    }
    if r.bits(1)? == 1 {
        r.bits(1)?;
    }
    let signal_at = r.pos;
    let has_signal = r.bits(1)? == 1;
    let (flag_at, insert): (usize, Vec<u8>) = if has_signal {
        r.bits(4)?; // video_format, video_full_range_flag
        let flag_at = r.pos;
        if r.bits(1)? == 1 {
            return None; // already declared
        }
        // colour_description_present_flag = 1, then the three values.
        (flag_at, std::iter::once(1).chain(byte_bits(&[primaries, transfer, matrix])).collect())
    } else {
        // video_signal_type_present_flag = 1, video_format 5 (unspecified), limited range,
        // colour_description_present_flag = 1, then the three values.
        let mut v = vec![1, 1, 0, 1, 0, 1];
        v.extend(byte_bits(&[primaries, transfer, matrix]));
        (signal_at, v)
    };
    splice(nal[0], &rbsp, flag_at, insert)
}

/// One SPS NAL with `bitstream_restriction_flag` set: no reordering, as many pictures
/// buffered as are referenced, everything else at its inferred value. `None` when it has
/// restrictions already, no VUI, or cannot be read.
fn restrict_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let max_refs = skip_to_vui(&mut r)?;
    if r.bits(1)? == 1 && r.bits(8)? == 255 {
        r.bits(32)?;
    }
    if r.bits(1)? == 1 {
        r.bits(1)?;
    }
    if r.bits(1)? == 1 {
        r.bits(4)?;
        if r.bits(1)? == 1 {
            r.bits(24)?;
        }
    }
    if r.bits(1)? == 1 {
        r.ue()?; // chroma_sample_loc_type_top_field
        r.ue()?;
    }
    if r.bits(1)? == 1 {
        r.bits(32)?; // num_units_in_tick
        r.bits(32)?;
        r.bits(1)?;
    }
    let nal_hrd = r.bits(1)? == 1;
    if nal_hrd {
        skip_hrd(&mut r)?;
    }
    let vcl_hrd = r.bits(1)? == 1;
    if vcl_hrd {
        skip_hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bits(1)?; // low_delay_hrd_flag
    }
    r.bits(1)?; // pic_struct_present_flag
    let flag_at = r.pos;
    if r.bits(1)? == 1 {
        return None; // already restricted
    }
    // motion_vectors_over_pic_boundaries_flag, max_bytes_per_pic_denom 2, max_bits_per_mb_denom
    // 1, log2_max_mv_length_horizontal/vertical 16, max_num_reorder_frames 0,
    // max_dec_frame_buffering.
    let mut insert = vec![1, 1];
    for v in [2, 1, 16, 16, 0, max_refs] {
        insert.extend(ue_bits(v));
    }
    splice(nal[0], &rbsp, flag_at, insert)
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cpb_cnt = r.ue()?;
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..=cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
    }
    r.bits(20)?; // four 5-bit lengths
    Some(())
}

fn ue_bits(v: u32) -> impl Iterator<Item = u8> {
    let x = v as u64 + 1;
    let len = 64 - x.leading_zeros();
    (0..len - 1).map(|_| 0).chain((0..len).rev().map(move |i| ((x >> i) & 1) as u8))
}

/// The NAL `header` + `rbsp` with the one bit at `flag_at` replaced by `insert`, the rest
/// copied without the old trailing bits, and the RBSP ended again.
fn splice(header: u8, rbsp: &[u8], flag_at: usize, insert: Vec<u8>) -> Option<Vec<u8>> {
    let mut payload_end = rbsp.len() * 8;
    while payload_end > 0 && (rbsp[(payload_end - 1) / 8] >> (7 - (payload_end - 1) % 8)) & 1 == 0 {
        payload_end -= 1;
    }
    payload_end = payload_end.checked_sub(1)?; // the stop bit
    let mut w = BitWriter::default();
    let mut src = BitReader::new(rbsp);
    for _ in 0..flag_at {
        w.bit(src.bits(1)? as u8);
    }
    src.bits(1)?;
    for b in insert {
        w.bit(b);
    }
    while src.pos < payload_end {
        w.bit(src.bits(1)? as u8);
    }
    w.bit(1);
    while w.n % 8 != 0 {
        w.bit(0);
    }
    let mut out = vec![header];
    out.extend_from_slice(&escape(&w.bytes));
    Some(out)
}

fn byte_bits(values: &[u8]) -> impl Iterator<Item = u8> + '_ {
    values.iter().flat_map(|v| (0..8).rev().map(move |i| (v >> i) & 1))
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// RBSP without emulation prevention bytes (00 00 03 -> 00 00).
pub(crate) fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention: 00 00 followed by 00..03 gets a 03 in between.
pub(crate) fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub(crate) fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }
    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    pub(crate) fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -k / 2 })
    }
}

#[derive(Default)]
pub(crate) struct BitWriter {
    pub(crate) bytes: Vec<u8>,
    pub(crate) n: usize,
}

impl BitWriter {
    pub(crate) fn bit(&mut self, b: u8) {
        if self.n % 8 == 0 {
            self.bytes.push(0);
        }
        if b != 0 {
            *self.bytes.last_mut().unwrap() |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h264_reader::nal::sps::SeqParameterSet;
    use h264_reader::nal::{Nal, RefNal};

    fn sps_of(stream: &[u8]) -> SeqParameterSet {
        let r = crate::screen_capture::hw_decode::h264_state::nal_ranges(stream).into_iter().find(|r| stream[r.start] & 0x1f == 7).unwrap();
        let nal = RefNal::new(&stream[r], &[], true);
        SeqParameterSet::from_bits(nal.rbsp_bits()).unwrap()
    }

    /// An SPS as Quick Sync writes it: High profile, 1280x720, a video signal type with
    /// limited range and no colour description, bitstream restrictions after it.
    fn quick_sync_like_sps() -> Vec<u8> {
        let mut w = BitWriter::default();
        let put = |w: &mut BitWriter, v: u32, n: u32| for i in (0..n).rev() { w.bit(((v >> i) & 1) as u8) };
        let ue = |w: &mut BitWriter, v: u32| {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            put(w, 0, len - 1);
            put(w, x, len);
        };
        put(&mut w, 100, 8); // profile_idc High
        put(&mut w, 0, 8);
        put(&mut w, 31, 8); // level 3.1
        ue(&mut w, 0); // sps id
        ue(&mut w, 1); // chroma 4:2:0
        ue(&mut w, 0);
        ue(&mut w, 0);
        put(&mut w, 0, 1);
        put(&mut w, 0, 1); // no scaling matrix
        ue(&mut w, 0); // log2_max_frame_num_minus4
        ue(&mut w, 2); // poc type 2
        ue(&mut w, 1); // refs
        put(&mut w, 0, 1);
        ue(&mut w, 79); // 80 mbs wide
        ue(&mut w, 44); // 45 mbs high
        put(&mut w, 1, 1); // frame_mbs_only
        put(&mut w, 1, 1); // direct_8x8
        put(&mut w, 0, 1); // no cropping
        put(&mut w, 1, 1); // vui present
        put(&mut w, 0, 1); // no aspect ratio
        put(&mut w, 0, 1); // no overscan
        put(&mut w, 1, 1); // video signal type
        put(&mut w, 5, 3);
        put(&mut w, 0, 1); // limited range
        put(&mut w, 0, 1); // no colour description
        put(&mut w, 0, 1); // no chroma loc
        put(&mut w, 0, 1); // no timing
        put(&mut w, 0, 1);
        put(&mut w, 0, 1);
        put(&mut w, 0, 1); // pic_struct
        put(&mut w, 1, 1); // bitstream restriction
        put(&mut w, 1, 1);
        ue(&mut w, 2);
        ue(&mut w, 1);
        ue(&mut w, 16);
        ue(&mut w, 16);
        ue(&mut w, 0); // num_reorder_frames
        ue(&mut w, 1);
        w.bit(1); // rbsp stop bit
        while w.n % 8 != 0 {
            w.bit(0);
        }
        let mut nal = vec![0, 0, 0, 1, 0x67];
        nal.extend_from_slice(&escape(&w.bytes));
        nal.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x80]); // an IDR slice follows
        nal
    }

    #[test]
    fn adds_the_colour_description_and_keeps_everything_else() {
        let before = quick_sync_like_sps();
        let b = sps_of(&before);
        assert!(b.vui_parameters.as_ref().unwrap().video_signal_type.as_ref().unwrap().colour_description.is_none());

        let after = declare_colour(&before, 1, 1, 1);
        let a = sps_of(&after);
        let vui = a.vui_parameters.as_ref().unwrap();
        let sig = vui.video_signal_type.as_ref().unwrap();
        let c = sig.colour_description.as_ref().expect("a colour description");
        assert_eq!((c.colour_primaries, c.transfer_characteristics, c.matrix_coefficients), (1, 1, 1));
        assert!(!sig.video_full_range_flag);
        assert_eq!(a.pic_width_in_mbs_minus1, b.pic_width_in_mbs_minus1);
        assert_eq!(a.pic_height_in_map_units_minus1, b.pic_height_in_map_units_minus1);
        assert_eq!(u8::from(a.profile_idc), 100);
        let br = vui.bitstream_restrictions.as_ref().expect("bitstream restrictions kept");
        assert_eq!(br.max_num_reorder_frames, 0);
        // The slice after it is untouched.
        assert!(after.ends_with(&[0, 0, 0, 1, 0x65, 0x88, 0x80]));
        // Already declared: left alone.
        assert_eq!(declare_colour(&after, 6, 6, 6), after);
    }

    /// The same SPS without a video signal type at all.
    fn sps_without_signal_type() -> Vec<u8> {
        let before = quick_sync_like_sps();
        let nal_end = before.windows(4).skip(4).position(|w| w == [0, 0, 0, 1]).unwrap() + 4;
        let rbsp = unescape(&before[5..nal_end]);
        // Find the video signal type flag by parsing, then drop it and its 5 bits.
        let mut r = BitReader::new(&rbsp);
        r.bits(24).unwrap();
        for _ in 0..4 { r.ue().unwrap(); }
        r.bits(2).unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.bits(1).unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.bits(4).unwrap(); // frame_mbs_only, direct_8x8, cropping, vui present
        r.bits(2).unwrap(); // aspect, overscan
        let at = r.pos;
        let total = rbsp.len() * 8;
        let mut src = BitReader::new(&rbsp);
        let mut w = BitWriter::default();
        while src.pos < total {
            let b = src.bits(1).unwrap() as u8;
            if src.pos - 1 == at {
                w.bit(0); // video_signal_type_present_flag = 0
                src.bits(5).unwrap(); // and its format/range/colour flag
                continue;
            }
            w.bit(b);
        }
        // Trailing: drop zero padding then realign (5 bits shorter now).
        let mut bits: Vec<u8> = (0..w.n).map(|i| (w.bytes[i / 8] >> (7 - i % 8)) & 1).collect();
        while bits.last() == Some(&0) { bits.pop(); }
        let mut w2 = BitWriter::default();
        for b in bits { w2.bit(b); }
        while w2.n % 8 != 0 { w2.bit(0); }
        let mut nal = vec![0, 0, 0, 1, 0x67];
        nal.extend_from_slice(&escape(&w2.bytes));
        nal
    }

    #[test]
    fn adds_a_whole_video_signal_type_when_there_is_none() {
        let before = sps_without_signal_type();
        let b = sps_of(&before);
        assert!(b.vui_parameters.as_ref().unwrap().video_signal_type.is_none());
        let after = declare_colour(&before, 1, 1, 1);
        let a = sps_of(&after);
        let vui = a.vui_parameters.as_ref().unwrap();
        let sig = vui.video_signal_type.as_ref().expect("a video signal type");
        assert!(!sig.video_full_range_flag);
        let c = sig.colour_description.as_ref().unwrap();
        assert_eq!((c.colour_primaries, c.transfer_characteristics, c.matrix_coefficients), (1, 1, 1));
        assert_eq!(vui.bitstream_restrictions.as_ref().unwrap().max_num_reorder_frames, 0);
        assert_eq!(a.pic_width_in_mbs_minus1, 79);
    }

    /// The Quick Sync SPS with its bitstream restrictions taken off (as NVENC writes them).
    fn sps_without_restrictions() -> Vec<u8> {
        let before = quick_sync_like_sps();
        let nal_end = before.windows(4).skip(4).position(|w| w == [0, 0, 0, 1]).unwrap() + 4;
        let rbsp = unescape(&before[5..nal_end]);
        let mut r = BitReader::new(&rbsp);
        skip_to_vui(&mut r).unwrap();
        r.bits(12).unwrap(); // aspect, overscan, signal type (1 + 5), chroma loc, timing, nal/vcl hrd
        r.bits(1).unwrap(); // pic_struct
        let at = r.pos;
        let mut w = BitWriter::default();
        let mut src = BitReader::new(&rbsp);
        for _ in 0..at {
            w.bit(src.bits(1).unwrap() as u8);
        }
        w.bit(0); // bitstream_restriction_flag
        w.bit(1);
        while w.n % 8 != 0 {
            w.bit(0);
        }
        let mut nal = vec![0, 0, 0, 1, 0x67];
        nal.extend_from_slice(&escape(&w.bytes));
        nal
    }

    #[test]
    fn adds_bitstream_restrictions_when_there_are_none() {
        let before = sps_without_restrictions();
        let b = sps_of(&before);
        assert!(b.vui_parameters.as_ref().unwrap().bitstream_restrictions.is_none());
        let after = declare_no_reordering(&before);
        let a = sps_of(&after);
        let vui = a.vui_parameters.as_ref().unwrap();
        let br = vui.bitstream_restrictions.as_ref().expect("bitstream restrictions");
        assert_eq!(br.max_num_reorder_frames, 0);
        assert_eq!(br.max_dec_frame_buffering, 1);
        assert!(!vui.video_signal_type.as_ref().unwrap().video_full_range_flag);
        assert_eq!(a.pic_width_in_mbs_minus1, 79);
        // Already restricted: left alone.
        assert_eq!(declare_no_reordering(&after), after);
        assert_eq!(declare_no_reordering(&quick_sync_like_sps()), quick_sync_like_sps());
    }

    #[test]
    fn streams_without_sps_pass_through() {
        let slice = [0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x00, 0x03, 0x01];
        assert_eq!(declare_colour(&slice, 1, 1, 1), slice.to_vec());
    }

    #[test]
    fn escaping_round_trips() {
        let rbsp = [0u8, 0, 1, 0, 0, 0, 0, 0, 3, 7, 0, 0];
        assert_eq!(unescape(&escape(&rbsp)), rbsp);
        assert!(!escape(&rbsp).windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
    }
}
