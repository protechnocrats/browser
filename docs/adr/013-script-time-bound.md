# ADR 013 — A time bound on runaway pages: the renderer watchdog

- **Status:** accepted (Milestone 38a, 2026-10-19)
- **Decides:** how a page that never finishes its work (a runaway script,
  above all) is stopped, and within what bound; what the Boa upgrade does and
  does not contribute.
- **Constrains:** `engine`'s process host (`process.rs`), anything a page
  thread waits for inside a tick, hosts that rely on `PageCrashed`, and M40's
  `ScriptRuntime` trait (its "interrupt").
- **Supersedes:** "a script execution budget … starting with the Boa upgrade"
  in the original Milestone 38 scope.

## Context

The hardening pass added Boa's loop-iteration limit (10 M per loop) and
recursion limit. A bare `while (true) {}` now stops at the limit (in about
0.1 s on Boa 0.22), but the limit counts per loop *instance*: nested loops
multiply (`for (;;) { for (var i = 0; i < 9e6; i++) {} }` runs for days), and
compute without loops (a long chain of calls) is not counted at all. A
determined page can still occupy its thread, and its renderer's CPU core,
indefinitely (P10).

Two ways to bound it were open:

1. **Interrupt the script inside the engine.** Checked against Boa 0.22.0
   (the version M38a upgrades to): `RuntimeLimits` still offers only loop,
   recursion, stack and backtrace limits. An instruction budget exists, but
   its error (`EngineError::NoInstructionsRemain`) is compiled only with
   Boa's `fuzz` feature. `Script::evaluate_async_with_budget` yields to an
   async executor every *n* instructions, which would let an executor drop a
   runaway top-level script — but it covers top-level script evaluation only,
   not functions called from Rust (event listeners, `requestAnimationFrame`
   callbacks, promise jobs), which is where most of a page's script runs.
2. **Kill the process.** Since ADR 016 every page runs in its own renderer
   process by default, the host never waits on a page (ADR 006), and the
   browser can end a renderer (`RendererControl::terminate`).

## Decision

1. **The browser process watches each renderer.** A renderer *owes* a tick
   from the moment the host sends one until it reports a tick at or after it
   (`ToHost::Published { completed_tick }`). If it owes a tick and has made no
   progress for **10 seconds**, the host:
   - reports `Guard::PageUnresponsive`;
   - marks the page crashed ("the page stopped responding for 10 s; its
     renderer was ended"), which hosts see as `EngineEvent::PageCrashed`;
   - ends the renderer process outright. There is no prompt and no grace
     period; a page that is killed is gone, as a crashed one is.

   "Progress" is a reported tick: each `Published` restarts the clock, so a
   page that is slow but keeps finishing ticks is never killed.
2. **Nothing pauses the clock.** Pausing it while the host owes the renderer
   a font (the one thing a page thread waits for synchronously, during
   layout) was considered and rejected: a compromised renderer could keep a
   font request always outstanding and stop the clock for good. Fetches never
   could pause it: script fetches are asynchronous, so a page that loops
   while a fetch is outstanding is still a runaway.
3. **What a page thread may wait for inside a tick must stay well under the
   bound.** A renderer waits at most 3 s for a font from the host (then falls
   back), down from 10 s; the web-font wait before a first frame
   (`ImageLoading::BeforeFirstFrame`) is capped at 5 s. Navigation loads run
   on their own thread and never count.
4. **The bound is the engine's, adjustable by the host:**
   `engine::UNRESPONSIVE_AFTER` (10 s), `Engine::set_unresponsive_after` for
   hosts and tests. The watchdog checks at least once a second (more often
   for short bounds), so a page is ended between 10 s and 11 s after it
   stopped making progress.
5. **Thread mode is not bounded by this.** A thread cannot be killed safely.
   Pages on threads (`Isolation::Threads`: tests, `app --single-process`)
   keep only Boa's runtime limits. This is stated, not hidden: the default is
   process mode, and process mode is what ships.
6. **The Boa upgrade (0.17 → 0.22) is for the advisories and maintenance,**
   not for the bound. If a later Boa (or another runtime, M40) offers a
   general interrupt, M40's `ScriptRuntime::interrupt` can stop a script and
   keep its page; the watchdog stays as the backstop, since it also catches a
   renderer stuck anywhere else (layout, a decoder, or a compromised
   renderer that ignores interrupts). *(M40, ADR 023 §6: the interface is
   `ScriptRuntime::interrupt_handle`; Boa's runtime returns `None`, the stub
   runtime supports it.)*

## Consequences

- A compromised renderer cannot escape the bound by claiming to be busy with
  something legitimate: nothing the renderer says pauses the clock.
- A legitimate page whose single tick takes more than 10 s of work (laying
  out a pathologically large document, say) is killed. Browsers show an
  "unresponsive page" prompt here; this engine has no prompt UI (tab chrome
  is not planned), so it ends the page, as decided for M38a.
- A suspended machine does not count: `Instant` is monotonic and does not
  advance during suspend on Linux.
- One more thread per process-mode page (the watchdog), sleeping between
  checks.

## Alternatives

- **Renderer-side watchdog** (a thread inside the renderer that aborts when
  script runs too long): finer-grained, but a compromised renderer disables
  it. Could be added later for a better error message; it would not replace
  the host's.
- **Async budgeted evaluation** for top-level scripts: covers only part of
  script execution (see Context), and would reshape the `js` crate just
  before M40 replaces it.
- **A prompt** ("wait or end the page"): needs shell UI that does not exist.
