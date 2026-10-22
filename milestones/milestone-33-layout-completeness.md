# Milestone 33 — Layout completeness

> **Status:** Done, with gaps. **Still open:** ~~per-side borders~~ (done in the gap pass, below), list markers, selectors (`:checked`, `:*-of-type`, `:has()`), `@supports` and gradients → `ROADMAP.md` backlog. ~~Text styling (`font-*`, `text-decoration`, `line-height`) → M35~~ — done in M35. Paint-only frames are ~8% slower (measured).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** the layout features real pages need, in the order Milestone 32's
corpus measured them (`max-width` 47 pages, `position` 44, `background` 43,
`display: none` 42, `:root` 42, `overflow` 40, `min-*`/`max-*` 38,
`inline-block` 37, `:before`/`:after` 37, `float`/`box-shadow` 33), plus Grid,
tables and flex-column grow/shrink.

**Builds on:** Milestone 31 (incremental layout to extend), Milestone 32 (the
corpus that ranked the work and checks every step).

**Decisions:** `docs/adr/014-layout-model.md` — the document is `<html>`, a
user-agent stylesheet as a cascade origin, derived ids for generated content,
boxed rare property groups, float containment, the layout stack contract.

## What was built (one commit per step)

| Step | Commit | What |
|---|---|---|
| 1 | `9331164` | **Lengths:** `Length` is `px + percent`; every unit (`em`, `rem`, `ex`, `ch`, `vw`, `vh`, `vmin`, `vmax`, `in`, `cm`, `mm`, `q`, `pt`, `pc`, `%`), `calc()` (exact: linear in px and %), `min()`/`max()`/`clamp()`. `font-size` inherits, applies first, relative to the parent; the root defines `rem`. `auto` margins centre. Viewport height reaches the cascade. |
| 2 | `303435c` | **Colours:** `rgb()`/`rgba()`/`hsl()`/`hsla()` (both syntaxes), 3/4/6/8-digit hex, the 148 named colours, `transparent`, `currentcolor`. |
| 3 | `678a639` | **`<html>` is the document**; **user-agent stylesheet** (cascade origin below author rules); `:root`; sibling margin collapsing; canvas colour from root/body; `conformance render FILE OUT.png`. |
| 4 | `d172f5d` | **`display`:** `none`, `inline`, `inline-block`, `inline-flex`; per-tag defaults in the cascade; atomic inline boxes (shrink-to-fit, baseline-aligned); replaced-element sizes, `width`/`height` attributes, aspect ratio; form-control default sizes. |
| 5 | `6ad9d00` | **`min-`/`max-width`/`-height`**; flex rewritten on CSS Flexbox §9.7 (freezing, automatic minimum, margins, `auto` margins, items laid out at their flexed width). |
| 6 | `fc6f38f` | **Backgrounds:** the `background` shorthand, `background-repeat`, pixel/edge-offset positions, percentage sizes; images carry UVs (tiling in one item; every image clipped to its box). WGSL shaders validated in `cargo test`. |
| 7 | `ebfa8c2` | **Positioning:** `relative`, `absolute`, `fixed`, `sticky`, `z-index`, paint order and scroll-aware hit testing. **CSS Nesting.** |
| 8 | `322317f` | **`overflow`:** clipping (rects, images, glyphs); scroll containers with their own offsets; the wheel scrolls the container under the pointer. |
| 9 | `1946edc` | **`opacity`, `visibility`.** |
| 10 | `bdbd580` | **`box-shadow`** (outer; GPU feathered-SDF quad, CPU raster). |
| 11 | `0259952` | **`::before`/`::after`** (and `:before`/`:after`), `content` strings/escapes/`attr()`. |
| 12 | `f619701` | **Flex column grow/shrink** (closes Milestone 10's scope cut). |
| 13 | `9f87366` | **Floats and `clear`**, line boxes around floats, formatting-context roots beside them. |
| 14 | `063a8d4` | **Tables:** automatic layout, `colspan`/`rowspan`, row/group boxes, `border-spacing`/`-collapse`, legacy attributes (`cellpadding`, `cellspacing`, `border`, `bgcolor`, `align`, `valign`, `width`, `height`). |
| 15 | `32ef1cb` | **CSS Grid:** track lists (`fr`, `minmax()`, `repeat()` incl. `auto-fill`/`auto-fit`), template areas, line/span/area placement, auto-placement, gaps. **Layout stack contract.** |
| — | `967ae51`, `8e0abf3` | Stack-overflow fix (see below). |
| — | `dc3730e` | Performance: measured against pre-M33 and won back most of the cost (see below). |

New fuzz targets: `css_length`, `css_color`, `css_background`, `css_grid`
(19 in total), plus structural stress inputs for nested `calc()`, CSS
nesting, huge grids. Every new parser is bounded (input length, nesting
depth, list sizes, spans) and has hostile-input tests.

## Bugs found on the way

Pre-existing, found by the corpus and fixed:

| Bug | Found on | Fix |
|---|---|---|
| A flex item's box moved but not its contents: every item's text drew where the first item's did | mozilla.org (overlapping text) | Items are translated as whole subtrees (step 5) |
| A nested CSS rule ended its parent at the nested `}` and dropped the rest of the stylesheet — 1,100 of ~1,200 rules of Wikipedia's skin | Wikipedia (unstyled) | Brace-matched blocks, CSS Nesting (step 7) |
| `font-size` did not inherit | every page | Step 1 |
| `margin: 0` (unitless zero) was rejected; `margin: 0 auto` ignored | most pages | Step 1 |
| Only `#rrggbb` and six colour names parsed | most pages | Step 2 |
| `:before`/`:after` (single colon) made the whole rule be dropped | 37 pages | Step 11 |

Caught before commit by the random-mutation layout oracle (incremental
layout must equal a fresh one; 2,500 sequences, now covering every feature
above): a relative root's reused children offset twice; an absolutely
positioned element inside inline content not taken out of flow; stale static
positions inside inline-blocks; reuse bookkeeping blind to generated boxes;
positioned children of a table laid out as cells.

Found by the fuzz suite's stress inputs: a 512-deep document overflowed a
2 MiB stack after the float/table steps grew layout's frames (each level
cloned a whole `Style`). Fixed by borrowing styles; the need is now a
documented contract (`layout::MIN_STACK_BYTES`, ADR 014) with a guard test.

## Performance

Measured on the same machine against the pre-M33 commit (`f2c7fb2`, built in
a separate worktree), interleaved runs, 10,000-row page
(`cargo run -p engine --release --example frame_bench` /
`--example frame_profile`):

| Frame | Pre-M33 | M33 | Change |
|---|---|---|---|
| First frame (end to end) | 56–58 ms | 60–68 ms | +7–17% (noisy) |
| Scroll | 3.8 ms | 4.1 ms | +8% |
| One DOM change | 14.0–14.7 ms | 4.3–4.6 ms | −69% |
| Caret/focus | 3.7 ms | 4.0–4.6 ms | +7–20% |

The first measurement of M33 was much worse (first frame +34%, paint-only
frames +20%): `Style` had grown from 328 to 848 bytes and `LayoutBox` from 192
to 288. Rare property groups were boxed (`Style` 496 bytes, bounded by a test;
`LayoutBox` 144, smaller than before M33), layout stopped cloning styles, and
paint order became allocation-free. The remaining paint-frame cost (~7% in an
isolated display-list benchmark) is the per-box work of the new features
(visibility, opacity, clipping, positioning).

M31's counters: the incremental and page-thread tests pass. One threshold was
raised on purpose — a DOM change on the 10k-row page lays out 4 elements, not
3, because the page's text input is now an inline-block on body's line.

## The corpus after M33

56/56 pages render, 0 crashes, all deterministic; goldens re-blessed at each
step after a visual check of the changed pages (example.com centred,
python.org's header and logo, mozilla.org's hero, Wikipedia styled with its
infobox, docs.python.org's sidebar). Every feature M32 ranked for M33 has left
the top of the unsupported-feature report. The top of it now (pages of 56):

| pages | feature | where |
|---|---|---|
| 50 / 49 / 49 / 46 | `font-family`, `text-decoration`, `font-weight`, `line-height` | M35 |
| 43 | `cursor` | not yet scheduled |
| 42 / 38 / 32 / 23 | `border-bottom` / `-top` / `-left` / `-right` (per-side borders) | **not yet scheduled** |
| 41 / 31 | `list-style`, `list-style-type` (list markers) | not yet scheduled |
| 35 / 35 / 33 / 31 | `:checked`, `:first-of-type`, `:has()`, `:disabled` | not yet scheduled |
| 28 | `@supports` | not yet scheduled |
| 28 | `background-image: linear-gradient()` | not yet scheduled |
| 26 | JS `window is not defined` | not yet scheduled |

## Acceptance criteria

- [x] Each feature ships with unit tests, and M32 snapshots that rendered
      wrong now render right and are golden (every step re-blessed after
      review).
- [x] The incremental-vs-fresh layout oracle covers every new feature
      (display, lengths, collapsing and auto margins, positioning, overflow,
      generated content, floats, tables, grid) — 2,500 sequences.
- [~] No regression of the M31 counters: the counters hold (one threshold
      raised for a real layout change, above). Wall-clock paint-only frames
      are ~8% slower on the 10k-row page — measured and recorded, not hidden.

## Not done / open

- ~~**Per-side borders** (`border-top` etc., 42 pages)~~ — closed in the gap
  pass (2026-10-21): `style::Style::border()` gives each side's width,
  style and colour (an unset colour is `currentcolor`, read at use), kept
  boxed and `None` without a declaration (ADR 014's rare properties); the
  side shorthands, the twelve longhands and the one-to-four-value box forms
  parse; layout sizes and places by each side; paint draws each side's
  strip, skipping sides that are fully transparent. 39 corpus goldens
  re-blessed after a scene-level comparison: borders previously ignored now
  drawn (+367 visible strips, most of them the same borders moved), sides a
  later declaration removes now gone (`border-top: none` after `border`, as
  on Gutenberg's rules), 272 fully transparent strips no longer emitted
  (invisible), and boxes moved or resized by the widths now counted, with
  their clips and some items culled at the viewport's edge — no other kind
  of change. Measured: first frames of the 10k pages 3–4% slower (DOM-change,
  scroll and caret frames level; not attributed to one site).
- **List markers** (41 pages) are the largest layout-adjacent gap left; not
  in M33's scope.
- Percentage heights resolve only against a definite height (grid/flex/
  explicit); block layout does not track definite containing heights.
- Floats always contained by their block container (ADR 014).
- Tables: `table-layout: fixed` laid out automatically, collapsing borders as
  zero spacing, `<col>` widths ignored. Grid: no named lines, dense or
  column auto-flow, track alignment, subgrid.
- Inset shadows, gradients, `opacity` as a group layer, scroll containers
  scrolled by script (`scrollTop`), sticky inside a scroll container.
- Standing Definition of Done: 883 tests, 0 failures (up from 773); clippy
  unchanged at 43; all guardrails pass; 19 fuzz targets.
