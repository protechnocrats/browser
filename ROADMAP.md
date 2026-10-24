# Roadmap

The single plan for the engine: what has been achieved, what comes next, and
the rules every milestone is held to. The principles it serves are in
`browser-engine-architecture.md` §1: memory-safe, GPU-first, thread-safe,
capability-based, zero-trust, privacy-first, vendor-independent.

- **Where things stand:** Milestones 1–41 are done (38 as 38a and 38b), as are a principles
  hardening pass (between M27 and M28), the principles-audit fixes, the
  performance-audit fixes and process isolation (every page in a sandboxed
  renderer process, ADR 016). M34 put clips, transforms and opacity groups on
  the GPU and made transform animations and container scrolls compositor
  updates (ADR 017). M35 replaced the text stack — shaping, bidi, line
  breaking, font fallback, the text properties and web fonts (ADR 009). M36
  made authority a handle: the profile grants each page its network and
  storage capabilities, scoped and revocable, through a new Browser Core
  (profiles, sessions, settings, permissions; ADR 010). M37 gave every row
  of the privacy boundary a named, tested policy: routes that fail closed,
  DNS over HTTPS with no default vendor, a filter engine, third-party
  isolation, and a fingerprint policy for `Date`, timers and fonts (ADR 011,
  012, 018, 019). M38a bounds a runaway page: the browser ends a renderer
  that stops finishing ticks for 10 s (ADR 013); Boa is at 0.22 and its
  advisory exceptions are gone. M38b made idle and hidden pages cost no CPU
  and lets an overloaded animation degrade its frame rate (ADR 020). M39–M39c
  made the DOM an arena and a layout change cost what it changes (ADR 021,
  022). M40 put JavaScript behind a runtime interface, with the bindings
  generated from WebIDL, one wrapper per node and every GC handle traced
  (ADR 023). M40b interned tag and attribute names where they are born,
  with a table per document, and made selectors match by atom (ADR 024).
  M41 made a renderer hold one site's documents — a page moves to another
  renderer when a navigation lands on another site — and moved
  `localStorage` and the enforcement of the network grant into the browser
  process (ADR 025); sandboxes beyond Linux were not built.
  The suite has 1,209 tests with 0 failures, clippy is at 28
  (a ratchet), there are 25 fuzz targets, and CI runs the script and page
  suites under AddressSanitizer.
  `AUDIT.md` has the verified detail and every open gap.
- **Next:** the unscheduled backlog in §4, ordered by what the real-site
  corpus measures. M41 closed what remained of the architecture review's A1
  but the non-Linux sandboxes (§4, process isolation); M40b closed A2's last
  item, interned names; M40 closed A3 and A4.

Every milestone has its own file under `milestones/`. That file covers what
was built, its acceptance criteria and what is still open. M1–M17 and Phases
A–F share one file, `milestones/milestones-01-17.md`.

---

## 1. Achieved

| # | Milestone | Status | In one line |
|---|---|---|---|
| 1–17 | [Vertical slice to a working engine](milestones/milestones-01-17.md) | ✅ | Parse → DOM → cascade → block/inline/flex layout → paint → GPU. Also text, images, networking, cookies, JS (Boa), storage, `fetch`, DOM mutation, rAF, input and forms. |
| A–F | [Deferred-item phases](milestones/milestones-01-17.md) | ✅ | Selectors and `@media`, custom properties, `justify` and `white-space`, `border-radius` and `outline`, `flex-wrap` and intrinsic sizing, GIF/WebP and `object-fit`, HTTP cache, XHR. |
| 18 | [Detached DOM nodes & `createElement`](milestones/milestone-18-detached-dom-nodes.md) | ✅ | Per-element `node_id`; real `createElement` and `appendChild`. |
| 19 | [Caret & specialized inputs](milestones/milestone-19-caret-and-specialized-inputs.md) | ✅ | Visible caret, click-to-position, checkbox and radio. |
| 20 | [Event model & validation](milestones/milestone-20-full-event-model-and-validation.md) | ✅ | Capture/target/bubble, `preventDefault`, `required`/`pattern`, `:valid`/`:invalid`. |
| 21 | [Page navigation](milestones/milestone-21-page-navigation.md) | ✅ | Links and GET forms load a new document on a background thread. |
| 22 | [Animation depth](milestones/milestone-22-animation-depth.md) | ✅ | `transition`, `@keyframes` and easing. `rotate()`/`skew()` draw since M34. |
| 23 | [Rich input & editing](milestones/milestone-23-rich-input-and-editing.md) | ✅ with gap | `contenteditable` and touch tap. Selection and drag-and-drop are in the backlog. |
| 24 | [Accessibility tree](milestones/milestone-24-accessibility-tree.md) | ✅ with gap | Role, name and state computed from the DOM. The OS screen-reader bridge is in the backlog. |
| — | [Principles hardening pass](milestones/hardening-pass-principles-audit.md) | ✅ | File access is a denied-by-default capability, plus the PSL, cookie rules, request hygiene, partitioned storage and cache, and linear-time `pattern`. |
| 25 | [Guardrails & CI](milestones/milestone-25-guardrails-and-ci-hardening.md) | ✅ with gap | `forbid(unsafe_code)` everywhere, a fuzz suite, a clippy ratchet, `cargo audit` and `cargo deny`. No run of the CI or libFuzzer jobs has been observed. |
| 26 | [Shared network context](milestones/milestone-26-shared-network-context.md) | ✅ | One cookie jar, cache and route per profile, and hop-by-hop redirects. |
| 27 | [Observability](milestones/milestone-27-observability-baseline.md) | ✅ | A `tracing` facade, 14 security guards, metrics and `app --trace`. |
| 28 | [Origin & site model](milestones/milestone-28-origin-and-site-model.md) | ✅ | An `origin` crate with `Origin`, `Site`, `StorageKey` and one CORS rule. |
| 29 | [Engine facade](milestones/milestone-29-engine-facade.md) | ✅ | An `engine` crate with commands in and events out. Hosts are thin. |
| 30 | [Page threads](milestones/milestone-30-page-threads.md) | ✅ | One thread per page, a GPU thread, a validated mailbox and contained panics. |
| 31 | [Incremental pipeline](milestones/milestone-31-incremental-pipeline.md) | ✅ | Incremental restyle and layout, each proven by an oracle. Scroll and caret frames skip cascade and layout. |
| 32 | [Conformance harness](milestones/milestone-32-conformance-harness.md) | ✅ with gap | A 56-page real-site corpus replayed offline, a ranked feature report and 15 fuzz targets. The `fontdue` hang blocks M35. |
| 33 | [Layout completeness](milestones/milestone-33-layout-completeness.md) | ✅ with gap | Units and `calc()`, colours, the UA sheet, `display`, min/max sizes, backgrounds, positioning, overflow, opacity, shadows, `::before`/`::after`, floats, tables and Grid. |
| — | [Principles-audit fixes](milestones/principles-audit-fixes.md) | ✅ | Capped glyph atlas, GPU texture limits and an error handler, an image-texture budget. A hostile page no longer exhausts GPU memory or crashes the window (verified on hardware). |
| — | [Process isolation](milestones/process-isolation.md) | ✅ | Every page in its own renderer process (Landlock + seccomp on Linux), behind the unchanged engine API; the network, cookies and cache stay in the browser process, which checks each request's origin. |
| 34 | [Compositing & GPU depth](milestones/milestone-34-compositing.md) | ✅ with gap | Clip, transform and group markers in the display list (ADR 017); `rotate()`/`skew()`, rounded clips and group opacity drawn; transform animations and container scrolls as compositor updates; a back buffer redrawn by damage; image atlas pages with LRU eviction; 211 → 3 draw calls on an image-heavy page; glyph-atlas compaction. A5 → M39b. |
| 35 | [Text subsystem](milestones/milestone-35-text-subsystem.md) | ✅ with gap | `ttf-parser` + `rustybuzz` + our rasterizer (ADR 009; `fontdue` and its hang gone); font fallback from host-granted fonts (served to renderers over IPC); `font-*`, `line-height`, `text-decoration`, `text-transform`, `direction`; UAX #9 bidi and UAX #14 line breaking; web fonts with CORS and bounded decoding. Script snapshots await a person's sign-off. |
| 36 | [Capabilities & Browser Core](milestones/milestone-36-capabilities-and-browser-core.md) | ✅ with gap | Authority as handles (ADR 010): `capability` and `storage` crates; script gets `fetch`/XHR/`localStorage` only through grants the profile mints per page, scoped to the document's storage key and revocable; unbuilt APIs' capabilities cannot be constructed; `browser_core` with `Profile`, `Session` (tabs, zoom), validated `Settings` and a `PermissionStore`, persisted; `check-capabilities.sh`. Process-mode storage and network enforcement → M41 (done there). |
| 37 | [Privacy layer](milestones/milestone-37-privacy-layer.md) | ✅ with gap | Route policy with proxy (HTTP/SOCKS) and system-VPN routes that fail closed (ADR 011); DNS over HTTPS with no default resolver (ADR 012); extra CA roots beside the bundled ones (ADR 018); a filter engine on every hop, third-party cookies blocked, storage keyed by top-level site, and a fingerprint policy for `Date`, rAF and fonts (ADR 019). WireGuard evaluated, not built. |
| 38a | [Script time bound & Boa upgrade](milestones/milestone-38a-script-bound-and-boa.md) | ✅ with gap | A renderer that owes a tick for 10 s with no progress is killed and its page reported crashed (ADR 013); Boa 0.17 → 0.22, ported, without its default features; both advisory exceptions removed. Thread mode is bounded only by Boa's limits. |
| 38b | [Frame and power budget](milestones/milestone-38b-frame-and-power-budget.md) | ✅ with gap | Pages woken by finished work instead of 8 ms polling; hidden pages (background tabs) do no animation work; a frame rate that steps down under load and back, input never degraded (ADR 020). Off-screen transitions are not skipped. |
| 39 | [Arena DOM](milestones/milestone-39-arena-dom.md) | ✅ with gap | Readers through `dom::ElementRef`; the document an arena of linked slots; `insertBefore`/`removeChild`/`replaceChild`/`cloneNode`/`createTextNode`, O(1) link updates (counted); one validity and editability rule in `dom`; `check-dom-encapsulation.sh` (ADR 021). Interned names measured and deferred (now M40b); A5 and partial display lists done in M39b. |
| 39b | [Layout coordinates & partial display lists](milestones/milestone-39b-layout-coordinates.md) | ✅ with gap | Box frames: a reused subtree moves in O(1), its bookkeeping kept across frames (A5); subtree bounds and binary search — a click examines 5 boxes on 10k rows, not 12,001; a page's display list updated in part after a layout change (≤ 12 boxes built of 40,000); node-id hashing for the style cache and reuse maps; a sticky-positioning bug fixed (ADR 022). A real one-element change on 10k rows: 25 → 7 ms. |
| 39c | [Changes that keep their size, patched in place](milestones/milestone-39c-patch-in-place.md) | ✅ | A change whose element keeps its size is laid out alone and spliced into the previous layout and display list (ADR 022 §8–10); intrinsic sizes measured at the change; the reuse flag became a frame stamp. A one-element change on 10k rows: 7 ms → 0.33 ms (23 ms in M39). |
| 40 | [Script runtime interface & generated bindings](milestones/milestone-40-script-runtime-and-bindings.md) | ✅ with gap | `script::ScriptRuntime` (ADR 023): Boa (`js`) and a stub runtime behind it, the host side — DOM operations, events, forms, editing, `fetch`/XHR, storage — runtime-independent; bindings generated from WebIDL through a checked `dispatch`; one wrapper per node, held weakly, its node in an internal slot; GC handles traced (`gc-handle-captures.txt` empty). 17 binding cases pass on both runtimes. Interned names → M40b; Boa has no interrupt. |
| 41 | [Site isolation & browser-process storage](milestones/milestone-41-site-isolation.md) | ✅ with gap | A renderer holds one site's documents (ADR 025): a navigation that lands on another site — link, typed URL, redirect — moves the page to a renderer that has held nothing, handed the response, behind the unchanged API; `localStorage` served by the browser through the page's grant, shared by pages of one origin; the network grant enforced by the browser, which withholds what a mislabelled request cannot use. No sandbox on macOS or Windows yet. |
| 40b | [Interned names](milestones/milestone-40b-interned-names.md) | ✅ | A `names` crate (ADR 024): well-known atoms equal everywhere, a `Name` that is an atom or its own text, a table per document; born interned in the tokenizer, `createElement` and the CSS parser, interned in place in the document; readers and selectors compare atoms, the rule index keys well-known types by atom. Corpus parse 7–10% faster, 10k first frames 7–8%, DOM changes 10%, a selector-heavy restyle 3–6%. |
| — | [Performance-audit fixes](milestones/performance-audit-fixes.md) | ✅ | Linear selector matching (was exponential: 11 s for one element); scroll and caret frames drawn from a per-layout display list (4.3 ms → 0.03 ms on 10k rows); incremental intrinsic sizing; a bounded fetch pool; progressive images; a heap use-after-free in Boa's teardown fixed (P12, ADR 015). |

For each milestone, the status column's "gap" is spelled out in its file,
under the status line at the top, and in `AUDIT.md`.

---

## 2. Definition of Done (every milestone)

A milestone is not done until all of these hold:

- [ ] `cargo build --workspace --release` is clean.
- [ ] `cargo test --workspace --release` has no failures. The test count never
      drops without an explanation in the milestone file.
- [ ] `scripts/clippy-ratchet.sh` passes: no new warnings. The baseline is
      28; check this line on its own before committing.
- [ ] The guardrails pass: `scripts/check-dependency-direction.sh`,
      `check-forbid-unsafe.sh`, `check-host-boundary.sh`,
      `check-panic-strategy.sh`, `check-gc-handles.sh`,
      `check-core-capabilities.sh`, `check-capabilities.sh`,
      `check-dom-encapsulation.sh`,
      `check-advisory-exceptions.sh` and
      `check-psl-freshness.sh`.
- [ ] Anything a renderer process sends the browser is decoded by the
      bounded `engine::ipc` codec, and a new message kind extends the
      `ipc_decode` fuzz seeds (ADR 016).
- [ ] Nothing new is exponential or whole-page per frame without a reason:
      a change that can do work proportional to the page (or worse) on
      every frame or every input has a counter-bounded test (as in
      `milestones/performance-audit-fixes.md`).
- [ ] Every new parser, decoder or protocol boundary has a fuzz target, and
      the fuzz suite passes
      (`cargo test --manifest-path fuzz/Cargo.toml --no-default-features`).
- [ ] Every new untrusted-input path has a hostile-input test (oversized,
      deeply nested, malformed) asserting no panic and bounded time and
      memory.
- [ ] No `unsafe` anywhere, unless there is a justified entry in
      `scripts/unsafe-exceptions.txt`. That file is empty today.
- [ ] The milestone file is written. Its status line says what is done and
      what is open, and where each open item goes. `AUDIT.md` and §1 of this
      file are updated. So is architecture doc §4, if threading changed.
- [ ] Every decision that constrains later work is an ADR in `docs/adr/`
      (§5).

---

## 3. Milestones 34–41 (done)

```
M34 Compositing & GPU depth ──┐
M35 Text subsystem            ├─► M38a Script bound & Boa ─► M38b Frame/power budget; M38a ──► M40 Script runtime & bindings
M36 Capabilities & Core ──► M37 Privacy layer ──► M41 Site isolation
M39 Arena DOM ──► M39b Layout coordinates & partial display lists ──► M39c Patch in place
              └──────────────────────────────────► M40 ──► M40b Interned names
```

| # | Milestone | Effort | Risk | Needs | Status |
|---|---|---|---|---|---|
| 34 | Compositing & GPU depth | L | Medium | M31, M33 | ✅ Done (A5 → M39, then M39b: done) |
| 35 | Text subsystem | XL | Medium | M32 fuzz targets. A shaping/font parser that survives the `fontdue` hang input. | ✅ Done (ADR 009) |
| 36 | Capability layer & Browser Core seed | L | Medium | M28, M30 | ✅ Done (ADR 010; process-mode storage and network enforcement → M41: done) |
| 37 | Privacy layer | XL | Medium | M28, M36 | ✅ Done (ADR 011, 012, 018, 019; WireGuard → backlog) |
| 38a | Script time bound & Boa upgrade | M | Medium | Process isolation | ✅ Done (ADR 013; thread mode bounded only by Boa's limits) |
| 38b | Frame and power budget | M | Low | M27, M31, M34 | ✅ Done (ADR 020; off-screen transitions → backlog) |
| 39 | Arena DOM | XL | High (touches every crate) | M31's oracles | ✅ Done (ADR 021; interned names → M40b; A5 and partial display lists → M39b) |
| 39b | Layout coordinates & partial display lists | L | Medium (moves goldens if done carelessly) | M39, M34 | ✅ Done (ADR 022) |
| 39c | Changes that keep their size, patched in place | M | Medium | M39b | ✅ Done (ADR 022 §8–10) |
| 40 | Script runtime interface & generated bindings | XL | High | M38a (Boa upgrade), M39 | ✅ Done (ADR 023; interned names → M40b) |
| 40b | Interned names | M | Medium (measured a loss once) | M39, M40 | ✅ Done (ADR 024) |
| 41 | Site isolation & browser-process storage | L | Medium | Process isolation (done), M36 | ✅ Done with gap (ADR 025; non-Linux sandboxes → §4) |

Effort key: S is under a session, M is one session, L is 2–3 sessions and XL
is 4 or more.

M34 comes first because M38 needs it. It also turns into pixels two things
that are already cascaded but not drawn: `rotate()`/`skew()` and `opacity` as
a group. M35 can run in parallel once its parser decision is made. M36 must
come before M37, because the privacy policies hang off its profile and
capability plumbing.

### Where the principles are violated today

A code audit on 2026-09-24 checked each principle in architecture doc §1
against the code; `AUDIT.md` has the verdict for each one. The table lists
every place the code falls short, and the work item that fixes it.

| # | Principle | Violation found in the code | Severity | Fixed by |
|---|---|---|---|---|
| P1 | Zero-trust, GPU-first | `text::GlyphAtlas` grows with no cap. About 20 glyphs at `font-size: 1024px` make it more than 16,384 px tall, measured on the 1,024-px-wide atlas; production's is 512 px wide, so it grows twice as fast. Every distinct `font-size` gets its own cache entry, forever. | **High:** a web page can trigger it | ✅ Fixed (audit fixes) |
| P2 | Zero-trust | `gpu_backend` uploads the atlas and images with no check against the device's texture limits, and has no GPU error handler. wgpu's default response to a validation error is a panic, on the shell's GPU thread, outside the per-page crash containment. The crash itself hasn't been reproduced on a GPU. | **High:** P1 makes it reachable | ✅ Fixed (audit fixes) |
| P3 | Zero-trust, GPU-first | GPU image textures (`image_textures`) are never freed, so a page can grow GPU memory for the whole life of the window. | Medium | ✅ Capped (audit fixes); ✅ LRU eviction and batching (M34) |
| P4 | Capability-based | Every page's script gets `fetch`, XHR and `localStorage` automatically. They are ambient authority, not granted capabilities. | Medium, by design until M36 | ✅ Fixed (M36, ADR 010) |
| P5 | Privacy-first | Boa's standard `Date` gives millisecond time and the local timezone offset to every page, and no policy restricts it. That's a fingerprinting surface. | Low | ✅ Fixed (M37, ADR 019) |
| P6 | Privacy-first | A `SameSite=None; Secure` cookie still goes out on third-party requests. `localStorage` is keyed by origin only, not by top-level site. | Medium, by design until M37 | ✅ Fixed (M37, ADR 019) |
| P7 | Privacy-first | There is no filter engine, route policy or DNS privacy yet. | Medium, by design until M37 | ✅ Fixed (M37, ADR 011, 012, 019) |
| P8 | Vendor-independent, user control | TLS trusts only the root list compiled into the binary (`webpki-roots`, Mozilla's list). The OS trust store and user or enterprise CAs are ignored, and trust changes only with a rebuild. | Low | ✅ Fixed (M37, ADR 018) |
| P9 | GPU-first | There are no compositing layers, and clipping and opacity are applied per item on the CPU side. `rotate()` and `skew()` don't render. | Medium, by design until M34 | ✅ Fixed (M34, ADR 017) |
| P10 | Power budget (secondary principle) | There's no frame budget, no background throttling, and no limit on total script time; Boa 0.17 can't be interrupted. | Medium | ✅ Fixed (M38a: a runaway page's renderer is ended, ADR 013; M38b: frame budget and hidden pages, ADR 020). Pages on threads still rely on Boa's limits. |
| P11 | Minimal-dependency (secondary principle) | `engine` pins `rayon = "=1.10.0"` exactly, with no stated reason, while `layout` uses `"1"`. An exact pin blocks updates. | Low | ✅ Fixed (audit fixes) |
| P12 | Memory-safe, zero-trust | A heap use-after-free when a page thread exits: `js` held script GC handles (rAF callbacks, listeners, pending fetch/XHR) in untraced captures, which Boa's teardown freed out of order. Reachable from any page that registers a callback; 16 of 300 unit-test runs aborted. Found in the performance audit (2026-09-26). | **High** | ✅ Fixed (performance-audit fixes, ADR 015) |

These hold, and the audit found nothing against them:
- memory-safe: no `unsafe`, and it's forbidden in every crate (but see P12:
  `unsafe` in a dependency was reachable through our untraced captures —
  found two days later, under AddressSanitizer);
- thread-safe: page threads with state they own, a thread-safe boundary checked at compile time, poison-tolerant locks;
- no phone-home;
- every parser has a fuzz target.

### Architecture review (2026-09-26)

Asked "is this architecture modular, maintainable and scalable for the long
term?", the review found the outer seams strong and several internals not.
What will age well: the enforced layering (core never depends on platform,
hosts only on `engine`, no `unsafe`, all checked by self-tested scripts); the
message-based engine API with plain `Send` data (ADR 005), which let process
isolation change the transport and not the API; traits at the replaceable
edges (`NetworkRoute`, `Renderer`, the shell trait, now `RendererLauncher`);
and the engineering discipline (ADRs, a Definition of Done, fuzzing,
incremental-versus-fresh oracles). What will not:

| # | Finding | Why it matters over decades | Where it goes |
|---|---|---|---|
| A1 | No process isolation: every page in the browser's process. | Memory safety ended at our own code (P12 was in Boa), so one corrupting page owned the browser; zero-trust checks shared its address space. | ✅ Done: [process isolation](milestones/process-isolation.md), ADR 016; site isolation and storage in the browser process, [M41](milestones/milestone-41-site-isolation.md), ADR 025. Remaining — sandboxes beyond Linux — §4. |
| A2 | The DOM is an owned tree of `Element`s: string tags, attributes in a `Vec`, no parent pointers, positions found through side indexes, text nodes not first-class, no `insertBefore`. | Every crate is written against it, so each milestone built on it raises the cost of replacing it. Mature engines use an arena of nodes addressed by id, with interned names. | ✅ Done in M39 (ADR 021); interned names in M40b (ADR 024) |
| A3 | Script bindings: elements named by `id` string, a new wrapper per `getElementById`, GC handles held outside Boa's heap (ADR 015), validity and editability logic copied between `js`, `style` and `accessibility`. | This does not stretch to the thousands of DOM APIs real sites use; engines that got there generate bindings from WebIDL. | ✅ Done in M40 (ADR 023): bindings generated from `crates/script/webidl/`, one wrapper per `NodeId`, handles traced, one rule each in `dom` (M39) |
| A4 | The JS engine is not behind an interface: Boa's types run through `js` (0.22 since M38a). | The architecture doc calls JS "a replaceable subsystem behind a runtime interface", but no such interface exists; Boa cannot be interrupted (its advisories were cleared by the M38a upgrade). | ✅ Done in M40 (ADR 023): `script::ScriptRuntime`, with Boa and a stub behind it; Boa still offers no interrupt |
| A5 | Layout boxes use absolute coordinates. | Moving a subtree touches every box in it (M31's remaining O(page) work). | ✅ Done in M39b (ADR 022): positions are in the parent's frame, and a box can shift its children's frame — a moved subtree is O(1), and fresh layouts keep their exact positions. (M34 found that O(1) moves also need `LayoutReuse`'s per-element positions made relative; its layers did not need relative boxes — items stay in document space under their layer's matrix.) |
| A6 | CSS properties are parsed and applied by matching strings, one by one. | Full CSS has hundreds of properties; engines generate the property code from one table. | Backlog, triggered by M35 (text adds many properties) |

The shape matches the successful from-scratch engines (Servo: Rust, parallel
style; Ladybird: generated bindings, multi-process); the gaps were A2–A4 (closed
by M39 and M40, interned names aside).

### Principles-audit fixes — done

P1–P3 and P11 are fixed; `milestones/principles-audit-fixes.md` has the
details and the hardware run. In short:
- the glyph atlas has a hard cap (`text::MAX_ATLAS_HEIGHT`), skips glyphs
  that don't fit, and clears between frames without thrashing;
- every GPU texture and the surface are checked against the device's
  limits, and oversized images are scaled down;
- a GPU error handler replaces wgpu's panic;
- image textures have a byte budget.

The remaining rows (P4–P8, P10) were fixed by M36–M38 below.

### Performance-audit fixes — done

A code audit for time complexity, concurrency and parallelism; every finding
is fixed with a test, oracle, guardrail or fuzz input that keeps it fixed.
`milestones/performance-audit-fixes.md` has each one, the before/after
measurements and the hardware run. In short:
- selector matching is dynamic programming, linear in compounds × context,
  and selectors are capped at 64 compounds (it was exponential);
- scroll, caret and focus frames draw from a display list built once per
  layout and indexed by band;
- intrinsic sizes, the rule index, `@keyframes`, parsed inline styles and
  ASCII advances are kept, not recomputed; custom properties are shared;
- fetches run on a bounded pool (16 workers, 6 per host); images decode in
  parallel, under one deadline, and appear as they arrive
  (`ImageLoading::Progressive`);
- the glyph atlas is shared and uploaded by band; GPU buffers are reused;
- the detached-element store is indexed and freed when unreachable;
- P12, a heap use-after-free found along the way, is fixed, with a guardrail
  and a sanitizer CI job.

### Milestone 34 — Compositing & GPU depth — done

> Done with gaps (2026-09-27): `milestones/milestone-34-compositing.md`,
> ADR 017. Every acceptance criterion below is met; A5 and display lists
> rebuilt in part on a layout change moved to M39, then to M39b. The scope is kept as
> planned, for the record.

**Goal:** the GPU-first pieces that architecture doc §5 describes and that are
still missing: retained layers, clipping on the GPU, transforms that render,
batching and bounded GPU memory.

**Starts from:** M31's damage information and stable identity, and M33's
clip, overflow and opacity semantics. M33 clips rects, images and glyphs per
item, on the CPU side. M30 provides the GPU thread.

**In scope**
- `PushClip`/`PopClip`, `PushTransform`/`PopTransform` and
  `PushOpacity`/`PopOpacity` in the display list. Clips map to scissor
  rects, and rounded clips use a stencil or an SDF.
- A transformed-quad path, with a per-instance 2×3 matrix, so that
  `rotate()` and `skew()` draw.
- Compositing layers. Scroll containers, `opacity < 1`, `transform` and
  `will-change` get their own render targets. Scrolling and transform
  animations then cost a composite, not a repaint.
- `opacity` as a group layer (today it multiplies per item).
- Image draw batching by atlas or array. LRU texture eviction inside the
  byte budget from the audit fixes (P3), so old images are evicted, not
  refused.
- Glyph atlas eviction and compaction, built on the hard cap from the audit
  fixes, so frequently used glyphs survive instead of the whole atlas being
  cleared. Today a hostile page's text that alone overflows the atlas is
  partly skipped and re-tried about once a second. The subpixel positioning decision is recorded.
- Damage-rect presentation, using M31's damage.
- Decode and raster work moved off the critical path, to a `rayon` pool that
  delivers results to the GPU thread.
- **Layout boxes positioned relative to their parent (architecture review
  A5).** Moving a reused subtree then costs O(1) rather than a walk over
  every box; hit testing and the display list accumulate offsets. The
  incremental-versus-fresh layout oracle covers the change.

**Out of scope:** WebGL and WebGPU for content, and canvas 2D.

**Acceptance criteria**
- [x] Golden scenes for clip, rotate, opacity and layers on the corpus.
- [x] A transform animation on a promoted layer does no cascade, layout or
      display-list work. Counters assert this.
- [x] GPU memory stays bounded under an adversarial page (thousands of images
      and glyph sizes). A test asserts the cap.
- [x] The draw-call count for an image-heavy page drops against a recorded
      baseline.
- [x] The paint-only frame cost M33 added (about 8%) is re-measured. It is
      recovered or explained.

### Milestone 35 — Text subsystem — done

> Done with gaps (2026-09-28): `milestones/milestone-35-text-subsystem.md`,
> ADR 009. The script snapshots still owe a person's one-time sign-off; the
> corpus needs a re-record to include web fonts. The scope is kept as
> planned, for the record.

**Goal:** text as its own subsystem. Today it is Latin-only, with one bundled
font and no shaping, bidi or fallback.

**Why it matters:** text styling tops the corpus report. After M33, of 56
pages, `font-family` is used by 50, `text-decoration` by 49, `font-weight` by
49 and `line-height` by 46.

**Blocker:** `fontdue` 0.8 hangs on a font with one corrupted `cmap` byte
(`fuzz/known-issues/fontdue-cmap-hang.md`). Web fonts cannot ship on it, so
the parser chosen in ADR 009 must survive that input.

**In scope**
- `font-weight`, `font-style`, `font-family`, `line-height`,
  `text-decoration` and `text-transform`.
- Font fallback. A font database sits behind a trait: the shell enumerates
  system fonts and the engine never does. Generic families and
  per-character fallback.
- Shaping, with `swash` or `rustybuzz` (pure Rust, no FFI). ADR 009 records
  the dependency, vendor and `unsafe` assessment. Ligatures, kerning and
  complex scripts.
- Bidi (UAX #9) and line breaking (UAX #14), plus the `direction` and
  `unicode-bidi` properties.
- Web fonts. `@font-face` fetched through `NetworkContext`, with CORS for
  fonts, format sniffing, size and glyph-count caps, and sanitization
  before parsing.
- A text metrics cache keyed by font, size and string.
- A font list that can be filtered from day one. Which fonts content sees is
  an M37 fingerprint policy.

**Out of scope:** vertical writing modes, a variable-font axes UI, colour
emoji and IME.

**Acceptance criteria**
- [x] Arabic, Hebrew, Devanagari and CJK snapshots render in the correct
      order with correct shaping. A human verifies them once, then they are
      frozen as goldens. *(Rendered and frozen; the human sign-off is owed.)*
- [x] Malformed and oversized font fuzzing runs clean, including the
      `fontdue` hang input.
- [x] A hostile web font (huge glyph count, recursive composite glyphs) is
      rejected within a bounded time and memory budget.
- [x] The `font-*`, `text-decoration` and `line-height` rows leave the top of
      the corpus report.

### Milestone 36 — Capability layer & Browser Core seed — done

> Done with gaps (2026-10-11):
> `milestones/milestone-36-capabilities-and-browser-core.md`, ADR 010.
> Storage persists in thread mode only, and the network capability is
> enforced inside the renderer in process mode — both → M41 (done there). The scope is
> kept as planned, for the record.

**Goal:** make capability-based real. Authority is granted, scoped and
revocable, never ambient. Add the minimum Browser Core (sessions,
permissions, profiles and settings) that later work needs.

**Already done:** `ImageCache`'s `FileAccess` is a denied-by-default
capability (hardening pass).

**In scope**
- A `browser_core` crate, above `engine` and below the shells, with:
  - `Profile`, which owns the `NetworkContext`, storage and settings;
  - `Session`, a list of pages or tabs;
  - `PermissionStore`.
- `Capability` types: unforgeable handles, not strings. `Network`,
  `Storage(Origin)`, `Clipboard`, `Geolocation` and so on. APIs that are not
  implemented yet exist as denied-by-default stubs, so they can never be
  wired ambiently.
- The `fetch`, XHR and storage bindings in `js` receive capability handles
  when constructed. A page without `Network` cannot fetch.
- Permission requests modelled as data. Decisions are scoped by origin,
  revocable and audited as M27 events. The prompt UI belongs to the shell.
- Storage persisted to disk under the profile. `localStorage` is
  in-memory today (hardening pass).
- Typed, versioned, validated `Settings`, persisted under the profile. The
  first settings are scroll direction, scroll speed, default zoom and a
  privacy-mode flag for M37.
- Multi-page wiring in `app`: `Vec<Page>` behind `Session`, and switching the
  active page.
- Extension hook points designed and documented, but not built.

**Out of scope:** an extensions runtime, sync, bookmarks, history UI and tab
chrome.

**Acceptance criteria**
- [x] A page built without `Network` cannot `fetch` (P4). The rejection is a
      capability error, not a network error.
- [x] No global mutable state is reachable from the `js` bindings. A CI
      script checks this.
- [x] Requesting a capability that was never granted cannot be expressed,
      checked at compile time where possible.
- [x] Settings round-trip through disk, reject corrupt or out-of-range
      values, and default safely.
- [x] Two pages from different origins cannot observe each other's state.
- [x] ADR 010 records the capability representation.

### Milestone 37 — Privacy layer — done

> Done with gaps (2026-10-11): `milestones/milestone-37-privacy-layer.md`,
> ADR 011, 012, 018, 019. WireGuard is evaluated, not built; no third-party
> filter list ships (they go stale, ADR 019). The scope is kept as planned, for the
> record.

**Goal:** every category of data crossing the privacy boundary
(architecture doc §6) has a named, tested policy.

**Already done:** request hygiene (a frozen UA, `Accept-Language`,
`Referer` policy), a site-partitioned HTTP cache, origin-partitioned
`localStorage`, `SameSite`, and host-only and `Secure` cookie rules
(hardening pass and M28).

**In scope**
- **Route policy (§6.1).** `NetworkRoute` objects behind a `RoutePolicy` with
  an explicit precedence: Private-Browsing override, then per-origin rule,
  then profile default, then `DirectRoute`. ADR 011 records it. The policy
  attaches to `NetworkContext::with_route`. Routes, in order of value:
  - `ProxyRoute` (HTTP and SOCKS);
  - `SystemVPNRoute`;
  - `WireGuardRoute`, evaluated behind the trait.

  The project ships no VPN service.
- **Filter engine.** An EasyList/EasyPrivacy-format parser and matcher, plus
  an AI-crawler list. Blocking happens at the network layer and is always
  on. Memory is bounded, and the parser is fuzzed because list files are
  untrusted. Lists are bundled or fetched only when the user asks.
- **Third-party cookie and storage blocking,** and partitioning by top-level
  site, using M28's `Site`. Today a `SameSite=None; Secure` cookie still
  rides third-party requests, and `localStorage` is keyed by origin rather
  than by (top-level site, origin) (hardening pass).
- **Fingerprint policy.** One `FingerprintPolicy` consulted by the bindings.
  It covers:
  - the font list;
  - bucketed screen and viewport sizes;
  - coarsened `performance.now` and rAF timestamps;
  - any future readback.
- **DNS privacy.** DoH with a configurable resolver and no hard-coded
  single vendor (ADR 012).
- **`Date` and timezone (P5).** The fingerprint policy covers `Date`: it
  limits how precise `Date.now()` and `new Date()` are, and decides what
  timezone content sees (the real one, UTC, or a user setting).
- **Certificate trust store (P8).** Decide whether to trust the OS store,
  the bundled `webpki-roots`, or both. Support user and enterprise CAs.
  Record the decision in an ADR.

**Out of scope:** a relay service, multi-hop routing, the content classifier,
and extension-supplied lists.

**Acceptance criteria**
- [x] A policy table lists each row of the §6 boundary table, with its
      policy and the test that proves it.
- [x] The filter engine blocks known-bad URLs in a golden corpus, stays
      within a documented per-request budget, and fuzzes clean.
- [x] Third-party isolation is verified with two sites embedding the same
      third party.
- [x] Route precedence is unit-tested for every override combination.
- [x] An unreachable `ProxyRoute` fails hard. It never falls back to a
      direct connection.
- [x] Scripts observe `Date` at the policy's resolution and timezone. A
      test proves it (P5).
- [x] A `SameSite=None; Secure` cookie isn't sent on a third-party request
      under the default policy (P6).

### Milestone 38 — split into 38a and 38b

M38 was split on 2026-10-19: it held six items plus the Boa upgrade, and only
the upgrade has a deadline (the `cargo audit` exceptions expire on
2026-12-31). Process isolation (ADR 016) also changed the script-budget
problem: a renderer can be killed, so a runaway script can be bounded without
an interrupt inside Boa. `about:engine`, deterministic replay and the thermal
hooks moved to the backlog (§4, operations); none of them is needed for the
power-budget principle.

### Milestone 38a — Script time bound & Boa upgrade — done

> Done with gaps (2026-10-19): `milestones/milestone-38a-script-bound-and-boa.md`,
> ADR 013. Thread mode is not bounded by the watchdog; no script interrupt
> exists in Boa 0.22 (→ M40). The scope is kept as planned, for the record.

**Goal:** a runaway script cannot hold a page or the browser (P10's script
half), and Boa is current, before the advisory exceptions expire.

**Already done:** loop-iteration and recursion limits in scripts (hardening
pass); every page in a renderer process the browser can end (ADR 016).

**In scope**
- **ADR 013:** the mechanism. The browser process watches each renderer: a
  renderer that has not finished a tick for 10 s is killed outright and its
  page reported as crashed. Thread mode, which cannot kill a thread, stays
  bounded by Boa's runtime limits (ADR 013 records why Boa 0.22 offers no
  more).
- The watchdog in `engine`'s process host, with a guard, a configurable bound
  for tests, and tests with an in-process and a real renderer.
- Boa 0.17 → 0.22, ported in `js` (not redesigned: M40 replaces the bindings),
  with ADR 015's GC-handle rules re-checked against the new version.

**Acceptance criteria**
- [x] An infinite-loop page in process mode is killed within 10 s of its tick
      (plus one watchdog check interval), reported as `PageCrashed`; other
      pages and the host keep running.
- [x] A page that is merely busy for less than the bound is not killed.
- [x] The Boa upgrade is done, and both `cargo audit` exceptions are gone.
- [x] AddressSanitizer runs over `js` and `engine` are clean on the new Boa.

### Milestone 38b — Frame and power budget — done

> Done with gaps (2026-10-19): `milestones/milestone-38b-frame-and-power-budget.md`,
> ADR 020. Off-screen `background-color` transitions are skipped since the
> gap pass (2026-10-21); window occlusion is not a visibility source. The scope is kept as
> planned, for the record.

**Goal:** "a page must not be able to peg the GPU or CPU indefinitely" as an
enforced property for frames and hidden pages (P10's remaining half).

**In scope**
- A per-frame budget (16.6 ms by default). Over budget, the frame degrades in
  a defined order:
  1. skip off-screen transitions;
  2. coalesce rAF;
  3. lower the frame rate.

  It never stalls input.
- Background-page throttling. Hidden pages have their timers and rAF clamped
  or suspended. A page over its CPU budget is throttled and reported.
- A page wakes when a fetch completes, instead of polling every 8 ms (M30) —
  a hidden page cannot reach near-zero CPU while it polls.

**Out of scope:** crash-report upload (no mandatory telemetry) and remote
diagnostics.

**Acceptance criteria**
- [x] A synthetic heavy page holds frame pacing by degrading, without adding
      input latency. Counters assert this.
- [x] A hidden page's CPU use falls to near zero, as measured on hardware.

---

### Milestone 39 — Arena DOM

> Done with gaps (2026-10-20): `milestones/milestone-39-arena-dom.md`,
> ADR 021. Interned names were built, measured as a net loss when interned
> after parsing, and deferred to M40 (then M40b); the layout items (A5, partial display
> lists) moved to M39b. The scope is kept as planned, for the record.

**Goal:** the node model long-lived engines use (architecture review A2).

**In scope**
- An arena of nodes addressed by `NodeId`, each with parent, first/last
  child and sibling links; text and comment nodes first-class.
- Interned tag and attribute names (atoms); attribute lookup by atom.
- The full tree mutation API: `insertBefore`, `replaceChild`, `removeChild`,
  `cloneNode`, `createTextNode` — each recorded for incremental restyle and
  layout (M31).
- `style`, `layout`, `accessibility`, `js` and `engine` migrated; the
  duplicated validity and editability logic moved into `dom`, once.
- The Milestone 31 restyle and layout oracles pass unchanged before and after
  (the migration's merge gate); the corpus goldens do not move.
- **From M34 (moved to M39b, not done here):** layout boxes positioned relative to their parent
  (architecture review A5), with `LayoutReuse`'s per-element positions made
  relative too, so moving a reused subtree is O(1); and display lists
  rebuilt only for the boxes a layout change touched (stable box identity).

**Acceptance criteria**
- [x] No `&mut Element` tree walks remain outside `dom`.
- [x] Mutating a node costs O(1) link updates, asserted by counters.
- [x] The oracles and every corpus golden pass unchanged.

### Milestone 39b — Layout coordinates & partial display lists

> Done with gaps (2026-10-20): `milestones/milestone-39b-layout-coordinates.md`,
> ADR 022. Relative positions took the form of box frames (a box shifts
> its children's frame) so fresh layouts keep their exact positions; a
> changed element's parent still visits all its children, and a fresh
> layout still draws from a build of its own. The demo page's first frame
> is 0.05 ms slower and scroll frames 2 µs slower, so the last criterion
> is not met. The scope is
> kept as planned, for the record.

**Goal:** a layout change costs what it changes. Architecture review A5,
plus the O(page) work M31, M34 and M39 left in change frames and clicks.

**Starts from** (checked against the code on 2026-10-20):
- `layout::LayoutBox` holds absolute `x`/`y`. `LayoutReuse::take` moves a
  reused subtree by shifting every box in it (`shift_descendants`,
  `carry_over` in `crates/layout/src/reuse.rs`): O(subtree) per move.
- `rendering::PageDisplayList` is built from the whole layout tree once per
  layout; a frame whose layout changed, and every frame of a page with a
  sticky box, builds it again in full (`build_display_list_for_frame`,
  called from `engine::frame`).
- `layout::hit_test` descends into every box (no pruning by bounds, since
  descendants may overflow), and `engine::frame::find_box_by_element_key`
  searches the whole tree: two O(page) walks per click.
- One-DOM-change frames on the 10,000-row benchmark take about 0.75 ms,
  29–34% more than after M39's step 2, not attributed; most of that frame is
  the two walks above (`milestones/milestone-39-arena-dom.md`).

**In scope**
- **ADR 022**, before any code: what a box's position is relative to (its
  parent's border-box origin is the obvious choice), where absolute positions
  are still computed, and how floating-point summation is kept from moving
  goldens (or how goldens are re-blessed: per-pixel comparison, every
  difference explained, as in M34).
- Layout boxes positioned relative to their parent. Layout algorithms may
  keep computing absolute positions internally and store relative ones when
  a child is attached.
- `LayoutReuse`'s per-element positions made relative too, so moving a
  reused subtree is O(1).
- Every consumer accumulating offsets: `rendering` (display list, page list,
  CPU raster), hit testing, `engine` (caret, focus ring, click-to-position,
  box search), compositor layers and transform animations, scroll
  containers, sticky positioning. About 350 coordinate reads.
- A subtree bounds rectangle per box (its own box united with its
  descendants' overflow, clipped where it clips), kept by layout and by
  moves, so hit testing and box searches prune to O(depth).
- Display lists rebuilt only for the boxes a layout change touched (stable
  box identity: the element key), spliced into the page list; sticky boxes
  handled without a full build.
- Re-measure M39's unattributed DOM-change regression afterwards.

**Out of scope:** tiled layer rasterization, a GPU process, 3D transforms.

**Acceptance criteria**
- [x] Moving a reused subtree costs O(1) box updates, asserted by a counter.
- [x] A one-DOM-change frame on a 10,000-row page builds display items only
      for the boxes its change touched, asserted by a counter.
- [x] A click on a 10,000-row page examines O(depth) boxes in hit testing
      and box search, asserted by a counter.
- [x] The incremental-versus-fresh layout oracle and the page-display-list
      equivalence test pass; the corpus goldens are unchanged, or re-blessed
      after a per-pixel comparison with every difference explained.
- [ ] `frame_bench` shows no frame slower than M39, measured on a quiet
      machine. (DOM-change frames 45–72% faster; the demo's first frame
      0.05 ms and 1k/10k scroll frames 2 µs slower; the machine was loaded.)

### Milestone 39c — Changes that keep their size, patched in place

> Done (2026-10-20): `milestones/milestone-39c-patch-in-place.md`, ADR 022
> §8–10. A one-element change on 10,000 rows: 7 ms → 0.33 ms. The scope is
> kept as planned, for the record.

**Goal:** a change that leaves its element's size alone costs that element,
not its parent's other children or the page's display list.

**In scope:** layout patched into the previous tree when the changed
element's box and margins come out the same (ADR 022 §8); the page's
display list patched to match (§9); the oracles extended to patched frames.

**Acceptance criteria**
- [x] A one-element change on a 10,000-row page that keeps the element's
      size visits O(depth) boxes in layout and builds only that element's
      items (the rest renumbered, not moved or built), asserted by counters.
- [x] The layout, page-list and hit-test oracles pass with patches in play;
      the corpus goldens are unchanged.
- [x] `frame_bench`'s one-DOM-change frame on 10,000 rows is under 1 ms,
      and no frame is slower than M39b.

### Milestone 40 — Script runtime interface & generated bindings

> Done with gaps (2026-10-20): `milestones/milestone-40-script-runtime-and-bindings.md`,
> ADR 023. The trait is in a new core crate, `script` (in `engine` it would
> be a dependency cycle; `engine` re-exports it). Interned names, which
> `AUDIT.md` had assigned here, are M40b. Boa 0.22 has no interrupt, so its
> runtime offers none; the stub runtime exercises the interface.

**Goal:** JavaScript as the replaceable subsystem architecture doc §2 says it
is (architecture review A3, A4).

**In scope**
- A `ScriptRuntime` trait in `engine`: evaluate, run jobs, call callbacks,
  interrupt (where the runtime can; ADR 013), report errors. Boa, upgraded in
  M38a, is one implementation.
- Bindings generated from WebIDL fragments for the implemented interfaces;
  wrappers keyed by `NodeId` (M39), one per node, cached.
- GC handles traced by the runtime, not released by hand (ADR 015's
  follow-up; `scripts/check-gc-handles.sh` then has nothing to allow).

**Acceptance criteria**
- [x] A test runtime (a stub implementing the trait) drives the DOM tests.
- [x] `getElementById` returns the same wrapper for the same node.
- [x] `scripts/gc-handle-captures.txt` is empty.

### Milestone 40b — Interned names — done

> **Done** (2026-10-21): [milestone file](milestones/milestone-40b-interned-names.md),
> ADR 024. Measured against M40: corpus parse 7–10% faster, 10k first frames
> 7–8%, DOM changes 10%, a selector-heavy restyle 3–6%. Class and id
> *values* stay strings (backlog).

**Goal:** names compared as integers where they are compared most (ADR 021
§5), measured as a gain.

**In scope**
- Tag and attribute names as atoms: a fixed table of well-known names equal
  in every document, a per-document table for the rest (never global: a page
  inventing names cannot grow memory that outlives it).
- Interned where names are born — the HTML parser, `createElement`,
  `setAttribute` — not after parsing (M39 measured that as a loss: 7% on
  first frames, 60% on clicks, from heap holes).
- Selector matching compares atoms (type, attribute and `id` selectors).

**Acceptance criteria**
- [x] `frame_bench` first frames and DOM-change frames on the 10k pages are
      no slower than M40, and selector-heavy restyles faster, on a quiet
      machine. (Faster on both; measured at load 3–7, as M40 was.)
- [x] A page inventing a million attribute names grows only its own
      document's table, freed with it (counted).
- [x] The restyle and layout oracles and the corpus goldens pass unchanged.

### Milestone 41 — Site isolation & browser-process storage — done

> Done with gaps (2026-10-22): `milestones/milestone-41-site-isolation.md`,
> ADR 025. Sandboxes beyond Linux were not built (they need FFI and those
> machines) → §4. One spare serves every site (a spare has held nothing).

**Goal:** finish what process isolation (ADR 016) started (architecture
review A1).

**In scope**
- Process per site: a cross-site navigation moves the page to a renderer for
  the new site; the origin lock (ADR 016 §5) narrows to that site.
- `localStorage` (and M36's persisted storage) served by the browser process
  over the renderer protocol, so pages of one origin share it again in
  process mode.
- Sandboxes beyond Linux (macOS `sandbox_init`/App Sandbox, Windows
  AppContainer), each behind `sandbox::enter`.
- A spare renderer per site class, and renderer reuse limits.
- Script network grants enforced by the browser process (M36 enforces them
  inside the renderer; ADR 010, Consequences): requests the renderer cannot
  mislabel as its document loader's.

**Acceptance criteria**
- [x] A cross-site link lands in a different process; same-site does not.
- [x] A renderer whose page has no network grant cannot get a script-style
      request (credentialed cross-origin) made by the browser.
- [x] Two pages of one origin share `localStorage` in process mode.
- [~] The sandbox self-test passes on each supported platform. *(Linux;
      macOS and Windows have no sandbox yet.)*

## 4. Backlog — not yet scheduled

Ordered by how many of the 56 corpus pages use each feature, as measured
after M33 by `cargo run -p conformance --release -- run`. The report ranks
unsupported features. The top of this list becomes the next milestone, unless
a gap blocks real pages first.

| Pages | Gap | Area |
|---|---|---|
| 43 | `cursor` | Shell or input |
| 41 / 31 | List markers (`list-style`, `list-style-type`) | Layout |
| 35 / 35 / 33 / 31 | `:checked`, `:first-of-type`, `:has()`, `:disabled` | Selectors |
| 28 | `@supports` | CSS parsing |
| 28 | `linear-gradient()` backgrounds | Paint (after M34) |

Every other item a milestone left open, grouped by area. The milestone
that deferred each item is in brackets, and its file has the reasoning.

- **Layout:**
  - percentage heights against block containing blocks [M33];
  - `table-layout: fixed`, collapsing borders and `<col>` widths [M33];
  - Grid named lines, dense and column auto-flow, track alignment and
    subgrid [M33];
  - floats that overflow their parent, a simplification in ADR 014 [M33];
  - column-direction flex wrap, and intrinsic `height` keywords
    (`min-`/`max-`/`fit-content`) [Phase D];
  - multi-column and writing modes [M33].
- **Box and paint:**
  - inset shadows, gradients and filters [M33];
  - per-corner, elliptical and `%` `border-radius`, and rounded borders
    (only the fill is rounded today) [Phase C];
  - `groove`, `ridge`, `inset` and `outset` borders [Phase C].
- **Text** (the parts M35 does not cover): `white-space: pre-line`, and
  exact leading and trailing whitespace in `pre` [Phase B].
- **Power:** skipping off-screen `@keyframes` animations, and transitions of
  properties that can move a box (only `background-color` ones are skipped,
  since the gap pass) [M38b]. (The window's occlusion is a visibility
  source since the gap pass.)
- **Scrolling:** `scrollTop` from script, and sticky positioning inside a
  scroll container [M33].
- **Selectors and CSS:**
  - `:hover` updated when the page scrolls under a still pointer (it
    follows pointer moves since the gap pass) [M4];
  - `:nth-last-child` and `:*-of-type` [Phase A];
  - structural pseudo-classes on ancestor compounds [Phase A];
  - the `[attr=v i]` flag [Phase A];
  - cascade layers, container queries and non-width media features
    [Phase A].
- **Animation:**
  - why libreoffice-home's and nodejs-about's text changed glyph width
    over 60 frames of an animation that changes nothing (seen when the gap
    pass stopped such animations keeping a page awake; goldens re-blessed
    at the idle capture) [M22];
  - 3D transforms (`matrix3d`, `perspective`) [M22];
  - scroll-driven animations and View Transitions [M22];
  - animatable properties beyond the allowlist, which is kept small on
    purpose for the power budget [M22].
- **Events:** `currentTarget`, the `bubbles` and `cancelable` flags,
  `composedPath()` and custom events [M20].
- **Forms and input:**
  - text selection, including `getSelection()` [M19, M23];
  - drag-and-drop, within the page and from other apps [M23];
  - context menus and a system clipboard [M23];
  - `<input type="file">` with a native picker [M23];
  - `range`, `date` and `color` inputs [M19];
  - rich `contenteditable` (`execCommand`, paste sanitization) [M23].
- **Navigation:**
  - history (back and forward) [M21];
  - POST forms and multipart form data [M21];
  - fragment navigation and `history.pushState` [M21];
  - `LoadRequest::Html` fetches no linked stylesheets or images, unlike a
    URL load [M29].
  - `url()` in an inline `style` attribute is never fetched (only
    stylesheets' are) [performance audit].
- **Frames:** `<iframe>`, `postMessage` and embedded storage keys. The
  `origin` types are ready (`StorageKey::embedded`) and nothing else is
  built [M28]. Third-party cookie and storage blocking is done (M37),
  tested through subresources.
- **JS and DOM API:**
  - window events (`load`, `DOMContentLoaded`, `resize`) — `window` is an
    `EventTarget` since the gap pass, but nothing fires at it — and
    `location` [M40];
  - `async`, `defer` and external `<script src>` [M11];
  - ~~`insertBefore`, `replaceChild`, `cloneNode` and `createTextNode`
    [M18]~~ — done in M39;
  - the long tail of the DOM API [M11] — since M40 each is an IDL line and a
    typed host method (`crates/script/webidl/`); `querySelectorAll` is
    where the corpus's mdBook page's script now stops;
  - wrappers of *detached* nodes kept while they carry expando properties
    (nodes in the document keep theirs since the gap pass); the IDL
    deviations marked in `crates/script/webidl/` [M40];
  - a runtime that can be interrupted behind `ScriptRuntime::interrupt_handle`
    (Boa 0.22 offers none) [M40].
- **Network:**
  - a WireGuard route over a user-space TCP/IP stack [M37, ADR 011 §5];
  - macOS and Windows system trust stores [M37, ADR 018];
  - replacing the `socks` 0.3 dependency (with `ureq` 3's connector API)
    [M37];
  - cosmetic (element-hiding) and regular-expression filter rules [M37];
  - CORS preflight, and methods other than GET [M28];
  - real async XHR [Phase F];
  - `Vary`-aware caching [M28];
  - streaming response bodies [Phase F];
  - WebSockets and SSE [Phase F];
  - IndexedDB and the Cache API [Phase F];
  - HSTS and certificate pinning, next to M37 [M8].
- **HTML:** the full insertion-mode state machine, encoding detection,
  templates, custom elements and shadow DOM [M3]. These are continuous
  hardening, driven by the corpus and the fuzzers.
- **Images:** AVIF, `<picture>`/`srcset`, animated GIF and WebP, and
  `object-position` [M7, Phase E].
- **Accessibility:**
  - the OS screen-reader bridge (`accesskit`) [M24];
  - exposing the tree through the engine API [M29];
  - the full `accname` algorithm (`aria-hidden`, `title`, generated
    content) [M24];
  - explicit ARIA roles and states [M24].
- **Style internals:** CSS property parsing and application generated from
  one property table rather than matched by string [architecture review A6].
- **Process isolation** (the parts M41 does not cover): sandboxes beyond
  Linux — macOS `sandbox_init`/App Sandbox, Windows AppContainer — which
  need FFI (an entry in `scripts/unsafe-exceptions.txt`) and a machine of
  each platform to build and self-test on [M41]; a GPU process; a network
  process separate from the browser process [ADR 016]; a `localStorage`
  cache in the renderer, if a page is measured to suffer from a round trip
  per operation [M41].
- **Performance:**
  - A change that alters its element's size still has its parent visit all
    its children (each moved over in O(1)), and the page's display list
    moves every item and re-bands it: O(page), ~7 ms on 10,000 rows. (Size-
    keeping changes are patched in place since M39c.) [M39b]
  - Patching (M39c) is ruled out by out-of-flow boxes placed from outside
    the changed element, and by opacity, transform or scroll ancestors for
    the display list — conservative checks [M39c]. A page with a sticky box
    still builds its list every frame [M31].
  - Each element with element children copies its ancestor list: O(depth)
    per element, bounded by 512 [performance audit].
  - `getElementById` builds a new wrapper (about 20 function objects) per
    call [performance audit].
  - The first frame is about 17% slower than before M31 [M31].
- **Hardware checks** (not code gaps, just never observed):
  - human input on the threaded window [M29, M30], and the tab and zoom
    shortcuts [M36].
  (GPU image drawing, paint order and live `--trace` were observed in M29
  and M34.)
- **Operations:**
  - a first real CI run [M25];
  - libFuzzer runs, which need `g++` [M25, M32];
  - OSS-Fuzz [M25];
  - burning down the clippy baseline, then CI with `-D warnings` [M25];
  - pixel comparison in the corpus (it compares scene JSON today) [M32];
  - the `cargo deny` duplicate-version warnings, which are informational
    [M25];
  - asserting the 100 MiB body-cap guard event [M27];
  - `about:engine` with live counters, deterministic replay (snapshots that
    record input and the clock), and thermal/power hooks as a shell trait
    [M38, split out of it].

**Deliberately not planned:**
- extensions runtime;
- tab and browser chrome UI;
- iOS and visionOS shells;
- a from-scratch JS engine;
- WebGL and WebGPU for content (curated API surface, architecture §6);
- canvas 2D, until the corpus shows demand;
- a JIT for Boa;
- service workers;
- the AI-content classifier;
- a relay service.

Video (`<video>`, codecs, A/V sync) has never been scoped. It needs its own
milestone-1-style scoping pass.

Each of these needs its own scoping pass when it is picked up. This plan only
makes sure none of them would require rewriting the foundation.

---

## 5. Architecture decision records

In `docs/adr/`. Each is written before the code it constrains.

| ADR | Decides | Status |
|---|---|---|
| 001 | Observability facade (`tracing`); no subscriber in core | Written (M27) |
| 002 | Public Suffix List vendoring and updates | Written (hardening pass) |
| 003 | Linear-time pattern matching for `pattern=` | Written (hardening pass) |
| 004 | Origin and Site types | Written (M28) |
| 005 | Engine API shape (message-based, `Send`) | Written (M29) |
| 006 | Page-thread actor model | Written (M30) |
| 007 | Panic containment; `panic = "unwind"` | Written (M30) |
| 008 | Dirty-tracking granularity and `node_id` identity | Written (M31) |
| 014 | Layout model: `<html>` root, UA origin, generated-content ids, boxed rare properties, float containment, stack contract | Written (M33) |
| 015 | Script GC handles held by Rust are released while Boa's heap is alive | Written (performance-audit fixes); superseded by ADR 023 §5 (M40) |
| 016 | Pages run in sandboxed renderer processes | Written (process isolation) |
| 017 | Compositing: structure in the display list, layers on the GPU | Written (M34) |
| 009 | The text stack: parser, shaper, rasterizer, bidi, line breaking, fonts | Written (M35) |
| 010 | Capability representation (and the extension hook points) | Written (M36) |
| 011 | Route precedence; routes fail closed; WireGuard evaluated | Written (M37) |
| 012 | DNS over HTTPS, with no default vendor | Written (M37) |
| 013 | Script time bound: the renderer watchdog; what Boa 0.22 offers | Written (M38a) |
| 018 | Certificate trust: bundled roots plus user CAs | Written (M37) |
| 019 | Privacy policies: filter engine, third parties, fingerprinting | Written (M37) |
| 020 | Frame and power budget: wake on completion, hidden pages, a degrading frame rate | Written (M38b) |
| 021 | The arena DOM: `ElementRef`, slots and links, fragments, the mutation API, one rule | Written (M39) |
| 022 | Layout coordinates: box frames, subtree bounds, partial display lists, patches in place | Written (M39b; §8–10 M39c) |
| 023 | The script runtime interface; bindings generated from WebIDL; one wrapper per node; traced GC handles | Written (M40) |
| 024 | Interned names: the `names` crate, born interned, a table per document, interned in place, selectors by atom | Written (M40b) |
| 025 | Site isolation: a renderer per site, pages moved on a response of another site, storage and the network grant enforced by the browser | Written (M41) |

---

## 6. Standing tracks

- **Docs:** each milestone updates its own file, `AUDIT.md` and §1 and §3 of
  this file. There is no other planning document. If one is needed, its
  content goes here.
- **Security review:** every milestone that adds an untrusted-input surface
  gets a focused review before it is merged. Those are web fonts (M35), and
  filter lists and DoH (M37).
- **Performance:** counters, not wall-clock time, gate regressions in tests.
  Wall-clock changes are measured against the previous milestone and
  recorded in the milestone file (as M31 and M33 did).
- **Dependencies:** each new dependency gets an ADR line covering its
  purpose, size, `unsafe` content, licence, maintainer health,
  replaceability, and whether it may appear in core crates.

## 7. Risks

| Risk | Mitigation |
|---|---|
| Boa cannot be interrupted (0.22 included), so a script budget can't be enforced in-thread. The audit exceptions expire on 2026-12-31. | M38a: the browser kills a renderer that stops finishing ticks (ADR 013); the Boa upgrade is done first in M38a. Thread mode keeps only Boa's loop and recursion limits. |
| `ttf-parser`, the parser for untrusted fonts since M35, is listed as unmaintained (RUSTSEC-2026-0192), and so is `rustybuzz`, which depends on it (RUSTSEC-2026-0206). | Both accepted with reasons in `deny.toml` and ADR 009: fuzz targets, hostile-font tests, and web-font bytes validated before parsing. If a parser bug turns up that upstream doesn't fix, replace the parser and shaper together, evaluated against the same fuzz corpus. |
| Incremental restyle or layout gets stale-state bugs. | The full-versus-incremental oracles are merge gates. Every new feature extends them. |
| New dependencies break the memory-safety or vendor stance. | The dependency ADR gate, a pure-Rust preference, and replaceability behind traits. |
| Scope creep toward "complete" web compatibility. | The curated API surface: a new Web API needs a written permission, fingerprint and capability policy first. |
| Estimates are optimistic. | Re-estimate after each milestone. M35 and M37 can be split. |
| M39 (arena DOM) touches every crate at once. | Done that way (four steps behind the oracles and goldens; ADR 021). |
| M39b's relative positions are summed in floating point, so absolute positions can differ in the last digits and move goldens. | Avoided by design (ADR 022: fresh layouts keep exact positions; only reused subtrees carry offsets). One golden moved, for a sticky-positioning bug fix, explained per pixel. |
| The renderer protocol drifts from the engine API (a field added to one and not the other). | The codec round-trip test covers every message; the scene-equality test compares thread and process mode frame by frame. |

## 8. Target scorecard

| Principle | Today | Target |
|---|---|---|
| Memory-safe | Met: no `unsafe` in our code, enforced in CI; dependencies exercised under AddressSanitizer in CI; GC handles traced, not released by hand (M40, ADR 023) | Unchanged |
| GPU-first | Met: GPU clips and transforms, opacity groups, compositor updates for transform animations and container scrolls, damage-based presentation, bounded GPU memory (M34); display lists updated in part after a layout change (M39b) | A page with a sticky box drawn without building its list every frame |
| Thread-safe | Met: page threads, GPU thread, `Send` boundary, contained panics | Unchanged |
| Capability-based | Met: script's network and storage are granted, scoped, revocable handles (M36); file access and process launching are host grants; the browser process enforces script grants and serves storage (M41) | Unchanged |
| Zero-trust | Mostly: every parser and the renderer protocol fuzzed and capped, GPU resources bounded, pages in sandboxed processes of one site each, whose requests and storage the browser checks (M41); no CI run observed | Every parser fuzzed in a running CI; sandboxes beyond Linux |
| Privacy-first | Met: a named, tested policy for every §6 row (M37) | Unchanged; WireGuard and system trust stores beyond Linux |
| Vendor-independent | Met: `cargo deny` bans | Unchanged |
| Observability | Partly: tracing and counters | Plus `about:engine` and deterministic replay (backlog, split out of M38) |
| Power budget | Met in process mode (M38a, M38b): pages woken, not polled; hidden pages paused; a degrading frame rate; runaway renderers ended | A runtime that can be interrupted behind M40's interface, so pages on threads are bounded too |
