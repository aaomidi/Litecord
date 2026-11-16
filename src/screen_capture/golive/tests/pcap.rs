//! T3: the packetizer against a depacketizer that is not ours. The RTP packets of a stream
//! (no DAVE, no transport encryption, the full header extension as it goes on the wire) are
//! written to a pcap file; GStreamer's `rtph264depay` puts the stream back together and
//! ffmpeg's software decoder decodes it. The pictures must be those of the prepared units.
//!
//! Only named software elements are used (`filesrc ! pcapparse ! rtph264depay ! filesink`),
//! with `GST_REGISTRY_UPDATE=no`: no capture, no GPU, no audio device.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::super::h264::{Preparer, Skip};
use super::super::rtp::{packetize_h264, video_packet, ExtProfile, Header, MAX_PAYLOAD};
use super::{access_units, framemd5, samples, synth, tool, WorkDir};

const PAYLOAD_TYPE: u8 = 101;

/// A classic pcap file, link type Ethernet: each record is 14 bytes of Ethernet, 20 of
/// IPv4 and 8 of UDP around one RTP packet.
struct Pcap {
    file: std::io::BufWriter<std::fs::File>,
    packets: u64,
}

impl Pcap {
    fn create(path: &Path) -> Self {
        let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        file.write_all(&0xa1b2_c3d4u32.to_le_bytes()).unwrap();
        file.write_all(&2u16.to_le_bytes()).unwrap();
        file.write_all(&4u16.to_le_bytes()).unwrap();
        for word in [0u32, 0, 65_535, 1] {
            file.write_all(&word.to_le_bytes()).unwrap();
        }
        Self { file, packets: 0 }
    }

    fn write(&mut self, rtp: &[u8]) {
        let udp_len = 8 + rtp.len();
        let mut frame = Vec::with_capacity(42 + rtp.len());
        frame.extend_from_slice(&[0; 12]);
        frame.extend_from_slice(&[0x08, 0x00]);
        frame.extend_from_slice(&[0x45, 0]);
        frame.extend_from_slice(&((20 + udp_len) as u16).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2]);
        frame.extend_from_slice(&50_000u16.to_be_bytes());
        frame.extend_from_slice(&50_002u16.to_be_bytes());
        frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(rtp);
        // 200 microseconds apart.
        let at = self.packets * 200;
        for word in [(at / 1_000_000) as u32, (at % 1_000_000) as u32, frame.len() as u32, frame.len() as u32] {
            self.file.write_all(&word.to_le_bytes()).unwrap();
        }
        self.file.write_all(&frame).unwrap();
        self.packets += 1;
    }
}

/// Packetizes `units` (already prepared) into `pcap` as the session would send them, and
/// writes the same units as an Annex B file.
fn write_stream(units: &[Vec<u8>], pcap: &Path, annex_b: &Path) -> u64 {
    let mut out = Pcap::create(pcap);
    let mut source = std::io::BufWriter::new(std::fs::File::create(annex_b).unwrap());
    let mut sequence = 65_000u16; // wraps on the way in the longer streams
    let mut transport_sequence = 0u16;
    for (n, unit) in units.iter().enumerate() {
        source.write_all(unit).unwrap();
        let payloads = packetize_h264(unit, MAX_PAYLOAD);
        let count = payloads.len();
        for (i, payload) in payloads.iter().enumerate() {
            sequence = sequence.wrapping_add(1);
            transport_sequence = transport_sequence.wrapping_add(1);
            let header = Header { marker: i + 1 == count, payload_type: PAYLOAD_TYPE, sequence, timestamp: 123_456 + n as u32 * 1500, ssrc: 0x1e240 };
            out.write(&video_packet(&header, ExtProfile::Full, (n as u32 * 4369) & 0xff_ffff, transport_sequence, payload));
        }
    }
    out.file.flush().unwrap();
    source.flush().unwrap();
    out.packets
}

/// GStreamer's depacketizer on a pcap file: the stream it rebuilds, as Annex B.
fn depay(pcap: &Path, out: &Path) {
    let caps = format!("application/x-rtp,media=video,clock-rate=90000,encoding-name=H264,payload={PAYLOAD_TYPE}");
    let result = Command::new("gst-launch-1.0")
        .env("GST_REGISTRY_UPDATE", "no")
        .arg("-q")
        .args(["filesrc", &format!("location={}", pcap.display()), "!", "pcapparse", "!", &caps, "!", "rtph264depay", "!", "video/x-h264,stream-format=byte-stream,alignment=au", "!", "filesink"])
        .arg(format!("location={}", out.display()))
        .stdin(Stdio::null())
        .output()
        .expect("gst-launch-1.0 runs");
    assert!(result.status.success(), "gst-launch-1.0: {}", String::from_utf8_lossy(&result.stderr));
}

fn tools() -> bool {
    tool("gst-launch-1.0", "--version") && tool("ffmpeg", "-version")
}

/// The pictures of `units` after our packetizer and GStreamer's depacketizer, against the
/// pictures of the units themselves.
fn check(name: &str, units: &[Vec<u8>], dir: &WorkDir) {
    let stem = name.replace(['/', '.'], "_");
    let (pcap, source, rebuilt) = (dir.0.join(format!("{stem}.pcap")), dir.0.join(format!("{stem}.src.h264")), dir.0.join(format!("{stem}.depay.h264")));
    let packets = write_stream(units, &pcap, &source);
    depay(&pcap, &rebuilt);
    let (expected, got) = (framemd5(&source), framemd5(&rebuilt));
    assert!(!expected.is_empty(), "{name}: ffmpeg decoded nothing of the prepared stream");
    assert_eq!(got.len(), expected.len(), "{name}: pictures after the independent depacketizer");
    assert!(got == expected, "{name}: a picture differs after the independent depacketizer");
    println!("{name}: {} units in {packets} packets -> GStreamer rtph264depay -> ffmpeg: {} of {} pictures identical", units.len(), got.len(), expected.len());
}

#[test]
fn golive_pcap_openh264_stream_through_an_independent_depacketizer() {
    if !tools() {
        return;
    }
    let dir = WorkDir::new("pcap-synth");
    let stream = synth::stream(60);
    for (name, stream) in [("openh264 320x240, SPS without a VUI (rewritten by the sink)", synth::without_vui(&stream)), ("openh264 320x240", stream)] {
        let mut preparer = Preparer::new();
        let units: Vec<Vec<u8>> = stream.iter().map(|au| preparer.prepare(au).unwrap().data).collect();
        check(name, &units, &dir);
    }
}

#[test]
fn golive_pcap_sample_streams_through_an_independent_depacketizer() {
    let samples = samples();
    if samples.is_empty() || !tools() {
        return;
    }
    let dir = WorkDir::new("pcap-samples");
    for sample in samples {
        let mut preparer = Preparer::new();
        let mut units = Vec::new();
        for (n, au) in access_units(&sample.data).iter().enumerate() {
            match preparer.prepare(au) {
                Ok(prepared) => units.push(prepared.data),
                Err(Skip::NoPicture) => {}
                Err(e) => panic!("{} unit {n}: {e:?}", sample.name),
            }
        }
        check(&sample.name, &units, &dir);
    }
}
