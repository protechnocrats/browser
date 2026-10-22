# Milestones 1–17, and Phases A–F — history

> **Status: complete.** Historical record of how Milestones 1–17 were scoped
> and built, and of Phases A–F, the follow-up pass that closed most of their
> deferred items. Current status: `AUDIT.md`. What comes next: `ROADMAP.md`.

Every milestone here has a real, tested implementation and its acceptance
criteria were checked one by one against the running code. Several shipped
with named, deliberate scope reductions; each section says what they were.
Many have since been closed (by Phases A–F below, or by Milestones 18–33);
`AUDIT.md` is the live answer to what is still open.

Architecture decisions made along the way (Rust, Boa, `wgpu`, pure-Rust
decoders, the privacy boundary) are recorded in
`browser-engine-architecture.md`.

---

## 1 — Vertical Slice
**Goal:** prove the full pipeline shape — parse → DOM → style → block
layout → paint → GPU — end to end, with the smallest possible feature
set (three colored boxes), before building any real feature on top of
an unproven architecture.
**Built:** the six-crate core skeleton (`dom`, `html_parser`,
`css_parser`, `style`, `layout`, `rendering`) plus `gpu_backend`/
`shell_desktop`, the frozen dependency-direction invariant (core crates
never touch platform — mechanically enforced by
`scripts/check-dependency-direction.sh`), and `#![forbid(unsafe_code)]`
in every core crate.
**Confirmed on real hardware twice**, in two different sessions: the
original 3-box demo (now gone — later milestones replaced `app`'s
hardcoded HTML), and this session, via the current demo, which
exercises the same pipeline stages.

## 2 — Text Rendering
**Goal:** real text renders as correctly shaped, positioned glyphs via
a GPU texture atlas — not just colored rectangles.
**Built:** the `text` crate (font loading, `fontdue`-based
rasterization, a growable atlas), `DisplayItem::Glyphs`, and
`gpu_backend`'s second render pipeline. One bundled font (DejaVu Sans)
— no fallback chain. LTR Latin only.
**Deferred:** line breaking (→ Milestone 5), font fallback, bidi,
complex-script shaping, variable fonts/`@font-face`, text
selection/cursor rendering.
**Status:** confirmed on real hardware this session (see `AUDIT.md`'s
real-hardware note) — text renders correctly through the full GPU
atlas path.

## 3 — Real HTML Parsing & Fuzzing
**Goal:** replace Milestone 1's toy parser with a real tokenizer/tree
builder with actual error recovery, and put a fuzzer on it from day
one — the crate where untrusted bytes become structured data.
**Built:** a named-state tokenizer (Data/TagOpen/TagName/attribute-value
states/comment/DOCTYPE), a tree builder with a real open-elements
stack, error recovery (unclosed tags close at EOF, mismatched closing
tags recover via nearest matching ancestor, stray closing tags are
discarded), and a `cargo-fuzz` harness targeting `parse_document`.
**Deferred:** the full HTML5 insertion-mode state machine, encoding
detection, templates/custom elements/shadow DOM — open-ended, hardened
incrementally as real input demands it, not a fixed scope to "finish."
**Status:** fully tested in-sandbox; the fuzz harness itself still
needs a real machine with a nightly toolchain to actually run.

## 4 — CSS Selectors & Cascade
**Goal:** style comes from real `<style>`-block stylesheets matched via
selectors with a real cascade, not just inline `style="..."`.
**Built:** full stylesheet parsing (`Stylesheet`/`Rule`/`Selector`/
`CompoundSelector`), type/class/ID/descendant-combinator matching, a
4-pass cascade (stylesheet normal → inline normal → stylesheet
`!important` → inline `!important`), specificity, and `color`
inheritance.
**Deferred at the time, since closed by Phase A:** child/sibling
combinators, attribute selectors, structural pseudo-classes, `@media`,
custom properties — see below.
**Phase A (done):** attribute selectors, `>`/`+`/`~` combinators,
`:first-child`/`:last-child`/`:only-child`/`:nth-child()`/`:not()`,
`@media (min/max/exact-width)`, custom properties (`--foo`/`var()`).
`:hover`/`:focus`/`:active` and `::before`/`::after` parse and count
toward specificity but never match — no interactivity state threaded
into the cascade yet. *(The first three match since the gap pass,
2026-10-21: `style::Interaction` — the hovered, pressed and focused
elements with their ancestors — is in every `MatchContext`; the engine
hit-tests pointer moves only when the sheet has such a rule, a new
`EngineInput::PointerUp` ends `:active`, and an interaction change
restyles only the elements whose state changed, as attribute changes
would be.)* Still deferred: `@layer`, container queries,
`:nth-of-type`/`:has()`, non-width media features.

## 5 — Inline Layout & Line Breaking
**Goal:** text wraps at the container edge; inline content flows
left-to-right within a block box, distinct from block stacking.
**Built:** word-level inline formatting context (`InlineWord`, greedy
wrapping), whitespace collapsing via `str::split_whitespace()`,
`text-align: left/center/right`, a tag-list-based inline/block
distinction (`INLINE_TAGS`, since no real `display` property existed
yet).
**Deferred at the time, since partly closed by Phase B:** full UAX #14
line-breaking, `white-space` variants, `text-align: justify`,
`vertical-align`.
**Phase B (done):** `text-align: justify`, `white-space: nowrap/pre/
pre-wrap`, `vertical-align` keywords, `<br>` now forces a real line
break. Still deferred: UAX #14, font fallback, bidi, complex-script
shaping, variable fonts/`@font-face`, pixel-exact `pre` whitespace,
`white-space: pre-line`.

## 6 — Box Model Completeness
**Goal:** the full CSS box model — padding, border, `box-sizing` — not
just width/height/margin/background-color.
**Built:** padding (all sides + shorthand), `border-width`/`-color`/
`-style: solid`, `box-sizing: content-box | border-box`, border
rendering (reusing the existing `Rect` display item — zero
`gpu_backend` changes needed).
**A real pre-existing bug found and fixed here:** `margin` had been
behaving like padding (inset space between a box and its OWN children)
since Milestone 1 — invisible until real padding existed to compare
against. Fixed as part of implementing margin box → border box →
padding box → content box as distinct layers.
**Deferred at the time, since closed by Phase C:** `border-radius`,
`border-style` beyond `solid`, `box-shadow`, `outline` — see below.
**Phase C (done):** `border-style: dashed/dotted/double`, `outline`
(painted outside the border box, never affects layout — this is the
exact mechanism this session's focus-ring fix reused), `border-radius`
(uniform length, SDF-rounded background fill; borders stay
square-cornered). Still deferred: `box-shadow` (needs a blur/compute
pass), per-corner/elliptical/`%` radius, rounded borders,
`groove`/`ridge`/`inset`/`outset`.

## 7 — Images
**Goal:** `<img>` and `background-image` decode and render real image
files (local fixtures — network loading is Milestone 8).
**Built:** the `images` crate (PNG via `png`, JPEG via `zune-jpeg`,
both pure-Rust), `DisplayItem::Image`, a third `gpu_backend` pipeline,
decode-once caching, `<img>` intrinsic sizing, `background-image`
paint order.
**Genuine planning gap, not a deferred item with a reason: video.**
Checked directly against the source — zero references to `<video>`,
codecs, or media playback anywhere in this codebase or any of the 17
milestone documents' own scope sections. Every other deferred item in
this project was named with a reason at some milestone; video simply
was never scoped in at all. It needs its own Milestone-1-style scoping
pass, not a quick addition to an image milestone.
**Concurrency fix (post-M17 audit):** image decoding used to be
sequential, one at a time, exactly when `layout`'s walk reached each
element — fixed with `ImageCache::preload_parallel` (`rayon`), a
pre-pass that decodes a whole page's image set concurrently before
layout begins.
**Phase E (done):** GIF/WebP decode (first-frame only for animated
files), `object-fit`, `background-size`/`background-position`. Still
deferred: AVIF, `<picture>`/`srcset`, animated playback, `object-position`.
**Status:** decode/cache/layout fully tested against real fixture
images; GPU rendering of an actual image still unconfirmed on real
hardware this session (this build's demo document doesn't include one).

## 8 — Networking
**Goal:** fetch a real document over HTTPS and run it through the full
pipeline — the milestone where hardcoded HTML goes away.
**Built:** the `network` crate, `NetworkRoute` trait with `DirectRoute`
(via `ureq` + `rustls`) as the only implementation, redirect-following,
timeout/error classification, `app --url <URL>` mode.
**An honest, documented exception to the memory-safety principle:**
`rustls`'s default crypto backend (`ring`) contains hand-written
assembly/C — no mature pure-Rust TLS crypto backend exists yet.
**Two real regressions found and fixed by later audits:** `app --url`
mode was silently dropped across Milestones 11–16's full rewrites of
`main.rs` (restored, now with concurrent stylesheet/image fetching);
network-fetched images were decoded into an `ImageCache` that then got
shadowed by an empty one before first paint (fixed), and relative
`<img src>`/network `background-image` were never actually fetched
despite being listed in scope (both fixed).
**Phase F (done):** in-memory HTTP cache (`Cache-Control`/`ETag`/
`Last-Modified`, `304` handling), disk-persistent cookies,
`XMLHttpRequest`, richer `fetch()` `Response` (`ok`/`statusText`/`url`/
`headers`/`.json()`). Still deferred: WebSockets, SSE, streaming
bodies, TLS hardening beyond defaults, IndexedDB, Cache API, Service
Workers.
**Status:** `network` itself unusually strongly verified (real local
HTTP server, real redirects); an actual real-website fetch was never
run in prior sandboxed sessions for lack of internet access — this
session's environment DOES have real internet access (confirmed via
`curl`), but `cargo run -p app -- --url <site>` wasn't run this pass —
a cheap, high-value follow-up.

## 9 — Persistent Storage: Cookies
**Goal:** `Set-Cookie`/`Cookie` handling across requests — the
HTTP-level state a large fraction of real sites need to function at
all (login walls, consent banners).
**Built:** `cookies.rs` in the `network` crate, wired into every
`DirectRoute::fetch` automatically — domain/path scoping,
`Max-Age`/`Secure`/`HttpOnly`/`SameSite` (parsed, not yet enforced —
no cross-site-request concept exists to enforce `SameSite` against).
**Deliberate simplification:** `Expires=` (an HTTP-date) is not
parsed; `Max-Age` takes precedence when both are present (matching RFC
6265 §5.3) and is what virtually all modern servers send anyway.
**Status:** fully tested — real two-request cookie round-trip verified
against a real local server.

## 10 — Flexbox Layout
**Goal:** `display: flex` for the common cases — the first real layout
algorithm beyond block/inline stacking.
**Built:** row and column `flex-direction`, all four `justify-content`
values, all four `align-items` values, `flex-grow`/`flex-shrink`/
`flex-basis` (two-pass: natural sizes, then grow/shrink-adjusted final
sizes so content correctly reflows), `gap`. First real CSS `display`
property (no tag can imply "this is a flex container" the way
`<span>` implies inline).
**Deliberate scope reduction:** column direction stacks/aligns/gaps
correctly but does NOT apply grow/shrink — row is overwhelmingly more
common in real usage.
**Deferred at the time, since closed by Phase D:** `flex-wrap`,
`order`, `align-content`, intrinsic-sizing edge cases — see below.
**Phase D (done):** `order`, `flex-wrap` (row direction), `align-content`
(all six values), and a genuinely new capability — a parallel
intrinsic-sizing pass (`measure_intrinsic_parallel`, `rayon`) powering
`width: min/max/fit-content` and fixing auto-width flex-row items that
used to collapse to 0. This is the first place layout MEASUREMENT (not
just style) got real multi-core work. Still deferred: CSS Grid (own
sub-project), `height` intrinsic keywords, real `min/max-width`
clamping in the flex algorithm, column-direction wrap.

## 11 — JavaScript Engine Integration
**Goal:** `<script>` tags execute via Boa, with minimal DOM bindings.
**Built:** `boa_engine` embedded (real toolchain-compatibility work to
get it building at all), `document.getElementById`/`querySelector`
(single-compound only), `.textContent`, `.style.<allowlist>`,
`console.log`, synchronous document-order script execution. The
safe-closure-capture pattern (`#[derive(Trace, Finalize)]` +
`#[unsafe_ignore_trace]`) was proven empirically before writing a
single real binding, and is used identically by every binding in this
crate since.
**A real bug caught on real hardware:** the first screenshot from a
real machine showed `<script>`'s raw JS source rendering as visible
page text. Root cause: `<script>` wasn't in `layout`'s inline-tag list,
so its own text child got laid out like ordinary content. Fixed with
an explicit non-visual-tag guard at BOTH places `layout` walks
children — fixing only one would have left the bug reachable through
the other path.
**Deferred:** event listeners (→ Milestone 15), `async`/`defer`
loading, the full DOM API surface (grown incrementally), `fetch()`/XHR
(→ Milestone 12), Canvas/WebGL, a JIT (Boa is interpreter-only, an
accepted tradeoff until a from-scratch engine is warranted by real
evidence).

## 12 — JS-Facing Storage & Fetch
**Goal:** `localStorage`/`sessionStorage` and `fetch()` work from a
running script.
**Built:** `getItem`/`setItem`/`removeItem`/`clear` (in-memory,
not origin-partitioned — no multi-origin concept exists yet);
`fetch()` returning a real `Promise`, synchronous under the hood but
chaining exactly as real code expects (`context.run_jobs()` drains the
microtask queue after every script evaluation).
**A real bug caught by this milestone's own tests:** the first
`fetch()` implementation threw a synchronous JS exception on network
failure instead of rejecting its `Promise` — meaning
`fetch(url).catch(...)` never even had a `Promise` to attach `.catch`
to. Fixed before shipping.
**Deferred at the time, since closed by Phase F:** `XMLHttpRequest`,
full `Response`/`Request` fidelity — see Milestone 8's Phase F note.

## 13 — DOM Mutation & Reactive Re-render
**Goal:** a JS-driven DOM mutation correctly re-runs style → layout →
paint and shows the result — the page becomes genuinely dynamic.
**Built:** a dirty flag plus `shell_desktop`'s new `tick` callback
(fired every event-loop iteration via `AboutToWait`, switching between
`ControlFlow::Poll`/`Wait` based on pending work), `appendChild`/
`removeChild`/`textContent=` mutation entry points. The core mechanism
this whole milestone rests on: `js::JsEnvironment` and `app`'s render
loop share the exact same `Rc<RefCell<dom::Element>>` — not a copy —
so a mutation is already reflected the next time layout reads the
tree, no synchronization needed.
**A real, named architectural gap:** `appendChild`'s JS signature is
`el.appendChild(tagName, textContent)` in one call, not the real
two-step `appendChild(document.createElement(tag))`, because a real
`createElement` needs to hand JS a stable reference to a DETACHED
element — this crate's "re-find by id in the live tree" object model
has no way to represent that. This is the single most-cited
architectural gap across later milestones' own notes, and is the first
item in the next milestone set.
**Deferred:** fine-grained incremental layout, mutation batching (both
now tracked as cross-cutting follow-up work, not milestone-specific).

## 14 — CSS Animations, Transitions & requestAnimationFrame
**Goal:** the engine re-renders over time, not just on mutation —
`transition`, `@keyframes`, and `requestAnimationFrame`.
**Built, fully:** `transform: translate()/scale()` (parsed, carried on
every `LayoutBox`, applied at PAINT time, pivoted around each node's
center) and `requestAnimationFrame` — a real per-frame callback loop,
verified to fire exactly once per tick even when a callback
re-registers itself.
**Deliberately NOT implemented, a considered trade:** CSS `transition`/
`@keyframes` declarative interpolation. Given this session's combined
effort budget across four milestones (11–14), `rAF` was fully
implemented instead — it delivers the same real animation capability
via JS today. Real interpolation needs per-element previous-value
tracking across frames, which needs the same stable per-element
identity Milestone 13's `createElement` gap is also blocked on — not a
coincidence; both wait on the same underlying fix.
**Known limitation:** transform doesn't propagate to descendant boxes
— a transformed node's own background/border/image/glyphs move, its
children's independently-computed display items don't.

## 15 — User Input & Interactivity
**Goal:** click, scroll, type, and focus dispatch as real DOM events.
**Built:** `shell_desktop` forwarding real `winit` input events,
hit-testing (`layout::hit_test_path`, a real depth-first walk —
topmost siblings checked first, a click on rendered text bubbles to
its containing element), `addEventListener('click'/'keydown'/'scroll')`
with basic bubbling, whole-document scroll, a real (if simplified)
focus concept.
**Deferred:** form elements (→ Milestone 16), the full event model
(capture phase, `stopPropagation`/`preventDefault`), touch input,
drag-and-drop, text selection, the accessibility tree.
**Status: confirmed interactively on real hardware this session** — a
real click and real Tab-key focus cycling both worked correctly
through the actual `winit` path, reported directly by a human at the
keyboard. This is stronger evidence than the GPU-rendering screenshot
confirmation every other milestone has relied on.

## 16 — Form Elements
**Goal:** `<input type="text">`, `<button>`, `<textarea>`, basic
`<select>` render as interactive widgets and participate in `<form>`
submission.
**Built:** text editing (type/backspace/arrow-key cursor movement,
insert-at-cursor not just append), `<button>`/`<input type="submit">`
firing a `submit` event on the enclosing form, Tab-order cycling
between focusable elements (recomputed from the live document each
press, so it survives DOM mutation).
**Named gaps, explicitly stated, not silent:** no click-to-position (a
click only hit-tests the element, not a sub-element text offset); no
visual caret indicator (cursor position IS tracked and used for
editing — nothing paints a line at it; needs focus/cursor state
threaded from `js` into `layout`/`rendering`, which don't know about JS
state today by design); no classic non-JS form navigation (needs a
"load a new page" concept this single-document engine doesn't have —
not form-specific, a whole-engine gap); no Enter-to-submit/no
Enter-activates-a-focused-button. *(Both closed in the gap pass,
2026-10-21: `script::events`' Enter default action, HTML §4.10.21.2.)*
**Additional gap found this session, not previously named anywhere:**
zero visual focus indicator on ANY focusable element (not just
inputs) — Tab/click correctly changed focus the whole time, but with
no visible feedback it looked broken. Fixed this session:
`app::apply_focus_outline` paints an outline around the focused
element's box each frame, reusing Milestone 6/Phase C's existing
`outline` paint path directly (real CSS `:focus` cascade support is a
separate, larger, still-open piece of work).
**Simplified value model, stated plainly:** an input's current text
lives in its `value` ATTRIBUTE (mutated live), not a separate `.value`
property the way real DOM distinguishes them.

## 17 — Integration: A Working Web Engine
**Goal:** load a real website end to end and prove the milestones
compose correctly under real-world conditions — deliberately a
bug-fixing/gap-filling milestone, not new-feature scope.
**Built:** a dedicated `integration_tests` crate (excludes
`gpu_backend`/`shell_desktop`/`app` so it can run without a display
server) proving the FULL non-GPU pipeline — parse → parallel cascade →
flexbox layout → JS mutation → real hit-test → click dispatch →
re-layout → paint-list generation — works together on combined
documents, plus explicit resilience tests (a thrown JS exception, a
broken image reference, neither crashes the rest of the page).
**Honest finding:** this surfaced zero NEW bugs, unlike Milestone 11's
real-hardware pass — every subsystem's own test suite already covered
its correctness thoroughly enough that composing them didn't reveal a
gap. A meaningfully different (and better) outcome than "we didn't
look," but not the same as confirming against a real, messy page.
**What was still missing as of this milestone's own writing:** a real
website test (no internet access in that sandbox), real GPU
confirmation, and the concrete feature gaps named throughout this
document. **This session's environment has since closed the first two
categories** — see `AUDIT.md`'s real-hardware verification note for
exactly what was and wasn't confirmed.

---

---

## Phases A–F — closing Milestones 2–17's deferred items

After Milestone 17, the items each milestone had deferred were triaged into
phases A–J. Phases A–F (below) were implemented and tested; Phases G, H and I
became Milestones 18–23, and Phase J's accessibility item became Milestone 24.
What each phase covered, and what it deferred at the time (much of which
Milestone 33 has since done — see `AUDIT.md`):

### Phase A — done

- **Attribute selectors**: `[a]`, `[a=v]`, `[a~=v]`, `[a|=v]`,
  `[a^=v]`, `[a$=v]`, `[a*=v]`, quoted or bare (`css_parser` +
  `style::matches_attr`).
- **Combinators**: child `>`, next-sibling `+`, subsequent-sibling `~`
  (`Selector` now carries a parallel `combinators` vec; `style`'s
  matcher walks the chain right-to-left across ancestors / previous
  siblings).
- **Structural pseudo-classes**: `:first-child`, `:last-child`,
  `:only-child`, `:nth-child(An+B | odd | even | k)`, `:not(<compound>)`.
- **`@media`**: `(min-width / max-width / width: <px>)`, ANDed;
  `screen`/`all` accepted-and-ignored, `print` + unknown features fail
  closed. Viewport width is threaded through
  `resolve_style_tree_parallel` (a layout pass passes the real width).
- **Custom properties**: `--foo: …` (inherited) + `var(--foo, fallback)`
  substituted during resolution, custom props cascading in a pass
  before everything else.
- *Parsed but inert* (documented in code): `:hover`/`:focus`/`:active`
  (no interactivity state in the cascade yet — always false; *match since
  the gap pass*),
  `::before`/`::after` (parsed; a pseudo-element rule correctly does
  **not** apply to the real element; no generated boxes yet).

  **Deferred in Phase A**: cascade layers (`@layer`), container
  queries, `:nth-of-type`/`:nth-last-child`/`:has()`, non-width media
  features, structural pseudo-classes on a *non-subject* compound
  reached by a descendant/child hop (sibling context isn't tracked for
  ancestors — they're evaluated as non-matching), `[attr=v i]`
  case-insensitive flag.

### Phase B — done

- **`text-align: justify`** — inter-word gaps stretched on every line
  except the last / any line ended by a forced break.
- **`white-space`**: `normal`, `nowrap` (no wrap), `pre` (preserve
  spaces + newlines, no wrap), `pre-wrap` (preserve + wrap). Inherited.
- **`vertical-align`** keywords: `baseline`/`top`/`middle`/`bottom`/
  `text-top`/`text-bottom`/`sub`/`super` (baseline offset within the
  line box; `sub`/`super` use a fixed font-size fraction).
- **`<br>`** now forces a line break (it previously rendered nothing —
  a pre-existing bug surfaced while adding hard-break support).

  **Deferred in Phase B** (unchanged from the plan — each its own
  sub-project, several needing new crates): full Unicode line-breaking
  (UAX #14), font fallback, bidi/RTL, complex-script shaping, variable
  fonts / `@font-face` / `font-weight` / `font-style`,
  pixel-exact whitespace preservation for `pre` (leading/trailing runs
  are approximated), `white-space: pre-line`.

### Phase C — done

- **`border-style`**: `dashed` / `dotted` / `double` (painted out of
  the existing solid-quad primitive — segmented strips for dashed/
  dotted, a two-strip frame for double; no new GPU pipeline).
- **`outline`**: `outline-width` / `-style` / `-color` / `-offset` and
  the `outline` shorthand — painted just outside the border box, never
  affects layout.
- **`border-radius`**: one uniform length. Threaded
  `style` → `layout::LayoutBox` → `DisplayItem::Rect { radius }` →
  `gpu_backend`'s `QuadInstance` → `quad.wgsl`, which now rounds every
  rect's corners with a signed-distance-field in the fragment shader.
  The shader/GPU half carries the **standing real-hardware caveat**
  (this sandbox has no GPU; the CPU-side data flow is fully tested).
  Only the *background fill* is rounded — border strips stay
  square-cornered (a stroked rounded-rect needs its own shader path).

  **Deferred in Phase C**: `box-shadow` (the largest item — needs a
  blur pass, the natural place to introduce `gpu_backend`'s first
  compute shader, per the plan), per-corner / elliptical / `%`
  `border-radius`, rounded *borders*, `groove`/`ridge`/`inset`/`outset`
  border styles.

### Phase D — done

- **`order`** — flex items lay out (and paint) in ascending `order`,
  ties broken by document order.
- **`flex-wrap`**: `wrap` / `wrap-reverse` for row direction — items
  break onto multiple lines by base size; `nowrap` keeps one line even
  when overflowing.
- **`align-content`**: `flex-start` / `center` / `flex-end` /
  `space-between` / `space-around` / `stretch` — distributes the
  wrapped lines in the cross axis.
- **Intrinsic sizing** — `width: min-content | max-content |
  fit-content`, powered by a new **parallel measurement pass**
  (`layout::measure_intrinsic_parallel`, `rayon`): each element's
  min/max-content width depends only on its descendants, so sibling
  subtrees are measured concurrently — the first place layout
  *measurement* (not just style) gets real multi-core work, as Phase
  D's "Concurrency angle" calls for. The positioning pass that consumes
  it stays single-threaded (genuine sequential dependencies). Verified
  against a sequential reference in the tests. The pass also fixes the
  documented Milestone 10 gap where an auto-width flex-row item with no
  `flex-basis` collapsed to 0 — it now takes its max-content width.

  **Deferred in Phase D**: CSS Grid (its own sub-project, per the
  plan), `height: min/max/fit-content`, structural pseudo on
  ancestors, real `min-width`/`max-width` clamping in the flex
  algorithm, column-direction wrap, per-line `align-items: baseline`.

### Phase E — done

- **GIF** (via `gif`) and **WebP** (via `image-webp`) decode — both
  pure-Rust, first-frame only for animated files. `decode_auto` sniffs
  all four formats by magic bytes.
- **`object-fit`**: `fill` / `contain` / `cover` / `none` /
  `scale-down` for `<img>` — the decoded image is drawn at a computed
  sub-rect (centred) rather than always stretched to the box.
- **`background-size`** (`auto` / `contain` / `cover` / `<length>`) +
  **`background-position`** (keyword + `%`) for `background-image`.
- Computed in `rendering::fitted_image_rect`; content that ends up
  larger than the box overflows (no clipping yet — a documented gap
  shared with Phase 2's overflow work).

  **Deferred in Phase E**: AVIF, `<picture>` / `srcset`, animated
  GIF/WebP playback (a real timing subsystem), `background-repeat`,
  raw-px `background-position` offsets, `object-position`.

### Phase F — done

- **HTTP caching** (`network::cache`): in-memory, GET-only — freshness
  via `Cache-Control: max-age` / `Expires`, conditional revalidation
  via `ETag` → `If-None-Match` and `Last-Modified` → `If-Modified-Since`,
  `304 Not Modified` serves the stored body. Wired into
  `DirectRoute::fetch` (so `fetch_async` gets it too). Verified
  end-to-end against the local mock server (a fresh second fetch never
  reaches the server; a stale one revalidates and gets a 304).
- **Disk-persistent cookies**: `CookieJar::serialize` / `load_from_str`
  / `save_to_path` / `load_from_path` — a tab-separated text format, no
  `serde`. Only persistent (non-session) cookies are written; expired
  entries are dropped on load.
- **Richer `fetch()` `Response`**: `ok`, `status`, `statusText`,
  `url`, `headers` (a `Headers`-ish object with case-insensitive
  `.get()` / `.has()`), and `.json()` alongside `.text()`.
- **`XMLHttpRequest`**: `new XMLHttpRequest()`, `open` /
  `setRequestHeader` / `send` / `getResponseHeader`, then `status` /
  `statusText` / `responseText` / `readyState` (4 after send), firing
  `onreadystatechange` and `onload`. Synchronous under the hood
  (consistent with the render loop and the original `fetch()`); GET
  only.

  **Deferred in Phase F**: WebSockets, Server-Sent Events, streaming
  response bodies, TLS hardening (HSTS / pinning), IndexedDB, the Cache
  API, and Service Workers (the last is where a second independent
  `js::JsEnvironment` on its own thread would first be needed — a real
  architectural step, per the plan). Real async XHR, non-GET methods,
  and request bodies are also still out.

### Concurrency

- Phase A: each element's `MatchContext` carries its preceding-sibling
  slice + index (known before its subtree is walked — the parallel
  style pass stays embarrassingly parallel).
- Phase D: **new `rayon` measurement pass** for intrinsic sizing —
  sibling subtrees measured concurrently; the consuming positioning
  pass stays single-threaded by necessity. This is the "highest-value
  concurrency win" the plan flagged for Phase D.
- Phase E: `ImageCache::preload_parallel` (already parallel from the
  Milestone-17 audit) covers the "decode a page's image set
  concurrently" angle; nothing new needed for the format additions.
- Phase F: `DirectRoute::fetch_async`'s background threads now share
  one `Mutex`-guarded HTTP cache alongside the cookie jar — the
  cache-lookup / revalidation path is exercised concurrently by the
  existing `multiple_concurrent_fetches_all_complete_correctly` test's
  shape.
- Phases B, C: paint / inline-layout only — no new parallelism.
