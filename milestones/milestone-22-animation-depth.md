# Milestone 22 — Animation Depth

> **Status:** Done, with gaps. **Still open:** ~~`rotate()`/`skew()` cascade and interpolate but do not render~~ — drawn since M34 (`milestone-34-compositing.md`). The parallel interpolation pass was deferred (reason below). ~~`@keyframes` keeps only its first and last stops, and `animation-timing-function` is not modelled~~ — both done in the gap pass (2026-10-21), below.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** close Milestone 14's largest deliberate scope cut — CSS
`transition`/`@keyframes` declarative interpolation — plus the
transform functions and easing curves deferred alongside it.

## Builds on
Milestone 18 (**hard dependency, not just convenient sequencing**:
real interpolation needs per-element previous-computed-value tracking
across frames, which needs the same stable per-element identity
Milestone 18 builds for `document.createElement` — the deferred-items plan is
explicit that this is a direct dependency, not incidental ordering).
Milestone 14 (the `rAF`/transform foundation this milestone extends,
not replaces — `rAF`-driven animation keeps working exactly as before).

## In scope
- **CSS `transition`**: given a property change (via a class swap, an
  inline style write, or any other style-affecting mutation) on a
  property in the existing Milestone 14 allowlist (`opacity`,
  `background-color`, `transform`, `width`/`height`/`margin`),
  interpolate from the old computed value to the new one over the
  declared duration, using the per-element identity from Milestone 18
  to remember "what was this element's value last frame."
- **CSS `@keyframes`** + `animation-name`/`-duration`/`-iteration-count`:
  the generalization of the same interpolation machinery — do this
  AFTER `transition` lands, not in parallel, since `@keyframes` reuses
  `transition`'s core interpolation logic against multiple keyframe
  stops instead of one old/new pair.
- **`transform: rotate()`/`skew()`** — the two most commonly requested
  2D transform functions Milestone 14 deferred.
- **Easing functions beyond linear**: `ease`/`ease-in`/`ease-out`/
  `ease-in-out` (the standard cubic-bezier presets) at minimum;
  `cubic-bezier()`/`steps()` if time allows within this milestone.
- **Concurrency**: once per-element animation state exists, computing
  each animating element's current interpolated value each frame is
  embarrassingly parallel (no cross-element dependency) — a natural
  `rayon` pass, potentially folded directly into
  `resolve_style_tree_parallel`'s existing full-tree walk rather than a
  second separate pass, since both need to visit every element once
  per frame/mutation anyway.

## Out of scope (deferred)
- 3D transforms (`matrix3d`, `perspective`) — real, separate, lower
  real-site-coverage-per-effort than 2D rotate/skew.
- Scroll-driven animations, the View Transitions API — newer CSS
  features, lowest urgency, land only once everything above is solid.
- Animating properties beyond the existing allowlist — Milestone 14's
  own reasoning still applies: an unbounded animatable-property list
  risks a page that pegs the CPU/GPU every frame (the architecture
  doc's "power budget" principle).

## Why this scope
`transition`/`@keyframes` is, by the deferred-items plan's own accounting, the
largest single deferred item across the entire next-phase plan — and
it was deliberately NOT attempted in Milestone 14 specifically because
the identity system it needs didn't exist yet. Sequencing this
directly after Milestone 18 (not interleaved with Milestones 19–21,
which don't touch animation at all) means the actual blocking
dependency is satisfied before work starts, not worked around.

## Acceptance criteria
- [x] A `transition: background-color 0.3s` on a class toggle (or a
      JS-driven inline-style change) visibly interpolates between the
      old and new color over the declared duration, not a hard snap —
      the exact criterion Milestone 14 left unchecked. **Verified two
      ways**: the state-machine engine (`TransitionRegistry::step`,
      `apply_transitions`) has direct unit tests covering trigger,
      mid-flight, completion, and interruption
      (`apply_transitions_interpolates_partway_through_the_duration`,
      `apply_transitions_snaps_to_target_once_duration_elapses`,
      `apply_transitions_interrupting_restarts_from_the_current_displayed_value`,
      `crates/style/src/lib.rs`); and, live in `app` on real hardware,
      a temporary debug probe confirmed the ACTUAL per-frame
      `background_color` value `apply_transitions`/`apply_animations`
      hand to layout genuinely cycles across real advancing wall-clock
      `Instant`s during a live `cargo run -p app` session (not just in
      an isolated unit test). **One honest gap**: final on-screen
      compositor confirmation (a screenshot showing the color visibly
      different a second apart) was inconclusive this session — the
      desktop had other windows overlapping/raised above the app's
      during capture, and screenshotting a Wayland client redrawing at
      150–200Hz uncapped (see the `AnimationRegistry`/`ControlFlow`
      note below) is itself an unreliable way to sample a specific
      instant. The DATA feeding the renderer is proven correct by
      direct instrumentation; the final GPU-present step wasn't
      independently reconfirmed by eye this pass.
- [x] A `@keyframes` animation loops correctly for its specified
      `animation-iteration-count`, including `infinite`. **Verified**:
      `apply_animations_interpolates_partway_through_a_cycle`,
      `apply_animations_loops_when_iteration_count_is_infinite`,
      `apply_animations_holds_at_the_to_stop_once_iterations_are_exhausted`,
      `apply_animations_restarts_when_animation_name_changes` — plus
      the same live-hardware instrumentation described above, run
      against `app`'s real `@keyframes pulse { ... } #pulse-box {
      animation: pulse 1.5s infinite; }` demo content.
- [ ] `transform: rotate(45deg)` and `skew(10deg, 0)` render correctly
      and compose correctly with existing `translate()`/`scale()` in a
      chain. **Deliberately NOT done, a real scope cut**: `rotate()`/
      `skew()` parse correctly into `Transform::rotate_deg`/
      `skew_x_deg`/`skew_y_deg`, cascade correctly, and interpolate
      correctly through `apply_transitions`/`apply_animations` (all
      covered by tests: `transform_rotate_parses_into_rotate_deg`,
      `transform_rotate_composes_with_translate_and_scale`,
      `transform_skew_parses_both_axes`,
      `transform_skew_x_and_y_individually`) — but are never actually
      GPU-rendered. `gpu_backend`'s `QuadInstance`/`quad.wgsl` and
      `rendering::apply_transform_to_items` only support axis-aligned
      rect repositioning (translate via x/y, scale via width/height);
      true rotation/shear needs a fundamentally different instanced-quad
      representation (a rotation matrix or four independent corners per
      quad, not a rect) — a real GPU-pipeline change, not a `style`/
      `layout`-level one, and out of reach within this milestone's
      time budget. This is the same "parsed but inert" pattern already
      established for `:hover`/`:focus` pseudo-classes — see
      `Transform`'s doc comment in `crates/style/src/lib.rs` — and sets
      up the rendering work cleanly for a focused follow-up.
- [x] At least the four standard easing keywords produce visibly
      different interpolation curves for the same transition (not all
      four look identical to linear). **Verified**:
      `ease_in_starts_slower_than_linear`,
      `ease_out_starts_faster_than_linear`, plus the underlying
      `cubic_bezier_y_for_x` bisection solver and `parse_easing`
      (4 keywords + `cubic-bezier(x1,y1,x2,y2)`) each have their own
      parsing tests.
- [ ] An animated page with N animating elements doesn't scale
      linearly-worse in frame time as N grows on a multi-core
      machine — the parallel interpolation pass verified against a
      sequential reference. **Deliberately deferred, a real scope
      cut, not silently dropped**: `apply_transitions_recursive`/
      `apply_animations_recursive` are plain sequential tree walks,
      same shape as `resolve_style_tree_parallel`'s SEQUENTIAL
      reference implementation that parallel version is checked
      against — no second `rayon` pass was added. Reasoning: every
      frame already pays for one full parallel cascade
      (`resolve_style_tree_parallel`) regardless of whether anything
      is animating; the transition/animation passes only visit
      elements that actually have a `transitions`/`animation-name`
      entry in their ALREADY-RESOLVED style (a cheap `HashMap`/`Vec`
      check per element, not new cascade work), so the marginal cost
      of the sequential walk is small relative to the cascade it rides
      alongside. Worth real profiling on a page with hundreds of
      simultaneously-animating elements before parallelizing — not
      assumed to be necessary without evidence.

## Implementation notes

- **`TransitionRegistry`/`AnimationRegistry`** (`crates/style/src/lib.rs`)
  are the persistent, cross-frame state this milestone's whole
  interpolation model rests on — keyed by `dom::Element::node_id`
  (Milestone 18's stable identity), NOT `element_ptr_key` (only valid
  within one borrow/frame, useless for remembering a value between
  frames). Both are owned by `app` for the life of the page and reset
  on navigation (Milestone 21) — without that reset, an entry left
  mid-flight at navigation time would never be revisited by the new
  document's tree walk, permanently pinning the redraw loop's
  `any_active()` check true and busy-looping forever after the old
  page was already gone.
- **The state machine's key subtlety**: `TransitionRegistry::step`
  returns the FROM value (t=0) on the exact frame a new target is
  first detected — matching real CSS (a transition starts at t=0 the
  instant it's triggered, not before). Interrupting an in-flight
  transition restarts the NEXT one from `last_displayed` (the current
  VISUAL value), not from the old target or the original starting
  value — real CSS interrupt semantics, verified directly by
  `apply_transitions_interrupting_restarts_from_the_current_displayed_value`.
  `ActiveTransition` deliberately does NOT freeze `duration`/`easing`
  at trigger time — `step` re-reads the CURRENT frame's spec each
  call, so a `transition-duration` change mid-flight (e.g. from a
  class swap that also changed something else) affects the rest of an
  already-running transition, matching real CSS rather than an easier
  freeze-at-start implementation.
- *(Closed in the gap pass, 2026-10-21: `KeyframesRule::stops` keeps
  every stop, comma lists included, capped at `MAX_KEYFRAMES`; each
  animatable property runs on the stops that set it — found by effect, so
  `background` or `margin-block` count — with the element's own value at
  0% and 100% where no stop does; `animation-timing-function` (default
  `ease`, as in CSS; also in the shorthand, whose second time is now the
  delay, not the duration) paces each segment unless its stop sets its
  own. An animation that sets nothing this engine animates no longer
  counts as running, so it does not keep a page awake. Corpus: mozilla.org's
  `93.75%, to` stops now parse and change nothing drawn; libreoffice-home
  and nodejs-about re-blessed — no longer kept ticking for 60 frames by
  such animations, they are captured at idle, and text in one box on each
  has different glyph widths; what changed it during those frames is not
  identified (backlog).)*
- *(Superseded in part: the first-and-last-stop limit and the missing
  `animation-timing-function` below were closed in the gap pass — see the
  note above. The implicit `forwards` fill and the single-valued
  `animation-name` still hold.)*
  **`@keyframes` scope cut, stated plainly** (Milestone 22): `KeyframesRule` only
  keeps the FIRST and LAST parsed stop (`from`/`0%` and `to`/`100%`);
  any middle stops (`0% {} 50% {} 100% {}`) are parsed (so malformed
  input doesn't panic) but discarded — `apply_animations` only ever
  does one linear-progress lerp from first to last stop, never a real
  multi-segment timeline. `animation-timing-function` isn't modeled at
  all (always linear progress within a cycle, independent of
  `transition`'s real easing support). A finished finite-iteration
  animation holds at its `to` stop (an implicit
  `animation-fill-mode: forwards`, not real CSS's `none` default) —
  chosen because reverting to the pre-animation base style after a
  finite animation completes would surprise anyone actually using
  this more than holding at the visible end state would.
- **`animation-name` is single-valued**, not real CSS's
  comma-separated list running several animations on one element at
  once — `Style::animation_name` is `Option<String>`, matching the
  same "one thing, not a list" simplification `transition-property`'s
  `All` variant sidesteps differently. The `animation` shorthand
  parses `<name> <duration> <easing-keyword-tokens, accepted but
  unused> <infinite|N>` in any order (space-separated token
  classification, same pragmatic style `transition`'s shorthand
  parser already uses).
- *(Closed in Milestone 33, step 2: `rgb()`/`rgba()`/`hsl()`/`hsla()` parse.)*
  **`rgb()`/`rgba()` color syntax still isn't supported anywhere in
  this codebase** (`Color::parse` only understands `#hex` and named
  keywords) — discovered while writing this milestone's tests (an
  early draft used `rgb(100,0,0)` and silently failed to parse,
  passing `None` through `Color::parse` unnoticed until a test
  assertion caught the resulting no-op). Not fixed here (out of this
  milestone's scope), but worth flagging as a real, separate gap for
  a future CSS-parsing milestone — real-world stylesheets use `rgb()`
  constantly.
- **`layout::layout_document_with_style_cache`** is a purely additive
  refactor of `layout_document` (now a thin wrapper: cascade, then
  delegate) — the seam `app` needed to run `apply_transitions`/
  `apply_animations` BETWEEN cascade resolution and layout, which
  `layout_document`'s original all-in-one signature had no hook for.
  Zero risk to the 90 existing `layout` tests (none of which call the
  new function; all still exercise the wrapper) — confirmed by running
  them unchanged before and after.
- **A real, documented performance/power characteristic, not a
  regression this milestone introduced**: once ANY element on the page
  has an `animation-name` with `animation-iteration-count: infinite`
  (or a JS `requestAnimationFrame` loop that keeps re-scheduling
  itself, which Milestone 13/14's own demo already does), `tick`'s
  "keep polling" condition stays true forever, and `shell_desktop`'s
  `ControlFlow::Poll` redraws as fast as the event loop can go — no
  frame-rate cap tied to display vsync exists anywhere in this engine
  yet. Confirmed via direct instrumentation this session: `build_scene`
  was invoked 150–200+ times/second sustained. This is NOT new in this
  milestone (the exact same unthrottled-`Poll` behavior already existed
  for M13/14's perpetual `requestAnimationFrame` demo), but this
  milestone makes it much easier to trigger (`animation-iteration-count:
  infinite` is one CSS declaration, no JS required) — worth flagging as
  a real, separate follow-up (cap the poll loop to the display's
  refresh rate) rather than silently accepting unbounded CPU/GPU usage
  on any page using an infinite animation.
- 22 new tests in `style` (rotate/skew parsing, easing math/parsing,
  transition-shorthand parsing, animation-*/`@keyframes` parsing, the
  `apply_transitions` and `apply_animations` engines including
  `any_active()` on both registries), 3 new tests in `css_parser`
  (`@keyframes` block parsing, including the "no stops" and "keeps
  only first/last stop" edge cases). 489 tests total across the
  workspace, zero failures. `app`'s live demo gained a real
  click-triggered `transition: background-color 0.4s ease` box and a
  real `@keyframes pulse { ... } animation: pulse 1.5s infinite;` box
  (deliberately NOT a `:hover`-triggered demo — `:hover` is still
  always-`false` in this engine's cascade, see
  `PseudoClass::Interactive`'s match arm in `style`, a pre-existing gap
  this milestone didn't touch; *`:hover` matches since the gap pass*) — confirmed rendering with correct
  initial layout/colors on real hardware (AMD Radeon 780M, Wayland/KDE)
  and confirmed animating via direct data instrumentation, though final
  on-screen visual reconfirmation was inconclusive this pass (see the
  first acceptance criterion's notes).
