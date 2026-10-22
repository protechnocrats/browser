# Milestone 30 — Threading model & page-thread migration

> **Status:** Done. **Still open:** ~~multi-tab (`Session`) → M36~~ — done in M36; ~~a total script-time budget → M38~~ — bounded in process mode by M38a's watchdog (ADR 013); pages on threads still rely on Boa's limits.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** implement the concurrency architecture of the architecture doc §4
while there is still one page to migrate: each page on its own thread,
a GPU thread, message passing only, contained panics.

**Builds on:** Milestone 29 (the message-shaped engine API), Milestone 27
(tracing). Decision records, written and committed (`4d82dd9`) before any
migration code: `docs/adr/006-page-thread-actor-model.md`,
`docs/adr/007-panic-containment.md`.

## What was built

- **Page threads** (`crates/engine/src/page_thread.rs`). Each page's thread
  builds and exclusively owns its `Page` — the `Rc` DOM, the Boa context, the
  caches, the glyph atlas. It sleeps on a condition variable until the host
  posts a tick, applies the queued commands, advances the page to the tick
  time, publishes, and calls the waker. 16 MiB stack (the spawned-thread
  default of 2 MiB is below the 8 MiB main thread the pipeline's recursion
  caps were sized for).
- **The mailbox**, the only state host and page share (`Send + Sync`,
  checked at compile time): a bounded, coalescing inbox (pointer moves and
  resizes replace, scrolls add; merging never crosses a click, key or load);
  the latest requested tick; a latest-wins frame slot; events; and the
  page's status (next wake, last finished tick, crashed).
- **Host side** (`Engine`): `update(now)` collects what pages finished and
  posts ticks to pages that are due, never waiting; `update_blocking(now,
  timeout)` also waits for them (headless hosts, tests); `set_waker` /
  `EngineConfig::waker`; `queued_commands(page)`; `close_page` shuts a page
  down without joining it; dropping the engine shuts all pages down.
- **Validation on receipt** (zero-trust, §4): non-finite coordinates and
  deltas, empty or over-64-byte key names, over-2-MiB URLs and over-100-MiB
  HTML are rejected (a rejected load reports `NavigationFailed`); viewports
  are clamped. Each rejection is an `engine_message_rejected` guard.
- **Panic containment**: the page thread's body runs under `catch_unwind`; a
  panic marks the page crashed, fires the `page_crashed` guard, and becomes
  `EngineEvent::PageCrashed { page, reason }`. `PageCommand::SimulateCrash`
  (a host-only `about:crash`) exercises it. `panic = "unwind"` is required
  and now checked (`scripts/check-panic-strategy.sh`, self-tested).
- **GPU thread** (`shell_desktop`): owns the `wgpu` device and surface and
  draws the newest scene from a latest-wins slot; the UI thread never waits
  for it. If it dies, the window closes with an error instead of hanging.
  The event loop now has a user-event proxy, which is the engine's waker.
- **Cross-thread profile state**: `LocalStorageStore` is `Arc<Mutex<..>>`
  (poison-tolerant): two pages of one origin on different threads share one
  `localStorage` area, as tabs do.
- **Named worker pool**: the shared `rayon` pool is built by `Engine::new`
  with named threads (`worker-N`); created lazily from a page thread, all 16
  workers had inherited the name `page-1`.

## Acceptance criteria

- [x] ADR merged and referenced from the architecture doc §4 (ADR 006, 007).
- [x] No `wgpu`/`winit` in the page-thread crate (`engine` is a core crate;
      `check-dependency-direction.sh`); nothing `Rc` crosses: `Mailbox`,
      `PageSetup` and every API type are asserted `Send` (+`Sync` for the
      shared ones) at compile time.
- [x] Page-thread panic → host survives, page reported crashed once, other
      page unaffected (`a_page_panic_is_contained_to_that_page`).
- [x] 1,000 rapid input events do not grow queues without bound: 2,000
      interleaved moves/scrolls coalesce to 2 queued commands; 5,000 keys are
      capped at `MAX_QUEUED_COMMANDS` (256)
      (`thousands_of_rapid_inputs_cannot_grow_a_pages_inbox_without_bound`,
      plus unit tests on the mailbox).
- [x] Hostile messages (NaN coordinates, a 4 GiB viewport, a 10 MB key
      string, an oversized document) are rejected or clamped, not trusted
      (`hostile_messages_are_rejected_not_trusted`, `validate` unit tests).
- [x] Frame time with the GPU thread separated is no worse than before,
      measured with `app --trace` on this machine (AMD Radeon 780M,
      Wayland), demo page, after warm-up, ~280 frames each:

      | | frame build median | p95 | `gpu_render` median |
      |---|---|---|---|
      | before (M29, single thread) | 0.939 ms | 1.124 ms | 0.750 ms |
      | after (M30, page + GPU threads) | 0.867 ms | 1.033 ms | 0.747 ms |
- [x] Two pages render concurrently in a headless test with independent
      state: while one page's thread is busy in a long click handler, the
      other page's frame arrives and `update()` returns in under 50 ms
      (`two_pages_render_concurrently_and_a_busy_page_blocks_neither_the_host_nor_the_other_page`);
      storage is shared within an origin across threads and not across
      origins.

Also verified on real hardware: the threaded app runs at a steady 60 fps
(288 frames in 5 s), pixels are correct (`#1e1e2e` → `(30,30,46)`), and the
process shows the intended threads: one `page-1`, one `gpu`, 16 `worker-N`.

Standing Definition of Done: 734 tests, 0 failures (up from 724); stable
across 15 repeated runs of the threaded tests and 3 full-suite runs; clippy
unchanged at 43; all guardrails pass (plus the new panic-strategy check).

## Deviations and notes

- Pages are driven by host ticks (ADR 006 §3) rather than their own timers,
  to keep the injected clock. Consequence: `update()`'s results for a tick
  arrive in a later `update()`; `update_blocking` is the synchronous form.
  The Milestone 29 tests moved to it.
- Fetch completion is still discovered by polling (a tick every 8 ms while
  a fetch is in flight); a page cannot yet wake itself when a response
  lands. Cheap, bounded, and a natural refinement when M38 adds budgets.
- Interactive input on the threaded window was not exercised by a human in
  this pass; the input paths are covered headlessly.

## Out of scope (unchanged)

Process isolation, sandboxing, site-per-process; a separate decode/raster
pool beyond the shared `rayon` pool.
