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
mod rate;
mod rtcp;
mod rtp;
mod synth;
mod wire;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::h264::find_nalus;

/// The `[GO LIVE]` lines logged so far in this test run, by all tests together.
static LOG_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LOG_IS_OURS: AtomicBool = AtomicBool::new(false);

struct TestLog;

impl log::Log for TestLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        let line = format!("{:5} {}", record.level(), record.args());
        if line.contains("[GO LIVE]") {
            LOG_LINES.lock().unwrap_or_else(|e| e.into_inner()).push(line.clone());
        }
        // (Through the test harness: shown for a failed test, or with --nocapture.)
        println!("{line}");
    }

    fn flush(&self) {}
}

/// Log lines of the module go to the test output, and are kept for `logged`.
pub(super) fn log_init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if log::set_logger(&TestLog).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
            LOG_IS_OURS.store(true, Ordering::SeqCst);
        }
    });
}

/// Whether a `[GO LIVE]` line containing `needle` was logged. `None` when another test of
/// the crate installed its own logger first (then nothing can be said).
pub(super) fn logged(needle: &str) -> Option<bool> {
    LOG_IS_OURS.load(Ordering::SeqCst).then(|| LOG_LINES.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|line| line.contains(needle)))
}

/// Runs an async test on a runtime of its own, and fails it when it takes too long.
pub(super) fn run<F: std::future::Future<Output = ()>>(limit_secs: u64, test: F) {
    log_init();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        if tokio::time::timeout(std::time::Duration::from_secs(limit_secs), test).await.is_err() {
            panic!("the test took more than {limit_secs} s");
        }
    });
}

/// Whether `program` can be run; a missing tool fails the test with `LITECORD_GOLIVE_REQUIRE=1`.
pub(super) fn tool(program: &str, version_flag: &str) -> bool {
    let found = std::process::Command::new(program).arg(version_flag).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    assert!(found || !required(), "LITECORD_GOLIVE_REQUIRE=1 but {program} cannot be run");
    if !found {
        println!("{program} is not installed: nothing tested");
    }
    found
}

/// The MD5 of every picture ffmpeg's software H.264 decoder makes of an Annex B file.
pub(super) fn framemd5(path: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-threads", "2", "-c:v", "h264", "-i"])
        .arg(path)
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(out.status.success(), "ffmpeg on {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.rsplit(',').next()).map(|h| h.trim().to_string()).collect()
}

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
