# ADR 005 — The engine's public API is commands in, events out

- **Status:** accepted (Milestone 29)
- **Decides:** the shape of the one API a host (the desktop shell, a headless test host, a future embedder) uses to drive the engine.
- **Constrains:** Milestone 30 (page threads), the shells, and every future embedder.

## Context

Until Milestone 29 the "engine" was a set of crates that `app` wired together
by hand: `app` built the DOM, owned the `JsEnvironment`, ran the cascade and
layout, hit-tested clicks, moved focus and painted the caret. The shell called
back into three `app` closures (`build_scene(w, h) -> Scene`, `tick() -> bool`,
`handle_input(event)`), all sharing `Rc<RefCell<..>>` state.

Milestone 30 moves each page onto its own thread. Any API in which the host
calls into a page and waits for an answer — `render(viewport) -> Scene`,
`tick(now) -> TickResult` — would then block the UI thread on a page thread,
which is exactly what page threads exist to prevent: a page stuck in a long
script or a big layout would freeze the window.

## Decision

1. **One crate, `engine`, is the only thing a host talks to.** Hosts may not
   depend on `dom`, `html_parser`, `css_parser`, `style`, `layout` or `js`;
   `scripts/check-dependency-direction.sh` enforces it for `app` and every
   `shell_*` crate.
2. **Commands in, events out.** The host sends `PageCommand`s
   (`Load`, `Input`, `Resize`) and receives `EngineEvent`s (`Frame`,
   `NavigationStarted`, `NavigationCommitted`, `NavigationFailed`, and from
   Milestone 30 `PageCrashed`). No call returns data computed by a page.
   `Engine::update(now)` advances the engine and returns the events that are
   ready; `Engine::next_wake()` says when to call it again if nothing else
   happens. In Milestone 29 `update` does the page work inline; in Milestone 30
   it only drains channels — **the host code does not change**.
3. **Every type crossing the API is plain data and `Send`** (checked at
   compile time in the crate): no `Rc`, no `RefCell`, no borrowed data, no
   engine-internal type. `Scene` is already `Send` (its atlas and image buffers
   are `Arc`s).
4. **The clock is injected.** `update(now)` is the only source of time for
   transitions, `@keyframes` and `requestAnimationFrame`; nothing on the
   pipeline path reads the wall clock. A fixed clock gives a reproducible
   frame, which is what deterministic replay (Milestone 38) builds on.
5. **Input arrives normalised.** `EngineInput::Scroll { dx, dy }` is in CSS
   pixels, positive `dy` meaning "move down the document". Natural versus
   traditional scrolling and scroll speed are the *shell's* input transform,
   applied before the engine sees anything; the engine has no scroll
   preference and no notion of wheel "lines".
6. **Pages are addressed by `PageId`**, a plain number — the seam for tabs
   (Milestone 36) and for moving pages across threads or processes later.

## Rejected

- *Synchronous `render(viewport) -> Scene` / `tick(now) -> TickResult`* (the
  first draft of the M25–M38 plan): simplest for a single thread, but it
  would have to change in Milestone 30 or make the UI thread wait on pages.
- *Callbacks from the engine into the host* (the old `build_scene` shape): the
  engine would call host code from whatever thread it runs on.
- *An async (`Future`-based) API*: needs an executor on both sides, which
  ADR 001 and the `network` crate already declined as unnecessary weight.
