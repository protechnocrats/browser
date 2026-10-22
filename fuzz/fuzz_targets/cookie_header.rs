//! libFuzzer entry point for `browser_fuzz::cookie_header` — see that function's
//! doc comment for what it covers. Run with:
//!
//!     cargo +nightly fuzz run cookie_header
//!
//! (needs `cargo install cargo-fuzz`, a nightly toolchain, and a C++
//! compiler for libFuzzer.) The logic lives in the library so the same
//! code also runs on stable via `cargo test` — see `tests/`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| browser_fuzz::cookie_header(data));
