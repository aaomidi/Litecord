//! Offline checks of the Go Live module. Nothing here talks to Discord: sockets are on
//! 127.0.0.1 only, no capture, GPU or audio device is opened, and H.264 is decoded in
//! software (the `openh264` crate, or `ffmpeg -c:v h264`).
//!
//! Tests that need no file make a small stream with OpenH264 (`synth`). Tests on real
//! streams of the fork's encoders read the directories named in `LITECORD_GOLIVE_SAMPLES`
//! (separated by `:`); without it they print "no samples, nothing tested" and pass. With
//! `LITECORD_GOLIVE_REQUIRE=1` a missing sample or tool fails the test instead.

mod crypto;
mod dave;
mod e2e;
mod fake;
mod h264;
mod pcap;
mod rtcp;
mod rtp;
mod synth;
mod wire;

use super::h264::find_nalus;

pub(super) fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len() % 2 == 0, "odd number of hex digits");
    digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
}

pub(super) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `LITECORD_GOLIVE_REQUIRE=1`: a sample or tool that is missing is a failure.
pub(super) fn required() -> bool {
    std::env::var("LITECORD_GOLIVE_REQUIRE").as_deref() == Ok("1")
}

pub(super) struct Sample {
    pub name: String,
    pub data: Vec<u8>,
}

/// The `.h264` files of the directories in `LITECORD_GOLIVE_SAMPLES`.
pub(super) fn samples() -> Vec<Sample> {
    let mut out = Vec::new();
    for dir in std::env::var("LITECORD_GOLIVE_SAMPLES").unwrap_or_default().split(':').filter(|d| !d.is_empty()) {
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(entries) => entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "h264")).collect(),
            Err(e) => {
                assert!(!required(), "sample directory {dir}: {e}");
                continue;
            }
        };
        files.sort();
        for path in files {
            let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            out.push(Sample { name: path.file_name().unwrap().to_string_lossy().into_owned(), data });
        }
    }
    if out.is_empty() {
        assert!(!required(), "LITECORD_GOLIVE_REQUIRE=1 but LITECORD_GOLIVE_SAMPLES names no .h264 file");
        println!("no samples, nothing tested");
    }
    out
}

/// The access units of an Annex B stream, start codes as they were: a new unit begins at
/// a slice that starts a picture, or at a parameter set, SEI or delimiter after a slice.
pub(super) fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut has_slice = false;
    for n in find_nalus(stream) {
        if n.len == 0 {
            continue;
        }
        let nal = &stream[n.payload..n.payload + n.len];
        let kind = nal[0] & 0x1f;
        let slice = kind == 1 || kind == 5;
        // first_mb_in_slice == 0 is a single 1 bit.
        let first = slice && nal.len() > 1 && nal[1] & 0x80 != 0;
        if has_slice && (matches!(kind, 6..=9) || first) {
            units.push(std::mem::take(&mut current));
            has_slice = false;
        }
        current.extend_from_slice(&stream[n.start..n.payload + n.len]);
        has_slice |= slice;
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

/// A directory for the files a test writes, removed when dropped (kept with
/// `LITECORD_GOLIVE_KEEP=1`). Under `LITECORD_GOLIVE_WORK` when that is set.
pub(super) struct WorkDir(pub std::path::PathBuf);

impl WorkDir {
    pub fn new(name: &str) -> Self {
        let base = std::env::var_os("LITECORD_GOLIVE_WORK").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("litecord-golive-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if std::env::var("LITECORD_GOLIVE_KEEP").as_deref() != Ok("1") {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
