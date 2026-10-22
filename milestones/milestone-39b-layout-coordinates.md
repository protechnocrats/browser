# Milestone 39b — Layout coordinates & partial display lists

> **Status:** Done, with gaps (2026-10-20). **Still open:** ~~the parent of
> a changed element still visits each of its children even when the change
> left the child's size alone; updating a page's display list moves every
> item and re-bands the list~~ — for changes that keep their element's
> size, both fixed in **M39c** (`milestone-39c-patch-in-place.md`; a
> one-element change on 10,000 rows 7 ms → 0.33 ms); a
> fresh layout still draws from a build of its own and a page with a sticky
> box still builds every frame, as before; the demo page's first frame is
> 0.05 ms slower, and scroll frames on the 1k/10k pages measured 2 µs
> slower, not attributed (below). Decision record: `docs/adr/022-layout-coordinates.md`.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** a layout change costs what it changes (architecture review A5),
plus the O(page) work M31, M34 and M39 left in change frames and clicks.

## What was built

1. **Box frames** (ADR 022 §1). A box's position is in its parent's frame,
   and `LayoutBox::subtree.child_offset` shifts the frame of its children.
   A fresh layout leaves every offset at zero — its positions, display items
   and pixels are what they were, bit for bit — and a subtree taken from the
   previous frame is moved in O(1): its root's position and offset.
   Readers below the top of a tree accumulate offsets: the display-list
   walk, hit testing, `scroll_range`, inline baselines, positioned
   layout's box origins, and `layout::Placed` (a box and its frame's
   origin; `flattened()` copies a small subtree into document space for the
   caret and focus ring).
2. **Reuse bookkeeping kept across frames** (§2). One map of each element's
   last layout and place in the tree, stamped with the frame that wrote it;
   a moved subtree's entries stay right in their frame, so the O(subtree)
   `carry_over` is gone. Siblings share their parent's path (found once).
   Stale entries are dropped when they outgrow the document
   (`retain_live`, called by the engine).
3. **Sticky constraints relative to the box** (§3) — see the bug below.
4. **Subtree bounds** (§4). Each box records where it or a descendant can
   be hit (cut to its clip; unbounded around `fixed`/`sticky` and one-axis
   scroll containers), whether its children are ordered by position, and
   its box count — computed by a pass at the end of layout that stops at
   reused subtrees. Code that changes a box after building or taking it
   (`LayoutBox::touch`: a table cell or flex/grid item stretched, a sticky
   child's constraints set) has it recomputed.
5. **Pruned hit testing** (§5). Subtrees whose bounds miss the point are
   skipped, ordered children are binary-searched, and the result
   (`layout::Hit`) carries each element's box, placed — a click no longer
   searches the tree for its target's box (`find_box_by_element_key` is the
   last resort for the focused box, after `box_of`).
6. **Partial display lists** (§6). A page's list records each element box's
   item range; after an incremental layout, `PageDisplayList::update` moves
   over (shifted) the items of every subtree the layout moved over and
   builds the rest. Frames whose layout changed incrementally draw from the
   updated list. Kept items are drawn only while the glyph atlas's
   placement (`GlyphAtlas::placement`: new on a new atlas, a clear, a
   compaction) is the one they were built under.
7. **Node-id hashing** (§7). `style::StyleCache` and the reuse maps use
   `dom::NodeIdHasher` instead of SipHash: a change that moves 10,000 rows
   makes about twenty lookups per row.

Found and fixed on the way:
- **A sticky box in a moved or translated container was drawn where its
  container used to be.** Its constraints were absolute and never moved with
  it. On the corpus page `nodejs-about`, the sticky sidebar (inside a
  container laid out and then moved down 380 px) was drawn at the top of
  the page, over the site navigation; it is now beside the article it
  belongs to. The page's golden was re-blessed after a per-pixel comparison
  (79 of 312 items differ, all the sidebar's — x ≤ 232; the other 55 pages
  are unchanged). Goldens are local, like the corpus: re-bless with `conformance run --bless
  nodejs-about` where the corpus is recorded.
- **The frame benchmark's "one DOM change" never changed the DOM.** Its
  click at x = 5 landed in the body's 8 px margin, missing the toggle; on the
  flex page the toolbar above pushed the toggle out of reach too. The
  column has timed a click that changed nothing since it was written — so
  M39's "29–34% slower DOM-change frames" were clicks, and the cost was the
  hit test walking every box (12,001 on that page). The click now lands on
  the toggle (x = 20, toolbar after it); both builds were measured with the
  fix.
- **The page thread gives a new document a new glyph atlas after the frame
  that committed it**, so a display list kept from that frame pointed into
  the old atlas: the first oracle run drew `osm-about` without its text.
  Lists now record the atlas placement they were built under.
- An opacity group that drew nothing drops the items its descendants wrote;
  their ranges are marked dropped, not copied from (found by the oracle).
- Shifting an empty transform layer moved its zero bounds, and a scroll
  layer's unused pivot: now left canonical, so a moved-over subtree's items
  equal a fresh build's.
- A moved-over `fixed` subtree (or the whole page) keeps its items pinned.

Tried and measured, not kept: moving `TextBox` out of line (box 192 → 144
bytes) — an allocation per word made layout slower; boxes padded to 192
bytes alone cost M39 2 ms on the 10,000-row layout, which fewer copies of
boxes paid back (§ Consequences of ADR 022).

## Acceptance criteria

- [x] **Moving a reused subtree costs O(1) box updates, asserted by a
      counter.** `moving_a_reused_subtree_costs_one_box_update_whatever_its_size`:
      over three frames, 89–91 subtrees holding over 8,000 boxes move for
      one update each (`LayoutReuse::moved_box_updates`).
- [x] **A one-DOM-change frame on a 10,000-row page builds display items
      only for the boxes its change touched, asserted by a counter.**
      `a_one_row_change_on_a_ten_thousand_row_page_builds_only_what_changed`:
      at most 12 boxes built (`PageDisplayList::built_boxes`), over 39,000
      moved over; the engine reports the same as the
      `display_boxes_built` gauge.
- [x] **A click on a 10,000-row page examines O(depth) boxes in hit testing
      and box search, asserted by a counter.**
      `a_click_on_a_ten_thousand_row_page_examines_a_handful_of_boxes`: 5
      boxes (12,001 when every box is visited), after a change that moved
      the rows; the click's box comes with the hit, no search.
- [x] **The incremental-versus-fresh layout oracle and the page-display-list
      equivalence pass; the corpus goldens are unchanged, or re-blessed after
      a per-pixel comparison with every difference explained.** The layout
      oracle now compares document-space positions, subtree bounds, order,
      box counts and sticky constraints; a new oracle
      (`an_updated_page_list_equals_one_built_in_full_over_random_mutation_sequences`,
      1,500 sequences) compares updated lists with full builds; another
      (`pruned_hit_testing_equals_visiting_every_box`, 600 sequences, page
      and container scrolls) compares pruned hit testing with visiting every
      box, as does the `layout_document` fuzz target. One golden re-blessed,
      explained above.
- [ ] **`frame_bench` shows no frame slower than M39, measured on a quiet
      machine.** Not met: DOM-change frames are 45–72% faster, first frames
      of the 1k and 10k pages 2–8% faster, caret frames 18–46% faster; but
      the demo's first frame does 0.05 ms more work, and scroll frames on
      the 1k and 10k pages measured 2 µs slower. The machine was not quiet
      (load average 5–7).

## Verification

Standing Definition of Done: 1,140 tests, 0 failures (up from 1,135);
clippy unchanged at 43; every guardrail and `test-guardrails.sh` pass; the
fuzz suite passes (`layout_document` now also checks pruned against full
hit testing and builds a page list). AddressSanitizer, CI's `sanitizer`
command plus `layout`, `rendering`, `text`, `style` and `dom` (the oracles
included), 3 rounds on the final code: 0 reports, every run passing.

## Measurements

`frame_bench` (median frame), interleaved with M39 (with the click fixed in
both), 16 rounds; the machine's load average was 5–7 throughout:

| Page | First frame | Scroll | One DOM change | Caret/focus |
|---|---|---|---|---|
| demo | 3.55 → 3.81 ms (+7%) | 0.167 → 0.117 ms | 0.146 → 0.081 ms | 0.142 → 0.076 ms |
| 1k rows | 7.72 → 7.56 ms | 0.024 → 0.026 ms | 1.94 → 0.55 ms (−72%) | 0.026 → 0.021 ms |
| 10k rows | 63.5 → 58.3 ms (−8%) | 0.036 → 0.038 ms | 25.1 → 7.1 ms (−72%) | 0.043 → 0.036 ms |
| 10k + flex/vars | 74.6 → 68.4 ms (−8%) | 0.038 → 0.038 ms | 27.6 → 7.9 ms (−72%) | 0.046 → 0.035 ms |

First frames are faster mostly from node-id hashing. The demo's first frame
measured +1% to +7% across four runs; its frame work (the `frame` span, best
of 15) is 0.05 ms slower — the bounds pass and the larger boxes. Scroll
frames on the 1k and 10k pages measured 2 µs slower in three of four runs
(0.036 → 0.038 ms on 10k rows), not attributed: a scroll frame does nothing
M39b changed but read the page list.

The one-DOM-change frame on the 10,000-row page, by stage (`diagnostics`
spans, best of 7):

| Stage | M39 | M39b |
|---|---|---|
| frame | 23.1 ms | 6.5 ms |
| layout | 15.3 ms | 3.8 ms |
| display list | 5.6 ms | 2.6 ms |

Layout is now the body visiting its 10,000 rows (each moved over in O(1),
about twenty hash lookups per row); the display list is the update moving
40,000 boxes' items and re-banding them (see *Not done / open*).

## Not done / open

- **A changed element's parent still visits all its children.** Each
  unchanged child is moved over in O(1), but there are 10,000 of them.
  *(M39c: when the changed element keeps its size it is patched in place;
  a size-changing change still goes this way → backlog.)*
- **Updating a page's list is O(items)**: it builds only the touched boxes,
  but moves every item into the new list and re-bands it. *(M39c: patched
  in place for size-keeping changes; otherwise still so → backlog.)*
- **A fresh layout's frame draws from a build of its own**, and the list is
  built by the first frame that reuses the layout (as in M39): banding the
  list in the first frame measured slower first frames.
- **Pages with a sticky box** still build their display list every frame.
- `LayoutBox` is 192 bytes (was 160).
