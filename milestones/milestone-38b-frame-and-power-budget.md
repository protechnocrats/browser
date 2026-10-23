# Milestone 38b — Frame and power budget

> **Status:** Done, with gaps (2026-10-19). **Still open:** ~~off-screen
> transitions are not skipped~~ (done in the gap pass, for `background-color`
> transitions — the one transitioned property that cannot move a box; ADR
> 020, amended); ~~the window itself is not a
> visibility source~~ (done in the gap pass: covered or minimised hides the
> active tab; not yet seen on a desktop); the budget was not exercised in the live app on the
> desktop this pass. Decision record: `docs/adr/020-frame-and-power-budget.md`.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** "a page must not be able to peg the GPU or CPU indefinitely" as an
enforced property for frames and hidden pages — P10's remaining half, after
M38a bounded runaway scripts.

## What was built

- **Pages are woken, never polled** (ADR 020 §1). `network::Notify` is
  called when a fetch's result is ready. A page's `NetworkContext` carries
  the page's notifier (`NetworkContext::with_notify`), so its stylesheets,
  images, web fonts and navigations wake it; script `fetch` and XHR, made
  through the page's network capability, get it from `js`
  (`JsEnvironment::set_wake`, `NetworkContext::fetch_async_notify`). Image
  and font decodes and the navigation thread notify when they finish.
  `Mailbox::request_tick` (any thread) asks the host for a tick; one made
  during a tick is kept for that tick's report. The renderer forwards a new
  wake time even when its tick has not changed. `POLL_INTERVAL` (8 ms) is
  gone.
- **Hidden pages are paused** (§2). `PageCommand::SetVisible(bool)`: hidden,
  a page runs no rAF callbacks, advances no CSS animation and builds no
  frames, but lands navigations and settles fetches (their script runs).
  Shown, it draws at once; animations restart from then. It is never dropped
  from a full inbox. Renderer protocol: command tag 6 with a strict 0/1 flag.
  `Session` hides every tab but the active one.
- **A degrading frame rate** (§3). An animation frame whose work — rAF
  callbacks, transitions and animations, the frame build, on the page
  thread's clock — takes more than half the interval steps the rate down:
  60, 30, 15, 7.5, 4 frames a second. Thirty frames in a row under an eighth
  of the interval step it back up. `Guard::FrameBudgetExceeded` reports each
  step down (39 guards); `FrameStats::animation_interval_ms` (on the wire
  too) says which level a frame was drawn at.
- **Input is not degraded** (§4): a tick with input runs at once and does
  animation work only if that is due.

Found and fixed on the way:
- **The host spun while a tick was posted.** `Engine::next_wake` said "now"
  whenever commands were queued, even with a tick already posted that would
  take them, so a host called `update` in a tight loop until the page thread
  picked the tick up — thousands of calls at each page start and input
  (2,579 in one test). It now waits for the page's wake-up.
- **The watchdog could be held off by repeated reports.** Forwarding wake-ups
  means a renderer now sends reports with an unchanged tick; under M38a's
  rule each would have reset the watchdog's clock. Only a newer finished
  tick counts as progress now (ADR 013).
- Two tests pinned the polling contract ("an image in flight keeps the page
  polling", "a navigation in flight needs polling"); they now assert the
  opposite, and the navigation test drives the engine only from its waker.
- A `Session` test opened two tabs back to back; the first is now hidden
  before it ever draws, so it has no frame to show when switched back to.
  That is the intended behaviour (a background tab draws when shown); the
  test now lets the first tab draw before opening the second, and checks the
  hidden tab draws nothing.

## Acceptance criteria

- [x] **A synthetic heavy page holds frame pacing by degrading, without
      adding input latency. Counters assert this.**
      `engine/tests/frame_budget.rs`: a page with 30 ms of script per frame
      settles at `animation_interval_ms == 67` and draws 10–18 frames a
      second, in thread and process mode, with at least two
      `guard.frame_budget_exceeded` steps; a click on it is drawn in under
      60 ms while degraded (the frame carrying it still reports 67); a page
      whose load ends returns to 17; a light page never leaves 17. With the
      budget disabled, the four tests that depend on it fail.
- [x] **A hidden page's CPU use falls to near zero, as measured.**
      `renderer/tests/real_processes.rs`:
      `a_hidden_pages_renderer_uses_almost_no_cpu` reads a real sandboxed
      renderer's CPU time from `/proc/<pid>/stat`: a page spending about 4 ms
      of script per animation frame used **44 clock ticks in 2 s visible
      (≈ 22% of a core) and 0 hidden**. `engine/tests/visibility.rs`: hidden,
      an animating page draws nothing and asks the host for at most two
      updates in 600 ms; its fetches still settle and the first frame after
      showing reflects them (thread and process mode; all fail when
      `SetVisible` is ignored).
- [x] Wake on completion, which hidden pages depend on:
      `engine/tests/wake_on_completion.rs` — the navigation, a script
      `fetch`, an XHR, an image and a web font each wake a page that the host
      updates only when woken (thread and process mode); with the fetch
      notification removed, nothing completes.

## Verification

Standing Definition of Done: 1,121 tests, 0 failures (up from 1,109); clippy
unchanged at 43; every guardrail and `test-guardrails.sh` pass; the fuzz
suite passes (24 targets; `ipc_decode` seeds `15_set_hidden` and
`16_visibility_not_a_bool` added, the three frame seeds regenerated for the
new field and checked byte for byte against `generate_seeds.py`). AddressSanitizer: the
CI `sanitizer` command, 3 rounds, 0 reports. One round had a test failure,
not a memory error: the 60 Hz cadence test drives a simulated clock, and
sanitized demo frames take more than half of 16.7 ms of real time, so the
budget halved the rate (31 frames instead of about 60) — the design (ADR 020,
Consequences). The CI job now skips that test, with the stack-depth and
loop-limit tests it already skipped for the same reason; it passes in the
plain release run.

## Measurements

`frame_bench` (median frame), three interleaved rounds against M38a
(the M38a build), same machine:

| Page | Frame | M38a | M38b |
|---|---|---|---|
| demo | first | 3.3–4.7 ms | 3.4–4.0 ms |
| | scroll / DOM change / caret | 0.20–0.22 / 0.14–0.19 / 0.14–0.19 ms | 0.16–0.17 / 0.14–0.15 / 0.14–0.15 ms |
| 10k rows | first | 63.9–67.2 ms | 61.9–63.2 ms |
| | scroll / DOM change / caret | 0.03–0.04 / 0.77–1.10 / 0.02 ms | 0.03–0.04 / 0.58–0.65 / 0.02 ms |
| 10k + flex/vars | first | 71.0–72.3 ms | 70.6–76.9 ms |

No frame got slower. Several got faster; the likely reason is the host no
longer spinning on `update` beside the page thread while a tick is posted,
but that was not isolated.

## Not done / open

- ~~**Off-screen transitions are not skipped**~~ — closed in the gap pass
  (2026-10-21): a page whose transitions in flight all animate
  `background-color` on boxes outside the viewport builds no frames for
  them and sleeps (`layout::LayoutReuse::still_rect_of` gives the box only
  when no transformed, fixed, sticky or scrolling ancestor can move it
  without a new layout). Measured in `visibility.rs`: 0 frames in 600 ms
  against 36 without the skip; scrolled into view, the box shows the
  colour its clock says. Transitions of `transform`, `width`, `height` and
  `margin` can move boxes on screen and are not skipped; `@keyframes`
  animations are not either.
- ~~**The window is not a visibility source**~~ — closed in the gap pass
  (2026-10-21): `shell_desktop` forwards winit's `Occluded` and a zero size
  (minimised) as `ShellHost::window_visible`; `app` passes it to
  `Session::set_window_visible`, which hides the active tab (and starts
  tabs opened meanwhile hidden). Tested in `Session` and the shell's
  tracker; whether a platform reports `Occluded` (winit documents it for
  some, not all) is not yet observed on a desktop 🚫.
- **Not run on the desktop:** the budget and hidden tabs in the live app.
  The CPU measurement used a real sandboxed renderer.
- Thermal and power hooks, `about:engine` and replay were moved to the
  backlog when M38 was split.
