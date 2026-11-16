//! T1: the NAL split, the preparation of access units and the SPS normaliser.

use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::{Nal, RefNal};

use super::super::h264::*;
use super::{access_units, hex, samples, synth, to_hex};
use crate::encoder::h264_vui::{escape, BitWriter};

pub(super) fn parsed(sps_nal: &[u8]) -> SeqParameterSet {
    SeqParameterSet::from_bits(RefNal::new(sps_nal, &[], true).rbsp_bits()).unwrap_or_else(|e| panic!("SPS {} does not parse: {e:?}", to_hex(sps_nal)))
}

/// What makes libwebrtc's receiver leave an SPS alone, read by an independent parser.
pub(super) fn webrtc_leaves_it_alone(sps_nal: &[u8]) -> bool {
    let sps = parsed(sps_nal);
    sps.vui_parameters
        .as_ref()
        .and_then(|vui| vui.bitstream_restrictions.as_ref())
        .is_some_and(|r| r.max_num_reorder_frames == 0 && r.max_dec_frame_buffering <= sps.max_num_ref_frames)
}

fn normalized(nal: &[u8]) -> Vec<u8> {
    match normalize_sps(nal).expect("a readable SPS") {
        Sps::Unchanged => nal.to_vec(),
        Sps::Rewritten(out) => out,
    }
}

/// The SPS NALs of the fork's GPU encoders (Constrained Baseline and Main at 1080p, 720p
/// and an odd size), as the design derived them: only max_dec_frame_buffering changes.
const VECTORS: [(&str, &str); 4] = [
    ("2742c02a8889403c0113f2ffe00020002d40404190000003001000000789f08846e0", "2742c02a8889403c0113f2ffe00020002d40404190000003001000000789f08846a0"),
    ("274d402a8889403c0113f2ffe00020002d40404190000003001000000789f08846e0", "274d402a8889403c0113f2ffe00020002d40404190000003001000000789f08846a0"),
    ("2742c0208889402802ddff80008000b501010640000003004000001e27c2211b80", "2742c0208889402802ddff80008000b501010640000003004000001e27c2211a80"),
    ("2742c01f8889406c1ef37ff80008000b50101064000003000400000301e27c2211b8", "2742c01f8889406c1ef37ff80008000b50101064000003000400000301e27c2211a8"),
];
/// libx264 without B-frames: already as libwebrtc wants it.
const UNCHANGED: &str = "67640020acb402802dd80880000003008000003c478c1950";

#[test]
fn golive_sps_vectors_of_the_forks_encoders() {
    for (input, expected) in VECTORS {
        let input = hex(input);
        assert!(!webrtc_leaves_it_alone(&input), "the vector is one libwebrtc rewrites");
        let out = normalized(&input);
        assert_eq!(to_hex(&out), expected);
        assert!(webrtc_leaves_it_alone(&out));
        // Only the bitstream restriction changed.
        let (before, after) = (parsed(&input), parsed(&out));
        assert_eq!((before.pic_width_in_mbs_minus1, before.pic_height_in_map_units_minus1, before.max_num_ref_frames), (after.pic_width_in_mbs_minus1, after.pic_height_in_map_units_minus1, after.max_num_ref_frames));
        assert_eq!(u8::from(before.profile_idc), u8::from(after.profile_idc));
        assert_eq!(before.vui_parameters.as_ref().unwrap().timing_info.is_some(), after.vui_parameters.as_ref().unwrap().timing_info.is_some());
        let (b, a) = (before.vui_parameters.unwrap().bitstream_restrictions.unwrap(), after.vui_parameters.unwrap().bitstream_restrictions.unwrap());
        assert_eq!((b.max_dec_frame_buffering, a.max_dec_frame_buffering), (2, 1));
        assert_eq!((b.max_bytes_per_pic_denom, b.log2_max_mv_length_horizontal), (a.max_bytes_per_pic_denom, a.log2_max_mv_length_horizontal));
        // Normalising twice changes nothing.
        assert_eq!(normalize_sps(&out), Some(Sps::Unchanged));
    }
    let unchanged = hex(UNCHANGED);
    assert!(webrtc_leaves_it_alone(&unchanged));
    assert_eq!(normalize_sps(&unchanged), Some(Sps::Unchanged));
}

#[test]
fn golive_openh264_sps_gains_a_vui_with_the_restriction() {
    let units = synth::stream(3);
    let sps = nal_units(&units[0]).find(|n| n[0] & 0x1f == NAL_SPS).expect("the first unit has an SPS").to_vec();
    let before = parsed(&sps);
    assert!(before.vui_parameters.is_none(), "OpenH264's SPS is expected to come without a VUI");
    assert!(!webrtc_leaves_it_alone(&sps));
    let out = normalized(&sps);
    assert_ne!(out, sps);
    let after = parsed(&out);
    assert!(webrtc_leaves_it_alone(&out));
    let restriction = after.vui_parameters.as_ref().unwrap().bitstream_restrictions.as_ref().unwrap();
    assert_eq!(restriction.max_dec_frame_buffering, before.max_num_ref_frames);
    // The defaults of an absent restriction, as libwebrtc writes them.
    assert_eq!((restriction.motion_vectors_over_pic_boundaries_flag, restriction.max_bytes_per_pic_denom, restriction.max_bits_per_mb_denom), (true, 2, 1));
    assert_eq!((restriction.log2_max_mv_length_horizontal, restriction.log2_max_mv_length_vertical), (16, 16));
    assert_eq!((after.pic_width_in_mbs_minus1, after.pic_height_in_map_units_minus1), (before.pic_width_in_mbs_minus1, before.pic_height_in_map_units_minus1));
    assert_eq!(after.frame_cropping, before.frame_cropping);
    assert_eq!(normalize_sps(&out), Some(Sps::Unchanged));
}

fn put(w: &mut BitWriter, value: u32, bits: u32) {
    for i in (0..bits).rev() {
        w.bit(((value >> i) & 1) as u8);
    }
}

fn ue(w: &mut BitWriter, value: u32) {
    let x = value + 1;
    let len = 32 - x.leading_zeros();
    put(w, 0, len - 1);
    put(w, x, len);
}

/// A High profile SPS with scaling lists, a VUI with aspect ratio, a colour description,
/// timing and a NAL HRD, and `restriction`: (max_num_reorder_frames,
/// max_dec_frame_buffering) or none.
fn rich_sps(refs: u32, restriction: Option<(u32, u32)>) -> Vec<u8> {
    let mut w = BitWriter::default();
    put(&mut w, 100, 8); // High
    put(&mut w, 0, 8);
    put(&mut w, 40, 8);
    ue(&mut w, 0); // sps id
    ue(&mut w, 1); // chroma 4:2:0
    ue(&mut w, 0);
    ue(&mut w, 0);
    put(&mut w, 0, 1);
    put(&mut w, 1, 1); // scaling matrix present
    for i in 0..8 {
        put(&mut w, (i == 0) as u32, 1); // only the first list
        if i == 0 {
            for _ in 0..16 {
                ue(&mut w, 0); // delta_scale 0: every entry 8
            }
        }
    }
    ue(&mut w, 4); // log2_max_frame_num_minus4
    ue(&mut w, 0); // poc type 0
    ue(&mut w, 4);
    ue(&mut w, refs);
    put(&mut w, 0, 1);
    ue(&mut w, 119); // 1920
    ue(&mut w, 67); // 1088
    put(&mut w, 1, 1); // frame_mbs_only
    put(&mut w, 1, 1); // direct_8x8
    put(&mut w, 1, 1); // cropping
    for crop in [0, 0, 0, 4] {
        ue(&mut w, crop);
    }
    put(&mut w, 1, 1); // VUI
    put(&mut w, 1, 1); // aspect ratio
    put(&mut w, 255, 8);
    put(&mut w, 1, 16);
    put(&mut w, 1, 16);
    put(&mut w, 0, 1); // overscan
    put(&mut w, 1, 1); // video signal type
    put(&mut w, 5, 3);
    put(&mut w, 0, 1);
    put(&mut w, 1, 1); // colour description
    put(&mut w, 1, 8);
    put(&mut w, 1, 8);
    put(&mut w, 1, 8);
    put(&mut w, 0, 1); // chroma loc
    put(&mut w, 1, 1); // timing
    put(&mut w, 1, 32);
    put(&mut w, 120, 32);
    put(&mut w, 1, 1);
    put(&mut w, 1, 1); // NAL HRD
    ue(&mut w, 0); // cpb_cnt_minus1
    put(&mut w, 4, 4);
    put(&mut w, 4, 4);
    ue(&mut w, 1000);
    ue(&mut w, 2000);
    put(&mut w, 0, 1);
    put(&mut w, 23, 5);
    put(&mut w, 23, 5);
    put(&mut w, 23, 5);
    put(&mut w, 24, 5);
    put(&mut w, 0, 1); // VCL HRD
    put(&mut w, 0, 1); // low_delay_hrd_flag
    put(&mut w, 0, 1); // pic_struct
    match restriction {
        None => put(&mut w, 0, 1),
        Some((reorder, buffering)) => {
            put(&mut w, 1, 1);
            put(&mut w, 1, 1);
            ue(&mut w, 0);
            ue(&mut w, 0);
            ue(&mut w, 10);
            ue(&mut w, 10);
            ue(&mut w, reorder);
            ue(&mut w, buffering);
        }
    }
    w.bit(1);
    while w.n % 8 != 0 {
        w.bit(0);
    }
    let mut nal = vec![0x67];
    nal.extend_from_slice(&escape(&w.bytes));
    nal
}

#[test]
fn golive_sps_with_a_full_vui_keeps_everything_but_the_restriction() {
    for (refs, restriction, changes) in [(4, None, true), (4, Some((2, 6)), true), (4, Some((0, 5)), true), (4, Some((1, 4)), true), (4, Some((0, 4)), false), (4, Some((0, 2)), false), (1, Some((0, 1)), false)] {
        let sps = rich_sps(refs, restriction);
        let before = parsed(&sps);
        assert_eq!(!webrtc_leaves_it_alone(&sps), changes, "{refs} {restriction:?}");
        let out = normalized(&sps);
        assert_eq!(out != sps, changes, "{refs} {restriction:?}");
        assert!(webrtc_leaves_it_alone(&out), "{refs} {restriction:?}");
        let after = parsed(&out);
        let (v0, v1) = (before.vui_parameters.as_ref().unwrap(), after.vui_parameters.as_ref().unwrap());
        assert_eq!(format!("{:?}", v0.aspect_ratio_info), format!("{:?}", v1.aspect_ratio_info));
        assert_eq!(format!("{:?}", v0.video_signal_type), format!("{:?}", v1.video_signal_type));
        assert_eq!(format!("{:?}", v0.timing_info), format!("{:?}", v1.timing_info));
        assert_eq!(format!("{:?}", v0.nal_hrd_parameters), format!("{:?}", v1.nal_hrd_parameters));
        assert_eq!(after.frame_cropping, before.frame_cropping);
        let r = v1.bitstream_restrictions.as_ref().unwrap();
        match restriction {
            // Rewritten: no reordering, as many frames buffered as referenced.
            Some(_) if changes => assert_eq!((r.max_num_reorder_frames, r.max_dec_frame_buffering, r.log2_max_mv_length_vertical), (0, refs, 10)),
            Some((reorder, buffering)) => assert_eq!((r.max_num_reorder_frames, r.max_dec_frame_buffering), (reorder, buffering)),
            None => assert_eq!((r.max_num_reorder_frames, r.max_dec_frame_buffering, r.max_bytes_per_pic_denom), (0, refs, 2)),
        }
        assert_eq!(normalize_sps(&out), Some(Sps::Unchanged));
    }
}

#[test]
fn golive_unreadable_sps_is_reported_not_guessed() {
    let sps = hex(VECTORS[0].0);
    for cut in [1, 3, 4, 9, 20, sps.len() - 2] {
        assert_eq!(normalize_sps(&sps[..cut]), None, "cut at {cut}");
    }
    assert_eq!(normalize_sps(&[]), None);
    // Not an SPS at all.
    assert_eq!(normalize_sps(&hex("28ce3c80")), None);
    let mut preparer = Preparer::new();
    let mut unit = vec![0, 0, 0, 1];
    unit.extend_from_slice(&sps[..9]);
    unit.extend_from_slice(&[0, 0, 0, 1, 0x28, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x25, 0x88, 0x84, 0x21]);
    assert_eq!(preparer.prepare(&unit).unwrap_err(), Skip::BadSps);
}

#[test]
fn golive_nal_split_follows_libwebrtc() {
    // 3-byte and 4-byte start codes, a unit ending in zeros, a zero before a start code.
    let buf = hex("000001 6742 00 000001 68ce 0000 00000001 6588 00 0000 000001 41 9a");
    let units: Vec<&[u8]> = nal_units(&buf).collect();
    assert_eq!(units, vec![&hex("6742")[..], &hex("68ce 00")[..], &hex("6588 00 00")[..], &hex("419a")[..]]);
    let found = find_nalus(&buf);
    // Each start code is 3 bytes, or 4 when a zero precedes it.
    assert_eq!(found.iter().map(|n| n.payload - n.start).collect::<Vec<_>>(), vec![3, 4, 4, 4]);
    assert_eq!(found[0].start, 0);
    // Zeros at the very end stay in the last unit; a start code in the last 3 bytes is not one.
    assert_eq!(nal_units(&hex("00000001 65 88 00 00")).collect::<Vec<_>>(), vec![&hex("65 88 00 00")[..]]);
    assert_eq!(nal_units(&hex("00000001 65 88 000001")).collect::<Vec<_>>(), vec![&hex("65 88 000001")[..]]);
    assert!(find_nalus(&[0, 0]).is_empty());
    assert!(find_nalus(&hex("65 88 84")).is_empty());
    // Emulation prevention bytes are not start codes.
    assert_eq!(nal_units(&hex("00000001 65 00 00 03 01 00 00 03 00 11")).count(), 1);
}

fn sps_pps() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let sps = hex(VECTORS[0].0);
    (sps.clone(), hex(VECTORS[0].1), hex("28ce3c80"))
}

fn unit(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
}

#[test]
fn golive_prepare_drops_delimiters_and_filler_and_normalises_the_sps() {
    let (sps, fixed, pps) = sps_pps();
    let idr = hex("2588 8400 1122 3344 5500");
    let aud = hex("09f0");
    let filler = hex("0cffffff80");
    let sei = hex("0605ffff80");
    // Mixed start codes, as encoders write them.
    let mut au = hex("00000001");
    au.extend_from_slice(&aud);
    for nal in [&sps[..], &pps, &sei, &filler] {
        au.extend_from_slice(&hex("000001"));
        au.extend_from_slice(nal);
    }
    au.extend_from_slice(&hex("00000001"));
    au.extend_from_slice(&idr);
    let mut preparer = Preparer::new();
    let prepared = preparer.prepare(&au).unwrap();
    assert_eq!(prepared.data, unit(&[&fixed, &pps, &sei, &idr]), "every unit behind 00 00 00 01, the IDR's trailing zero kept");
    assert!(prepared.keyframe && !prepared.sets_added);
    assert_eq!(prepared.sps_rewritten, Some(true));
    assert_eq!(nal_types(&prepared.data), vec![7, 8, 6, 5]);
    // Preparing what was prepared changes nothing.
    let again = preparer.prepare(&prepared.data).unwrap();
    assert_eq!(again.data, prepared.data);
    assert_eq!(again.sps_rewritten, Some(false));

    // A P frame: nothing added.
    let p = hex("219a 0102 0304");
    let prepared = preparer.prepare(&unit(&[&aud, &p])).unwrap();
    assert_eq!(prepared.data, unit(&[&p]));
    assert!(!prepared.keyframe && prepared.sps_rewritten.is_none() && !prepared.sets_added);

    // Units without a picture are not sent, and no start code is no access unit.
    assert_eq!(preparer.prepare(&unit(&[&aud, &filler])).unwrap_err(), Skip::NoPicture);
    assert_eq!(preparer.prepare(&unit(&[&sps, &pps])).unwrap_err(), Skip::NoPicture);
    assert_eq!(preparer.prepare(&hex("2588 8400 11")).unwrap_err(), Skip::NotAnnexB);
}

#[test]
fn golive_keyframe_without_parameter_sets_gets_the_cached_ones() {
    let (sps, fixed, pps) = sps_pps();
    let idr = hex("2588 8400 1122");
    let sei = hex("0605ffff80");
    let mut preparer = Preparer::new();
    // Nothing seen yet: the keyframe cannot be sent.
    assert_eq!(preparer.prepare(&unit(&[&idr])).unwrap_err(), Skip::NoParameterSets);
    // Parameter sets alone are remembered, though not sent.
    assert_eq!(preparer.prepare(&unit(&[&sps])).unwrap_err(), Skip::NoPicture);
    assert_eq!(preparer.prepare(&unit(&[&idr])).unwrap_err(), Skip::NoParameterSets, "a PPS is still missing");
    assert_eq!(preparer.prepare(&unit(&[&pps])).unwrap_err(), Skip::NoPicture);
    let prepared = preparer.prepare(&unit(&[&sei, &idr])).unwrap();
    assert_eq!(prepared.data, unit(&[&fixed, &sei, &pps, &idr]), "the normalised SPS first, the PPS before the picture");
    assert!(prepared.keyframe && prepared.sets_added && prepared.sps_rewritten.is_none());
    // Only the missing one is added.
    let prepared = preparer.prepare(&unit(&[&sps, &idr])).unwrap();
    assert_eq!(prepared.data, unit(&[&fixed, &pps, &idr]));
    assert!(prepared.sets_added);
    let prepared = preparer.prepare(&unit(&[&pps, &idr])).unwrap();
    assert_eq!(prepared.data, unit(&[&fixed, &pps, &idr]));
}

#[test]
fn golive_openh264_stream_prepares_to_units_libwebrtc_leaves_alone() {
    let units = synth::stream(25);
    let mut preparer = Preparer::new();
    let mut keyframes = 0;
    for (n, au) in units.iter().enumerate() {
        let prepared = preparer.prepare(au).unwrap_or_else(|e| panic!("unit {n}: {e:?}"));
        assert_eq!(prepared.keyframe, n % synth::KEYFRAME_EVERY == 0, "unit {n}");
        keyframes += prepared.keyframe as usize;
        for nal in nal_units(&prepared.data) {
            assert!(!matches!(nal[0] & 0x1f, NAL_AUD | NAL_FILLER));
            if nal[0] & 0x1f == NAL_SPS {
                assert!(webrtc_leaves_it_alone(nal));
            }
        }
        if prepared.keyframe {
            assert_eq!(&nal_types(&prepared.data)[..2], &[7, 8]);
        }
    }
    assert_eq!(keyframes, 3);
}

#[test]
fn golive_sample_streams_prepare_to_units_libwebrtc_leaves_alone() {
    for sample in samples() {
        let units = access_units(&sample.data);
        let mut preparer = Preparer::new();
        let (mut keyframes, mut rewritten, mut sent) = (0, 0, 0);
        for (n, au) in units.iter().enumerate() {
            let prepared = match preparer.prepare(au) {
                Ok(prepared) => prepared,
                Err(Skip::NoPicture) => continue,
                Err(e) => panic!("{} unit {n}: {e:?}", sample.name),
            };
            sent += 1;
            keyframes += prepared.keyframe as usize;
            rewritten += (prepared.sps_rewritten == Some(true)) as usize;
            for nal in nal_units(&prepared.data) {
                assert!(!matches!(nal[0] & 0x1f, NAL_AUD | NAL_FILLER), "{} unit {n}", sample.name);
                if nal[0] & 0x1f == NAL_SPS {
                    assert!(webrtc_leaves_it_alone(nal), "{} unit {n}: {}", sample.name, to_hex(nal));
                    assert_eq!(normalize_sps(nal), Some(Sps::Unchanged));
                }
            }
            if prepared.keyframe {
                let types = nal_types(&prepared.data);
                assert!(types.contains(&7) && types.contains(&8), "{} unit {n}: {types:?}", sample.name);
            }
        }
        assert!(sent > 0 && keyframes > 0, "{}: {sent} units, {keyframes} keyframes", sample.name);
        println!("{}: {} access units, {sent} sent, {keyframes} keyframes, SPS rewritten in {rewritten}", sample.name, units.len());
    }
}
