# Milestone 18 — Detached DOM Nodes & `document.createElement`

> **Status:** Done. **Still open:** nothing from this milestone's scope. (Its
> two wrapper types and the stale-handle gap below are gone since Milestone
> 40: one wrapper per node, attached or not, ADR 023 §4.) Its deferred items are listed under *Out of scope* below; each has a home in `ROADMAP.md` (§4, backlog).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** give `js` a real object model for an element that exists but
isn't attached to the document yet — the single architectural gap
named repeatedly since Milestone 13's own audit, and the highest-
leverage item in the whole next phase of work: several later
milestones (real `appendChild`, `contenteditable`, CSS
`transition`/`@keyframes` interpolation) are blocked on this, directly
or indirectly.

## Builds on
Milestone 13 (`appendChild(tagName, textContent)` — the one-call
simplified signature this milestone replaces with the real two-step
form), Milestone 11 (the DOM binding pattern), `dom::element_ptr_key`
(the existing pointer-identity mechanism this milestone's stable
handle is a cousin of — see "Why this scope" for the difference).

## In scope
- A real per-element identity/handle system that survives across JS
  calls — `dom::element_ptr_key` is explicitly NOT reusable for this
  (its own doc comment states it's only valid "while the SAME borrowed
  tree is still alive," i.e. within one function call, not across
  separate JS statements like `var el = document.createElement('div')`
  followed later by `parent.appendChild(el)`).
- `document.createElement(tagName)` returning a JS-visible handle to a
  real, detached `dom::Element` that isn't part of the live tree.
- Real `element.appendChild(childHandle)` — the actual two-argument-free
  DOM signature — replacing Milestone 13's `appendChild(tagName,
  textContent)` simplification. The old one-call form can be removed
  once this lands (nothing else in the demo depends on it staying).
- `element.setAttribute`/`getAttribute`/`removeAttribute` on a detached
  element (needed to configure it — set an id, a class — before
  attaching it).
- Enough of `dom`'s existing query surface (`find_by_id`, etc.) working
  correctly once a created-then-appended element is actually in the
  tree — no special-casing should be needed there if the handle design
  is right.

## Out of scope (deferred)
- `document.createTextNode`, `element.removeChild`'s real signature
  (Milestone 13's `removeChild` may already be close enough — verify,
  don't rebuild speculatively), `cloneNode`, `insertBefore`,
  `replaceChild` — grow the mutation API incrementally, same policy
  every JS milestone since 11 has used, not all at once here.
- `contenteditable` — a real, separate, larger problem that NEEDS this
  milestone's identity system but isn't part of it.
- Any change to `dom::element_ptr_key`'s existing use (parallel style
  cache, hit-testing) — that mechanism is correct and sufficient for
  its own single-borrow use case; this milestone adds a NEW mechanism
  alongside it, not a replacement.

## Why this scope
Every later milestone that touches "JS creates something new" runs
into the same wall: this engine's whole object model is "re-find an
element by `id` string in the live tree," which has no way to
represent something not in the tree yet. Fixing this once, properly,
here — rather than each later milestone inventing its own narrow
workaround — is exactly the "do it early, several other items get
easier afterward" reasoning the deferred-items plan scoped this milestone with.

## Acceptance criteria
- [x] `document.createElement('div')` returns a value JS can hold in a
      variable, configure (`setAttribute`, `.textContent`), and later
      pass to `appendChild` — and the element only becomes visible
      (renders, is findable via `getElementById`) after that
      `appendChild` call, not before. **Verified**:
      `created_element_is_detached_until_appended`,
      `created_element_becomes_findable_after_append_child`
      (`crates/js/src/lib.rs`).
- [x] A script that creates several elements, configures them, and
      appends them in a loop behaves correctly — no id collisions, no
      elements appearing before they're attached, no panic if a
      created-but-never-attached element is simply dropped
      (garbage-collected on the JS side without ever entering the DOM).
      **Verified**: `creating_and_appending_many_elements_in_a_loop_works`,
      `dropping_created_elements_without_appending_does_not_panic`.
- [x] The existing Milestone 13 `appendChild`-mutation demo still works
      after switching to the real two-step signature — same visible
      behavior, real API shape. **Verified**: the same two tests this
      capability originally shipped with
      (`append_child_adds_a_new_element_and_sets_dirty`,
      `append_child_sets_dirty_flag`) now exercise the real
      `document.createElement(...)` + `appendChild(el)` form and still
      pass; `app`'s live demo also gained a real, visible instance of
      this (see Implementation notes).
- [x] `dom::element_ptr_key`'s existing callers (parallel style
      resolution, hit-testing) are unaffected — this milestone adds a
      new mechanism, it doesn't touch or slow down that one. **Verified**:
      neither `element_ptr_key`/`find_by_ptr_key` nor any of their
      call sites (`style::resolve_style_tree_parallel`,
      `layout::hit_test_path`, `js::dispatch_click`) were touched; all
      existing click-dispatch tests pass unchanged.

## Implementation notes

- **One new identity field, not a rewrite of the existing one**:
  `dom::Element` gained `pub node_id: u64`, assigned from a
  process-global `AtomicU64` counter in `Element::new` — the first
  counter-based id allocator in this codebase (confirmed by search
  before adding it). `find_by_node_id`/`find_by_node_id_mut` mirror
  `find_by_id`/`find_by_id_mut`'s exact recursive shape.
  `PartialEq` had to be hand-written (excluding `node_id`) instead of
  derived — otherwise every existing test that compares a
  parsed/hand-built "expected" tree against a real one would have
  started failing, since two content-identical elements now get
  different `node_id`s by construction order. A dedicated test
  (`equality_ignores_node_id`) guards this directly.
- `html_parser`'s tree builder had two direct `Element { .. }`
  struct-literal construction sites (bypassing `Element::new`) — found
  by grepping the whole workspace before writing any code, not by
  trial and error; fixed to route through `Element::new` so parsed
  elements get real node ids too. `style`/`layout`'s test helpers
  already went through `Element::new`.
- **Two separate wrapper types, deliberately not a unification**:
  `getElementById`/`querySelector`'s existing id-string-based wrapper
  is unchanged except its `appendChild`; `createElement` returns a new,
  separate node_id-based wrapper
  (`build_detached_element_wrapper`) that only ever resolves within the
  detached forest. This is narrower than rewriting the whole object
  model around `node_id`, and a real, explicit consequence: a
  `createElement` handle held across its own attachment goes stale
  (further mutation through that SAME variable silently no-ops —
  `a_stale_handle_after_attachment_silently_no_ops_not_panics` tests
  this exact case). Continuing to mutate the element via
  `document.getElementById` after attachment works correctly. *(Unified in
  Milestone 40: the test is now
  `a_created_elements_handle_keeps_working_after_attachment`.)*
- **`app`'s live demo** now has a real, visible instance of this
  milestone's capability: each click on the counter button also
  `createElement`s a small `<div>`, sets its text, and `appendChild`s
  it into a new `<div id="click-log">` — confirmed rendering correctly
  on real hardware (Wayland/KDE, this session's environment) via the
  updated headline text actually appearing on screen; the click-driven
  log-entry behavior itself is covered by the `js` crate's automated
  tests (no input-injection tool was available this session to
  synthesize a real click for an on-screen confirmation of that
  specific path).
- 9 new tests in `crates/js/src/lib.rs`, 5 new in `crates/dom/src/lib.rs`
  (unique node_ids, `find_by_node_id_mut`, `extract_descendant_by_node_id`
  — including a nested/grandchild case — `remove_attr`, and the
  equality-ignores-node_id regression guard). 416 tests total across
  the workspace, zero failures; dependency-direction lint still passes
  (no new external dependency — `AtomicU64` is `std`).
