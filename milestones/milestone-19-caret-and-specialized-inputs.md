# Milestone 19 — Visible Caret, Click-to-Position & Specialized Inputs

> **Status:** Done. **Still open:** richer input types (date, colour, range, file) are in the `ROADMAP.md` backlog (forms and input).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** close the most visible remaining Milestone 16 gaps — text
editing has no visual cursor and can't be clicked into position — and
add the highest-value specialized input types (checkbox/radio) that
Milestone 16 explicitly deferred.

## Builds on
Milestone 16 (form elements — text editing, focus, tab order already
work; this milestone makes editing visually legible and adds input
types), Milestone 18 (not a hard dependency, but land after it —
`setAttribute` on a fresh `<input type="checkbox">` benefits from the
same machinery).

## In scope
- **Visible caret indicator**: a blinking (or at least visible) line
  painted at the focused text input's current cursor position.
  Milestone 16's own notes on why this was deferred are the starting
  point: needs focus + cursor-position state threaded from
  `js::JsEnvironment` into `layout`/`rendering`, which don't know about
  JS state today (a deliberate layering boundary — see the
  architecture doc). This milestone's job is closing that gap
  correctly, likely via a paint-time overlay (same shape as this
  session's focus-outline fix — a post-layout pass, not a cascade
  change) rather than by weakening the layering boundary itself.
- **Click-to-position**: clicking partway through an input's existing
  text places the cursor at that offset, not just focuses the element.
  Needs sub-element hit-testing (a text offset within a box, not just
  "which element"), extending `handle_input`'s `MouseClicked` path.
- **`<input type="checkbox">`**: renders a real checked/unchecked
  visual state, toggles on click, `checked` is JS-observable
  (`element.checked`, not just an attribute string).
- **`<input type="radio">`**: same, plus real radio-group semantics —
  checking one radio in a `name`-grouped set unchecks the others.

## Out of scope (deferred)
- `range`/`date`/`color`/`file` input types — checkbox/radio are the
  highest-value remaining types; the rest are real, separate, smaller
  additions once these two prove the pattern.
- Client-side validation (`required`, `pattern`, `:invalid` styling) —
  Milestone 20.
- `contenteditable` — needs Milestone 18's identity system AND this
  milestone's caret work, but is its own substantially larger problem
  (arbitrary rich content, not a single text run).
- Text selection (click-drag to select a range) — a related but
  distinct feature from click-to-position; its own later item.

## Why this scope
Milestone 16 shipped with editing that genuinely works but is
invisible (no caret) and imprecise (no click-to-position) — real,
named limitations that make the existing text-input feature feel
broken even though it isn't. Checkbox/radio are the two most common
real-world form controls after text/submit, already implemented.
Closing these together is a contained, well-understood unit of layout
+ paint + input-dispatch work, not a new algorithm class.

## Acceptance criteria
- [x] Clicking into a text input shows a visible caret at the click
      position (not just at the start/end of existing text). **Verified**
      — `app::caret_x_for_cursor`/`cursor_index_for_click_x` (new,
      symmetric functions sharing one word-walking iterator) compute
      real pixel positions from the input's own laid-out word boxes;
      the caret paints as a new display-list overlay in `build_scene`,
      and clicking sets the cursor via the new
      `JsEnvironment::set_cursor_position`.
- [x] Typing, arrow keys, and backspace all move/use the caret's
      current visual position correctly — the caret and the actual
      edit-cursor state (`js::JsEnvironment`'s existing cursor tracking)
      never disagree. **Verified** — the caret reads the SAME
      `cursor_position(id)` `apply_native_text_editing` already
      mutates; no new, separate cursor-state was introduced.
- [x] A checkbox toggles visually and via `element.checked` on click.
      **Verified**: `checked_accessor_reads_and_writes_the_checked_attribute`,
      `clicking_a_checkbox_toggles_it` (`crates/js/src/lib.rs`);
      `checked_checkbox_renders_a_filled_background` (`crates/layout/src/lib.rs`).
      **One real scope note, not silently assumed**: "its state
      round-trips correctly through a form's submit event" is true in
      the sense that a `submit` listener can read `.checked` and it's
      already correct by then (native toggle happens before listeners
      fire, same ordering `apply_native_text_editing` established) —
      but this engine still has NO actual form-data
      serialization/collection at all (unchanged from Milestone 16;
      that's Milestone 21's job, listed there as "form fields as query
      parameters"). Nothing here claims otherwise.
- [x] A `name`-grouped set of radio buttons enforces mutual exclusion —
      checking one visually and programmatically unchecks the others
      in the same group. **Verified**:
      `clicking_a_radio_unchecks_others_in_the_same_group`,
      `clicking_a_radio_in_a_different_group_does_not_affect_it`;
      `radio_renders_fully_round_via_border_radius`.

## Implementation notes

- **Caret positioning is anchored on REAL layout, not re-derived
  geometry**: `caret_x_for_cursor` walks the input's own synthesized
  word `LayoutBox`es (the exact boxes `layout::layout_element` already
  positions for a text input's `value` — see Milestone 16) and only
  measures the PREFIX within whichever word the cursor falls in, via
  `text::Font::measure`. `cursor_index_for_click_x` is the exact
  reverse (nearest character boundary to a click x), sharing one
  `walk_input_words` iterator so the whitespace/word-boundary
  bookkeeping has a single implementation.
- **Documented, narrow scope boundary**: both functions assume the
  input's value renders on ONE line — correct for what a real text
  input actually does (scrolls, never wraps), but this engine's
  "value renders as ordinary wrapping inline content" model (Milestone
  16) doesn't itself prevent wrapping a very long value, so a wrapped
  value's caret may land on the visually wrong line. Real, bounded,
  matches this project's existing "close enough for the common case"
  discipline elsewhere (e.g. `pre`'s whitespace handling).
- **`document.createElement`-built checkboxes/radios work for free**:
  since sizing/visual state key off the live element's `type`/`checked`
  attributes (not anything computed at creation time), a checkbox
  built via Milestone 18's `document.createElement` and attached via
  real `appendChild` renders and toggles identically to one parsed
  from HTML — no special-casing needed, confirming Milestone 18's own
  "no special-casing should be needed... if the handle design is
  right" framing.
- **`dom::Element::find_by_ptr_key_mut`** (new) — the checkbox/radio
  native toggle in `dispatch_click` needed to MUTATE an element found
  via a hit-test ptr key; only an immutable `find_by_ptr_key` existed
  before. Purely additive; every existing `find_by_ptr_key` caller
  (parallel style cache, hit-testing) is untouched.
- 12 new tests (5 in `js`, 6 in `layout`, 1 in `dom`). 428 tests total
  across the workspace, zero failures. `app`'s live demo gained a real
  checkbox and a 3-way radio group with a status line, confirmed
  rendering correctly on real hardware (window opened, updated
  headline text visible) — clicking them specifically wasn't
  re-confirmed on-screen this pass (no input-injection tool available,
  same limitation noted for Milestone 18), covered instead by the
  automated `js`/`layout` tests above.
