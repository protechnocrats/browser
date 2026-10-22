//! Corpus test: parses every fixture in `tests/fixtures/` and asserts
//! only that `parse_document` doesn't panic — the actual point of
//! this milestone. Whether a given fixture parses to `Ok` or
//! `Err(NoHtmlElement)` is secondary (several are deliberately
//! missing a real `<html>` element or are otherwise extremely
//! malformed); a clean `Result` either way, with no process abort, is
//! the pass condition.
//!
//! This is the "small, real-shaped test corpus" from
//! milestones/milestones-01-17.md's acceptance criteria
//! — not a substitute for the fuzz harness in `fuzz/`, but a fast,
//! deterministic check that runs on every `cargo test`.

use std::fs;
use std::path::Path;

#[test]
fn every_fixture_parses_without_panicking() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;

    for entry in fs::read_dir(&fixtures_dir).expect("fixtures directory should exist") {
        let entry = entry.expect("readable directory entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }

        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {path:?}: {e}"));

        // The assertion IS the call itself: if this panics, the test
        // fails with a stack trace pointing at exactly which fixture
        // triggered it. We don't additionally assert Ok/Err — several
        // fixtures are intentionally missing <html> or are pure
        // garbage input.
        let _ = html_parser::parse_document(&content);
        checked += 1;
    }

    assert!(
        checked >= 5,
        "expected at least 5 fixture files, found {checked} — did the fixtures directory move?"
    );
}
