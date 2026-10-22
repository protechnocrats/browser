# Milestone 24 — Accessibility Tree

> **Status:** Done, with gaps. **Still open:** the tree exists and is tested; the OS screen-reader bridge (e.g. `accesskit`) is not built → `ROADMAP.md` backlog (accessibility).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** close the highest-priority unscoped gap `ROADMAP.md` itself
names: "flagged throughout the architecture doc as a
founding-principles-adjacent concern, never actually scheduled...
worth being direct: this is a real gap... scope it deliberately early,
don't let it drift to the end by default." This is the third of the
three milestones implemented in this pass (after Milestones 22 and
23), pulled forward specifically because of that explicit priority
flag rather than sequenced from the deferred-items plan's numbered phases like
Milestones 18–23 were.

## Builds on
Milestones 1–17 (a real DOM tree to walk), Milestone 16/19/20 (the
interactive state — focus, `checked`, `required`/validity — this
milestone reads to compute accessible STATE, not just structure).
Genuinely independent of Milestones 18/21/22/23's own subject matter
(detached nodes, navigation, animation, rich editing) — this is a
different axis of the engine entirely.

## In scope
- A new `accessibility` crate (core, platform-free — same dependency-
  direction invariant `dom`/`style`/`layout`/`rendering` already
  hold) that builds a real accessibility tree from a `dom::Element`
  tree: one `AccessibleNode` per DOM element, each with a computed
  **role** (the HTML-AAM-inspired native-semantics mapping: `<a
  href>` → link, `<button>`/`<input type=submit|button>` → button,
  `<input type=text>`/`<textarea>`/`contenteditable` → text box,
  `<input type=checkbox>` → checkbox, `<input type=radio>` → radio
  button, `<h1>`–`<h6>` → heading with a real level, `<img>` → image,
  `<ul>`/`<ol>`/`<li>` → list/list item, `<form>` → form, `<p>` →
  paragraph, everything else → generic — matching real browsers'
  "unstyled div has no special role" fallback).
- A real **accessible-name computation**, following the actual
  precedence order browsers/`accname` use (simplified, not the full
  spec algorithm): `aria-label` > `aria-labelledby` (resolves each
  referenced id's own text content) > an associated `<label for=id>`
  elsewhere in the document > `alt` (images) > the element's own text
  content (for name-from-content roles: links, buttons, headings) >
  none.
- Real **state** per node: `focused` (against the currently focused
  element id), `checked` (for checkbox/radio, from the real `checked`
  attribute Milestone 19 already gives them), `disabled`, `required`,
  and a narrower, a11y-specific `invalid` (required-and-empty only —
  not `style`'s full `pattern`-aware `:invalid`, a documented scope
  cut, see implementation notes).
- Proof this reflects the real, current document: a test built against
  `app`'s own actual demo HTML (not just synthetic isolated fixtures),
  confirming real elements (the search input, the submit button, a
  checkbox, the navigation link, headings) get correct roles/names.

## Out of scope (deferred)
- **Real OS/AT-SPI screen-reader integration** (actually exposing this
  tree to a live screen reader via a platform accessibility API —
  Linux AT-SPI, Windows UIA, macOS NSAccessibility, or a cross-platform
  layer like the `accesskit` crate that wraps all three). This is the
  literal "genuinely separate work" the deferred-items plan's Phase J calls it —
  a new external dependency, platform-specific wiring, and a real
  screen reader to verify against, none of which fit alongside the
  rest of this pass. The tree this milestone builds is the exact data
  a future `accesskit` integration would consume; nothing here would
  need to be redesigned to wire it up later.
- Full spec-accurate accessible-name computation (the real `accname`
  algorithm has additional rules — `aria-hidden` subtree exclusion,
  CSS-generated content, `title` attribute fallback, embedded-control
  name computation) — the precedence chain above covers the common,
  high-value cases; the rest is real, incremental depth for later.
- ARIA roles/states beyond native HTML semantics (`role="..."`
  overrides, `aria-checked`/`aria-expanded`/etc. on non-native
  widgets) — this milestone computes role/state from native HTML
  semantics only; explicit ARIA authoring is a real, separate
  extension once native coverage is solid.
- Color-contrast checking, focus-order/landmark auditing, or any other
  "accessibility linting" feature — a different tool shape (a
  checker/auditor) than "expose the page's real structure to AT,"
  which is this milestone's actual goal.

## Why this scope
`ROADMAP.md` is explicit and direct about this being the one item that
keeps drifting: flagged in the architecture doc's founding principles,
never scheduled. The highest-leverage, lowest-risk slice is the real
DATA MODEL — role/name/state computation against the actual DOM — kept
strictly on the "core" side of this project's dependency-direction
invariant, the same way `dom`/`style`/`layout` already are. Wiring it
to a live screen reader is real, substantial, platform-specific
integration work on the SAME side of that boundary `gpu_backend`/
`shell_desktop` already occupy — deferred with the same "documented,
not silently dropped" discipline every other milestone this session
has used for its own scope cuts.

## Acceptance criteria
- [x] Every native HTML element with a well-known ARIA role (link,
      button, text box, checkbox, radio button, heading with correct
      level, image, list/list item, form, paragraph) gets that role;
      everything else gets a generic/no-role fallback — not silently
      dropped from the tree. **Verified**: `link_role_requires_a_real_href`,
      `button_role_covers_button_tag_and_input_submit_button`,
      `checkbox_and_radio_roles`,
      `plain_text_input_and_textarea_and_contenteditable_are_textboxes`,
      `heading_levels_are_real_not_just_tagged_as_headings`,
      `list_list_item_form_paragraph_and_image_roles`,
      `an_unstyled_div_gets_the_generic_fallback_role`
      (`crates/accessibility/src/lib.rs`).
- [x] Accessible name computation follows the correct precedence:
      `aria-label` beats `aria-labelledby` beats an associated
      `<label>` beats `alt` beats text content, verified with a test
      per precedence level (not just the happy path). **Verified**:
      `aria_label_wins_over_everything_else`,
      `aria_labelledby_resolves_a_referenced_elements_text_content`,
      `a_label_for_beats_text_content_and_alt`,
      `alt_provides_the_name_for_an_image`,
      `text_content_names_a_link_or_button_with_nothing_else_set`,
      `an_input_never_gets_its_name_from_its_own_text_content`,
      `empty_aria_label_falls_through_to_the_next_precedence_level`,
      `text_content_name_collapses_source_formatting_whitespace`
      (the last one caught and fixed a REAL bug this milestone's own
      end-to-end test surfaced — see implementation notes).
- [x] Interactive state (`focused`, `checked`, `disabled`, `required`,
      `invalid`) is computed correctly and changes when the underlying
      DOM/focus state changes — verified against a real, already-
      cascading-correctly document, not a hand-built fixture only.
      **Verified**: `focused_state_matches_the_focused_element_id`,
      `checked_is_some_only_for_checkbox_and_radio_roles`,
      `disabled_and_required_reflect_the_real_attributes`,
      `invalid_is_true_only_for_a_required_and_empty_text_box`, plus
      the real-document test below re-checks all of these against a
      genuinely parsed page, not just synthetic fixtures.
- [x] The tree built from `app`'s own real demo document assigns
      correct roles/names to real, identifiable elements in it (not
      just a minimal synthetic test document). **Verified**:
      `accessibility_tree_from_the_real_demo_html_has_correct_roles_and_names`
      (`app/src/lib.rs`) parses `app::HARDCODED_HTML` — the ACTUAL
      constant `main.rs` renders, not a copy — and checks real roles/
      names/state on the search input, submit button, checkbox,
      navigation link, and `contenteditable` note. `HARDCODED_HTML`
      was moved from a private `const` in `main.rs` to `pub const` in
      `app/src/lib.rs` specifically to make this possible, the same
      "factor out of `main.rs` so it's testable" move this crate's own
      module doc comment already describes for `fetch_page`.

## Implementation notes

- **New `accessibility` crate, added to the workspace and to
  `scripts/check-dependency-direction.sh`'s checked-crate list** —
  depends only on `dom`, exactly the same "core, platform-free" tier
  `dom`/`style`/`layout`/`rendering` already occupy. It does NOT
  depend on `style` or `js`: validity (`invalid`) and focus
  (`focused`) are recomputed from raw DOM attributes/an externally-
  passed focused-id rather than reusing `style::is_element_valid` or
  `js::JsEnvironment::focused_element_id` directly — a deliberate,
  narrow duplication (see `AccessibleState::invalid`'s own doc
  comment), the same "a small duplicate beats the wrong dependency"
  call `style`'s own `is_element_valid` already makes for its `dom`
  duplication, kept consistent rather than reached for a shortcut.
- **A real bug this milestone's own real-document test caught and
  fixed, not just passed around**: the FIRST version of
  `accessible_name` used `text.trim()` (ends only) on `text_content()`
  output. Against `accessibility`'s own hand-built synthetic
  fixtures, that looked fine (values were already single-line
  strings). Running the SAME code against `app`'s real, multi-line,
  indented demo HTML immediately failed —
  `<a id="nav-link" href="...">a real\n external link</a>`'s source
  formatting leaked straight into the computed accessible name as a
  literal embedded newline and indentation. Fixed with a real
  `normalize_whitespace` helper (collapses any whitespace run to one
  space) applied everywhere a name is derived from text content —
  exactly the value of testing against a REAL document instead of
  only synthetic ones, which this milestone's own acceptance criteria
  insisted on for this reason.
- **The `\0text-content:` sentinel-prefixed map key** (in
  `collect_labels`) is a real, deliberate choice, not an accident:
  `aria-labelledby` and `<label for>` are two DIFFERENT associations
  that can both target the same element by different means (a
  `<label for="x">` names `x` from the label's OWN text; an
  `aria-labelledby="x"` elsewhere names ITSELF from `x`'s own text) —
  sharing one `HashMap` with a sentinel prefix on one of the two key
  spaces avoids a second full tree walk for what would otherwise be a
  second, nearly-identical map.
- **`role_for`'s `contenteditable` branch mirrors `js`'s own
  Milestone-23 rule exactly** (bare attribute or `="true"`, only a
  literal `"false"` opts out) — duplicated logic across the crate
  boundary, same pattern as every other `app`/`js` split in this
  project, now extended to `accessibility`/`js` too (neither of which
  is allowed to depend on the other).
- 24 new tests in `accessibility` (23 unit/property tests plus its own
  from-a-realistic-document integration test) and 1 new test in `app`
  (against the REAL demo HTML, not a copy) — 518 tests total across
  the workspace, zero failures. `./scripts/check-dependency-direction.sh`
  passes with `accessibility` added to its checked list.
- **Not wired into `main.rs`'s render loop** — there is genuinely
  nothing to wire it TO yet (no platform AT-SPI/UIA/NSAccessibility
  consumer exists in this codebase), so adding a call site with no
  real downstream effect would be motion without function, not
  progress. The `app`-level test is the closest thing to "real usage"
  available without that platform integration — see "Out of scope"
  above for what a real follow-up would need.
