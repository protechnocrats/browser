# ADR 020 — Frame and power budget: wake on completion, hidden pages, a degrading frame rate

- **Status:** accepted (Milestone 38b, 2026-10-19)
- **Decides:** when a page is given CPU time: what wakes it, what a hidden
  page does, and how an animating page that cannot keep up degrades.
- **Constrains:** `engine` (the page loop, the mailbox, the renderer
  protocol), `network` (a completion callback on fetches), `js` (its
  fetches), `browser_core::Session` (tab visibility), and any future source
  of background work for a page.
- **Supersedes:** the 8 ms fetch polling of Milestone 30 (`POLL_INTERVAL`,
  "a page cannot yet wake itself when a response lands").

## Context

Architecture doc §5 asks that "a page must not be able to peg the GPU/CPU
indefinitely". Three things worked against it (P10):

1. A page waiting for anything in the background — a script `fetch`, an
   image, a web font, a navigation — was ticked every 8 ms until it arrived:
   125 wake-ups a second for a page doing nothing.
2. The engine had no notion of a page nobody can see. A background tab with
   a `requestAnimationFrame` loop or an infinite CSS animation built 60
   frames a second for no one.
3. An animating page whose frames take longer than the frame interval ran
   behind forever, every tick doing a full frame's work, with input queued
   behind it.

## Decision

1. **Background work wakes the page; nothing polls.** `network::Notify` is a
   callback a fetch calls when its result is ready. A page's
   `NetworkContext` carries the page's notifier
   (`NetworkContext::with_notify`), so every fetch made through it —
   stylesheets, images, web fonts, navigations — wakes the page; script
   fetches, made through the page's network *capability* (which the profile
   minted, ADR 010), are given the notifier by `js` explicitly. Image and
   font decodes and the navigation thread notify when they finish. The
   notifier asks the host for a tick (`Mailbox::request_tick`, which any
   thread may call); in process mode the renderer forwards the request as a
   `Published` status with a new wake time. `POLL_INTERVAL` is gone: a page
   with work in flight and nothing else to do is not ticked at all.
2. **Hidden pages are paused, not stopped.** `PageCommand::SetVisible(bool)`.
   A hidden page runs no `requestAnimationFrame` callbacks, advances no CSS
   transition or animation, and builds no frames; it still lands
   navigations, settles fetches and runs the script they call back into, so
   a page that is shown again is up to date. Shown again, it draws at once,
   and its animations restart from that moment (no burst to catch up).
   `Session` hides every tab but the active one. Pages start visible.
3. **An animating page that cannot keep up lowers its frame rate.** The
   animation interval (`FRAME_INTERVAL`, 60 Hz) steps through 1, 2, 4, 8 and
   15 times itself (60, 30, 15, 7.5 and 4 frames a second). An animation
   frame whose work (rAF callbacks, transitions, the frame build), measured
   on the page thread's clock, takes more than half the current interval
   steps down one level, reported once per step
   (`Guard::FrameBudgetExceeded`); 30 animation frames in a row under an
   eighth of the interval step back up. The gap between the two thresholds
   keeps a steady load from oscillating. `FrameStats::animation_interval_ms`
   says which level a frame was drawn at.
4. **Input is never degraded.** A tick with input queued runs at once,
   whatever the animation level: it applies the input and draws a frame for
   it, and runs animation work only if that is due. Degrading the animation
   rate leaves the page thread idle most of the time, which is where input
   lands. `requestAnimationFrame` callbacks were already coalesced to one
   run per animation frame (Milestone 29).

## Consequences

- An idle page — visible or hidden, waiting for a response or not — costs no
  CPU between events. A missed notification would leave a page waiting
  forever rather than 8 ms late; every source of background work has a test
  that the page wakes with no host polling.
- Animation timing on a heavy page depends on the machine (wall-clock work
  time). The frame pipeline itself stays deterministic for a given clock;
  replay (backlog) must record the level changes or run with degradation
  off.
- A background page's CSS animations do not progress while hidden — the
  same as browsers that throttle hidden tabs to zero frames, and visible
  as a jump when shown.

## Alternatives

- **Keep polling, slower** (say every 250 ms): simple and robust against a
  missed notification, but still wakes idle pages forever, and makes every
  load up to 250 ms late.
- **A global "current page" notifier** set per thread and picked up by the
  network layer: no plumbing, but ambient state of exactly the kind ADR 010
  removed from the bindings.
- **Skip off-screen transitions first**, before lowering the frame rate (the
  first step in the original M38 scope): needs, per animated element, its
  box's position in the last layout and a guarantee that the animated
  property cannot move it on screen. Deferred: lowering the rate bounds the
  cost of every animation, on screen or not, and a hidden page now does no
  animation work at all.
  *(Done in the gap pass, 2026-10-21, for the case the guarantee covers:
  transitions of `background-color` — which paints inside its own border
  box — on boxes outside the viewport that no transformed, fixed, sticky or
  scrolling ancestor can move without a new layout. They neither build
  frames nor keep the page awake; a scroll, resize or change brings a frame,
  and the value follows from the clock.)*
