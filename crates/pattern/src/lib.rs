//! Matching an HTML form control's `pattern` attribute against its value —
//! the one place page-supplied regular expressions are evaluated.
//!
//! ## Why this is its own crate, on a linear-time engine
//!
//! The attribute (`<input pattern="…">`) is attacker-controlled, and it
//! used to be evaluated with a *backtracking* engine (`regress`, the JS
//! flavour). Backtracking engines have exponential worst cases: a page
//! with `pattern="(a+)+$"` and a 24-character value froze the cascade for
//! 0.8 s, 28 characters for 4.7 s — and `:invalid` is re-evaluated every
//! frame, so the freeze was permanent. (Measured in the principles audit.)
//!
//! [`regex`] compiles to automata and guarantees matching time linear in
//! the input, whatever the pattern. That is the zero-trust property
//! (architecture doc §1): no page-controlled input may make the engine
//! spend unbounded time. Compile-time cost is bounded too (pattern length,
//! program size and nesting limits).
//!
//! ## What is deliberately not supported
//!
//! Anything that needs backtracking — backreferences (`\1`) and lookaround
//! (`(?=…)`, `(?<!…)`) — does not compile here. Such a pattern is treated
//! as **absent** (the HTML spec also says an unusable `pattern` is
//! ignored), reported via the `PatternRejected` guard. `\d`/`\w` are
//! Unicode-aware, where JavaScript's are ASCII-only; form patterns are
//! overwhelmingly ASCII, and this only widens what matches.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Longest `pattern` attribute evaluated. Real form patterns are tens of
/// characters; this stops a multi-megabyte pattern from being compiled
/// every frame.
pub const MAX_PATTERN_LEN: usize = 512;

/// Cap on the compiled program's size — bounds compile time and memory
/// for constructs like `(?:a{1000}){1000}`.
const COMPILED_SIZE_LIMIT: usize = 256 * 1024;
/// Cap on the lazily-built DFA cache used while matching.
const DFA_SIZE_LIMIT: usize = 1024 * 1024;
/// Cap on parenthesis nesting (a compile-time stack-depth guard).
const NEST_LIMIT: u32 = 40;

/// Compiled patterns cached per thread (the cascade runs on `rayon`
/// workers, and a per-thread cache needs no lock and no shared state).
/// `None` records "unusable", so a bad pattern is not recompiled — or
/// re-reported — every frame.
const CACHE_CAPACITY: usize = 256;
thread_local! {
    static CACHE: RefCell<HashMap<String, Option<Rc<regex::Regex>>>> = RefCell::new(HashMap::new());
}

fn compile(pattern: &str) -> Option<regex::Regex> {
    if pattern.len() > MAX_PATTERN_LEN {
        diagnostics::guard(diagnostics::Guard::PatternRejected, "pattern attribute longer than the limit; ignored");
        return None;
    }
    // Implicitly anchored, as the HTML spec requires: the WHOLE value must
    // match. The group keeps a top-level `a|b` inside the anchors.
    let anchored = format!("^(?:{pattern})$");
    match regex::RegexBuilder::new(&anchored)
        .size_limit(COMPILED_SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
    {
        Ok(re) => Some(re),
        Err(_) => {
            diagnostics::guard(
                diagnostics::Guard::PatternRejected,
                "pattern attribute is invalid or needs backtracking (backreference/lookaround); ignored",
            );
            None
        }
    }
}

fn compiled(pattern: &str) -> Option<Rc<regex::Regex>> {
    CACHE.with(|cache| {
        if let Some(hit) = cache.borrow().get(pattern) {
            return hit.clone();
        }
        let entry = compile(pattern).map(Rc::new);
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_CAPACITY {
            cache.clear(); // simple and bounded; patterns are few and stable per page
        }
        cache.insert(pattern.to_string(), entry.clone());
        entry
    })
}

/// Whether `value` matches the form-control `pattern` (whole-value match).
///
/// - `Some(true)` / `Some(false)` — the pattern is usable; here is the answer.
/// - `None` — the pattern is unusable (too long, invalid, or needs
///   backtracking); callers treat that as *no pattern constraint*.
///
/// Time is linear in `value.len()` for every usable pattern.
pub fn matches(pattern: &str, value: &str) -> Option<bool> {
    compiled(pattern).map(|re| re.is_match(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics::{collector::capture, Guard};
    use std::time::{Duration, Instant};

    #[test]
    fn ordinary_form_patterns_behave() {
        assert_eq!(matches("[A-Za-z]{3,}", "abc"), Some(true));
        assert_eq!(matches("[A-Za-z]{3,}", "ab"), Some(false));
        assert_eq!(matches(r"\d{3}-\d{4}", "555-1234"), Some(true));
        assert_eq!(matches(r"\d{3}-\d{4}", "555-12345"), Some(false), "the WHOLE value must match, not a substring");
        assert_eq!(matches(r"[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}", "me@example.com"), Some(true));
        assert_eq!(matches("cat|dog", "cat"), Some(true));
        assert_eq!(matches("cat|dog", "catdog"), Some(false), "alternation stays inside the anchors");
        assert_eq!(matches("", ""), Some(true));
    }

    #[test]
    fn catastrophic_backtracking_patterns_finish_fast_on_hostile_input() {
        // Each of these froze the previous backtracking engine for seconds
        // to forever on a value this size; here they are linear.
        let hostile = format!("{}!", "a".repeat(200_000));
        for pattern in ["(a+)+$", "(a*)*$", "(a|aa)+$", "(a|a?)+$", "^(([a-z])+.)+[A-Z]([a-z])+$", "(x+x+)+y"] {
            let started = Instant::now();
            let _ = matches(pattern, &hostile);
            assert!(started.elapsed() < Duration::from_secs(2), "{pattern} took {:?}", started.elapsed());
        }
    }

    #[test]
    fn the_exact_audit_case_is_now_instant() {
        // Measured before the fix: 24 chars = 0.8 s, 28 chars = 4.7 s (per frame).
        let started = Instant::now();
        for n in [24usize, 28, 40, 1000] {
            assert_eq!(matches("(a+)+$", &format!("{}!", "a".repeat(n))), Some(false));
        }
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }

    #[test]
    fn patterns_needing_backtracking_are_ignored_and_reported_not_evaluated() {
        for pattern in [r"(a)\1", "(?=x)x", "(?<!a)b", "(?<=a)b"] {
            let (result, c) = capture(|| matches(pattern, "aa"));
            assert_eq!(result, None, "{pattern}");
            assert!(c.saw_guard(Guard::PatternRejected), "{pattern}");
        }
    }

    #[test]
    fn invalid_and_oversized_patterns_are_bounded_and_reported() {
        let (r, c) = capture(|| matches("[unclosed", "x"));
        assert_eq!(r, None);
        assert!(c.saw_guard(Guard::PatternRejected));

        let (r, c) = capture(|| matches(&"a".repeat(MAX_PATTERN_LEN + 1), "x"));
        assert_eq!(r, None);
        assert!(c.saw_guard(Guard::PatternRejected));

        // Compile-time bombs finish quickly and are refused, not built.
        for bomb in ["(?:a{1000}){1000}", "((((((((((((((((((((((((((((((((((((((((((((a))))))))))))))))))))))))))))))))))))))))))))", "a{10000000}"] {
            let started = Instant::now();
            let _ = matches(bomb, "a");
            assert!(started.elapsed() < Duration::from_secs(2), "{bomb:.30} took {:?}", started.elapsed());
        }
    }

    #[test]
    fn an_unusable_pattern_is_reported_once_not_every_call() {
        // The cache remembers "unusable": the per-frame cascade must not
        // re-report (or recompile) it 60 times a second.
        let (_, c) = capture(|| {
            for _ in 0..100 {
                let _ = matches("(?=never-reported-twice)x", "x");
            }
        });
        assert_eq!(c.guard_events(Guard::PatternRejected).len(), 1);
    }

    #[test]
    fn the_cache_is_bounded() {
        for i in 0..(CACHE_CAPACITY * 3) {
            let _ = matches(&format!("p{i}"), "x");
        }
        CACHE.with(|c| assert!(c.borrow().len() <= CACHE_CAPACITY));
    }

    #[test]
    fn garbage_never_panics() {
        for p in ["", "(", ")", "\\", "[", "]", "{", "*", "+", "?", "\u{0}", "\u{10FFFF}", "(?P<n>a)(?P<n>b)", "\\p{Nope}"] {
            let _ = matches(p, "x");
            let _ = matches(p, "");
        }
    }
}
