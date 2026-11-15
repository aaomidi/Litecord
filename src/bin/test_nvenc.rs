//! Windows-only test program; its source is in `windows/test_nvenc.rs`.
//! On other systems it builds as a stub, so a plain `cargo build` works everywhere.

#[cfg(windows)]
include!("windows/test_nvenc.rs");

#[cfg(not(windows))]
fn main() {
    eprintln!("test_nvenc only runs on Windows.");
}
