# Milestone 20 — Full Event Model & Client-Side Validation

> **Status:** Done. **Still open:** ~~Enter-to-submit~~ (done in the gap pass, 2026-10-21); the rest of *Out of scope* below is in the `ROADMAP.md` backlog (forms and input).
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** the DOM event system grows from "bubbling-phase click/keydown/
scroll only" (Milestone 15's explicit scope cut) to the real event
model real sites' JS actually depends on — capture phase,
`stopPropagation`/`preventDefault` — and forms gain basic client-side
validation.

## Builds on
Milestone 15 (event dispatch/bubbling — this milestone extends the
same `js::invoke_listeners` mechanism, doesn't replace it), Milestone
19 (specialized input types — validation rules like `required` apply
to them too).

## In scope
- **Capture phase**: `addEventListener('click', handler, true)` fires
  handlers root-to-target before the existing bubble (target-to-root)
  phase runs — the other half of the real DOM event flow.
- **`stopPropagation()`**: called on an event object, halts further
  bubbling/capturing for that dispatch.
- **`preventDefault()`**: suppresses whatever "native" behavior would
  otherwise follow — concretely, this is what Milestone 16's
  `apply_native_text_editing` doc comment already names as the reason
  native text editing currently happens unconditionally ("Real browsers
  let a JS listener call `preventDefault()` to suppress this;
  `preventDefault` isn't implemented... so native editing always
  happens here, unconditionally" — this milestone closes that specific
  named gap).
- A real, if still not spec-complete, event object: `type`, `target`,
  `stopPropagation()`, `preventDefault()`, `defaultPrevented`.
- **Client-side form validation**: `required` (a required field blocks
  submission if empty), `pattern` (regex match blocks submission if it
  fails), and `:invalid`/`:valid` CSS pseudo-classes becoming real
  (the first real interactivity-state threaded into the cascade —
  `style::matches_compound`'s `PseudoClass::Interactive` arm, which has
  always returned `false`, gets its first real case).

## Out of scope (deferred)
- `:hover`/`:focus`/`:active` matching in the cascade — related (same
  `PseudoClass::Interactive` arm) but a separate, larger piece: it
  needs live mouse-position/focus state threaded into every style
  resolution, not just a validation pass at submit time. Land
  `:invalid`/`:valid` first since form validation is the more
  contained, higher-value case; revisit `:hover`/`:focus` once real
  usage demands it.
- `currentTarget` and the rest of the full DOM event object surface
  (`bubbles`/`cancelable` flags, `composedPath()`, custom events) —
  `currentTarget` specifically would need updating per phase-step as
  the SAME shared event object bubbles (which element is "currently"
  handling it changes each step); real, contained, deliberately left
  for whenever a real site's listener actually needs it — `target`
  alone already covers the overwhelmingly common case. Grow
  incrementally, same policy every JS milestone has used.
- Enter-key-submits-a-form — still not implemented (a real, separate
  gap from this milestone's actual scope; tracked at Milestone 21,
  which already needs to touch form-submission for classic
  navigation).
- Async/regex-heavy custom validators running off the main thread —
  the deferred-items plan's own note on this: worth keeping in mind if a real site's
  validation turns out to be slow enough to matter, not a default
  design requirement for this milestone.

## Why this scope
`preventDefault` in particular is named directly in Milestone 16's own
implementation notes as a real, specific gap blocking correct-per-spec
text editing. Capture phase and `stopPropagation` are the two most
commonly-depended-on event features real sites' JS assumes exist.
Validation is grouped here (not with Milestone 19's specialized inputs)
because it's fundamentally an event-flow concern — validation runs at
submit time and can block the default submit behavior, which is
exactly what `preventDefault` newly makes expressible.

## Acceptance criteria
- [x] A capture-phase listener on an ancestor fires before a
      bubble-phase listener on the clicked element itself, for the
      same click. **Verified**:
      `capture_phase_fires_before_target_which_fires_before_bubble`
      (`crates/js/src/lib.rs`) — checks the actual firing order
      (`outer-capture, inner-target, outer-bubble`), not just that all
      three fired.
- [x] `event.stopPropagation()` called in a target's listener prevents
      an ancestor's listener (capture or bubble) from firing for that
      same dispatch. **Verified**:
      `stop_propagation_on_the_target_prevents_the_ancestor_bubble_listener`.
- [x] `event.preventDefault()` called in a `keydown` listener on a
      focused text input suppresses the native character-insertion
      behavior for that keypress — verifying the exact gap named in
      Milestone 16's own notes is closed. **Verified**:
      `prevent_default_on_keydown_suppresses_native_text_editing`. Also
      extended to the OTHER native default action this codebase has —
      checkbox/radio toggle (Milestone 19) — since fixing the
      event-then-default-action ordering to support `preventDefault`
      at all meant fixing it consistently, not just for keydown:
      `prevent_default_on_a_checkbox_click_suppresses_the_native_toggle`.
- [x] A `<form>` with a `required` empty field does not fire `submit`
      when its button is clicked (and does fire once the field is
      filled); `:invalid`/`:valid` CSS actually changes the field's
      rendered style to match. **Verified**:
      `submit_is_blocked_when_a_required_field_is_empty`,
      `submit_fires_once_the_required_field_is_filled`, plus the same
      pair for `pattern` (`submit_is_blocked_when_a_field_does_not_match_its_pattern`,
      `submit_fires_when_a_field_matches_its_pattern` — a real
      ECMAScript-flavor regex match via `regress`, not a stand-in);
      `:valid`/`:invalid` CSS verified separately in `style`'s own
      tests (`required_empty_input_matches_invalid_not_valid` and 4
      others).

## Implementation notes

- **Fixing the event-then-native-action ORDER was the real
  prerequisite for `preventDefault`, not just adding the method**: both
  `dispatch_keydown`'s native text editing and `dispatch_click`'s
  checkbox/radio toggle used to run BEFORE listener dispatch
  (unconditionally — there was nothing to prevent yet). Both now run
  AFTER dispatch, gated on `!event.defaultPrevented`, matching real
  browsers' actual event-then-default-action sequencing. This was a
  precondition for `preventDefault` meaning anything, not a separate
  change bundled in for convenience.
- **One shared event object per dispatch, not one per listener** —
  `build_event_object` builds it once; `stopPropagation`/
  `preventDefault` mutate `Rc<Cell<bool>>` flags checked between each
  phase step in `dispatch_with_capture_and_bubble`, matching real DOM
  semantics (the SAME event instance is what every listener in one
  dispatch sees).
- **`:valid`/`:invalid` needed a `css_parser` fix, caught by writing
  the `style` tests, not assumed**: `parse_pseudo_class`'s
  `Interactive` bucket only recognized an explicit allowlist
  (`hover`/`focus`/`active`/`link`/`visited`/...) — `valid`/`invalid`
  weren't in it, so `input:invalid` failed to parse at all (silently
  un-matchable, this parser's deliberate fail-closed behavior for
  unrecognized pseudo-classes) until added.
- **`pattern` validation reuses `regress`** (the ECMAScript-flavor
  regex engine already a real dependency of `js`, for Boa's own
  `RegExp` support) rather than adding the more common `regex` crate —
  same regex flavor the HTML5 spec's `pattern` attribute actually
  specifies, and zero new dependency surface for `js`; `style` gained
  it as one new, small, already-vetted dependency.
- **Validity logic is deliberately duplicated, not shared, between
  `style` and `js`** (`style::is_element_valid` / `js::is_field_valid`+
  `form_is_valid`) — sharing it would mean one of those two crates
  depending on the other, a worse architectural coupling than ~15
  duplicated lines; see either function's doc comment for the fuller
  reasoning (the same tradeoff `dom::query_selector` already made
  against depending on `style`/`css_parser`).
- **Deliberately NOT implemented**: `currentTarget` (see Out of scope),
  Enter-key-submits-a-form (tracked at Milestone 21).
- `app`'s live demo: the search input is now `required` with real
  `:invalid`/`:valid` border-color CSS (red when empty, green when
  filled — updates live as you type, since typing already sets the
  dirty flag) — confirmed rendering on real hardware (window opened
  without crashing after the change; the interactive validation itself
  wasn't re-confirmed via a real click/keypress this pass, same
  input-injection limitation noted for Milestones 18-19, covered
  instead by the automated tests above).
- 14 new tests (9 in `js`, 5 in `style`), 1 `css_parser` fix (no new
  tests needed there — its own existing pseudo-class parsing tests
  already cover the shape; `style`'s new tests are what actually
  exercise the fixed allowlist end to end). 442 tests total across the
  workspace, zero failures.
