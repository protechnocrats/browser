# Milestone 39c — Changes that keep their size, patched in place

> **Status:** Done (2026-10-20). **Still open:** a change that alters its
> element's size still takes M39b's path (its parent visits all its
> children, the display list is updated in O(items)); patching is skipped
> under the conditions ADR 022 §8–9 lists (an ancestor sized from its
> content, an out-of-flow box placed from outside, an opacity, transform or
> scroll ancestor for the display list…). Decision record:
> `docs/adr/022-layout-coordinates.md` §8–9. Overall status: `AUDIT.md` ·
> the plan: `ROADMAP.md`.

**Goal:** the O(page) work M39b left in a change frame — the changed
element's parent visiting all its children, and the display list moving and
re-banding every item — removed for changes that keep their element's size,
the common case (text in a fixed box, a class that changes colour, a
counter).

## What was built

1. **Layout patched in place** (ADR 022 §8, `layout/src/patch.rs`). When
   every changed element lies in one element C's subtree, C and its
   ancestors are placed by block flow and none of them is sized from its
   content, C alone is laid out again where it was. If its border box and
   bottom margin come out the same (and it is not empty), its new box
   replaces the old one in the previous tree; the ancestors keep theirs,
   with exact box counts and bounds grown to cover the new box. Out-of-flow
   boxes are handled conservatively: the patch is abandoned if one inside C
   is (or was) placed by a containing block outside C, or if one placed by
   an ancestor belonged to an element since removed.
2. **The display list patched in place** (§9). C's item range is built
   again and spliced in; the ranges and band entries after it are
   renumbered, C's old items leave their bands and the new ones enter, and
   the enclosing ranges grow or shrink. Not when an ancestor of C has an
   opacity group, a transform, a scroll layer or is fixed or sticky, when C
   changed paint layer (became positioned, or changed `z-index` — it would
   paint elsewhere among its siblings), or when building C needs a sticky
   box or overflows the glyph atlas: then M39b's update runs.
3. **Intrinsic sizes measured at the change.** On a page that needs
   intrinsic sizing, when one element holds every change and was and still
   is an in-flow block, only its subtree is measured; if what it contributes
   to its parent (its sizes and its own padding and border, now cached) is
   unchanged, no ancestor is measured. Otherwise the ancestors are — not the
   subtree again.
4. **`Subtree::frame` replaces `Subtree::reused`.** The flag said "moved
   over this frame", so after a patch the display list's update path saw
   every sibling as new and built all 40,005 boxes; a frame stamp says
   "unchanged since frame N", which a kept box is.
5. `dom::ElementRef::parent` and `::document`, for the checks above.

Found and fixed on the way (all by the new oracles, before any reached a
page):
- A `position: fixed` root with an auto width is sized from its content:
  the first ancestor check (`width_intrinsic` only) missed it; the check is
  now layout's own per-element rule (`intrinsic::sized_from_content`).
- An element changed from `display: flex` to inline is no longer placed by
  block flow; the patch now requires the target to still be a block in
  flow.
- An absolutely positioned element inside the target, whose containing
  block is outside, sits in a container that can move within the target;
  its recorded static position is relative to that container, so comparing
  static positions missed it. Now any such element rules the patch out.
- A change can make the target the containing block of an out-of-flow
  descendant that an ancestor used to place — and a removed element's box
  can still be in an ancestor's children. Both are now checked in the old
  tree.
- A box that became positioned paints after its in-flow siblings: its
  items cannot go where the old ones were.
- After a patch, an ancestor's grown bounds may order its children
  differently from exact bounds; binary search needs only the recorded
  bounds ordered, which is what the oracle now checks.

## Acceptance criteria

- [x] **A one-element change on a 10,000-row page that keeps the element's
      size visits O(depth) boxes in layout and builds only that element's
      items, asserted by counters.**
      `a_size_keeping_change_on_a_ten_thousand_row_page_is_patched_in_place`:
      for a text change and a class change, one element laid out, nothing
      moved, the patch path two levels deep, at most 5 boxes built for the
      display list (the other 40,000 boxes' items stay; later ones are
      renumbered, not moved or built); `a_change_deep_in_a_page_relays_out_one_ancestor_chain`
      now lays out 1 element, not 2;
      `a_change_that_keeps_its_contribution_measures_only_its_element`
      measures one element's intrinsic sizes, not its ancestors'.
- [x] **The layout, page-list and hit-test oracles pass with patches in
      play; the corpus goldens are unchanged.** New:
      `patched_layouts_equal_fresh_layouts_over_random_mutation_sequences`
      (7,500 frames, over 600 patched) and
      `a_patched_page_list_equals_one_built_in_full_over_random_mutation_sequences`
      (5,000 frames, over 300 lists patched), both checking viewport-level
      equality too. Every golden unchanged.
- [x] **`frame_bench`'s one-DOM-change frame on 10,000 rows is under 1 ms,
      and no frame is slower than M39b.** 0.33 ms on both 10,000-row pages
      (from 7.0 and 7.9 ms). First frames measured +2–4% on the 10,000-row
      pages under load average 7; their frame work, best of 9 by stage, is
      level or faster (35.1 vs 35.4 ms, 36.0 vs 37.1 ms), and their totals
      equal (56.1 vs 56.0 ms; 54.6 vs 56.7 ms).

## Verification

Standing Definition of Done: 1,144 tests, 0 failures (up from 1,140);
clippy unchanged at 43; every guardrail and `test-guardrails.sh` pass; the
fuzz suite passes. AddressSanitizer, CI's `sanitizer` command plus `layout`,
`rendering`, `text`, `style` and `dom` (the new oracles included), 3 rounds:
0 reports, every run passing.

## Measurements

`frame_bench`, interleaved with M39b, 16 rounds, two runs (load average
5–7):

| Page | First frame | Scroll | One DOM change | Caret/focus |
|---|---|---|---|---|
| demo | 3.69 → 3.71 ms | 0.117 → 0.117 ms | 0.083 → 0.081 ms | 0.077 → 0.078 ms |
| 1k rows | 7.66 → 7.67 ms | 0.026 → 0.025 ms | 0.54 → 0.11 ms (−79%) | 0.022 → 0.020 ms |
| 10k rows | 58.6 → 60.1 ms | 0.038 → 0.038 ms | 6.99 → 0.33 ms (−95%) | 0.035 → 0.024 ms |
| 10k + flex/vars | 68.3 → 70.7 ms | 0.037 → 0.038 ms | 7.89 → 0.34 ms (−96%) | 0.034 → 0.024 ms |

The one-DOM-change frame on 10,000 rows by stage (best of 7): frame 6.0 →
0.24 ms; layout 3.6 → 0.15 ms; display list 2.3 → 0.08 ms. Against M39:
23.1 → 0.24 ms.

## Not done / open

- A change that alters its element's size still goes M39b's way.
- After a patch, ancestors' subtree bounds can be larger than exact until
  they are laid out again (hit testing then prunes a little less).
- The checks for out-of-flow boxes are conservative; on pages with many
  absolutely positioned elements, fewer changes are patched.
