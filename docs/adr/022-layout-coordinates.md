# ADR 022 — Layout coordinates: box frames, subtree bounds, partial display lists

- **Status:** accepted (Milestone 39b, 2026-10-20); decisions 8–10 added in
  Milestone 39c
- **Decides:** what a layout box's position is relative to, how a laid-out
  subtree is moved, what layout records about a subtree for hit testing, and
  how a page's display list follows a layout change.
- **Constrains:** `layout` (every algorithm that moves or edits a box after
  building it), `rendering` (the display-list walk, `PageDisplayList`),
  `engine` (hit testing, the caret, the focus ring, box searches), and any
  future reader of a `LayoutBox` tree.
- **Supersedes:** "boxes use absolute coordinates" (architecture review A5);
  `LayoutReuse`'s per-frame copy of its bookkeeping for moved subtrees
  (`carry_over`, Milestone 31); a display list built in full by every frame
  whose layout changed (Milestone 31, kept by M34).

## Context

Milestone 31 made layout incremental: a subtree nothing changed is taken from
the previous frame's tree instead of being laid out again. But taking it
cost O(subtree) twice — every box in it was translated to the new position,
and the subtree's bookkeeping (each element's recorded position and place
in the tree) was moved from the previous frame's maps to this frame's — and
after that, three more walks of the whole page followed a one-element
change:

1. the frame whose layout changed built a display list for the whole page,
   and the next frame built the page's banded list again;
2. hit testing a click descended into every box (descendants may overflow
   their parent, so a parent's box proves nothing);
3. the engine searched the whole tree for the clicked input's box.

On a 10,000-row page those walks are most of a click's cost (M39 measured
the click-and-change frame at ~0.75 ms, almost none of it restyle or
layout).

Making positions relative to the parent (A5 as written) fixes the first cost
but changes every position from a computed number to a sum of numbers: in
floating point, `(c − p) + p` is not always `c`, so fresh layouts would draw
differently in the last bit, and every layout algorithm would convert on
each attach. About 350 coordinate reads would change meaning at once.

## Decision

1. **A box's position is in its parent's frame; a box can shift the frame of
   its children.** `LayoutBox::subtree.child_offset` is added to every
   descendant's position (and baseline). A fresh layout leaves every offset
   at zero, so its positions are document positions, bit for bit, exactly
   as before. Moving a subtree moves its root's `x`/`y` and its
   `child_offset`: O(1), whatever the subtree's size. Layout algorithms keep
   computing in document space and keep translating the boxes they build
   eagerly (`translate`, unchanged, adds to every box's own position, which
   stays right under any offsets); only taking a subtree from the previous
   frame uses the offset. A reader that goes below a box's children
   accumulates offsets: `layout::Placed` (a box and the origin of the frame
   it is in) does it for single lookups; walks carry the origin.
2. **Positions recorded for reuse are in the frame their box is in, and are
   kept across frames.** `LayoutReuse` keeps one map of each element's last
   layout (width, depth, outer position) and place in the box tree, stamped
   with the frame that wrote it. Laying an element out overwrites its entry;
   a moved subtree's descendants keep theirs, still right in their frame.
   A take uses only entries from earlier frames, and computes the move from
   the element's recorded outer position and its new one — the frame shifts
   between them cancel. An element's link and its layout are written in the
   same frame when its parent places its box; a box placed another way (a
   flex item, a table cell) leaves its link older than its layout, and is
   never found by the link. Siblings share their parent's path, found once
   per parent. `carry_over` and its O(subtree) walk are gone. Entries of
   removed elements are dropped when the maps outgrow the document
   (`LayoutReuse::retain_live`).
3. **Sticky constraints are relative to the sticky box** (the room it has
   above and below within its containing block), so they move with it.
   (They were absolute and were not translated with a moved subtree: a
   sticky box in a reused subtree was clamped to where its container used
   to be.)
4. **Each box records its subtree's hit area and size.** `subtree.bounds` —
   its border box united with its children's bounds (in its frame), cut to
   its clip where it clips both axes; unbounded if a descendant is `fixed`
   or `sticky` (they move with scrolling) or it is a scroll container that
   clips only one axis — and `subtree.boxes`, its box count.
   `subtree.ordered` says its children are in increasing order of both bound
   edges with none positioned. A pass at the end of layout computes them for
   boxes laid out this frame and stops at reused subtrees, which carry
   theirs; a reused box that an algorithm changes after taking it (a table
   cell stretched to its row) is marked not reused (`LayoutBox::touch`) and
   recomputed. Unknown bounds (`None`, as in a hand-built tree) never prune.
5. **Hit testing prunes by bounds and binary-searches ordered children**,
   and returns, with the path, each element's outermost box it hit, placed
   (`layout::Hit`) — a click needs no search for its target's box. The
   focused element's box is found by its recorded place in the tree
   (`LayoutReuse::box_of`, O(depth)) before any search. Hit testing reports
   how many boxes it examined.
6. **The page's display list follows a layout change in part.**
   `PageDisplayList` records, per element box, its item range and position.
   After an incremental layout, `PageDisplayList::update` walks the new tree
   and, at each box unchanged since the previous layout (M39b:
   `subtree.reused`; since M39c `subtree.frame` older than the root's,
   decision 10) whose range the list has, copies that range shifted by the
   box's move instead of building it; boxes laid out this frame are built. Every subtree's items are contiguous (paint order is per box, and
   `fixed` boxes are hoisted to the root), so a copy is one slice; ranges
   are found by walking the previous list's ranges in order, with an index
   built only when a box is not where the walk expects it. A frame whose
   layout changed incrementally draws from the updated list, as a scroll
   frame does. The list is built in full instead when there is none for the
   previous layout, or it was built under another glyph-atlas placement
   (`GlyphAtlas::placement`: a new atlas, a clear, a compaction — copied
   items would point at moved glyphs). A fresh layout still draws from a
   build of its own, as before (banding a list would only add to a first
   frame), and a page with a sticky box builds each frame (as before).
7. **Maps keyed by node id hash with `dom::NodeIdHasher`** — `style`'s
   cache and `layout`'s reuse bookkeeping, as the document's slots already
   did (ADR 021): ids are assigned by the engine, so a multiply is as good
   as SipHash and several times cheaper, and a change that moves 10,000
   rows makes about twenty lookups per row.

8. **(M39c) A change that keeps its element's size is patched into the
   previous layout in place.** When every changed element lies in one
   element C's subtree, C and its ancestors are each placed by their
   parent's block flow (each has a link), none of them sizes itself from
   its content's intrinsic size (`width_intrinsic`, `height_intrinsic`),
   and C is not laid out beside floats, C alone is laid out again — where
   it was, at the width it had. If its border box and bottom margin come
   out as before (and it is not empty: an empty box's margins collapse
   through it), nothing after it moves and no ancestor's size changes: its
   new box replaces the old one in the previous tree, and the ancestors
   keep theirs, their box counts adjusted and their bounds grown to cover
   the new box (a superset; a later layout of the ancestor makes them
   exact). Out-of-flow boxes stay right only if nothing they depend on
   moved: the patch is abandoned if a changed element is out of flow, or
   inside an out-of-flow element whose containing block is outside C, or if
   laying C out recorded a different static position for any out-of-flow
   element. Otherwise — or if the size changed — the frame lays out as in
   decision 2, from a previous tree that has lost the parts of C already
   taken (they are laid out fresh). A patch costs O(depth + C's work).
   C and its ancestors must be blocks in flow (layout's own classification)
   and not sized from their content (`intrinsic::sized_from_content`: a
   flex, grid or table box, a shrink-to-fit float, inline-block or
   out-of-flow box, an intrinsic-size keyword). Out-of-flow boxes rule a
   patch out when one in C's subtree is placed by a containing block
   outside C, or when an ancestor's box holds one for an element inside C
   or since removed. On a page that needs intrinsic sizing, C's subtree is
   measured first; if what C contributes to its parent (its sizes, and its
   own padding and border, now in the cache) is unchanged, no ancestor is
   measured.
9. **(M39c) The page's display list follows a patch in place.** C's item
   range is built again and spliced in; later ranges and band entries are
   renumbered (integer work), C's old items leave their bands and its new
   ones enter, and the enclosing ranges grow or shrink. Not when an
   ancestor of C has an opacity group, a transform, a scroll layer or is
   `fixed` or sticky (their markers depend on what they contain), when C
   changed paint layer (became positioned or changed `z-index`: it paints
   elsewhere among its siblings — each range records its box's layer),
   when building C needs a sticky box or overflows the glyph atlas, or when
   the list is not drawable: then decision 6's update runs.
10. **(M39c) "Unchanged since the previous layout" is a frame stamp.**
   `Subtree::frame` is the layout frame that built a box or last changed
   it (set by the pass at the end of layout, and on a patched chain); a box
   older than its tree's root is unchanged since then but for where it is.
   (M39b's flag — "moved over this frame" — made every box kept by a patch
   look new to decision 6's update, which then built them all.)

## Consequences

- Every walk of a `LayoutBox` tree below one level must add `child_offset`;
  `scripts/` cannot check that, so the incremental-versus-fresh oracle
  compares *absolute* positions, bounds and box counts, and a new oracle
  compares an updated `PageDisplayList` with one built in full, over random
  mutation sequences.
- After a patch, an ancestor's subtree bounds may be larger than exact
  (never smaller), and its children may be marked unordered when they are
  ordered; the oracles check bounds as a superset and order as an
  implication.
- `LayoutBox` grows by 32 bytes, to 192 (40,000 boxes on the 10,000-row
  page: 1.3 MB). Padding the M39 box to 192 bytes alone measured +2 ms on
  that page's layout; fewer box copies pay for it (a block's child list is
  reserved once, an inline run's boxes are handed over, not copied).
  Moving `TextBox` out of line (144 bytes) measured slower still: an
  allocation per word.
- `LayoutReuse::reused` still counts boxes moved over (from `subtree.boxes`),
  and `moved_box_updates` counts the box updates moving them cost.
- A fresh layout's boxes, display items and pixels are unchanged.

## Alternatives

- **Positions relative to the parent everywhere (A5 as written):** the
  float re-summation above, every layout algorithm converting, and no
  benefit over decision 1 for anything this engine does.
- **Translating eagerly in O(1) everywhere** (layout's own `translate` also
  using the offset): fresh layouts would sum nested moves in a different
  order and move goldens in the last bit; the eager walks are fresh-layout
  cost, paid once per box laid out.
- **Bounds only, without binary search:** a parent with 10,000 children
  would still check each one.
- **A spatial index for hit testing** (an R-tree per page): faster for
  pathological overlap, but a second structure to keep in step with
  incremental layout; bounds per box ride on the tree that is already kept.
- **Retaining display items per box in the layout tree:** couples `layout`
  to `rendering`'s item types (a dependency-direction violation).
