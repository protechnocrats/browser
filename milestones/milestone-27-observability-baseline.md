# Milestone 27 — Observability baseline

> **Status:** Done, with gaps. **Still open:** `about:engine` → backlog (it was M38's, and moved out when M38 was split). (The live-window `--trace` path and the `gpu_render` span, unexercised here, were exercised on hardware in M34.)
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** internal tracing and counters *before* the pipeline becomes
multi-threaded (architecture doc §9). Every security control that used to
degrade silently becomes visible — in `app --trace`, and assertable in tests.
Decision record: [`docs/adr/001-observability-facade.md`](../docs/adr/001-observability-facade.md).

## In scope (all done)
- New core crate `diagnostics` (`forbid(unsafe_code)`): re-exports the
  `tracing` facade (`default-features = false`), plus
  - **`Guard`** — a closed enum of 14 controls (HTML/`:not()`/`@media`/
    style/layout/inline/a11y depth caps, response-body and image-dimension
    caps, font-size clamp, too-many-redirects, redirect-scheme refusal,
    cookie rejection, cross-origin block). `diagnostics::guard(g, detail)`
    bumps a per-guard counter **and** emits a WARN event on
    `engine::guard`. A control without a `Guard` is a review-blocking gap.
  - **`metrics`** — relaxed atomics: `frames_built`, a frame-time histogram
    (<4/<8/<16.6/<33.3/<66.6/≥66.6 ms), `style_cache_entries`,
    `layout_box_count`, image-cache hits/misses, glyph-atlas generation
    bumps, fetch and navigation counters, `pending_fetches`, one counter per
    guard. Costly gauges (walking the layout tree) run only under
    `metrics::detailed()`, which only `--trace` enables.
  - **`collector`** (feature-gated, off for core crates) — an in-process
    subscriber: `capture(|| …)` for tests, `Collector::install_global` for
    `--trace`. Bounded (`MAX_RECORDS`); a flooding page cannot grow it.
- `app::frame::build_frame` extracted from `main.rs`'s `build_scene` closure
  (with its helpers) so a frame can be built and traced without a window.
  Spans: `frame` ▸ `cascade`, `transitions`, `animations`, `layout`,
  `focus_outline`, `display_list`, `caret`, `scene_snapshot`; plus
  `parse_html`/`parse_css` in `fetch_page`, and `gpu_render` in
  `shell_desktop`.
- Events: `engine::network` fetch started/finished/failed (URL sanitized:
  no userinfo, query or fragment), `engine::navigation` started/finished/
  failed, and every guard.
- `app --trace`: installs the collector before the startup fetch, prints a
  `startup` report and then one report per frame to stderr. Off by default,
  in which case no subscriber exists and every span/event is a no-op.
- `scripts/check-dependency-direction.sh` now also fails a core crate that
  depends on a tracing subscriber/logging sink or enables `diagnostics`'
  `collector` feature in its normal dependencies; shown to fail on both.

## Out of scope
`about:engine` UI and frame budgeting (M38); per-page counters (M30/M31);
crash-report upload (never — no mandatory telemetry).

## Acceptance criteria
- [x] `app --trace` prints stage timings for a frame of the demo page —
      verified headlessly through the same code path
      (`a_demo_frame_reports_every_pipeline_stage_as_a_nested_span`), e.g.
      `frame 2.8 ms ▸ cascade 1.8 ▸ layout 0.5 ▸ display_list 0.4`. **Not
      verified:** the flag in a live window (no display in the test
      environment).
- [x] Hostile-input tests assert the corresponding event fired: `:not()`,
      `@media`, HTML nesting (exactly one event per document), font-size
      (clamp and reject), pixel cap, a11y depth, redirect scheme, redirect
      loop, cookie domain mismatch, cross-origin block; style/layout depth
      via the process counter (they run on `rayon` workers, where a
      thread-local capture cannot see events). Benign inputs trip nothing.
      Not asserted: the 100 MiB response-body cap (too costly to trigger).
- [x] No core crate has a subscriber dependency
      (`scripts/check-dependency-direction.sh`).
- [x] A traced frame draws exactly what an untraced one does.

## Implementation notes

1. **A test that passed alone failed 24 runs in 25 in parallel — with an
   empty capture.** `tracing` caches per-callsite interest; while exactly one
   dispatcher is registered it rebuilds that cache against the *calling
   thread's* dispatcher only, so a callsite first hit by an unrelated
   subscriber-less thread is cached as "never" and the capturing thread's
   events there vanish. A one-off `rebuild_interest_cache()` did not help (the
   callsite registers mid-test). Fix: `capture` first ensures a permanent
   global no-op sink that answers `Interest::sometimes` and `enabled() ==
   false`, so there are always ≥ 2 dispatchers. A regression test hammers the
   race over 16 callsites: it fails 5/5 with the sink disabled, passes 5/5 with
   it, and `css_parser`'s suite went 1/25 → 25/25.
2. Hand-rolled ~250-line collector instead of `tracing-subscriber`: the
   latter's dependency tree buys nothing this needs, and its convenience makes
   "print from a core crate" one line away — the boundary the ADR keeps hard.
3. Counters are process-global (one engine per process), so tests assert on
   deltas only. Exact per-page counts are M31's requirement.
4. Spans and events emitted on `rayon` workers or background threads are
   invisible to a thread-local `capture`; documented on `capture`.
5. `gpu_render` is reported with the *next* frame's stages, because the
   report is printed when a frame is built. `shell_desktop`'s change compiles
   but the span was not exercised (needs a GPU window).
6. Diagnostics never carry URL secrets (`loggable_url`); a test feeds a URL
   with credentials, a token and a fragment and asserts none appear.

## Known gaps
Per-page counters and exact "zero layout on a scroll-only frame" assertions
(M31); `about:engine` (M38); the `gpu_render` span and the live `--trace`
window path are unexercised.
