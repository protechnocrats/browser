# Milestone 31 — Incremental pipeline

> **Status:** Done. **Still open:** the remaining O(page) work in change frames is listed below; the first frame is ~17% slower than before this milestone (measured).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** stop re-running the whole pipeline every frame. Before this
milestone every frame — a scroll, a caret move, a one-attribute change —
ran the full cascade, the full layout and the full display list.

**Builds on:** Milestone 30 (the page thread owns the state), Milestone 27
(measurement). Decision record: `docs/adr/008-dirty-tracking-and-identity.md`.

## What was built

1. **One identity** (`dom::NodeId`). `element_ptr_key` — an element's memory
   address, valid for one borrow of one tree — is gone; style caches,
   intrinsic sizes, `LayoutBox::element_key`, hit-test paths, event dispatch
   and the accessibility text map use `node_id`.
2. **`dom::Document`**: the only way to mutate a live document
   (`set_attribute`, `remove_attribute`, `set_text_content`, `append_child`,
   `set_inline_style_property`, `mutate_elements`). No `&mut Element` access
   to a live document exists, so an unrecorded change cannot be written.
   Each mutation bumps a generation and appends a `MutationRecord`
   (bounded at 10,000, then "everything changed"). An incrementally
   maintained index — `id → elements`, `element → (parent, child index)` —
   makes `find_by_id` / `find_by_node_id` O(depth) with O(elements) memory.
3. **Incremental restyle** (`style::StyleEngine`): only the subtrees a
   mutation record can affect are re-cascaded (rules in ADR 008), with the
   restyled roots' real ancestor and sibling context. Transitions and
   `@keyframes` visit only elements that have them.
4. **Layout subtree reuse** (`layout::LayoutReuse`): an element that is not
   layout-dirty (mutated, restyled, or an ancestor of one) and is laid out
   at the same width is *moved* out of the previous frame's layout tree and
   translated, not laid out again. Flex items are never reused (their
   container repositions them). O(elements) bookkeeping, no subtree clones.
5. **Frame classes** (`engine::frame::build_frame`): a scroll, caret or focus
   frame reuses the last layout — no cascade, no layout; the focus ring is
   painted at display-list time so the reused tree is never modified. The
   display list is culled to the viewport and diffed against the previous
   frame for a damage rectangle (`Scene::damage`).
6. **Per-frame counters** (`FrameStats` on `EngineEvent::Frame`): elements
   restyled, full vs incremental restyle, animated elements, layout work
   (`Reused`, `Full { boxes }`, `Incremental { laid_out, reused_boxes }`),
   display items, culled items, damage. Per page, deterministic.

## Correctness oracles (merge gates)

- **Restyle:** incremental == full cascade after every step of 10,000 random
  mutation sequences over seven stylesheets (sibling combinators, structural
  pseudo-classes, attribute selectors, `:not()`, custom properties,
  `@media`, transitions). Three deliberately broken invalidation rules were
  each caught within the first 60 sequences.
- **Layout:** incremental == fresh layout (float tolerance) after every step
  of 1,500 random sequences (block, inline, flex, borders, padding, inline
  style writes, subtree replacement), with the reuse path exercised (>10,000
  boxes moved). Three broken reuse rules (ignore the dirty set, ignore the
  width key, forget to shift carried-over positions) were each caught.

## Acceptance criteria

- [x] Property test: incremental restyle == full restyle over ≥ 10,000
      random mutation sequences
      (`style::incremental::tests::incremental_restyle_equals_full_restyle_over_random_mutation_sequences`).
- [x] A scroll-only frame performs zero cascade and zero layout, asserted via
      counters (`engine/tests/incremental.rs`: `restyled == 0`,
      `layout == Reused`); likewise a focus or caret move.
- [x] A single attribute change on a 10k-element page restyles O(changed
      subtree), asserted via counters: 2 elements (the element and its
      child) of 20,001 in `style`; 1 element through the whole engine; and
      layout re-lays out ≤ 3 elements while moving >20,000 boxes.
- [x] `element_ptr_key` removed from `layout`/`js`/`app`; hit-testing uses
      `node_id`.
- [x] `find_by_id` is O(depth), independent of document size; index
      invariants tested under `appendChild`, subtree replacement, `id`
      attribute changes, `mutate_elements`, duplicate ids, a 100,000-element
      document and a 100,000-deep chain; navigation teardown builds a fresh
      `Document`.
- [x] Idle page: zero frames built (`an_idle_page_builds_no_frames`,
      `next_wake() == None`).

## Measurements

`cargo run -p engine --release --example frame_bench` (public API only, so
the same file measured the Milestone 30 code). This machine is noisy (the
same run varied up to 2x after compiles), so the table is the **best of
five alternating runs** per binary, median frame time in ms, 800x600:

| page | frame | before (M30) | after (M31) | |
|---|---|---|---|---|
| demo | first | 3.39 | 3.82 | |
| | scroll | 0.33 | 0.21 | 1.6x |
| | one DOM change | 0.31 | 0.15 | 2.1x |
| | caret/focus | 0.27 | 0.14 | 1.9x |
| 1k rows | first | 6.33 | 7.53 | |
| | scroll | 2.96 | 0.39 | 7.6x |
| | one DOM change | 2.56 | 0.94 | 2.7x |
| | caret/focus | 2.32 | 0.38 | 6.1x |
| 10k rows | first | 47.09 | 55.01 | |
| | scroll | 28.82 | 3.77 | 7.6x |
| | one DOM change | 27.61 | 14.60 | 1.9x |
| | caret/focus | 28.97 | 3.69 | 7.9x |

**The first frame is ~13–19% slower**: the document index, the style
engine's changed-set and the layout reuse bookkeeping are paid once up front
so every later frame is cheaper. Measuring this found two avoidable costs
that were then removed (building the layout dirty set with no previous
layout to reuse: 13.5 ms of a 10k-row first frame; and cloning every
element's `Style` into a second, final-styles map on each full restyle).

**What is still O(page) in a DOM-change frame**, and why the change column
improves less than scroll: the display list is rebuilt from the whole
layout tree before culling; unchanged siblings of a re-laid-out element are
moved one by one (layout boxes use absolute coordinates, so moving a
subtree touches every box in it — relative coordinates would make that
O(1), a larger change to `rendering` and hit-testing); and the intrinsic-
sizing check walks the tree. Each is a measured, known next step.

Parallel interpolation was considered and not built: interpolation now
visits only animated elements (a handful), so there is nothing to split.
Parallel positioning remains blocked by block layout's sequential
dependencies (see `layout::layout_document`).

## Bugs found by the new counters, fixed

- A caret move (arrow keys, Home/End) rewrote the input's `value`, a real
  mutation: a restyle and a 30,000-box relayout for moving a caret.
- Glyph runs compared unequal whenever any new glyph was rasterised (the
  atlas generation is part of the item), so typing one character damaged
  every line of text on screen.
- The first damage diff (common prefix/suffix) turned two small changes at
  opposite ends of the list into whole-screen damage; replaced by a
  re-synchronising walk.

## Test changes, on purpose

- The demo golden was re-blessed for culling after checking it equals the
  old snapshot minus exactly its off-screen items (312 → 205).
- `backspace_at_start_of_input_is_a_no_op_not_a_panic` now asserts the
  value is empty and that no mutation happened, instead of pinning the old
  side effect of writing `value=""`.
- Two headless tests were adjusted so what they measure is on screen.

Standing Definition of Done: 753 tests, 0 failures (up from 734); clippy
unchanged at 43; all guardrails pass.
