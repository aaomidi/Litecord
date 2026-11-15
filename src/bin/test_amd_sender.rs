//! Windows-only test program; its source is in `windows/test_amd_sender.rs`.
//! On other systems it builds as a stub, so a plain `cargo build` works everywhere.

#[cfg(windows)]
include!("windows/test_amd_sender.rs");

#[cfg(not(windows))]
fn main() {
    eprintln!("test_amd_sender only runs on Windows.");
}
