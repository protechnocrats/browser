# ADR 017 — Compositing: structure in the display list, layers on the GPU

- **Status:** accepted (Milestone 34, 2026-09-27)
- **Decides:** how clips, transforms and opacity reach the renderers; what a
  compositor frame is; how the GPU backend draws, batches and bounds memory;
  what the glyph atlas keeps when it is full.
- **Constrains:** `rendering` (display list, walk, batching, page list),
  `gpu_backend`, `engine` (frame building, IPC), `style` (animation
  report), `text` (atlas).
- **Supersedes:** Milestone 33's per-item clipping and fading on the CPU, and
  Milestone 14's own-items-only transforms.

## Context

Until Milestone 34 the display list was flat. A clip cut each item's geometry
(rects, image texture coordinates, glyph texels) on the CPU; `opacity`
multiplied every item's alpha, so overlapping members of a faded subtree
showed through each other; `transform` moved only a box's own items and could
not rotate or skew. Every image was its own texture and its own draw call,
image memory was refused past a budget instead of evicted, a full glyph atlas
was cleared entirely, and every frame redrew the whole window. A scroll
container's scroll, and any transform animation, rebuilt the display list.

## Decision

1. **Structure is in the display list.** `PushClip`/`PopClip` (a rect and a
   corner radius, in the current space), `PushTransform`/`PopTransform` (a
   2×3 matrix, `TransformLayer`) and `PushEffect`/`PopEffect` (a group at an
   opacity) bracket what they apply to. Items stay in document coordinates.
   A transform applies to the box and its whole subtree, pivoted on the box
   centre (`transform-origin: center`, the only origin supported).
2. **One walk.** `rendering::walk` turns the markers into, per drawing item,
   its world transform and its clip, plus group begin/end steps. The CPU
   rasteriser, the pass planner and the GPU instance builder all use it, so
   they cannot disagree. It tolerates stray pops and closes open groups: a
   scene can come from an untrusted renderer process.
3. **Clips are per pixel, in both renderers.** An axis-aligned clip is a
   world rectangle with a corner radius; a clip under rotation is also kept
   exactly in its own space (`LocalClip`). Coverage is taken at pixel
   centres, with a one-pixel ramp on rounded edges — the same function in
   `walk::Clip::coverage` and `common.wgsl`.
4. **Transformed subtrees are culled, banded and damaged as units**, by the
   layer's bounds (`effective_bounds`). A layer's bounds are its content's
   bounds through its matrix; a scroll layer's are also cut to its clip, so a
   long scrolled list off screen costs nothing.
5. **Opacity is a group.** The GPU draws a group into a layer (a target the
   size of the back buffer, one per nesting depth, at most 16 — deeper
   groups draw into their parent) and composites it at its opacity,
   premultiplied; the CPU rasteriser does the same with its layer stack.
6. **Compositor frames.** An element whose transform may change on its own —
   `will-change: transform`, an animation, a transition on `transform` — gets
   a layer even at the identity (`BoxExtras::promoted`). When an animation
   frame changes only such transforms (`style::AnimationReport::composited`),
   the engine updates the layout boxes' transforms and the page display
   list's layer matrices in place (`update_transform_layers`), re-derives
   layer bounds and bands, and draws: no cascade, layout or display-list
   build (`FrameStats::composited`). A scroll container's content sits on a
   **scroll layer** whose matrix is the scroll offset
   (`TransformLayer::scroll`); a container scroll is the same kind of update
   (`update_scroll_layers`).
7. **The GPU draws into a kept back buffer**, then copies it to the window.
   Every scene carries a `sequence` and the `damage_base` its damage is
   relative to; when the back buffer holds that frame (and the size,
   viewport and background are unchanged), only the damage rectangle is
   redrawn (scissored, groups included). Anything else — a skipped frame,
   another page's frame, an image that could not be drawn last time — is a
   full redraw. The host renumbers frames that come from a renderer process.
8. **Batching.** Per-instance transform and clip mean clips and transforms
   never split a draw call; only a group boundary does. An item joins the
   last batch of its kind when it overlaps nothing painted since (bounded
   look-ahead), which paints the same pixels; each family's instances stay
   one contiguous range per batch.
9. **Images live in an `ImageStore`**: images up to 256 px a side share
   2048² atlas pages (so they batch), larger ones get a texture of their
   own; the byte budget (256 MiB) is kept by evicting the least recently
   used — never what the current frame uses. Only when nothing can be
   evicted is an image not drawn (reported once).
10. **The glyph atlas compacts instead of clearing.** Each glyph records the
    last frame that used it; on overflow the next build repacks the glyphs
    the last frame used (copied, not rasterized again) and evicts the rest.
    If one frame's glyphs alone overflow the atlas, it is cleared, and only
    every 60 frames, as before.
11. **No subpixel glyph positioning.** Glyphs are cached per (character,
    rounded pixel size) and drawn at fractional positions with bilinear
    sampling on the GPU (nearest on the CPU). Subpixel variants would
    multiply atlas use by the number of phases for a gain on low-DPI screens
    only; revisit with M35's shaper.
12. **Decoding is off the page thread.** An arrived image is decoded on the
    worker pool; a tick collects finished decodes and never waits for one.
    A decoder panic is caught and treated as "does not decode" (a panic on a
    pool thread would abort the process). Glyph rasterization was measured
    and stays inline: pre-rasterizing a cold build's glyphs in parallel
    gained nothing (fontdue takes microseconds per glyph) and scanning the
    page's text for them cost up to 3 ms on 10,000 rows.

## Consequences

- The IPC codec carries the new items (tags 4–9) and checks every number is
  finite; the host re-sequences frames.
- Goldens changed where semantics changed — rounded clips round, pixel-centre
  clip edges, groups, AA on rounded clip edges — each verified by rendering
  the corpus before and after and comparing pixels (13 of 56 pages differ,
  in 1–595 pixels, all explained).
- The GPU path is checked against the CPU rasteriser on real hardware
  (`gpu_backend/tests/gpu_matches_raster.rs`, headless); CI without a GPU
  skips it.
- Layout boxes keep absolute coordinates (architecture review A5): making a
  subtree move O(1) also needs `LayoutReuse`'s per-element bookkeeping made
  relative, so A5 moves to M39 (since split: done in M39b, ADR 022).

## Alternatives

- **Clip geometry on the CPU (as before).** Cannot rotate a clip, cannot round
  one, and every renderer must re-implement the cutting.
- **Stencil clipping.** Needs a stencil attachment per target and a pass per
  clip change; an SDF test per fragment is simpler at our scale.
- **Tiled rasterization of layers (WebRender/Skia style).** Right for large
  layer counts; unnecessary before a page has many promoted layers. The back
  buffer plus damage gives most of the win.
- **Texture arrays for images.** Limits every image to one size; atlas pages
  plus standalone textures handle both small and large images.
