//! Reproduce one fuzz input outside the fuzzer:
//!
//!     cargo run --release --no-default-features --example replay -- <target> <file>
//!
//! Works for a crash artifact from `cargo fuzz` (artifacts/<target>/crash-*)
//! or any corpus seed. Unlike `cargo fuzz run`, needs no nightly and no
//! C++ compiler, so it is the quickest way to bisect a failure.

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(target), Some(path)) = (args.next(), args.next()) else {
        eprintln!("usage: replay <target> <file>\ntargets: {}", browser_fuzz::TARGETS.iter().map(|t| t.0).collect::<Vec<_>>().join(", "));
        std::process::exit(2);
    };
    let Some((_, f)) = browser_fuzz::TARGETS.iter().find(|(n, _)| *n == target) else {
        eprintln!("unknown target {target:?}");
        std::process::exit(2);
    };
    let data = std::fs::read(&path).expect("could not read input file");
    f(&data);
    println!("ok: {target} handled {} bytes from {path}", data.len());
}
