# ADR 006 — One thread per page; the host talks to it only by message

- **Status:** accepted (Milestone 30) — written and reviewed before any migration code, as the plan requires.
- **Decides:** the engine's concurrency architecture (architecture doc §4).
- **Constrains:** every future subsystem that holds page state; tabs (M36); process isolation (later).

## Context

After Milestone 29 every page lives on the UI thread, inside the `Engine`. A
page stuck in a long script, a huge layout or a slow decode freezes the
window: the event loop cannot even process "close". The architecture doc (§4)
asks for per-page worker threads, a dedicated GPU thread, intra-page data
parallelism, and an isolation model that can later become separate processes.

The page's state is `!Send` by construction: Boa's `Context` is `!Send`, and
the DOM is shared between the JS bindings and the pipeline as
`Rc<RefCell<dom::Element>>` (78 `Rc`s in `js` alone).

## Decision

1. **Each page is owned by exactly one thread** (an actor). The page thread
   builds its `Page` itself — DOM, stylesheet, image cache, Boa context,
   transition/animation registries, last layout, glyph atlas — and nothing
   else ever holds a reference to it. `Rc`/`RefCell` stay, and stay correct:
   they are confined to one thread by construction, which the compiler
   checks (a `Page` cannot be moved into another thread's closure).
2. **The host and the page share only three things**, all behind one
   `Arc<PageMailbox>`:
   - an **inbox** of commands (bounded, coalescing — see 5) plus the latest
     requested **tick time**;
   - an **outbox** of events plus a **single frame slot** (latest wins: a
     page that renders faster than the host presents replaces its own
     unpresented frame instead of queueing frames);
   - the page's **status** (next wake time, last completed tick, crashed).
   Everything in them is plain `Send` data (compile-time checked). No
   borrowed reference, `Rc` or engine-internal type crosses.
3. **The host still drives time.** `Engine::update(now)` posts a tick at `now`
   to each page that is due, and collects whatever pages have finished; a page
   thread does work only when it holds a tick. This keeps ADR 005's injected
   clock (a page's time is whatever the host says it is — deterministic
   replay stays possible) and means an idle page's thread is blocked on a
   condition variable, costing nothing.
   `Engine::update_blocking(now, timeout)` additionally waits (bounded) until
   the pages it ticked have finished — the synchronous behaviour a headless
   host or a test wants. The desktop host uses the non-blocking form and a
   **waker** (`EngineConfig::waker`): a page thread calls it after publishing
   results, and the shell turns that into an event-loop wake-up.
4. **A dedicated GPU thread** in `shell_desktop` owns the `wgpu` device and
   surface. The UI thread hands it the newest `Scene` through a latest-wins
   slot (plus the newest size); it never waits for the GPU. Page threads
   never touch `wgpu` (they live in `engine`, a core crate, which the
   dependency script already forbids from depending on it).
5. **Backpressure and coalescing**: the inbox holds at most
   `MAX_QUEUED_COMMANDS`; pointer moves replace a queued pointer move, scroll
   deltas add into a queued scroll, resizes and ticks keep only the latest.
   If the inbox is still full, the oldest coalescable input is dropped and a
   diagnostics guard fires. A slow page therefore costs bounded memory, and
   at most one frame per page is ever in flight.
6. **Zero-trust at the boundary** (§4): the page thread validates every
   command on receipt — viewport finite and clamped, coordinates finite, key
   names and URLs length-bounded, HTML bounded — even though today's only
   sender is our own host. A message from a future, isolated host process is
   exactly as trustworthy as one from a compromised one.
7. **Intra-page data parallelism stays `rayon`**, called from inside the page
   thread (style resolution, intrinsic sizing, image decode). The global
   `rayon` pool is shared by all pages.
8. **Shutdown and cancellation**: closing a page sets a flag and wakes its
   thread, which drops the page — its in-flight fetches and navigation are
   abandoned (their channels disconnect). `Engine` joins nothing on the UI
   thread: a page stuck in a script cannot block closing it.

## Rejected alternatives

- **Shared DOM behind `Arc<Mutex<..>>`** so any thread could touch any page:
  trades a compile-time guarantee for runtime locks — contention, deadlock
  risk, and "thread-safe by convention", which §1 explicitly rejects.
- **A process per page now**: the real isolation boundary, and where this is
  heading, but it needs IPC serialisation, sandboxing and a broker first. The
  mailbox design keeps it a transport swap: every message is plain data.
- **An async runtime (tokio) with pages as tasks**: a page does long,
  CPU-bound, non-yielding work (layout, a script); on a task scheduler that
  starves other pages or needs `spawn_blocking` for everything. Threads are
  the honest model, and `std` is enough (ADR 001's minimal-dependency stance).
- **Pages driving their own clock** (timers inside the page thread): simpler
  wake-ups, but the frame clock would no longer be injected.

## Consequences

- Events from a tick arrive after the tick, not inside the same `update` call;
  hosts that need them synchronously use `update_blocking`.
- A panic in a page thread is contained to that page: ADR 007.
- Each page thread gets an explicit 16 MiB stack (the platform default for a
  spawned thread is 2 MiB, less than the 8 MiB main thread every recursion
  bound in the pipeline was sized against).
