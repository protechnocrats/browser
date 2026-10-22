# Milestone 29 — Engine facade crate

> **Status:** Done. **Still open:** see *Not done / open* below.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** one crate, `engine`, is the only thing a host talks to; the
pipeline wiring and engine-owned behaviour move out of `app`, behind an API
Milestone 30 can put on threads without changing it.

**Builds on:** Milestone 26 (context injection), the `app::Page` seam.
Decision record: `docs/adr/005-engine-api-shape.md`.

## What was built

- **New core crate `engine`**, with a *commands in, events out* API:
  `Engine::new(EngineConfig)`, `create_page(PageParams) -> PageId`,
  `send(PageId, PageCommand)` (`Load(Url | Html)`, `Input(EngineInput)`,
  `Resize(Viewport)`), `update(now) -> Vec<EngineEvent>` (`Frame`,
  `NavigationStarted`, `NavigationCommitted`, `NavigationFailed`),
  `next_wake()`, `close_page`. Every API type is plain data and `Send`
  (a compile-time assertion in `lib.rs`).
- **Moved from `app` into `engine`** (with `git mv`, so history follows):
  `Page`, `fetch_page`, `navigate_async` and the resource helpers
  (`loader.rs`), the frame pipeline, caret, focus outline and
  click-to-position math (`frame.rs`), and — from `main.rs`'s closures —
  hit-testing, click dispatch, tab order, scrolling and navigation starts
  (`input.rs`). All 27 of `app`'s tests moved with the code and pass.
- **`app` is a host**: flags, the demo page (now `app/src/demo.html`),
  and a `ShellHost` that forwards input and hands the newest frame to the
  shell. It depends on `engine`, `shell_desktop` and `diagnostics` only.
- **`shell_desktop` is a presenter**: `run(title, w, h, ScrollSettings,
  impl ShellHost)`. It forwards normalised input and size changes, calls
  `host.update(now)` once per event-loop turn, presents the newest scene,
  and sleeps until `wake_at` (`ControlFlow::WaitUntil`/`Wait`).
- **Scroll is normalised in the shell** (`ScrollSettings { invert, speed,
  line_height }`): the engine sees CSS-pixel deltas with positive `dy` =
  further down. The sign now follows `winit`'s documented convention
  (positive `y` = content moves down); the old code added `y` directly,
  which scrolled the "wrong" way relative to it. `--invert-scroll`
  reverses it. Scrolling is also clamped to the document now (it used to
  run past the end into blank space).
- **One editability rule**: `dom::Element::is_text_editable` /
  `editable_value`. The two copies disagreed — `js` treated every
  `<input>` as a text field, so typing with a checkbox or submit button
  focused rewrote its `value` (regression test added).
- **Clock injection**: `update(now)` is the pipeline's only time source
  (transitions, `@keyframes`, rAF timestamps from the page's time origin).
  `grep -rn "Instant::now" crates/style crates/layout` is empty.
- **`rendering::scene_to_json`**: a deterministic scene serialisation
  (image ids renumbered, coordinates at 2 decimals, pixel hashes), used by
  the golden test and the headless host; it is Milestone 32's golden
  format.
- **`examples/engine_headless`**: load a file or URL, print the first
  frame as JSON.

## Found and fixed on real hardware while verifying

- **Every colour on screen was too light.** `gpu_backend` rendered into the
  surface's first format — sRGB on this machine — while its inputs are
  already sRGB-encoded, so the GPU encoded them twice. Measured before:
  `#1e1e2e` → `(96,96,118)`, `#4ecdc4` → `(150,232,227)` (exactly double
  encoding). A non-sRGB format is now preferred; measured after: `(30,30,46)`
  and `(78,205,196)`. Pre-existing since Milestone 1; found because this
  milestone ran the app and measured pixels instead of eyeballing.
- **Animation ran as fast as the loop spun.** Every event-loop turn ran
  rAF and built a frame: 618 frames in ~3.5 s (~175 fps) with `--trace`.
  Animation work now runs at most once per `FRAME_INTERVAL` (60 Hz, ideal
  cadence without drift) however often a host calls `update`; input still
  gets an immediate frame. Measured after: 287 frames in ~4.8 s.
- The hardening pass's GPU paint-order change (`plan_draw_batches`), which
  was unverified on hardware, is now confirmed: text draws over its
  backgrounds on screen.

## Acceptance criteria

- [x] A second, headless host using only the `engine` crate builds and
      passes: `crates/engine/tests/headless_host.rs` (9 tests, compiled as
      a separate crate — load, render, dump JSON, click, scroll, resize
      with hostile values, background URL navigation, failed navigation,
      determinism, idleness, frame-rate limit) and the `engine_headless`
      example.
- [x] The dependency script fails if a host imports an engine-internal
      crate: `scripts/check-host-boundary.sh`, run by
      `check-dependency-direction.sh`, self-tested in `test-guardrails.sh`
      (fails on a synthetic host → `dom`, passes via `engine`).
- [x] `grep -rn "Instant::now" crates/style crates/layout` is empty.
- [x] Identical rendered output for the demo page before and after the
      move: the snapshot recorded in commit `5d7fe76` (before the move) is
      matched by `engine`'s `the_demo_pages_first_frame_matches_the_pre_move_golden_snapshot`.

Standing Definition of Done: 724 tests, 0 failures (up from 706); clippy
43 (down from 44, baseline lowered); all guardrails pass; ADR 005 written
before the code.

## Deviations from the plan, and why

- The plan sketched `page.render(viewport) -> Scene` and
  `page.tick(now) -> TickResult`. Both return page-computed data, which in
  Milestone 30 would block the UI thread on a page thread; ADR 005 replaces
  them with commands and events so M30 changes the transport, not the API.
- `take_navigation()` became the `Navigation*` events.
- `LoadRequest::Html` does not fetch linked stylesheets/images (only a URL
  load does); the demo page needs none.

## Not done / open

- Interactive input on the real window was not re-exercised by a human in
  this pass (Wayland does not allow synthesised input here); the input
  paths are covered headlessly through the engine API.
- The accessibility tree is still not exposed by the engine API (nothing
  consumes it until an OS bridge exists).
