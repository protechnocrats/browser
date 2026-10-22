//! libFuzzer entry point for `browser_fuzz::filter_list` — see that function's
//! doc comment for what it covers. Run with:
//!
//!     cargo +nightly fuzz run filter_list
//!
//! (needs `cargo install cargo-fuzz`, a nightly toolchain, and a C++
//! compiler for libFuzzer.) The logic lives in the library so the same
//! code also runs on stable via `cargo test` — see `tests/`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| browser_fuzz::filter_list(data));
