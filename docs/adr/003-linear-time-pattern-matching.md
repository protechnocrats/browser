# ADR 003 — Form `pattern` attributes are matched by a linear-time engine

- **Status:** accepted (hardening pass after Milestone 27)
- **Decides:** which regular-expression engine evaluates page-supplied patterns.
- **Constrains:** `style` (`:valid`/`:invalid`), `js` (submit validation), anything that later evaluates a page-supplied regex.

## Context

`<input pattern="…">` is attacker-controlled, and it was evaluated with `regress`,
a *backtracking* JavaScript-flavour engine. Backtracking engines have exponential
worst cases, and the principles audit measured it: `pattern="(a+)+$"` with a
24-character value froze the cascade for 0.8 s, 28 characters for 4.7 s — and
`:invalid` is re-evaluated on every frame, so one attribute was a permanent
freeze. That violates zero-trust (architecture doc §1): no page-controlled input
may make the engine spend unbounded time.

## Decision

1. **Match with Rust's `regex` crate, behind a small `pattern` core crate.** It
   compiles to automata; matching time is *linear in the input* whatever the
   pattern. Compile time and memory are bounded too (`size_limit`,
   `dfa_size_limit`, `nest_limit`), and patterns over 512 bytes are refused.
2. **One implementation.** `style` and `js` each had a private copy; both now call
   `pattern::matches`.
3. **An unusable pattern is treated as absent**, and reported once
   (`Guard::PatternRejected`). That covers invalid syntax, oversized patterns, and
   constructs that *need* backtracking — backreferences and lookaround. The HTML
   spec already says an unusable `pattern` is ignored.
4. **Per-thread compile cache** (256 entries), because the cascade runs on `rayon`
   workers and a per-thread cache needs no lock and no shared state; it also
   remembers "unusable" so a bad pattern is not recompiled or re-reported every
   frame.

## Known divergence

`\d`, `\w`, `\s` are Unicode-aware in `regex` where JavaScript's are ASCII-only, and
lookahead/backreferences are unsupported. Form patterns are overwhelmingly simple
ASCII; the divergence only widens what matches, and unsupported patterns fail
*open* (no constraint) rather than wedge the page.

## Dependency assessment: `regex` 1.x

| | |
|---|---|
| Purpose | Linear-time regular-expression matching. |
| Size | `regex`, `regex-automata`, `regex-syntax`, `aho-corasick`, `memchr` (already present). Default features trimmed. |
| `unsafe` | Present inside `regex-automata`/`memchr` (performance code, widely audited); our crates remain `forbid(unsafe_code)`. |
| License | MIT OR Apache-2.0 (allowed by `deny.toml`). |
| Maintenance | rust-lang; very active. |
| Replaceable | Yes — only the `pattern` crate touches it. |
| In core crates | Only via `pattern`. |

## Alternatives rejected

- **Keep `regress`, add a step/time limit.** `regress` exposes no interrupt or step
  budget, and a watchdog thread cannot stop a spinning matcher in safe Rust.
- **Detect "evil" patterns with a heuristic** (nested quantifiers). Incomplete —
  `(a|aa)+$` has no nested quantifier and is still exponential — and a filter a
  page can probe. A linear-time guarantee is structural.
- **Memoise results and accept the first freeze.** Turns a permanent freeze into a
  one-time one; still a freeze.
