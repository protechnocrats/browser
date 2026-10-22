# Milestone 39 — Arena DOM

> **Status:** Done, with gaps (2026-10-20). **Still open:** interned names
> were built, measured as a net loss here, and deferred to **M40**, then
> **M40b** (ADR 021
> §5; branch `m39-atoms-measured`) — done there, interned where names are
> born (ADR 024); layout boxes relative to their parent
> (architecture review A5) and display lists rebuilt only in part on a
> layout change → **M39b** (done there, ADR 022); one-DOM-change frames on
> the 10,000-row benchmark page measured 29–34% slower than after step 2 —
> M39b found that the benchmark's click changed nothing, and its cost was
> the hit test walking every box (below). Decision record:
> `docs/adr/021-arena-dom.md`. Overall status: `AUDIT.md` · the plan:
> `ROADMAP.md`.

**Goal:** the node model long-lived engines use (architecture review A2): an
arena of nodes addressed by id, with parent and sibling links; the full tree
mutation API; one copy of the rules that were duplicated across crates.

## What was built

In four steps, each behind the M31 restyle and layout oracles and the 56
corpus goldens, which never moved.

1. **Every reader goes through `dom::ElementRef`** (`17e69b5`). A `Copy`
   handle to an element wherever it lives — an owned `Element` (a fragment)
   or a document's arena — with `tag()`, `attr()`, `children()` (yielding
   `ChildRef::Element` or `ChildRef::Text`), `element_children()`,
   `text_content()` and the rest. `style`, `layout`, `accessibility`, `js`
   and `engine` were moved onto it with storage unchanged; `Document` stopped
   dereferencing to its root `Element`. Public entry points take
   `impl Into<ElementRef>`, so owned test trees pass unchanged.
2. **The document is an arena** (`cb6da3f`). One `Vec` of node slots, each
   with its parent, first and last child, previous and next sibling as `u32`
   links (a sentinel for none), a child count, and its data (element, text,
   or a free slot). Traversal follows links; `NodeId → slot` (a
   multiplicative hasher: ids are engine-assigned) and the `id`-attribute
   index answer lookups. Freed slots are reused; ids never are. `Element`
   stays the fragment type: `Document::new` and the insertion methods move
   fragments in, keeping element ids; text nodes get ids in the document.
3. **The full mutation API** (`cad00ae`). `insert_before`, `move_before`
   (relinks an attached node; refuses cycles and the root), `remove_child`
   (a fragment keeping its ids), `replace_child`, `clone_node` (fresh ids),
   `Fragment::text`. Each is O(1) link updates, counted
   (`Document::link_updates`). `MutationKind::Inserted` records an inserted
   or moved node so its subtree is restyled. In `js`: `insertBefore`,
   `removeChild`, `replaceChild`, `cloneNode`, `document.createTextNode`;
   `appendChild` moves an attached node; a removed node comes back as a
   detached handle with the same id.
4. **One rule, once** (`e8f1600`). Form-field validity
   (`ElementRef::is_valid`: `required`, `pattern` via the linear-time
   matcher) replaces three copies — `style`'s `:valid`/`:invalid`, `js`'s
   submission gate, `accessibility`'s `invalid` (which now also sees
   `pattern` mismatches). Editability is `dom`'s rule everywhere.
   `scripts/check-dom-encapsulation.sh` keeps it that way.

Found and fixed on the way:
- **Moved or re-inserted nodes kept stale styles.** The parent's `ChildList`
  restyles only new children (or all, under structural selectors); a moved
  node is not new, so its inherited and descendant-selector styles would
  have stayed. `MutationKind::Inserted` fixes it; with it removed, the
  extended restyle oracle fails at sequence 24.
- **A first arena was 15% slower on 10k-row first frames and 44% on
  10k-row DOM changes.** Brought level: `Page` takes the parsed tree by
  value instead of cloning it into the document; restyle roots are pruned
  by ancestry instead of child-index paths (O(siblings) per level in an
  arena); handles hold their node's data; one arena access per child step;
  compact links; free slots marked in band.
- A tool of mine for applying compiler fixes fed one compile's byte offsets
  to two fixers in turn and corrupted `engine/src/page.rs`; the four engine
  files were restored and redone, and every other migrated crate's diff was
  checked for misplaced insertions (none).
- `fragment_size` (used to reserve the arena) counted elements twice.

Changed on purpose:
- The heavy-page frame-budget test accepted only settling at 67 ms; on a
  loaded machine a frame takes the second step down, to 133. It now accepts
  either, with pacing checked against the level (`frame_budget.rs`).
- A recording test expects the extra `Inserted` record of an append (it
  does not bump the generation: one mutation, one generation).
- Accessibility's `invalid` is no longer narrower than `:invalid` (M24 had
  documented that as a scope cut).

## Acceptance criteria

- [x] **No `&mut Element` tree walks remain outside `dom`.**
      `scripts/check-dom-encapsulation.sh` (run by
      `check-dependency-direction.sh`, self-tested in `test-guardrails.sh`)
      fails non-test code outside `dom` that takes `&mut Element` or matches
      or builds `Node::Element`/`Node::Text`, except the fragment builders
      listed with reasons in `scripts/dom-fragment-builders.txt` (the parser,
      `js`'s detached forest and its bindings, generated content, the
      loader's pass over a parsed tree before it becomes a document).
- [x] **Mutating a node costs O(1) link updates, asserted by counters.**
      `mutations_cost_constant_link_updates_on_a_large_document`: on a
      100,000-child parent, an insert costs 1 link update, a move 2, a
      removal 1, a replacement 2.
- [x] **The oracles and every corpus golden pass unchanged.** After every
      step; the restyle and layout oracles now also generate insertions,
      removals, replacements, moves, clones and text insertions. A 3,000-step
      random sequence keeps the arena and its indexes consistent.

## Verification

Standing Definition of Done: 1,135 tests, 0 failures (up from 1,121);
clippy unchanged at 43; every guardrail, the new one included, and
`test-guardrails.sh` pass; the fuzz suite passes (a `js_script` seed,
`06_tree_mutations`, drives the new API, including cycles and hostile
arguments).
AddressSanitizer, CI's `sanitizer` command with `dom` added (the arena, its
mutation API and the random-mutation consistency test included), 3 rounds:
0 reports, every run passing.

## Measurements

`frame_bench` (median frame), interleaved, 8–12 rounds. The machine was
noisy throughout: KDE's file indexer was crashing several times a second
and core-dumping (5,487 dumps in six hours, none of them ours), so only
consistent differences are reported.

| Page | Frame | Step 1 (readers on handles, owned storage) | Arena (step 2) | M39 (step 4) |
|---|---|---|---|---|
| 10k rows | first | 59.1 ms | 59.5 ms | 60.4–60.7 ms |
| | one DOM change | 0.61 ms | 0.59 ms | 0.74–0.76 ms |
| 10k + flex/vars | first | 67.4 ms | 69.4 ms | 69.5–69.7 ms |
| | one DOM change | 0.61 ms | 0.59 ms | 0.76–0.78 ms |

Scroll and caret frames, and every frame of the demo and 1k-row pages, are
level within noise.

**Not attributed here: one-DOM-change frames are 29–34% slower after steps
3–4 than after step 2.** That frame is almost all input handling (its own
restyle, layout and display list take 0.02 ms): a hit test and a box search
that each walk the whole layout tree — 40,000 boxes here — which makes the
frame sensitive to where those boxes sit in memory. Ruled out by
measurement: text nodes in the id index; the four new methods on every
element wrapper. Interned names were the one change that reproducibly
slowed it (by about 60%), and they were taken out. *(Milestone 39b: the
benchmark's click landed in the body's margin, so this column timed a click
that changed nothing — which is why restyle and layout cost nothing; the
cost was the hit test, which M39b prunes to 5 boxes.)*

## Not done / open

- **Interned names** → M40, split to **M40b** (ADR 021 §5): built and measured on branch
  `m39-atoms-measured`; they pay only when interned in the parser and used
  by selector matching.
- **Layout boxes relative to their parent (A5), `LayoutReuse`'s positions
  relative, and display lists rebuilt in part on a layout change → M39b.**
  They change the layout coordinate model that `rendering`, hit testing,
  the compositor and about 350 coordinate reads rely on, and summing
  relative offsets in floating point can move goldens — a separate change
  from the DOM model, with its own ADR (022).
- **Comments** are not kept (ADR 021 §4, amended).
- ~~The DOM-change regression above~~ — a click, not a DOM change; its
  full-tree hit test is pruned in M39b.
- ~~`js`'s detached elements still have their own wrapper type, and
  `insertBefore`/`removeChild` exist on attached elements only — M40
  replaces both wrapper types with generated bindings keyed by `NodeId`.~~
  — done in M40: one wrapper per `NodeId`, every member on it, attached or
  not (ADR 023 §4).
