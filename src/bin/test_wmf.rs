//! Windows-only test program; its source is in `windows/test_wmf.rs`.
//! On other systems it builds as a stub, so a plain `cargo build` works everywhere.

#[cfg(windows)]
include!("windows/test_wmf.rs");

#[cfg(not(windows))]
fn main() {
    eprintln!("test_wmf only runs on Windows.");
}
