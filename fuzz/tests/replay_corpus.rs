//! Stable-Rust regression coverage for the fuzz targets — runs anywhere
//! `cargo test --no-default-features` runs (no nightly, no C++
//! toolchain, no libFuzzer). Three jobs:
//!
//! 1. **Registration** — every target in `browser_fuzz::TARGETS` has a
//!    `fuzz_targets/<name>.rs` wrapper, a `[[bin]]`, and committed seeds,
//!    and vice versa. A target cannot exist without being exercised.
//! 2. **Replay** — every committed seed (including every past crash /
//!    security-pass regression) runs through its target: no panic, and
//!    finishes within a time bound.
//! 3. **Mutation smoke** — a deterministic, seeded set of mutations of
//!    every seed (bit flips, truncation, splices, repeats). Not
//!    coverage-guided like libFuzzer, but it catches shallow
//!    panics/hangs anywhere `cargo test` runs, and is fully reproducible:
//!    a failure prints the exact input.
//!
//! Run with `--release`: the recursion-depth caps in the parsers were
//! sized against optimized stack frames (see `.github/workflows/ci.yml`).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use browser_fuzz::TARGETS;

/// Generous: layout of a 700-deep tree finishes in milliseconds; this
/// bound exists to catch accidental quadratic/exponential blowups and
/// hangs, not to be a benchmark.
const PER_INPUT_TIME_BOUND: Duration = Duration::from_secs(10);
/// Default kept small so `cargo test` stays fast in CI; raise it for a
/// deeper local hunt: `FUZZ_MUTATIONS=5000 cargo test --release
/// --no-default-features mutations`.
const DEFAULT_MUTATIONS_PER_SEED: usize = 60;
const MAX_MUTATED_LEN: usize = 16 * 1024;

fn fuzz_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn seeds(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = fuzz_dir().join("corpus").join(target);
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().is_file() {
                out.push((e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// Runs one input; returns an error string on panic or on exceeding the
/// time bound. Never propagates the panic, so ALL failures get reported.
fn run_one(target: &str, f: fn(&[u8]), label: &str, input: &[u8]) -> Result<(), String> {
    // Stack overflows and allocation failures ABORT the process — no
    // `catch_unwind` can report them — so `FUZZ_VERBOSE=1 cargo test
    // -- --nocapture` prints each input first: the last line before an
    // abort is the culprit.
    if std::env::var_os("FUZZ_VERBOSE").is_some() {
        eprintln!("running [{target}] {label} ({} bytes)", input.len());
    }
    let start = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| f(input)));
    let elapsed = start.elapsed();
    if result.is_err() {
        return Err(format!("[{target}] PANIC on {label} ({} bytes): {:?}", input.len(), preview(input)));
    }
    if elapsed > PER_INPUT_TIME_BOUND {
        return Err(format!("[{target}] SLOW ({elapsed:?}) on {label} ({} bytes): {:?}", input.len(), preview(input)));
    }
    Ok(())
}

fn preview(input: &[u8]) -> String {
    let cut = &input[..input.len().min(200)];
    format!("{}{}", String::from_utf8_lossy(cut), if input.len() > 200 { "…" } else { "" })
}

// ---- 1. Registration -------------------------------------------------

#[test]
fn every_target_is_fully_registered() {
    let manifest = std::fs::read_to_string(fuzz_dir().join("Cargo.toml")).unwrap();
    let mut problems = Vec::new();

    for (name, _) in TARGETS {
        if !fuzz_dir().join("fuzz_targets").join(format!("{name}.rs")).is_file() {
            problems.push(format!("{name}: missing fuzz_targets/{name}.rs wrapper"));
        }
        if !manifest.contains(&format!("name = \"{name}\"")) {
            problems.push(format!("{name}: missing [[bin]] entry in Cargo.toml"));
        }
        if seeds(name).is_empty() {
            problems.push(format!("{name}: no committed seeds under corpus/{name}/ (run generate_seeds.py)"));
        }
    }

    // And the reverse: no orphan wrapper that TARGETS doesn't know about.
    for e in std::fs::read_dir(fuzz_dir().join("fuzz_targets")).unwrap().flatten() {
        let stem = Path::new(&e.file_name()).file_stem().unwrap().to_string_lossy().into_owned();
        if !TARGETS.iter().any(|(n, _)| *n == stem) {
            problems.push(format!("{stem}: fuzz_targets/{stem}.rs exists but is not in browser_fuzz::TARGETS"));
        }
    }

    assert!(problems.is_empty(), "fuzz target registration problems:\n  {}", problems.join("\n  "));
    assert!(TARGETS.len() >= 6, "Milestone 25 requires at least 6 fuzz targets");
}

// ---- 2. Replay -------------------------------------------------------

#[test]
fn every_committed_seed_replays_cleanly() {
    let mut failures = Vec::new();
    let mut total = 0;
    for (name, f) in TARGETS {
        for (label, bytes) in seeds(name) {
            total += 1;
            if let Err(e) = run_one(name, *f, &label, &bytes) {
                failures.push(e);
            }
        }
    }
    assert!(total > 0);
    assert!(failures.is_empty(), "{} of {total} seeds failed:\n  {}", failures.len(), failures.join("\n  "));
}

// ---- 2b. Structural stress ------------------------------------------

/// Inputs too large to commit as seeds, and beyond libFuzzer's default
/// 4 KiB `max_len` — but exactly the shape of the bugs that abort a
/// process (stack overflow, unbounded allocation) rather than panic.
/// Generated here so every `cargo test` replays them. Several mirror
/// the security-pass regression unit tests (`css_parser`'s 100,000-deep
/// `:not()`, `html_parser`'s deep nesting) at the SAME depth: a
/// shallower seed cannot reach the native stack limit, so it cannot
/// prove the depth cap is what stands between us and a crash — found
/// the hard way while validating this harness (a 400-deep `:not()` seed
/// replayed cleanly even with the cap deleted).
fn stress_inputs() -> Vec<(&'static str, String, Vec<u8>)> {
    const N: usize = 100_000;
    let mut v: Vec<(&'static str, String, Vec<u8>)> = Vec::new();
    let mut add = |t: &'static str, l: &str, data: String| v.push((t, l.to_string(), data.into_bytes()));

    add("parse_document", "100k nested <div>", "<div>".repeat(N));
    add("parse_document", "100k mismatched closes", format!("{}{}", "<b><i>".repeat(N / 2), "</u></p>".repeat(N / 2)));
    add("parse_document", "1 MiB of attribute", format!("<a href=\"{}\">x</a>", "a".repeat(1 << 20)));
    add("parse_document", "100k unclosed comments", "<!--".repeat(N));
    add("parse_document", "100k entities", "&#x41;&amp;&bogus;".repeat(N));

    let deep_not = format!("{}a{}", ":not(".repeat(N), ")".repeat(N));
    add("parse_stylesheet", "100k nested :not()", format!("{deep_not}{{color:red}}"));
    add("parse_stylesheet", "100k nested @media blocks", format!("{}p{{x:y}}{}", "@media (min-width:1px){".repeat(N / 4), "}".repeat(N / 4)));
    add("parse_stylesheet", "100k unclosed braces", "a{".repeat(N));
    add("parse_stylesheet", "100k nested style rules", format!("{}x:y{}", ".a{".repeat(N), "}".repeat(N)));
    add("parse_stylesheet", "wide nested selector lists", format!("{}{{{}{{x:y}}}}", vec![".p"; 1000].join(","), vec!["&.c"; 1000].join(",")));
    add("css_length", "100k nested calc(", format!("{}1px{}", "calc(".repeat(N), ")".repeat(N)));
    add("css_grid", "100k-track list", "1px ".repeat(N));
    add("css_grid", "huge spans and lines", "999999 / span 999999".to_string());
    add("css_grid", "1000 auto-fill tracks of 0.001px", "repeat(auto-fill, 0.001px)".to_string());
    add("css_length", "100k-term calc sum", format!("calc({})", vec!["1px"; N].join(" + ")));
    add("css_length", "100k-argument min()", format!("min({})", vec!["1em"; N].join(",")));
    add("parse_stylesheet", "100k selector list", format!("{}{{a:b}}", (0..N).map(|i| format!(".c{i}")).collect::<Vec<_>>().join(",")));
    add("parse_stylesheet", "100k descendant chain", format!("{}{{a:b}}", "a ".repeat(N)));
    add("parse_stylesheet", "100k child combinators", format!("{}{{a:b}}", "a>".repeat(N)));
    add("parse_stylesheet", "100k attribute selectors", format!("a{}{{a:b}}", "[x]".repeat(N)));
    add("parse_stylesheet", "1 MiB declaration value", format!("p{{color:{}}}", "x".repeat(1 << 20)));
    add("parse_stylesheet", "100k nested functions", format!("p{{width:{}1px{}}}", "calc(".repeat(N), ")".repeat(N)));

    add("parse_inline_style", "1M semicolons", ";".repeat(1 << 20));
    add("parse_inline_style", "100k nested parens", format!("width:{}1{}", "(".repeat(N), ")".repeat(N)));
    add("parse_inline_style", "100k declarations", "color:red;".repeat(N));

    add("url_origin", "1 MiB relative path", format!("https://example.com/\n{}", "../".repeat(1 << 18)));
    add("url_origin", "1 MiB host", format!("http://{}.com/\nx", "a".repeat(1 << 20)));
    add("url_origin", "100k userinfo @", format!("http://{}example.com/\n/", "u@".repeat(N)));

    add("cookie_header", "1 MiB cookie value", format!("a={}; Path=/\nexample.com\n/", "v".repeat(1 << 20)));
    add("cookie_header", "100k attributes", format!("a=b{}\nexample.com\n/", "; Secure".repeat(N)));
    add("cookie_header", "100k-line persisted jar", "a\tb\texample.com\t/\t99999999999\t1\t0\n".repeat(N));

    add("webidl_parse", "100k nested Promise<", format!("interface A {{ {}long f(); }};", "Promise<".repeat(N)));
    add("webidl_parse", "100k extended-attribute brackets", "[".repeat(N));
    add("webidl_parse", "100k members", format!("interface A {{ {} }};", "attribute long a;".repeat(N)));
    add("pattern_matches", "100k value vs catastrophic pattern", format!("(a+)+$\n{}!", "a".repeat(N)));
    add("pattern_matches", "1 MiB pattern", format!("{}\nx", "a?".repeat(1 << 19)));
    add("pattern_matches", "100k nested groups", format!("{}a{}\na", "(".repeat(N), ")".repeat(N)));
    add("pattern_matches", "100k alternations", format!("{}\nz", (0..N).map(|i| format!("w{i}")).collect::<Vec<_>>().join("|")));
    add("psl_parse", "100k-rule list", format!("{}\0a.b.c", (0..N).map(|i| format!("rule{i}.example")).collect::<Vec<_>>().join("\n")));
    add("psl_parse", "1 MiB single rule", format!("{}\0x", "a.".repeat(1 << 19)));
    add("psl_parse", "100k wildcards", format!("{}\0a", "*.x\n".repeat(N)));
    add("psl_parse", "100k-label host", format!("com\0{}", "a.".repeat(N)));
    add("url_origin", "100k-label host site", format!("http://{}com/\nx", "a.".repeat(N)));

    let deep_divs = format!("<html><body>{}x{}</body></html>", "<div>".repeat(N), "</div>".repeat(N));
    add("layout_document", "100k nested divs", format!("{deep_divs}\0div{{margin:1px;padding:1px}}"));
    add("layout_document", "100k nested flex", format!("{deep_divs}\0div{{display:flex;gap:1px;flex:1}}"));
    add("layout_document", "1 MiB unbreakable word", format!("<html><body><p>{}</p></body></html>", "w".repeat(1 << 20)));
    add("layout_document", "100k words", format!("<html><body><p>{}</p></body></html>", "word ".repeat(N)));
    add("layout_document", "10k siblings", format!("<html><body>{}</body></html>", "<span>a</span>".repeat(10_000)));
    // Performance audit: selectors whose matching backtracked through every
    // combination of ancestors or siblings (exponential; one of these took
    // longer than a minute before the matcher was memoized).
    let deep_512 = format!("<html><body>{}<span>x</span>{}</body></html>", "<div>".repeat(510), "</div>".repeat(510));
    add("layout_document", "60-compound descendant selector on a 512-deep tree", format!("{deep_512}\0.x {}span{{color:red}}", "div ".repeat(60)));
    add("layout_document", "mixed descendant/child selector on a 512-deep tree", format!("{deep_512}\0.x {}span{{color:red}}", "div > div ".repeat(30)));
    add("layout_document", "sibling-combinator chain across 10k siblings", format!("<html><body>{}<span>x</span></body></html>\0.x {}span{{color:red}}", "<p>a</p>".repeat(10_000), "~ p ".repeat(20)));
    // ADR 016: the process boundary. Claims far beyond the data, and a
    // frame of millions of tiny items.
    add("ipc_decode", "every length field at u32::MAX", String::from_utf8_lossy(&[0u8, 0xFF, 0xFF, 0xFF, 0xFF]).into_owned());
    add("ipc_decode", "1 MiB of glyph-run tags", format!("\u{0}\u{0}\u{0}\u{0}\u{0}\u{1}{}", "\u{2}".repeat(1 << 20)));
    add("layout_document", "100k-deep :not() cascade", format!("<html><body><p class=a>t</p></body></html>\0p{}.b{}{{color:red}}", ":not(".repeat(N), ")".repeat(N)));
    v
}

#[test]
fn structural_stress_inputs_neither_crash_nor_blow_the_time_bound() {
    let mut failures = Vec::new();
    for (target, label, bytes) in stress_inputs() {
        let (_, f) = TARGETS.iter().find(|(n, _)| *n == target).expect("stress input names an unknown target");
        // The default test-thread stack — the same one real callers of
        // these functions get on a non-main thread.
        if let Err(e) = run_one(target, *f, &label, &bytes) {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{} stress failures:\n  {}", failures.len(), failures.join("\n  "));
}

// ---- 2c. Whole-font corruption (Milestone 32) ------------------------

/// The mutation smoke test caps inputs at 16 KiB, which truncates any real
/// font; this corrupts the COMPLETE bundled font instead — flipped bytes in
/// its table directory, glyph data and hinting — the way a hostile web font
/// would arrive.
#[test]
fn corrupted_copies_of_a_real_font_never_panic() {
    let font = include_bytes!("../../crates/text/assets/DejaVuSans.ttf");
    let (_, f) = TARGETS.iter().find(|(n, _)| *n == "font_parse").unwrap();
    let mut rng = Rng(0xF0_4E7_C0DE);
    let mut failures = Vec::new();
    assert!(run_one("font_parse", *f, "the intact font", font).is_ok());
    let rounds = std::env::var("FUZZ_FONT_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40usize);
    for round in 0..rounds {
        let mut bytes = font.to_vec();
        for _ in 0..1 + rng.below(32) {
            // Bias towards the first 64 KiB: the table directory and the
            // tables every parser reads first.
            let limit = if rng.below(2) == 0 { bytes.len().min(64 * 1024) } else { bytes.len() };
            let i = rng.below(limit);
            bytes[i] = rng.next() as u8;
        }
        // `FUZZ_FONT_DUMP=<round>` writes that round's font to disk (to
        // reproduce a hang, which never returns to be reported).
        if std::env::var("FUZZ_FONT_DUMP").ok().and_then(|v| v.parse::<usize>().ok()) == Some(round) {
            let _ = std::fs::write(fuzz_dir().join(format!("font-round-{round}.ttf")), &bytes);
        }
        if let Err(e) = run_one("font_parse", *f, &format!("corruption round {round}"), &bytes) {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{} font failures:\n  {}", failures.len(), failures.join("\n  "));
}

// ---- 3. Mutation smoke -----------------------------------------------

/// xorshift64* — tiny, dependency-free, deterministic.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const INTERESTING: &[&[u8]] = &[
    b"\0", b"<", b">", b"{", b"}", b"(", b")", b"\"", b"'", b"\\", b";", b":", b"&#", b"%00", b"\xff\xff\xff\xff",
    b"999999999999", b"-1", b"1e999", b":not(", b"url(", b"</", b"<!--", b"\r\n", b"/*",
];

fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(6) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 if !v.is_empty() => {
                let i = rng.below(v.len());
                v.truncate(i);
            }
            2 if !v.is_empty() => {
                let i = rng.below(v.len());
                v.remove(i);
            }
            3 => {
                let tok = INTERESTING[rng.below(INTERESTING.len())];
                let i = rng.below(v.len() + 1);
                for (k, b) in tok.iter().enumerate() {
                    v.insert(i + k, *b);
                }
            }
            4 if !v.is_empty() => {
                // Repeat a random slice (grows nesting / repetition).
                let a = rng.below(v.len());
                let b = a + rng.below((v.len() - a).min(64) + 1);
                let chunk = v[a..b].to_vec();
                let times = 1 + rng.below(40);
                let at = rng.below(v.len() + 1);
                for _ in 0..times {
                    if v.len() + chunk.len() > MAX_MUTATED_LEN {
                        break;
                    }
                    for (k, byte) in chunk.iter().enumerate() {
                        v.insert(at + k, *byte);
                    }
                }
            }
            _ if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = rng.next() as u8;
            }
            _ => {}
        }
    }
    v.truncate(MAX_MUTATED_LEN);
    v
}

#[test]
fn deterministic_mutations_of_every_seed_never_panic_or_hang() {
    let mutations_per_seed = std::env::var("FUZZ_MUTATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MUTATIONS_PER_SEED);
    let mut failures = Vec::new();
    let mut executed = 0usize;
    for (name, f) in TARGETS {
        for (seed_label, bytes) in seeds(name) {
            // Deterministic per (target, seed): a failure reproduces.
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (seed_label.len() as u64) ^ ((bytes.len() as u64) << 20) ^ (name.len() as u64) << 40);
            for m in 0..mutations_per_seed {
                let input = mutate(&bytes, &mut rng);
                executed += 1;
                let label = format!("mutation #{m} of {seed_label}");
                if let Err(e) = run_one(name, *f, &label, &input) {
                    failures.push(e);
                    if failures.len() >= 10 {
                        break;
                    }
                }
            }
        }
    }
    eprintln!("mutation smoke: {executed} inputs across {} targets", TARGETS.len());
    assert!(failures.is_empty(), "{} mutation failures:\n  {}", failures.len(), failures.join("\n  "));
}
