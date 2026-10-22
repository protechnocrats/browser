# ADR 008 — Incremental pipeline: identity, mutation records, invalidation

- **Status:** accepted (Milestone 31)
- **Decides:** how elements are identified across frames, how DOM changes are recorded, and how the style, layout and paint stages decide what to redo.
- **Constrains:** every future DOM-mutating binding, every layout feature (M33), compositing (M34).

## Context

Before Milestone 31 every frame ran the whole pipeline — cascade for every
element, layout for every box, the full display list — whether the frame was
caused by a DOM change, a caret blink, or a scroll. Elements were identified in
caches by `element_ptr_key` (their memory address), valid for one borrow of one
tree. `find_by_id` was a tree walk, repeated on every script property access.

## Decision

1. **Identity is `dom::NodeId`** (Milestone 18's per-process-unique `node_id`).
   Every cache, hit-test path and event target uses it; `element_ptr_key` is
   deleted.
2. **A live document is a `dom::Document`**, which owns the root element and
   is the ONLY way to mutate it: `set_attribute`, `remove_attribute`,
   `set_text_content`, `append_child`, `set_inline_style_property`,
   `mutate_elements`. There is no `&mut Element` access to a live document, so
   an unrecorded change cannot be written. Each mutation bumps a generation
   counter and appends a record `{ target: NodeId, kind: Attributes | ChildList }`.
   Records are drained by the frame pipeline; past `MAX_MUTATION_RECORDS`
   they collapse into "everything changed" (bounded memory under a mutation
   storm).
3. **The document keeps an id index**: `id → [NodeId]` and
   `NodeId → child-index path`, maintained incrementally by every mutation
   (an append indexes only the new subtree; `set_text_content` un-indexes only
   the removed one). `find_by_id` and `find_by_node_id` cost O(tree depth) —
   independent of document size — instead of a walk of the whole tree.
4. **Style invalidation** (the rules, given the selectors the engine
   supports — no `:has()`, no `:empty`):
   - an **attribute** change on E restyles E's subtree; if the stylesheet uses
     sibling combinators (`+`, `~`), also the subtrees of E's later siblings;
   - a **child-list** change on P restyles the subtrees of P's new children;
     if the stylesheet uses structural pseudo-classes or sibling combinators,
     all of P's children's subtrees (a new last child changes who is
     `:last-child`);
   - a new stylesheet, a viewport width change (media queries), or overflowed
     records restyle everything.
   A restyled root takes its parent's cached style for inheritance and its real
   ancestor and sibling context for matching. **Correctness oracle:** a
   property test compares incremental and full restyle over thousands of
   random mutation sequences; it is a merge gate, not a nice-to-have.
5. **Layout invalidation**: an element is layout-dirty if it was mutated or its
   final (post-animation) style changed; every ancestor of a dirty element is
   re-laid out. A clean subtree is reused from the previous frame's layout
   when laid out at the same available width — its boxes translated to the
   new position — so a change deep in a page re-lays out one ancestor chain
   plus the changed subtree. A viewport change relays out everything. Same
   oracle: incremental layout equals full layout over random mutations.
6. **Frames are classified by what changed**:
   - *paint-only* (scroll, caret, focus): no cascade, no layout — the last
     layout is reused and only the display list (culled to the viewport) is
     rebuilt;
   - *animation* (a transition or `@keyframes` in flight): the cascade is
     reused; interpolation, layout and paint run;
   - *mutation*: incremental restyle and relayout, as above.
   Per-frame counters (elements restyled, boxes laid out vs reused) are
   reported with each frame (`FrameStats`) — per page, not process-global —
   so tests assert on work done, not on wall-clock time.
7. **Damage**: each frame carries the rectangle that differs from the previous
   frame (a display-list diff), for Milestone 34's damage-based presentation.

## Consequences

- Script bindings must use the `Document` API; a binding that needs a new kind
  of mutation adds a recorded method rather than reaching into the tree.
- Layout features that break "a subtree's layout depends only on its own
  style and its available width" (floats, positioned elements; M33) must
  widen the cache key or opt their subtrees out of reuse, and extend the
  oracle to prove it.
