# Performance-audit fixes — time complexity, concurrency, and a heap use-after-free

> **Status:** Done (2026-09-26, before Milestone 34). **Still open:** see
> *Not done / open* at the end; each item has a home in `ROADMAP.md` §4.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** fix every finding of a code audit for time complexity, concurrency
and parallelism, and make each fix one a later change cannot silently undo — a
counter-bounded test, an equivalence oracle, a guardrail or a fuzz stress
input per finding. Working on it found a heap use-after-free in Boa's teardown
(P12 in `ROADMAP.md` §3), fixed here too (ADR 015).

## What the audit found, and what happened

| # | Finding | Fix | What keeps it fixed |
|---|---|---|---|
| 1 | **Selector matching backtracked exponentially.** `.x div div div div div span` (about 50 bytes) against a 100-deep document took **11.4 s** for one element; 200 deep ran past 100 s. No cap on compounds per selector. A page could freeze its thread. | `style::selectors::PrefixMatcher`: selectors with two or more combinators are matched by dynamic programming over (compound, ancestor or sibling), each answered once: O(compounds × (ancestors + siblings)). `css_parser::MAX_SELECTOR_COMPOUNDS` = 64 (a longer selector is rejected, `Guard::CssSelectorTooLong`). The `::before`/`::after` match no longer clones the selector. | Equivalence with the old backtracking matcher over 20,000 random selectors. Compound-test budgets panic at once if matching stops being linear (verified: the old matcher fails them immediately). Three fuzz stress inputs (512-deep tree with 60 compounds, mixed `>`, 10k-sibling `~` chain). |
| 2a | Every frame — a scroll, a caret move — built display items for the **whole** layout tree, shaping every glyph run through the atlas, then culled. | `rendering::PageDisplayList`: built once per layout in document space, bucketed in 256 px bands; a scroll, caret or focus frame shifts and culls only the bands the viewport overlaps. Fixed boxes are kept apart; a page with a sticky box, or a build that overflowed the glyph atlas, builds per frame. The builder writes into one vector (no per-box and per-clip copies). | `integration_tests/tests/page_display_list.rs`: the list equals a per-frame build at ten scroll offsets, with fixed boxes, clips, a scrolled container, opacity, transforms and hidden boxes (verified to fail if fixed items are scrolled); a frame examines under a fifth of a long page's items. `FrameStats::display_list_built` is asserted false for scroll and focus frames. |
| 2b | Any page with a flex container, table, grid, float or inline-block **re-measured the whole page's text** (intrinsic sizing) on every layout frame. | `IntrinsicCache` lives in `LayoutReuse` across frames; only dirty elements and their ancestors are re-measured. Finding changed images and newly-needed sizing walks the dirty set, not the page. | `a_change_on_a_flex_page_re_measures_only_its_ancestor_chain` (≤ 2 of 2,000 elements, via `LayoutReuse::intrinsic_measured`); the incremental-vs-fresh layout oracle (verified to fail when stale sizes are reused). |
| 2c | Every `createElement` handle access scanned every detached subtree: O(N²) for N created elements; never-attached elements lived for the page's life. | `js::detached::DetachedForest`: O(1) root lookup, per-node paths; a subtree no script handle can reach is freed when the collector drops its last handle. Appending an element into its own descendant is refused. | Element-visit counter bound (3,000 elements: < 60,000 visits; the old scan measured 9,041,983); a GC test that 20,000 dropped elements are freed. |
| 2d | The rule index was rebuilt on every mutation frame. | Built once per stylesheet in `StyleEngine` (with the `@keyframes` map, which was cloned every animated frame). | `the_rule_index_is_built_once_per_stylesheet` (thread-local build counter). |
| 3 | Per-element cascade costs: custom properties cloned into every element; `var()` substitution allocated for every declaration; inline styles re-parsed every restyle; every worker took one global lock twice per element. | Custom properties are an `Arc` shared copy-on-write; substitution borrows when there is no `var(`; parsed inline styles are cached by text (bounded at 8,192); results go to per-worker shards; single-child levels skip the task pool. | `custom_properties_are_shared_not_copied` (pointer equality); `the_inline_style_cache_is_reused_and_bounded`; the 10,000-sequence incremental restyle oracle (it writes inline styles). |
| 4 | Per-frame copies: the whole display list cloned for the damage diff; the atlas copied (up to 2 MiB) by copy-on-write whenever a glyph was added while a scene held it; the whole atlas re-uploaded; three GPU buffers created per frame; word text cloned into every box; the layout tree counted every frame. | `Scene::items` is an `Arc` shared with the frame memo. `text::AtlasPixels`: the atlas in 64-row bands, each shared copy-on-write, so a write copies one band. `gpu_backend::uploads`: only bands whose pointer changed are uploaded (correct even when scenes are skipped); instance buffers are kept and grown geometrically. Word text is moved; boxes are counted only when reported. | `packing_while_a_snapshot_is_held_copies_only_the_band_it_writes`; `only_changed_bands_are_uploaded_again`, `a_skipped_scene_is_caught_up_…`; `instance_buffers_grow_geometrically_…`. |
| 5a | One OS thread per fetch, unbounded: 500 images, 500 threads and connections. | `network::pool::FetchPool`: 16 workers, 6 per host, 4,096 queued; past that a fetch fails (`Guard::FetchQueueFull`). | Pool tests: per-host and total bounds under a 300-request burst, one slow host doesn't block others, a full queue refuses, a panicking job keeps its worker. |
| 5b | Page images decoded one after another; each subresource had its own 10 s deadline (N slow ones waited N × 10 s), found by 5 ms sleep-polling. | Parallel decode (`rayon`) into the cache; one deadline for all subresources; `FetchHandle::wait_until` blocks on the result. | `a_page_load_waits_for_its_images_all_together_and_decodes_what_arrived` (four images, three stalled: returns at the one deadline). |
| 5c | The first frame waited for every image. | `ImageLoading::Progressive` (the default): a page is shown once its document and stylesheets load; each image is decoded and laid out when it arrives. `BeforeFirstFrame` keeps the old behavior for hosts that compare frames (the conformance corpus, whose goldens stay deterministic). | `tests/progressive_images.rs`: committed and drawn while the server holds the image back; drawn (at its size) once released; `BeforeFirstFrame` waits. |
| 6 | Text measurement asked the font for each character's metrics every time. | `Font` keeps an ASCII advance table per size (exact; bounded at 64 sizes). | `cached_advances_match_the_font_exactly_and_stay_bounded`. |

## Found on the way: a heap use-after-free in Boa's teardown (P12)

The engine's unit tests aborted with `malloc(): corrupted double-linked list`
in 16 of 300 runs on the unmodified code (plus one segfault), and in 0 of 120
with one test thread. AddressSanitizer (nightly, `-Zsanitizer=address`)
reported a heap use-after-free on its first run: at thread exit Boa frees its
heap in no particular order, and the `requestAnimationFrame` callback list —
GC handles held in an `Rc` that a native closure captures untraced
(`#[unsafe_ignore_trace]`) — was dropped after the objects it pointed to. Event
listeners and pending `fetch`/XHR requests are held the same way. Reachable
from any page that registers a callback, when its thread ends.

Fixed (ADR 015): `JsEnvironment`'s `Drop` releases all four while the heap is
alive, and a page's script garbage is collected when the page goes.
`scripts/check-gc-handles.sh` fails the build on an untraced capture holding a
handle that `Drop` does not release (self-tested); CI's new `sanitizer` job
runs the `js`, `engine` and display-list suites under AddressSanitizer three
times per change. After the fix: 0 sanitizer reports in 125 local runs of the
engine suites and 3 of the CI command; 0 failures in 100 runs of the engine
unit tests (it had been 26 in 300).

A second, unrelated flake was fixed on the way: one test read a process-wide
gauge that tests running in parallel wrote.
`diagnostics::metrics::detailed_on_this_thread` scopes detailed metrics to
one thread.

## Measurements

`cargo run -p engine --release --example frame_bench` (median frame, 800 × 600,
public API), against the unmodified commit built in a separate worktree, three
interleaved rounds each; ranges are across rounds (this machine is noisy). The
new page is the 10k-row page plus a flex toolbar and 200 custom properties on
`:root`.

| Page | Frame | Before | After |
|---|---|---|---|
| 10k rows | first | 65–74 ms | 56–66 ms |
| | scroll | 4.3–4.6 ms | 0.03–0.05 ms |
| | one DOM change | 4.6–7.1 ms | 0.6–2.7 ms |
| | caret/focus | 4.1–4.5 ms | 0.02–0.03 ms |
| 10k + flex/vars | first | 96–114 ms | 62–76 ms |
| | scroll | 7.1–7.4 ms | 0.03–0.04 ms |
| | one DOM change | 7.7–7.9 ms | 0.5–2.3 ms |
| | caret/focus | 6.2–7.6 ms | 0.02–0.04 ms |
| demo | all | unchanged (within noise) | |

The scroll and caret numbers are frames drawn from the page's display list; the
first frame after a layout change still builds it (O(page)). Measured on
hardware (AMD Radeon 780M, Wayland): the app on a 2,000-row page with an image,
a fixed header and a flex box drew 218 frames in 8 s, with no panic, GPU error
or guard event; its display-list stage had a median of 0.03 ms; the page
committed before its image finished loading.

Standing Definition of Done: 924 tests, 0 failures (up from 895); clippy
unchanged at 43; every guardrail and its self-test pass (plus the new
`check-gc-handles.sh`); the fuzz replay/stress suite passes (19 targets, three
new stress inputs). No new parser: no new fuzz target. No new external
dependency.

## Not done / open

- A frame whose layout changed still builds the whole display list, and a
  page with a sticky box, or a scroll container being scrolled, builds it every
  frame — incremental display lists were M34's; M34 made container scrolls
  and transform animations in-place layer updates, and partial rebuilds on
  a layout change moved to M39, then M39b (done there; M39c patches
  size-keeping changes in place).
- Layout reuse still moves a reused subtree box by box (absolute
  coordinates; M31's note).
- Each element with element children copies its ancestor list for its
  children: O(depth) per element, bounded by `MAX_STYLE_DEPTH` (512).
- ~~`getElementById` builds a new wrapper (about 20 function objects) per
  call~~ — since M40 one wrapper per node, a prototype object (counted:
  10,000 calls, one wrapper); inline-style `url()` images are never fetched
  (either loading mode).
- ~~GC handles are still held outside Boa's heap (released on `Drop`); making
  them traced is for the Boa upgrade (M38, ADR 013).~~ — traced since M40
  (ADR 023 §5); `gc-handle-captures.txt` is empty.
