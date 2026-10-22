# Milestone 38a — Script time bound & Boa upgrade

> **Status:** Done, with gaps (2026-10-19). **Still open:** pages on threads
> (`Isolation::Threads`: tests, `app --single-process`) are not bounded by
> the watchdog — only by Boa's loop and recursion limits (ADR 013 §5); a
> general script interrupt waits for a runtime that has one (M40 built the
> interface, `ScriptRuntime::interrupt_handle`; Boa 0.22 offers none); the
> watchdog was not exercised in the live app on hardware this pass (a real
> sandboxed renderer was, in tests). Decision record:
> `docs/adr/013-script-time-bound.md`. Overall status: `AUDIT.md` · the plan:
> `ROADMAP.md`.

**Goal:** a runaway script cannot hold a page or the browser (P10's script
half), and Boa is current before its two `cargo audit` exceptions expire on
2026-12-31. Split from the original Milestone 38 (the frame and power budget
is M38b).

## What was built

- **The renderer watchdog** (`engine::process`, ADR 013). A renderer *owes*
  a tick from the moment the host sends one until it reports a tick at or
  after it. One that owes a tick and reports no progress for
  `engine::UNRESPONSIVE_AFTER` (10 s) is ended outright: `Guard::
  PageUnresponsive`, the page marked crashed ("the page stopped responding
  for 10 s; its renderer was ended"), the process killed and reaped. Only
  what the host sent and received counts; nothing the renderer says pauses
  the clock. `Engine::set_unresponsive_after` changes the bound for new
  pages (tests use 0.5–1 s). One sleeping thread per process-mode page,
  checking at least once a second.
- **In-tick waits kept under the bound:** a renderer waits at most 3 s for a
  font from the host (was 10 s), then falls back; the web-font wait before a
  first frame (`ImageLoading::BeforeFirstFrame`) is capped at 5 s (it was
  the 10 s subresource deadline).
- **Boa 0.17 → 0.22**, ported, not redesigned (M40 replaces the bindings):
  - `Context` lost its lifetime; `FunctionObjectBuilder` takes the realm;
    global names are `JsString`s; `JsError::to_opaque` became `into_opaque`.
  - **Time:** Boa 0.21 moved the wall clock out of `HostHooks` into a
    `Clock`. The M37 fingerprint policy now sits in both: `PolicyClock`
    coarsens `system_time_millis` (what every `Date` reads), and
    `PolicyHooks::local_timezone_offset_seconds` gives the real offset or
    UTC. Boa's monotonic clock, used only for its own job scheduling, is
    left as it is. The `Date` policy tests pass unchanged, in both modes.
  - **No default features**, as with 0.17. Boa 0.22's defaults add
    `Temporal` (whose `Temporal.Now` reads the timezone through its own
    provider, around the policy), `Float16Array` and `Math.sumPrecise`: new
    script APIs that need a policy before they are exposed.
  - `Context::run_jobs` now returns an error when a promise job fails; it is
    reported like any script error (`report_script_error("promise job")`),
    never dropped.
  - The `indexmap`, `regress` and `time` pins in `js` existed only for an old
    1.75 sandbox toolchain and blocked the upgrade; removed.
- **Advisory exceptions:** RUSTSEC-2024-0444 (Boa, fixed from 0.19) and
  RUSTSEC-2025-0003 (`fast-float` 0.2.0, now gone; Boa uses `fast-float2`
  0.2.4, past its own advisory's fix at 0.2.2) are removed from `deny.toml`
  and `.cargo/audit.toml`. Two exceptions remain, neither with an expiry:
  `paste` (build-time, via wgpu's `metal`) and `ttf-parser` (ADR 009).

Found on the way:
- On Boa 0.22 a bare `while (true) {}` stops at the 10 M loop limit in about
  0.1 s (0.17 took far longer), so it no longer makes a runaway page. The
  watchdog tests use nested loops, which the per-instance limit does not
  stop (ADR 013, Context) — the case the watchdog exists for.
- The host sends a page a new tick while it still owes one whenever input is
  queued, so a slow page under a moving pointer always owes a tick. A first
  test of "slow but alive" did not cover that and passed even with the
  progress reset removed; it now moves the pointer throughout.

## Acceptance criteria

- [x] An infinite-loop page in process mode is killed within 10 s of its
      tick (plus one watchdog interval), reported as `PageCrashed`; other
      pages and the host keep running. `engine/tests/unresponsive.rs`:
      `an_infinite_loop_is_ended_within_the_bound_and_other_pages_carry_on`
      (bound 0.5 s; ended between the bound and three times it; reported
      once; the guard counter moves; another page keeps drawing).
      `renderer/tests/real_processes.rs`:
      `a_runaway_script_has_its_renderer_ended_by_the_watchdog` (a real
      sandboxed renderer: ended, its process gone, the other page still
      navigates).
- [x] A page that is merely busy for less than the bound is not killed:
      `a_page_that_is_slow_but_keeps_finishing_ticks_is_not_ended` (each
      frame busy for half the bound, over four bounds, the pointer moving);
      `an_idle_page_is_never_ended`. Both watchdog rules were checked to be
      load-bearing: with the watchdog disabled the runaway test fails, and
      with the progress reset removed the slow-page test fails.
- [x] The Boa upgrade is done, and both `cargo audit` exceptions are gone.
      `cargo audit`/`cargo deny` are not installed locally (CI runs them);
      instead, every crate the upgrade added was checked against a fresh
      clone of the RustSec database (every locked version is inside its
      advisory's patched range) and against `deny.toml`'s licence list (all
      allowed).
- [x] AddressSanitizer over `js` and `engine` is clean on the new Boa — see
      Verification.

## Verification

Standing Definition of Done: 1,109 tests, 0 failures (up from 1,105: the
four watchdog tests); the 56 corpus goldens unchanged on the new Boa; clippy
unchanged at 43; every guardrail and `test-guardrails.sh` pass
(`check-advisory-exceptions.sh`: 2 exceptions, in sync); the fuzz suite
passes (24 targets; the fuzz workspace's own lockfile moved to Boa 0.22 with
it). No new IPC message, so no new `ipc_decode` seeds.

AddressSanitizer, on Boa 0.22: the CI `sanitizer` job's command (`js` and
`engine` suites, the watchdog tests included, plus the display-list suite),
3 rounds: 0 reports, every run passing. That is the check that matters for
ADR 015: its use-after-free lived in Boa's teardown, and the rule that keeps
it fixed (handles released while the heap is alive) holds on the new
version.

## Not done / open

- **Thread mode is not bounded** by the watchdog (ADR 013 §5).
- **No general script interrupt:** Boa 0.22's instruction budget is
  fuzz-only, and its budgeted async evaluation covers top-level scripts, not
  callbacks (ADR 013). M40's `ScriptRuntime::interrupt_handle` is where one
  goes (ADR 023 §6); Boa's runtime returns `None`.
- ~~**GC handles** are still held outside Boa's heap and released by hand (ADR
  015); traced handles are M40's.~~ — traced since M40 (ADR 023 §5).
- **Not run on the desktop:** the watchdog in the live app (a runaway tab in
  a window). The real sandboxed renderer was ended in tests.
