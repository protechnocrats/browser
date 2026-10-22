# ADR 021 — The arena DOM

- **Status:** accepted (Milestone 39, 2026-10-19); §4 and §5 amended during M39; §5 implemented by ADR 024 (M40b)
- **Decides:** how a live document stores its nodes, how every other crate
  reads them, how nodes are named (ids and interned names), and which tree
  mutations exist.
- **Constrains:** `dom`, and every crate that reads a document: `style`,
  `layout`, `accessibility`, `js`, `engine`, `html_parser`, `conformance`.
  M40's generated bindings are written against the node model chosen here.
- **Supersedes:** the owned-tree half of ADR 008 (`Document` as an
  `Element` tree with a side index); ADR 008's mutation records and
  invalidation rules stay.

## Context

Architecture review A2 (`ROADMAP.md` §3): the DOM is an owned tree of
`Element`s — string tags, attributes in a `Vec`, no parent pointers,
positions found through a side index (`NodeId → (parent, child index)`),
text nodes without identity, and only `appendChild` and `textContent=` as
mutations. Every crate reads it by walking `&Element` and matching on
`Node::Element` / `Node::Text`. Each milestone built on it raised the cost of
replacing it; mature engines use an arena of nodes addressed by id, with
interned names.

Two facts shape the migration:

1. Not every node a reader sees is in a document. Layout builds throwaway
   elements for `::before`/`::after`; `js` keeps `createElement`'d subtrees
   detached until they are appended; hundreds of style and layout tests build
   trees by hand.
2. The incremental pipeline's oracles (M31) and the corpus goldens (M32) are
   the merge gate: the migration must not change what any page draws.

## Decision

1. **Readers use a handle, not a type.** `dom::ElementRef<'a>` is a `Copy`
   handle to an element wherever it lives: in a document's arena, or an owned
   `Element` (generated content, detached subtrees, tests). It offers what
   readers need — `tag()`, `attr()`, `attributes()`, `node_id()`,
   `children()` (yielding `ChildRef::Element` or `ChildRef::Text`),
   `element_children()`, `text_content()`, and the editability and validity
   rules (decision 6). `style`, `layout`, `accessibility`, `engine` and `js`
   read only through it. Public entry points take `impl Into<ElementRef>`,
   so a caller with an owned tree passes `&element` unchanged.
2. **The document is an arena.** `Document` holds a `Vec` of node slots;
   each node has its kind (element, text, comment), its `NodeId`, and its
   parent, first and last child, and previous and next sibling as slot
   indices. Traversal follows those links — no lookups. External lookups by
   `NodeId` (script handles, caches, hit-test paths) go through one
   `NodeId → slot` map. Freed slots are reused through a free list; a
   `NodeId` is never reused, so a stale id finds nothing rather than another
   node.
3. **Owned `Element` stays, as a fragment.** The parser, `createElement`,
   generated content and tests build owned subtrees; `Document::new` and the
   insertion methods move a fragment into the arena keeping its `NodeId`s;
   removing a subtree takes it out as a fragment with the same ids, so a
   script handle to a removed node stays valid and can be inserted again.
   Moving a node already in the document (`appendChild` of an attached node,
   `insertBefore` within the tree) relinks it in place.
4. **Every node in a document has an identity.** Text nodes get `NodeId`s
   from the same counter as elements (still below `MAX_ELEMENT_NODE_ID`, so
   generated-content ids stay disjoint), and are indexed like elements. A
   fragment's text children are anonymous until inserted; a lone text node
   (`createTextNode`, a removed text child) is a `Fragment::Text` with its
   id. *Comments are not kept* (amended during M39): readers would have to
   skip them, which breaks `Children`' exact size, and nothing reads them
   until `childNodes` exists; the parser drops them, as before.
5. **Names are interned per document — deferred to M40 (amended during
   M39).** The design: tag and attribute names as `Atom`s, a fixed table of
   well-known names (`id`, `class`, `style`, the HTML tags …) equal in every
   document plus a per-document table for the rest (never global: a page
   inventing names cannot grow memory that outlives it), and integer
   comparison for lookups by a well-known atom. It was built and measured
   (branch `m39-atoms-measured`): interning every name as `Document::new`
   moves a parsed fragment in cost first frames 7% on a 10,000-row page,
   and freeing the parser's name strings and reallocating every attribute
   list left heap holes that layout's allocations filled, making clicks —
   whose hit test walks the whole box tree — about 60% slower. Interning
   pays only where the names are born: in the parser, with selectors
   matching by atom. That belongs with M40's generated bindings. *(M40 split
   it to M40b: it is parser and style work, ADR 023, Alternatives. Done in
   M40b, interned where names are born, in a crate of their own:
   ADR 024.)*
6. **One rule, once.** Text editability and form-field validity (`required`,
   `pattern`) move into `dom`, on `ElementRef`. `style` (`:valid`,
   `:invalid`), `js` (submission, native editing) and `accessibility`
   (`invalid`, text-box role) ask there. (They are copies today — M20, M23
   and M24 each say so.) `dom` gains a dependency on the `pattern` crate for
   `pattern=`, which is already linear-time (ADR 003).
7. **The full tree mutation API.** `Document::append_child`, `insert_before`,
   `remove_child`, `replace_child`, `set_text_content`, `create_text_node`,
   `clone_node(deep)` — each O(1) link updates (plus the size of what is
   inserted from a fragment, which must be built anyway), each recorded as a
   `ChildList` mutation on the parent for ADR 008's invalidation. Removal
   also invalidates the following siblings' structural pseudo-classes, as
   insertion already does. `js` exposes `insertBefore`, `removeChild`,
   `replaceChild`, `cloneNode` and `document.createTextNode`.
8. **The migration is in two steps, each behind the oracles.** First every
   reader moves to `ElementRef` while storage is unchanged (no behaviour can
   change); then storage becomes the arena underneath `ElementRef`. The M31
   restyle and layout oracles and every corpus golden pass unchanged after
   each step; the oracles gain the new mutations.

## Consequences

- No crate but `dom` can hold `&mut Element` on a live document, or walk one
  by matching `Node::Element`; `scripts/check-dom-encapsulation.sh` (new)
  fails a build that does outside `dom`, test code and fragment builders
  excepted.
- `Document` no longer dereferences to an `Element`; `document.root()` is an
  `ElementRef`.
- An arena never shrinks its `Vec` while the document lives; freed slots are
  reused, so its size is the document's peak, not its churn.
- Generated bindings (M40) wrap `NodeId`s and ask the document; a wrapper
  per node is cached there, not here.

## Alternatives

- **A `DomNode` trait** implemented by `&Element` and the arena, with
  readers generic over it: no enum match per access, but every reader
  becomes generic (compile time, code size), and a trait object would cost
  more than the enum.
- **A global interner** (as Servo's `string_cache`): atoms comparable across
  documents, but hostile content grows it forever, and it is global mutable
  state (ADR 010).
- **Arena-owned detached nodes** (as browsers do, with a node's owner
  document): needs reference counting of script handles in `dom`; the
  fragment model keeps lifetime with `js`'s detached forest, which already
  frees unreachable subtrees.
