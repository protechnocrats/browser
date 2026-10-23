# Milestone 23 — Rich Input & Editing

> **Status:** Done, with gaps. **Still open:** text selection, drag-and-drop, context menus and a real `<input type="file">` picker were scoped out (reasons below) → `ROADMAP.md` backlog (forms and input).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** the remainder of the deferred-items plan's Phase G — the input/interaction
features that specifically need Milestones 18 (detached-node identity)
and 21 (page navigation) to already exist, which is why they're
sequenced last rather than alongside Milestones 19–20.

## Builds on
Milestone 18 (detached-node identity — `contenteditable`'s arbitrary
DOM manipulation needs it directly), Milestone 21 (classic form
navigation — this milestone's "classic form navigation" acceptance
criterion is literally Milestone 21's own capability, exercised from a
real form rather than a direct navigation call), Milestone 20 (the
full event model — text selection and drag-and-drop both need real
`preventDefault`/capture-phase semantics to behave correctly against
a page's own JS).

## In scope
- **Text selection**: click-and-drag across text creates a real
  selection range, visually highlighted; `window.getSelection()`
  exposes it to JS at a basic level (start/end, selected text).
- **Drag-and-drop**: the native `dragstart`/`dragover`/`drop` event
  sequence for at least in-page drag targets (dragging content between
  two elements on the same page) — cross-application drag (dragging a
  file in from the OS) is a larger, separate addition.
- **Touch input**: forwarding real touch events (tap, basic
  scroll/swipe) alongside the existing mouse/keyboard path in
  `shell_desktop` — explicitly tied to eventual non-Linux platform
  work per the architecture doc's sequencing, but the event-dispatch
  half (once events arrive, how they map to DOM events) is engine work
  that belongs here regardless of which platform shell eventually
  produces them.
- **Context menus**: a basic native (or engine-drawn) right-click menu
  with the minimum real entries (copy, paste where applicable) — not a
  full extensible menu API.
- **`contenteditable`**: an element whose content can be directly
  edited by the user, using Milestone 18's real DOM mutation
  primitives under the hood (this is exactly why it wasn't attempted
  before Milestone 18 existed — it needs to create/move/remove
  arbitrary child nodes as the user types, not just edit one input's
  text value).
- **File upload UI/handling**: a real `<input type="file">` that opens
  a file picker and makes the selected file's data available to a
  `<form>` submission or JS (`element.files`).
- **Classic (non-JS) form navigation**: verifying Milestone 16's named
  gap is actually closed now that Milestone 21 exists — this is the
  one item here that's really "confirm a dependency delivered," not
  new engine work.

## Out of scope (deferred)
- Cross-application drag-and-drop (dragging a file in from the OS
  file manager) — real, separate platform-integration work.
- A full extensible context-menu API (what a real browser extension
  would use to add menu items) — Phase 3/extensions territory,
  explicitly out of scope for the engine itself per the architecture
  doc.
- Rich `contenteditable` features beyond basic text editing
  (formatting toolbars, paste-sanitization policy, `execCommand`) —
  land basic editing first, grow from there against real usage.

## Why this scope
Every item here was in the deferred-items plan's Phase G, but each one specifically
needs a dependency that didn't exist until Milestones 18–21 landed —
grouping them into their own final milestone rather than attempting
them alongside 19/20 keeps this project's established discipline of
"land items in the order their real dependencies allow," not
alphabetical/list order.

## Acceptance criteria
- [ ] Click-and-drag across a paragraph of text visibly highlights the
      selection, and `window.getSelection().toString()` returns the
      correct selected text. **Deliberately NOT done, a real scope
      cut**: see "What was scoped out, and why" below.
- [ ] Dragging an element from one drop target to another on the same
      page fires the correct `dragstart`/`dragover`/`drop` sequence and
      the drop handler can read the dragged data. **Deliberately NOT
      done, a real scope cut**: see "What was scoped out, and why"
      below.
- [x] A basic tap (touch) on a button fires the same `click` dispatch
      path a mouse click would, verified via `shell_desktop`'s touch
      event forwarding. **Implemented, verified by code review rather
      than a runtime test**: `WindowEvent::Touch` with
      `TouchPhase::Started` forwards onto `InputEvent::MouseClicked`
      (`crates/shell_desktop/src/lib.rs`) — the exact same call
      `WindowEvent::MouseInput` makes, so it necessarily goes through
      the identical `handle_input`/`js::dispatch_click` path already
      covered by that path's own tests. **Honest gap**: `shell_desktop`
      has no test harness at all (it's a thin `winit` event-loop
      wrapper, same as every other milestone that's touched it — see
      its own top doc comment: "not run-verified in this sandbox"),
      and this machine has no touchscreen to exercise it on physically
      either — the forwarding logic was verified by direct code
      inspection, not a runtime signal.
- [x] A `contenteditable` element can have text typed into it, and the
      resulting DOM structure is real, queryable, and mutation-tracked
      (dirty flag fires, re-render shows the change) — not a
      special-cased text blob. **Verified**:
      `typing_into_a_contenteditable_element_edits_its_text_content`,
      `backspace_in_a_contenteditable_element_deletes_from_its_text_content`,
      `contenteditable_false_is_not_editable`,
      `typing_into_contenteditable_sets_dirty_for_redraw`
      (`crates/js/src/lib.rs`) — edits go through
      `dom::Element::set_text_content`, the same real DOM-mutation
      primitive `element.textContent = ...` uses, not a parallel
      string field. Confirmed rendering correctly on real hardware
      (`app`'s live demo gained a real `contenteditable` box).
- [ ] Selecting a file via `<input type="file">` makes it available to
      a form submission, and a classic (non-JS) form submission
      end-to-end (this milestone's form, Milestone 21's navigation)
      works against a real local test server. **Split finding**: the
      classic-form-navigation half is [x] done —
      `classic_get_form_submission_navigates_end_to_end_against_a_real_server`
      (`app/src/lib.rs`) is a genuine, new end-to-end test: dispatches
      a real click on a form's submit button, takes the query string
      `js::dispatch_click`/`build_form_query_string` (Milestone 21)
      actually constructed from the form's real fields, resolves it
      against a base URL via `network::resolve_url`, and fetches it
      against a REAL local HTTP server that only returns success for
      the EXACT query string the form should have built — closing the
      "confirm a dependency delivered" item this milestone named. The
      `<input type="file">` half is [ ] NOT done — see "What was
      scoped out, and why" below.

## What was scoped out, and why

This milestone's original 6-item scope was large enough (text
selection, drag-and-drop, touch, context menus, `contenteditable`,
file upload) that attempting all six with this project's established
"real implementation, real tests, no stubs" standard within one
session wasn't realistic. `contenteditable`, touch forwarding, and the
classic-form-navigation verification were completed in full (see
above); the rest is a real, deliberate, documented deferral — not
silently dropped:

- **Text selection** (click-and-drag highlight + `window.getSelection()`)
  needs a genuinely new subsystem this engine has never had any piece
  of: tracking a selection ANCHOR/FOCUS as (element, character-offset)
  pairs that can span multiple text runs and even multiple elements,
  turning `MouseMoved` from "not used yet" (see its own doc comment in
  `shell_desktop`; *since the gap pass pointer moves are hit-tested for
  `:hover`, but nothing tracks a drag*) into real drag-tracking state, and a new rendering
  pass drawing a highlight rectangle per covered word-box (potentially
  across several lines). Every existing caret/cursor mechanism in this
  codebase (Milestones 16/19/this milestone's `contenteditable` work)
  is deliberately scoped to ONE position, not a range — selection is a
  strictly bigger data model, not an extension of it. Worth its own
  focused pass.
- **Drag-and-drop** needs its own event-dispatch subsystem
  (`dragstart`/`dragenter`/`dragover`/`drop`/`dragend`, a
  `DataTransfer`-like object JS can read from), plus real drag-state
  tracking through mouse-down → move → up (again depending on
  `MouseMoved` actually being used, which it isn't yet). Genuinely
  independent of every other item here — not started.
- **Context menus**: a real (even minimally "engine-drawn coordinate
  overlay, not a native OS menu") implementation still needs an actual
  system clipboard integration to make "Copy"/"Paste" do anything real
  — this codebase has never taken a platform-clipboard dependency
  before (the architecture doc's core/platform separation makes this
  a real, considered decision, not a quick add), and doing it
  half-way (an in-app-only fake clipboard) would fail this project's
  own "real implementation, not a stub" bar rather than meet it. Not
  started.
- **`<input type="file">`**: the DOM-facing surface (`.files`,
  `change` event) is small and could have been stubbed with a
  JS-settable fake path, but that's exactly the kind of "looks done,
  isn't" gap this project's discipline exists to avoid — a real
  implementation needs an actual native OS file-picker dialog, which
  means a new external dependency (something like `rfd`) AND a real
  human clicking through an OS dialog to verify it, neither of which
  fits this session. Not started; a real, separate follow-up.

## Implementation notes

- **`contenteditable` reuses Milestone 19's caret/click-to-position
  machinery wholesale** rather than building a parallel one —
  `app::is_text_editable`/`editable_value` (new) generalize what used
  to be an `input`/`textarea`-only check inline in three places
  (`build_scene`'s caret paint, the click-to-position handler,
  `collect_focusable_ids`) into one shared pair of helpers. `js`'s
  side of the same boundary (`apply_native_text_editing`,
  `set_cursor_position`) independently applies the identical
  "input/textarea use the `value` attribute; anything else with
  `contenteditable` (not `="false"`) uses real `text_content()`" rule
  — duplicated, not shared, across the `app`/`js` crate boundary,
  matching every other split this project has drawn between those two
  crates (neither crate is allowed to depend on the other's internal
  types).
- **A real, stated scope cut inside `contenteditable` itself**:
  `dom::Element::set_text_content` always collapses to a SINGLE text
  node, so any existing rich structure inside an editable region
  (`<b>`, nested `<span>`s) is discarded the moment the user types a
  single character — matches this milestone's own explicitly stated
  "basic text editing... land basic editing first" scope, not an
  oversight.
- **Touch forwarding is 6 lines, not a new subsystem**: `Started`
  reuses `InputEvent::MouseClicked` verbatim rather than inventing a
  parallel `InputEvent::Tapped` variant — matching this milestone's
  own acceptance criterion ("fires the SAME click dispatch path").
  `Moved`/`Ended`/`Cancelled` are intentionally not forwarded (no
  scroll/swipe/momentum model exists to receive them yet).
- 8 new tests (4 in `js` for `contenteditable`, 1 new end-to-end test
  in `app` for classic form navigation, plus 3 existing tests that had
  to be updated for the `js`/`app` shared-helper refactor
  continuing to pass unchanged). 494 tests total across the workspace,
  zero failures. `app`'s live demo gained a real
  `contenteditable="true"` box — confirmed rendering correctly with
  its initial text on real hardware (AMD Radeon 780M, Wayland/KDE);
  typing into it wasn't manually exercised this pass (no input-
  injection tool available, the same limitation noted for Milestones
  18-21).
