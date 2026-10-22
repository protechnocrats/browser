//! Milestone 32 — counting what real pages ask for that the engine does not
//! support: CSS properties and values, at-rules and selectors, script errors
//! (usually a missing Web API), document encodings.
//!
//! The conformance harness (`crates/conformance`) turns these counts into a
//! ranked report, so the layout and text milestones (M33, M35) are
//! prioritised by what the corpus actually uses rather than by guesses.
//!
//! **Off by default** and local-only. When off, [`record`] is one relaxed
//! atomic load: the cascade calls it for every unknown declaration it
//! meets, so it must cost nothing in normal use. When on (only the harness
//! turns it on), counts go into one bounded, process-global map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Distinct (kind, name) pairs kept; past this, new names are dropped (the
/// counts already kept still grow). Hostile content cannot grow the map
/// without bound by inventing property names.
pub const MAX_ENTRIES: usize = 4096;

/// Longest name kept (longer ones are cut at a character boundary).
pub const MAX_NAME_BYTES: usize = 80;

fn table() -> &'static Mutex<HashMap<(&'static str, String), u64>> {
    static TABLE: OnceLock<Mutex<HashMap<(&'static str, String), u64>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Turns recording on or off (the harness's switch).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Counts one use of an unsupported feature: `kind` is a fixed category
/// (`"css-property"`, `"css-value"`, `"css-at-rule"`, `"css-selector"`,
/// `"js-error"`, `"charset"` …), `name` what was used. A no-op unless
/// enabled.
pub fn record(kind: &'static str, name: &str) {
    if !enabled() {
        return;
    }
    let mut cut = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    let name = name[..cut].to_string();
    let mut table = table().lock().unwrap_or_else(PoisonError::into_inner);
    let at_capacity = table.len() >= MAX_ENTRIES;
    match table.get_mut(&(kind, name.clone())) {
        Some(count) => *count += 1,
        None if !at_capacity => {
            table.insert((kind, name), 1);
        }
        None => {}
    }
}

/// Every count so far, sorted by kind then name.
pub fn snapshot() -> Vec<(&'static str, String, u64)> {
    let table = table().lock().unwrap_or_else(PoisonError::into_inner);
    let mut out: Vec<_> = table.iter().map(|((k, n), c)| (*k, n.clone(), *c)).collect();
    out.sort();
    out
}

/// Clears every count (between pages).
pub fn reset() {
    table().lock().unwrap_or_else(PoisonError::into_inner).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test (the table is process-global; parallel tests would race).
    #[test]
    fn recording_is_off_by_default_bounded_and_resettable() {
        reset();
        record("css-property", "never-counted");
        assert!(snapshot().is_empty(), "off by default");
        set_enabled(true);
        record("css-property", "grid-template-columns");
        record("css-property", "grid-template-columns");
        record("css-value", &"x".repeat(1000));
        let snap = snapshot();
        assert_eq!(snap.iter().find(|(_, n, _)| n == "grid-template-columns").map(|e| e.2), Some(2));
        assert!(snap.iter().all(|(_, n, _)| n.len() <= MAX_NAME_BYTES));
        for i in 0..(MAX_ENTRIES * 2) {
            record("css-property", &format!("p{i}"));
        }
        assert!(snapshot().len() <= MAX_ENTRIES, "bounded");
        record("js-error", "é".repeat(100).as_str()); // cut at a char boundary, no panic
        reset();
        assert!(snapshot().is_empty());
        set_enabled(false);
    }
}
