# Milestone 25 — Guardrails & CI hardening

> **Status:** Done, with gaps. **Still open:** the libFuzzer jobs and CI workflows are written but have never run (no `g++` locally, no CI run yet). ~~Two `cargo audit` exceptions for Boa 0.17 expire 2026-12-31~~ — removed by the Boa 0.22 upgrade (M38a).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** make the discipline that already existed non-optional, cover the
two mechanical gaps (`forbid(unsafe_code)` coverage; fuzzing that had
never run), and stop trusting guardrails that have never been seen to
fail. First of the three "guardrails" milestones (M25–M27) of the M25–M38 plan.

## Builds on
`.github/workflows/ci.yml` and `scripts/check-dependency-direction.sh`
(both from the module-split pass), the security-pass regression tests, and
`fuzz/` (one target, `parse_document`, never run).

## In scope (all done)
- `accessibility` forbids unsafe (`Cargo.toml` lint + `#![forbid]`).
  `scripts/check-forbid-unsafe.sh` fails if **any** core crate lacks the
  forbid, **and** fails if a crate under `crates/` is listed in neither
  `scripts/core-crates.txt` nor `scripts/platform-crates.txt` — so a new
  core crate cannot skip the rule by never being added to a list.
  `scripts/test-guardrails.sh` builds a synthetic workspace and proves the
  script fails on the bad cases and passes on the good ones.
- Seven fuzz targets (`parse_document`, `parse_stylesheet`,
  `parse_inline_style`, `decode_auto`, `url_origin`, `cookie_header`,
  `layout_document` — the last runs parse → cascade → layout → display
  list). Bodies live in a `browser_fuzz` library shared by the libFuzzer
  binaries **and** by stable-Rust tests, so the same code runs where no
  C++ toolchain exists.
- Committed seed corpus (`fuzz/generate_seeds.py`): real image fixtures,
  the security-pass hostile inputs (deep `:not()`, extreme nesting, WebP/
  PNG header bombs, oversized font-size, malformed cookies) and per-target
  well-formed examples.
- CI: `fuzz-smoke` (60 s per target on every PR), `fuzz-nightly.yml`
  (10 min per target, uploads crashes and the grown corpus), a
  `supply-chain` job (`cargo audit`, `cargo deny`, exception-sync check),
  the clippy ratchet, the forbid check and the guardrail self-test.
- `deny.toml`: license allow-list; sources; bans that express the
  architecture in dependency terms — `wgpu` only via `gpu_backend`, `winit`
  only via `gpu_backend`/`shell_desktop`, no other browser engine or embedded
  web view (Blink/WebKit/CEF/WebView2/Servo/driver crates), no C TLS stack.
- `scripts/clippy-ratchet.sh` + `scripts/clippy-baseline.txt` (44): CI fails
  if the warning count rises. Counts distinct `(file, line, lint)`, parsed
  from clippy's JSON.
- `scripts/check-advisory-exceptions.sh`: `deny.toml` and `.cargo/audit.toml`
  must list the same advisory ids, and any exception whose reason carries
  `EXPIRES YYYY-MM-DD` fails CI once that date passes.

## Out of scope
Burning down the clippy backlog; OSS-Fuzz onboarding (M32).

## Acceptance criteria
- [x] `accessibility` forbids unsafe; the script fails a synthetic core
      crate that omits it (6 cases in `test-guardrails.sh`, incl. `warn`-only
      and forbid under the wrong table).
- [x] ≥ 6 fuzz targets exist (7) and `cargo fuzz list` discovers all of them.
      **Not verified here:** each running clean for its 60 s smoke budget in
      CI — libFuzzer needs `g++`, absent on the development machine, and the
      workflows have not been executed on GitHub. What *was* run: every seed,
      every structural stress input and 200,000 deterministic mutations
      across all seven targets, on stable (`cd fuzz && cargo test --release
      --no-default-features`).
- [x] A deliberately reintroduced known bug is caught. **First attempt
      failed** (see notes); after strengthening the harness, deleting the
      `:not()` depth cap makes the stress suite abort with a stack overflow,
      and restoring it makes it pass.
- [x] `cargo audit` passes with 4 documented exceptions; `cargo deny check`
      passes (advisories, bans, licenses, sources). The `wgpu` ban was shown
      to fire (temporarily adding `wgpu` to `layout` → `bans FAILED`).
- [x] Clippy baseline committed; the ratchet was shown to fail on an increase
      (and did, once, on Milestone 26's new code — the two warnings were
      fixed, not re-baselined).

## Implementation notes — what the harness found

The point of fuzzing is to find things; it did, before it was ever run in CI.

1. **Unbounded `font-size` aborted the process.** `font-size: 99999999px`
   reached glyph rasterization, which allocates a bitmap ∝ px²; the request
   was ~11 PB and `alloc` aborts (not a panic — `catch_unwind` cannot see
   it). The security pass had fixed the *panic* on a too-wide glyph but its
   own comment said "nothing clamps `font-size`". Fixed in two layers:
   `style` ignores non-finite/negative values and clamps to
   `MAX_FONT_SIZE_PX` (1024); `text` independently sanitizes at the
   rasterizer (`MAX_FONT_PX`), because it must not trust its callers.
2. **Nested `@media` overflowed the stack** (~25,000 levels, a 600 KB
   stylesheet), and copied the block body at every level (also O(n²)). Same
   class as the `:not()` bug, different path. Nesting is now capped
   (`MAX_AT_RULE_NESTING_DEPTH` = 32); deeper blocks are consumed and
   dropped, never recursed into.
3. **The first "prove the harness finds a reintroduced bug" attempt failed.**
   With the `:not()` cap deleted, the 400-deep seed replayed cleanly — the
   original unit test uses 100,000 levels, which a 4 KiB libFuzzer input can
   never reach. Corpus seeds cannot substitute for that; the replay suite now
   generates structural stress inputs at 100,000-deep programmatically. This
   is recorded because it means **coverage-guided fuzzing alone would not
   have found the `:not()` bug either**; the stress inputs are part of the
   safety net, not an extra.
4. **`cargo audit`: 5 vulnerabilities → 2.** Upgraded `rustls` (0.23.31 →
   0.23.45), `time` (0.3.36 → 0.3.55), and unpinned `url` (→ 2.5.8, which
   pulls `idna` 1.x) and `zeroize`, all pins that existed only for the old
   1.75 sandbox toolchain. Remaining: `boa_engine` 0.17 (RUSTSEC-2024-0444)
   and its transitive `fast-float` (RUSTSEC-2025-0003) — fixed only by a
   Boa ≥ 0.19 migration, a project of its own. Both are exceptions with an
   `EXPIRES 2026-12-31`, mechanically enforced. Two unmaintained crates
   (`paste`, `ttf-parser`) are listed with reasons.
5. **A clippy ratchet that undercounts is worse than none.** The first
   version deduplicated on the message text alone (count: 15) and so would
   have let a new instance of an existing lint through; the second parsed
   `--message-format=short`, which cargo does not replay from cache (count:
   0, silently). It now parses JSON, and a parser failure is a hard error
   rather than "zero warnings". Real baseline: 44.
6. `image-webp` rejects a bare VP8X header bomb in its own validation before
   the decode-size cap is reached, so that cap only fires for a well-formed
   animated WebP with a huge canvas. The cap is now a small function
   (`check_decode_dimensions`) tested directly (see M27).

## Known gaps
- `fuzz-smoke`/`fuzz-nightly`/`supply-chain` workflows are unexecuted; their
  commands were run individually here except libFuzzer itself.
- No OSS-Fuzz. Fuzz targets for fonts, `@font-face`, `Content-Type`/header
  parsing and the JS→DOM binding surface are M32's scope.
- `cargo deny` reports duplicate-version warnings (`windows-sys`, `syn`,
  `hashbrown`, …) — informational; not policy.
