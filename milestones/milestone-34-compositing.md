# Milestone 34 — Compositing & GPU depth

> **Status:** Done, with gaps (2026-09-27). **Still open:** layout boxes
> relative to their parent (architecture review A5) and display lists
> rebuilt only in part on a layout change → **M39b** (split from M39; done there, ADR 022); tiled layer
> rasterization (not needed yet). Decision record:
> `docs/adr/017-compositing.md`. Overall status: `AUDIT.md` · the plan:
> `ROADMAP.md`.

**Goal:** the GPU-first pieces architecture doc §5 describes that were still
missing: clipping on the GPU, transforms that render, layers, batching,
bounded GPU memory, damage-limited presentation (P3, P9).

## What was built

- **Structure in the display list.** `PushClip`/`PopClip` (rect + corner
  radius), `PushTransform`/`PopTransform` (a 2×3 matrix, `TransformLayer`)
  and `PushEffect`/`PopEffect` (an opacity group). `rendering::walk` gives
  every renderer each item's world transform and clip; it tolerates stray
  pops (scenes can come from an untrusted renderer). `rendering::Affine`
  and `css_transform` build CSS transforms, pivoted on the box centre.
- **Transforms render** — `rotate()`, `skew()`, `scale()`, `translate()` —
  and apply to the whole subtree (Milestone 14 moved only the box's own
  items). Clips under a rotation are exact, in their own space.
- **Rounded clips.** `overflow: hidden` with `border-radius` clips to the
  rounded inner edge (it was clipped square).
- **Opacity is a group:** the subtree is drawn into a layer and composited
  once at its opacity; overlapping members no longer show through each
  other.
- **Compositor frames.** Elements with `will-change: transform`, an
  animation, or a transition on `transform` get a transform layer even at
  the identity. A frame whose animations change only such transforms updates
  the layers in place — layout boxes and the page display list — with no
  cascade, layout or display-list build (`FrameStats::composited`). A scroll
  container's content is on a **scroll layer**; scrolling it is the same
  kind of update (it used to rebuild the display list). The engine numbers
  scenes (`Scene::sequence`, `damage_base`); the host renumbers renderer
  frames.
- **A new GPU backend path** (`gpu_backend`): shared WGSL (`common.wgsl`)
  with per-instance transform and clip; instance data built on the same walk
  (`instances.rs`); a kept **back buffer** with damage-scissored redraws;
  group layers (up to 16 deep) composited premultiplied (`passes.rs`,
  `composite.wgsl`); a headless mode that reads the back buffer back.
- **Batching.** Clips and transforms never split a draw call. An item joins
  the last batch of its kind if it overlaps nothing painted since (bounded
  look-ahead) — the same pixels, far fewer calls.
- **`ImageStore`:** small images share 2048² atlas pages; a 256 MiB budget
  is kept by LRU eviction, never of what the frame uses (images used to be
  refused once the budget was full).
- **Glyph atlas compaction:** on overflow the glyphs the last frame used are
  repacked (copied) and the rest evicted, instead of clearing everything.
- **Decoding off the page thread:** an arrived image decodes on the worker
  pool; a tick never waits for one (a decoder panic is caught).

Found and fixed on the way:
- `layout_element_sized` grew past the stack contract on rustc 1.98 (debug
  frames of 5.5 KB × 512 nesting levels of flex): the block-flow and
  box-finishing code moved into functions of their own. The contract test
  (`stack_depth.rs`) failed at the previous commit too.
- The accessibility depth-cap tests overflowed the 2 MiB test-harness stack:
  test threads now get a page thread's 16 MiB (`.cargo/config.toml`).
- `page_threads.rs`'s busy handler (3M loop iterations) took 19 s in a debug
  build, past the test's 10 s wait: it is now 1.5 s of wall-clock time.

## Acceptance criteria

| Criterion | Evidence |
|---|---|
| Golden scenes for clip, rotate, opacity and layers | `engine/tests/compositing.rs`: a page with a rounded clip, a rotated subtree, a group, a promoted layer and a scroll container — golden scene plus pixel checks (the corner cut, the rotated corner outside the box, the overlap at one layer's 50%). The corpus goldens were re-blessed after a pixel comparison (below). |
| A transform animation on a promoted layer does no cascade, layout or display-list work | `a_transform_animation_runs_on_the_compositor_and_matches_a_full_build`: `restyled == 0`, layout `Reused`, `display_list_built == false`, `composited == 1`, the right angle each frame, and items equal to a full build from the same layout. Container scrolls: `a_container_scroll_is_a_compositor_update` and the page-list equivalence test. |
| GPU memory bounded under an adversarial page | `thousands_of_images_stay_within_the_budget_and_recent_ones_survive` (4,000 images, small and large, over 200 frames); the glyph atlas's cap tests, plus compaction. |
| Draw calls for an image-heavy page drop against a recorded baseline | `an_image_heavy_page_needs_fewer_draw_calls_than_the_baseline`: **211 → 3** (30 icons + 60 cards with background, thumbnail and caption). On the GPU: 20 images in one page draw in one call (`images_sharing_an_atlas_page_draw_in_one_call`). |
| The M33 paint-only cost re-measured | Below: recovered for scroll, DOM-change and caret frames; the isolated display-list build is 2–4% slower than before M34. |

## Verification

**GPU against CPU, on hardware** (`gpu_matches_raster.rs`, headless on the
AMD Radeon 780M): share of pixels differing by more than 48 per channel —

| Scene | Differing | Largest difference |
|---|---|---|
| rects, a rounded rect, a shadow | 0.00% | 10 |
| a rotated subtree with text | 0.76% | 143 (glyph filtering) |
| a rounded clip around an image | 0.00% | 1 |
| cards batched out of order, one under a later badge | 0.00% | 14 |
| group opacity over overlapping rects | 0.00% | 9 |
| a clip under rotation, in a group, with text | 0.22% | 87 (glyph filtering) |

A damage-only redraw leaves the back buffer byte-identical to a full redraw
(`a_damage_redraw_equals_a_full_redraw`).

**Corpus pixels.** All 56 pages rendered before and after (CPU rasteriser),
compared per pixel: 43 identical; 13 differ in 1–595 pixels, every one
explained — content inside rounded `overflow: hidden` boxes now has rounded
corners; clip edges at fractional positions follow the pixel-centre rule the
GPU uses; groups composite once; rounded clips antialias their straight
edges as rounded fills do. After the scroll-layer change, 56 of 56 were
identical to that verified render. Goldens re-blessed.

**On hardware** (`app --url` a page with the compositing features and a
spinning promoted element; Wayland, Linux 7.2): process mode, 466 GPU frames
in 8 s (58 fps), no GPU error, no guard report. Single-process with
`--trace`: 345 of 348 frames went through `compositor_update` (layout ran
twice, for the first frames); a frame took about 0.1 ms to build and 1.2 ms
of GPU work. The demo page also ran in a window for over ten minutes on the
new path without an error.

## Measurements

Interleaved against the previous commit, same machine, release builds.

`frame_bench` (median frame):

| Page | Frame | Before | M34 |
|---|---|---|---|
| 10k rows | first | 46.0–48.1 ms | 46.6–48.8 ms |
| | scroll | 0.04–0.09 ms | 0.03–0.07 ms |
| | one DOM change | 0.46–0.63 ms | 0.47–0.53 ms |
| | caret/focus | 0.03–0.04 ms | 0.03–0.04 ms |
| 10k + flex/vars | first | 51.7–52.6 ms | 53.1–53.4 ms |

An isolated display-list build of the 10k-row layout (40 builds, median):
10.2–10.4 ms before, 10.6–10.7 ms now (+2–4%). A first version was +30%:
a `HashMap::get_mut` per glyph to record its last use — now a `Cell` behind
a shared lookup. The M33 paint-only cost (+8% on scroll frames, measured
before the performance audit's page display list) is gone: scroll frames
draw from the page list and cost what they did.

**Measured and dropped:** pre-rasterizing a cold build's glyphs in parallel.
Even at 1,880 distinct glyphs it gained nothing (fontdue takes microseconds a
glyph), and scanning a long page's text for them cost up to 3 ms.

Standing Definition of Done: 969 tests, 0 failures (up from 943); clippy
unchanged at 43; every guardrail and its self-test pass; the fuzz replay
passes (20 targets; a new `ipc_decode` seed carries every new item; the
libFuzzer runs need `g++`, run in CI).

## Not done / open

- **A5, layout positions relative to the parent → M39b** (done there; M39 did the DOM
  half). Moving a subtree in
  O(1) also needs `LayoutReuse`'s per-element positions made relative (its
  `carry_over` walks every descendant).
  Today a one-DOM-change frame on 10,000 rows costs 0.5 ms in all.
- **Display lists rebuilt in part** on a layout change (and for sticky
  boxes) → M39b, with stable box identity (done there).
- `transform-origin` other than the centre, 3D transforms, `filter`,
  `mix-blend-mode`, `clip-path` → backlog.
- Tiled layer rasterization; a GPU process → backlog.
