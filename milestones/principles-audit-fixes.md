# Principles-audit fixes — GPU resources a page can exhaust

> **Status:** Done (before Milestone 34). **Still open:** ~~glyph-atlas
> eviction/compaction, LRU image-texture eviction and batching~~ — done in
> M34 (`milestone-34-compositing.md`);
> P4–P10 of the audit → M34/M36/M37/M38 — all fixed there (P10 by M38a and
> M38b; `ROADMAP.md` §3).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** no web page can exhaust GPU memory or crash the window — the
violations of *zero-trust* and *GPU-first* the 2026-09-24 code audit found
(P1–P3), plus a dependency-hygiene item (P11). Deliberately small: hard
limits now; Milestone 34 builds eviction and batching on top of them.

## What the audit found

| # | Finding | Measured |
|---|---|---|
| P1 | `text::GlyphAtlas` doubled its height whenever it ran out of room, with no cap; every distinct `font-size` (to the pixel) is its own entry, kept forever. | A probe: ~20 characters at 1,024 px (the largest allowed size) grew a 1,024-wide atlas past 16,384 px; 30 reached 32,768 px (32 MiB). Production's atlas is 512 wide, so taller still. |
| P2 | `gpu_backend` created textures at whatever size it was given — the atlas, every image, the window surface — with no check against the device's `max_texture_dimension_2d` (8,192 with the default limits it requests), and installed no GPU error handler. wgpu's default for a validation error is a panic, on the shell's GPU thread, outside per-page crash containment. | Reachable three ways: P1's atlas; any image wider or taller than 8,192 px (the decoder accepts up to 16,384 × 16,384, so a 10,000 × 10 image is fine to decode); a window larger than the limit. |
| P3 | GPU image textures (`image_textures`) were never freed. | By inspection: one texture per distinct image for the window's life. |
| P11 | `engine` pinned `rayon = "=1.10.0"` exactly, with no reason recorded (`layout` uses `"1"`). | The commit that added it (M30) gives none. |

## What was built

- **Glyph atlas cap (P1).** `text::MAX_ATLAS_HEIGHT` (4,096 rows: 2 MiB at
  the production width, thousands of body-text glyphs, below every device
  limit). A glyph that does not fit is drawn as its advance only (layout
  does not move), left uncached, and reported once per overflow
  (`Guard::GlyphAtlasFull`). `GlyphAtlas::begin_frame` — called by
  `rendering::build_display_list_with_scrolls` before any glyph run takes
  atlas coordinates — clears an overflowed atlas back to its initial size
  under a new generation, so the next frame re-packs only what it uses.
  Clearing is safe there because glyph coordinates are fetched from the
  atlas every time a display list is built, never kept across frames.
  Two refinements came from the hardware run (below):
  - **No thrashing.** If a single frame's glyphs overflow a *freshly
    cleared* atlas, clearing again next frame only repeats that work, so
    it is re-cleared every `ATLAS_RECLEAR_FRAMES` (60, about a second)
    instead. An overflow later on (the page's text changed) still clears
    at the next frame.
  - **Metrics before rasterizing.** A glyph's size is looked up
    (`Font::glyph_metrics`) before it is rasterized, and a glyph that will
    not fit is never rasterized. Otherwise a full atlas re-rasterized every
    skipped 1,024 px glyph every frame just to find it did not fit. Output
    is identical (the golden scenes did not change).
- **GPU limits checked (P2)** (`gpu_backend::limits`, testable without a
  GPU): every texture is checked against `device.limits()` before
  creation. An oversized image is scaled down (nearest sampling; images
  draw with normalised coordinates, so it lands in the same place at lower
  resolution) and reported (`Guard::GpuTextureTooLarge`); an atlas over the
  limit is not uploaded and text is not drawn from stale texels; the
  surface is clamped to the limit on creation and resize. A
  `device.on_uncaptured_error` handler reports any GPU error
  (`Guard::GpuError`) instead of panicking the GPU thread.
- **Image texture budget (P3).** `limits::MAX_IMAGE_TEXTURE_BYTES`
  (256 MiB). Past it, a new image is not uploaded (and not drawn), reported
  once (`Guard::GpuImageBudget`). LRU eviction stays M34.
- **`rayon` (P11)** relaxed to `"1"`; `Cargo.lock` unchanged (1.10.0).
- Four new `diagnostics::Guard`s: `glyph_atlas_full`,
  `gpu_texture_too_large`, `gpu_image_budget`, `gpu_error`.

## Tests

- `text`: the audit's hostile inputs (every letter and digit at 1,024 px;
  400 distinct sizes) stay within the cap with bounded memory, and the
  overflow is reported once. A full atlas skips a glyph and keeps earlier
  glyphs valid for the rest of the frame. An overflow after the first frame
  clears at the next frame. An overflow on a fresh atlas is re-cleared (and
  reported) only once per `ATLAS_RECLEAR_FRAMES`, over 180 frames. A glyph
  taller than the cap is skipped.
- `gpu_backend::limits`: size checks (including the audit's 10,000 × 10
  case and zero sizes), aspect-preserving fitting that never rounds a side
  to zero, nearest downscaling, a short-buffer refusal, and a budget that
  refuses past its limit and cannot overflow.
- `engine/tests/hostile_content.rs`: the hostile page through the real
  engine: a frame is produced, the atlas stays within the cap, the guard
  counter moves, and the next frame still draws text.

## Acceptance criteria

- [x] Hostile atlas input stays within the cap with bounded memory (tests
      above).
- [x] A page with that content produces a frame, the guard reports the
      overflow, no panic.
- [x] A unit test shows the texture-size check rejects an image or atlas
      over the device limit.
- [x] Checked on the real GPU (below).
- [x] Standard Definition of Done (`ROADMAP.md` §2): 895 tests, 0 failures
      (up from 883: 12 new); clippy unchanged at 43; all guardrails and the
      fuzz suite pass. No new parser, so no new fuzz target.

## Verification on hardware

AMD Radeon 780M, Wayland/KDE, 2026-09-24. `cargo run -p app --release --
--url http://127.0.0.1:8765/ --trace` against a local page containing every
letter and digit at `font-size: 1024px`, 80 distinct large sizes, a
10,000 × 10 PNG, and a small `@keyframes` animation (so frames keep coming
and the GPU thread's reports are printed).

| Run | Frames in ~8 s | `glyph_atlas_full` reports | `display_list` median | Other |
|---|---|---|---|---|
| Cap only | 135 | 134 (every frame) | 66 ms (first frame) | No panic. |
| + re-clear throttle | 249 | 6 | 20 ms | No panic. |
| + metrics before rasterizing | **471** (60 fps) | **9** (about once a second) | **0.06 ms** | No panic. `gpu_texture_too_large`: "image 10000x10 exceeds the device limit 8192; scaled to 8192x8", reported once. |

The screenshot shows the image drawn in place (all 20 stripes), the glyphs
that fit drawn, and the rest skipped. The first run is why the two
refinements exist: memory was bounded, but a hostile page could still burn
a frame's worth of CPU every frame (the power-budget principle).

Not exercised: the `GpuError` handler (no way to provoke a device error
safely on purpose) and the image budget's refusal (256 MiB of distinct
images); both are covered by the unit-tested logic they call.
